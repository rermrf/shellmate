//! A shell session channel acting as a terminal backend.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Pty};
use shellmate_core::{OutputSink, PtyDims, TerminalBackend};
use tokio::sync::mpsc;

use crate::connection::Connection;
use crate::error::SshError;

/// Terminal type requested for the PTY.
pub const TERM: &str = "xterm-256color";

/// A session channel whose PTY and shell are already set up but which has not
/// been connected to a terminal yet. Data the server sends meanwhile is kept.
pub struct PendingShell {
    connection: Connection,
    channel: Channel<Msg>,
    dims: PtyDims,
    /// Messages that arrived before the shell request was answered.
    backlog: Vec<ChannelMsg>,
}

impl PendingShell {
    pub(crate) async fn open(connection: Connection, dims: PtyDims) -> Result<Self, SshError> {
        let mut channel =
            connection.inner.handle.channel_open_session().await.map_err(|err| SshError::Channel(err.to_string()))?;
        let mut backlog = Vec::new();

        channel
            .request_pty(
                true,
                TERM,
                dims.cols.into(),
                dims.rows.into(),
                dims.pixel_width.into(),
                dims.pixel_height.into(),
                &[(Pty::IUTF8, 1)],
            )
            .await
            .map_err(|err| SshError::Channel(err.to_string()))?;
        expect_reply(&mut channel, &mut backlog, "分配终端 (PTY)").await?;

        channel.request_shell(true).await.map_err(|err| SshError::Channel(err.to_string()))?;
        expect_reply(&mut channel, &mut backlog, "启动 shell").await?;

        Ok(Self { connection, channel, dims, backlog })
    }

    /// Connects the shell to `sink` and starts pumping data. `dims` is the
    /// terminal's current size; a window change is sent if it differs from the
    /// size the PTY was opened with. May be called from any thread: the pumping
    /// tasks are spawned on the connection's runtime.
    pub fn start(self, sink: OutputSink, dims: PtyDims) -> ShellChannel {
        let Self { connection, channel, dims: opened, backlog } = self;
        let (read, write) = channel.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let closing = Arc::new(AtomicBool::new(false));

        if dims != opened {
            let _ = tx.send(Command::Resize(dims));
        }
        let runtime = connection.inner.runtime.clone();
        runtime.spawn(pump_output(read, backlog, sink, connection.clone(), closing.clone()));
        runtime.spawn(pump_input(rx, write));
        ShellChannel { tx, closing, _connection: connection }
    }
}

/// Waits for the server's answer to a channel request, setting aside any
/// output that shows up in between.
async fn expect_reply(channel: &mut Channel<Msg>, backlog: &mut Vec<ChannelMsg>, what: &str) -> Result<(), SshError> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => return Err(SshError::Channel(format!("服务器拒绝了{what}请求"))),
            Some(ChannelMsg::Close) | None => return Err(SshError::Channel("服务器关闭了会话通道".into())),
            Some(msg @ (ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. } | ChannelMsg::ExitStatus { .. })) => {
                backlog.push(msg);
            }
            Some(_) => {}
        }
    }
}

enum Command {
    Data(Vec<u8>),
    Resize(PtyDims),
    Close,
}

/// How the remote program ended, as far as the channel told us.
enum Ending {
    Status(u32),
    Signal,
}

/// Reads the channel until it closes, then reports how it ended.
async fn pump_output(
    mut read: ChannelReadHalf,
    backlog: Vec<ChannelMsg>,
    sink: OutputSink,
    connection: Connection,
    closing: Arc<AtomicBool>,
) {
    let mut queue: VecDeque<ChannelMsg> = backlog.into();
    let mut ending: Option<Ending> = None;
    loop {
        let msg = match queue.pop_front() {
            Some(msg) => Some(msg),
            None => read.wait().await,
        };
        match msg {
            Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => sink.output(&data),
            Some(ChannelMsg::ExitStatus { exit_status }) => ending = Some(Ending::Status(exit_status)),
            Some(ChannelMsg::ExitSignal { .. }) => ending = Some(Ending::Signal),
            Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    if closing.load(Ordering::Acquire) {
        return;
    }
    match ending {
        Some(Ending::Status(code)) => sink.exited(Some(code)),
        Some(Ending::Signal) => sink.exited(None),
        None => match connection.disconnect_reason() {
            Some(reason) => sink.disconnected(format!("连接已断开：{reason}")),
            None if connection.is_closed() => sink.disconnected("连接已断开"),
            // The server closed this channel on its own while the connection lives on.
            None => sink.exited(None),
        },
    }
}

/// Forwards input and window changes to the channel in order.
async fn pump_input(mut rx: mpsc::UnboundedReceiver<Command>, write: ChannelWriteHalf<Msg>) {
    while let Some(command) = rx.recv().await {
        let sent = match command {
            Command::Data(bytes) => write.data_bytes(bytes).await,
            Command::Resize(dims) => {
                write
                    .window_change(
                        dims.cols.into(),
                        dims.rows.into(),
                        dims.pixel_width.into(),
                        dims.pixel_height.into(),
                    )
                    .await
            }
            Command::Close => break,
        };
        if sent.is_err() {
            break;
        }
    }
    let _ = write.close().await;
}

/// A running shell on an SSH connection; plugs into a terminal as its backend.
///
/// Keeps the connection alive. `write` and `resize` only queue work, so they
/// are safe to call from the UI thread. Output, remote exit (`exit-status`)
/// and connection loss are reported through the [`OutputSink`] given to
/// [`PendingShell::start`].
pub struct ShellChannel {
    tx: mpsc::UnboundedSender<Command>,
    closing: Arc<AtomicBool>,
    _connection: Connection,
}

impl TerminalBackend for ShellChannel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.tx.send(Command::Data(bytes.to_vec())).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    fn resize(&mut self, cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> io::Result<()> {
        let dims = PtyDims { cols, rows, pixel_width, pixel_height };
        self.tx.send(Command::Resize(dims)).map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.tx.send(Command::Close);
    }
}

impl Drop for ShellChannel {
    fn drop(&mut self) {
        self.close();
    }
}
