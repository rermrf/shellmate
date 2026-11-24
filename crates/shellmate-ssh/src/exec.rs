//! Commands on exec channels: [`Connection::exec`] runs one command and collects its output,
//! [`Connection::exec_stream`] follows a long-running one line by line. Neither allocates a PTY, so
//! the output is exactly what the command wrote and stdout and stderr stay apart.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use russh::client::Msg;
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::sync::mpsc;

use crate::connection::Connection;
use crate::error::SshError;
use crate::watchdog::Watchdog;

/// How much of each of stdout and stderr [`Connection::exec`] keeps by default (1 MiB).
pub const DEFAULT_MAX_OUTPUT: usize = 1 << 20;
/// How long a single line of [`ExecStream`] may be by default; the rest of a longer line is dropped.
pub const DEFAULT_MAX_LINE: usize = 256 << 10;
/// How many lines an [`ExecStream`] queues for a reader that is slower than the command; beyond that
/// the channel is not read any more, so the server has to wait (flow control).
const STREAM_QUEUE: usize = 256;
/// How much of the stderr of a streamed command is kept (the end of it).
const STDERR_TAIL: usize = 4096;
/// How long closing a channel may take before it is given up on.
const CLOSE_LIMIT: Duration = Duration::from_secs(2);

/// Limits of one [`Connection::exec_with`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecOptions {
    /// How long the whole call (opening the channel, running the command, collecting the output) may
    /// take. [`Connection::exec`] uses the time budget the connection was made with.
    pub timeout: Duration,
    /// How many bytes of stdout and of stderr are kept each; more output ends the call early with
    /// [`ExecOutput::truncated`] set (the command is not waited for).
    pub max_output: usize,
}

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecStatus {
    /// It exited with this code.
    Code(u32),
    /// It was killed by this signal (as the server named it).
    Signal(String),
    /// The channel closed without saying: the server did not send a status, or the call stopped
    /// reading because the output was too long.
    Unknown,
}

impl ExecStatus {
    /// The exit code, if there is one.
    pub fn code(&self) -> Option<u32> {
        match self {
            ExecStatus::Code(code) => Some(*code),
            _ => None,
        }
    }

    /// Whether the command exited with code 0.
    pub fn success(&self) -> bool {
        self.code() == Some(0)
    }
}

/// What [`Connection::exec`] collected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// What the command wrote to stdout (at most [`ExecOptions::max_output`] bytes).
    pub stdout: Vec<u8>,
    /// What it wrote to stderr (at most [`ExecOptions::max_output`] bytes).
    pub stderr: Vec<u8>,
    /// How it ended.
    pub status: ExecStatus,
    /// Output beyond the limit was not kept.
    pub truncated: bool,
}

impl ExecOutput {
    /// stdout as text (invalid UTF-8 becomes U+FFFD).
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr as text (invalid UTF-8 becomes U+FFFD).
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Limits of one [`Connection::exec_stream_with`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    /// The longest line that is kept; the rest of a longer line is dropped.
    pub max_line: usize,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self { max_line: DEFAULT_MAX_LINE }
    }
}

/// Collects the messages of an exec channel for [`Connection::exec`].
struct Collect {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<ExecStatus>,
    max: usize,
    truncated: bool,
    eof: bool,
    closed: bool,
}

impl Collect {
    fn new(max: usize) -> Self {
        Self { stdout: Vec::new(), stderr: Vec::new(), status: None, max, truncated: false, eof: false, closed: false }
    }

    fn keep(buffer: &mut Vec<u8>, data: &[u8], max: usize, truncated: &mut bool) {
        let room = max.saturating_sub(buffer.len());
        if data.len() > room {
            *truncated = true;
        }
        buffer.extend_from_slice(&data[..data.len().min(room)]);
    }

    fn feed(&mut self, msg: ChannelMsg) {
        match msg {
            ChannelMsg::Data { data } => Self::keep(&mut self.stdout, &data, self.max, &mut self.truncated),
            ChannelMsg::ExtendedData { data, ext: 1 } => {
                Self::keep(&mut self.stderr, &data, self.max, &mut self.truncated)
            }
            ChannelMsg::ExitStatus { exit_status } => self.status = Some(ExecStatus::Code(exit_status)),
            ChannelMsg::ExitSignal { signal_name, .. } => {
                self.status = Some(ExecStatus::Signal(format!("{signal_name:?}")))
            }
            ChannelMsg::Eof => self.eof = true,
            ChannelMsg::Close => self.closed = true,
            _ => {}
        }
    }

    /// Nothing more is worth waiting for: the output is full, the channel is closed, or the command
    /// has said how it ended and its output is over.
    fn done(&self) -> bool {
        self.truncated || self.closed || (self.eof && self.status.is_some())
    }
}

impl Connection {
    /// Runs `command` on an exec channel (no PTY) and collects what it writes. The command is
    /// handed to the user's login shell as given, like `ssh host command`.
    ///
    /// Uses the time budget the connection was made with and keeps at most [`DEFAULT_MAX_OUTPUT`]
    /// bytes of each of stdout and stderr: a command that writes more ends the call early with
    /// [`ExecOutput::truncated`] set instead of filling the memory. A command that runs longer than
    /// the budget gives [`SshError::ExecTimeout`]; a connection that drops meanwhile gives
    /// [`SshError::Disconnected`]. Must run inside the SSH runtime.
    pub async fn exec(&self, command: &str) -> Result<ExecOutput, SshError> {
        self.exec_with(command, ExecOptions { timeout: self.inner.watchdog_limit, max_output: DEFAULT_MAX_OUTPUT })
            .await
    }

    /// Like [`exec`](Self::exec) with other limits.
    pub async fn exec_with(&self, command: &str, options: ExecOptions) -> Result<ExecOutput, SshError> {
        let run = async {
            let (mut read, _closing, backlog) = self.open_exec(command).await?;
            let mut collect = Collect::new(options.max_output);
            for msg in backlog {
                collect.feed(msg);
            }
            while !collect.done() {
                match read.wait().await {
                    Some(msg) => collect.feed(msg),
                    None => break,
                }
            }
            // Whatever ended the call (the command, too much output, the time limit), the channel
            // is closed when `_closing` goes: a command that still listens is told to go away.
            if collect.status.is_none() && !collect.truncated && self.is_closed() {
                return Err(SshError::Disconnected(self.disconnect_reason().unwrap_or_else(|| "连接已断开".into())));
            }
            Ok(ExecOutput {
                stdout: collect.stdout,
                stderr: collect.stderr,
                status: collect.status.unwrap_or(ExecStatus::Unknown),
                truncated: collect.truncated,
            })
        };
        tokio::time::timeout(options.timeout, run)
            .await
            .map_err(|_| SshError::ExecTimeout { limit: options.timeout })?
    }

    /// Runs `command` on an exec channel (no PTY) and follows what it writes to stdout line by
    /// line, for commands that run for as long as somebody listens (a collector loop,
    /// `docker events`).
    ///
    /// The stream holds the connection open; dropping it closes the channel, which makes a command
    /// that writes to a closed pipe die of SIGPIPE at its next write. Lines are split on `\n` (the
    /// `\n` is not part of them) and decoded lossily; a line longer than
    /// [`DEFAULT_MAX_LINE`] is cut. stderr is not part of the lines; the end of it is kept for
    /// diagnostics ([`ExecStream::stderr_tail`]). Must run inside the SSH runtime.
    pub async fn exec_stream(&self, command: &str) -> Result<ExecStream, SshError> {
        self.exec_stream_with(command, StreamOptions::default()).await
    }

    /// Like [`exec_stream`](Self::exec_stream) with other limits.
    pub async fn exec_stream_with(&self, command: &str, options: StreamOptions) -> Result<ExecStream, SshError> {
        let (read, closing, backlog) = self.open_exec(command).await?;
        let (tx, rx) = mpsc::channel(STREAM_QUEUE);
        let ending = Arc::new(Mutex::new(Ending::default()));
        self.inner.runtime.spawn(pump_lines(
            read,
            closing,
            backlog,
            tx,
            ending.clone(),
            self.clone(),
            options.max_line,
        ));
        Ok(ExecStream { rx, ending, _connection: self.clone() })
    }

    /// Lossless byte chunks, with no output or execution-time limit. The caller
    /// owns the channel lifetime: dropping this handle closes it even while idle.
    /// Backpressure is direct (no detached reader or unbounded queue).
    pub async fn exec_spooled(&self, command: &str) -> Result<ExecSpool, SshError> {
        let (read, closing, backlog) = self.open_exec(command).await?;
        Ok(ExecSpool {
            read,
            _closing: closing,
            queue: backlog.into(),
            connection: self.clone(),
            status: None,
            done: false,
        })
    }

    /// Opens a session channel and starts `command` on it. Returns what reads the channel, what closes
    /// it when dropped, and what arrived before the server answered the request.
    async fn open_exec(&self, command: &str) -> Result<(ChannelReadHalf, CloseOnDrop, Vec<ChannelMsg>), SshError> {
        let open = async {
            let channel =
                self.inner.handle.channel_open_session().await.map_err(|err| SshError::Channel(err.to_string()))?;
            let (mut read, write) = channel.split();
            // Install before sending exec or awaiting its acknowledgement: cancellation at
            // either await must close the channel too.
            let closing = CloseOnDrop { write: Some(write), runtime: self.inner.runtime.clone() };
            closing.write.as_ref().unwrap().exec(true, command.as_bytes().to_vec()).await.map_err(|err| SshError::Channel(err.to_string()))?;
            let mut backlog = Vec::new();
            loop {
                match read.wait().await {
                    Some(ChannelMsg::Success) => break,
                    Some(ChannelMsg::Failure) => return Err(SshError::Channel("服务器拒绝了执行命令请求".into())),
                    Some(ChannelMsg::Close) | None => return Err(SshError::Channel("服务器关闭了会话通道".into())),
                    Some(msg) => backlog.push(msg),
                }
            }
            Ok((read, closing, backlog))
        };
        Watchdog::new(self.inner.watchdog_limit).guard(open).await
    }
}

/// Closes the channel when dropped (on the SSH runtime, so that it also works from anywhere else).
struct CloseOnDrop {
    write: Option<ChannelWriteHalf<Msg>>,
    runtime: tokio::runtime::Handle,
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(write) = self.write.take() {
            self.runtime.spawn(async move {
                // Nothing to do if the server closed it first or the connection is gone.
                let _ = tokio::time::timeout(CLOSE_LIMIT, write.close()).await;
            });
        }
    }
}

/// An untruncated stdout/stderr fragment in SSH channel arrival order.
pub struct ExecChunk {
    /// Unmodified channel bytes.
    pub bytes: Vec<u8>,
    /// True for SSH extended-data stream 1.
    pub stderr: bool,
}
/// Lossless channel reader; early drop requests KILL and closes its channel.
pub struct ExecSpool {
    read: ChannelReadHalf,
    _closing: CloseOnDrop,
    queue: std::collections::VecDeque<ChannelMsg>,
    connection: Connection,
    status: Option<ExecStatus>,
    done: bool,
}
impl Drop for ExecSpool {
    fn drop(&mut self) {
        // A non-writing remote command may survive pipe closure. This ownership
        // rule is specific to the AI spool handle; legacy exec/line streams keep
        // their existing CloseOnDrop behavior.
        if !self.done && self.status.is_none() && let Some(write) = self._closing.write.take() {
            self._closing.runtime.spawn(async move {
                let _ = tokio::time::timeout(CLOSE_LIMIT, async {
                    let _ = write.signal(russh::Sig::KILL).await;
                    let _ = write.close().await;
                }).await;
            });
        }
    }
}
impl ExecSpool {
    /// Exit status once supplied by the server.
    pub fn status(&self) -> Option<ExecStatus> {
        self.status.clone()
    }
    /// Next stdout/stderr chunk; None means the channel finished.
    pub async fn next_chunk(&mut self) -> Result<Option<ExecChunk>, SshError> {
        while !self.done {
            let msg = match self.queue.pop_front() {
                Some(msg) => Some(msg),
                None => self.read.wait().await,
            };
            match msg {
                Some(ChannelMsg::Data { data }) => {
                    return Ok(Some(ExecChunk {
                        bytes: data.to_vec(),
                        stderr: false,
                    }));
                }
                Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                    return Ok(Some(ExecChunk {
                        bytes: data.to_vec(),
                        stderr: true,
                    }));
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    self.status = Some(ExecStatus::Code(exit_status))
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    self.status = Some(ExecStatus::Signal(format!("{signal_name:?}")))
                }
                Some(ChannelMsg::Close) | None => self.done = true,
                _ => {}
            }
        }
        if self.connection.is_closed() && self.status.is_none() {
            Err(SshError::Disconnected(
                self.connection
                    .disconnect_reason()
                    .unwrap_or_else(|| "连接已断开".into()),
            ))
        } else {
            Ok(None)
        }
    }
}

/// What is known about the end of a streamed command.
#[derive(Default)]
struct Ending {
    status: Option<ExecStatus>,
    stderr: Vec<u8>,
}

fn lock(ending: &Mutex<Ending>) -> MutexGuard<'_, Ending> {
    ending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The lines a command writes, as they come.
///
/// Holds the connection open for as long as it lives, like a shell channel does; dropping it closes
/// the channel.
pub struct ExecStream {
    rx: mpsc::Receiver<String>,
    ending: Arc<Mutex<Ending>>,
    _connection: Connection,
}

impl ExecStream {
    /// The next line, or `None` once the command is over (or the channel or the connection is gone).
    pub async fn next_line(&mut self) -> Option<String> {
        self.rx.recv().await
    }

    /// How the command ended, once the stream is over and the server said.
    pub fn status(&self) -> Option<ExecStatus> {
        lock(&self.ending).status.clone()
    }

    /// The end (at most 4 KiB) of what the command wrote to stderr, as text.
    pub fn stderr_tail(&self) -> String {
        String::from_utf8_lossy(&lock(&self.ending).stderr).into_owned()
    }
}

impl std::fmt::Debug for ExecStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExecStream")
    }
}

/// Splits the byte stream of a channel into lines.
struct Lines {
    pending: Vec<u8>,
    /// The current line is over the limit: what follows up to the next `\n` is dropped.
    overflow: bool,
    max: usize,
}

impl Lines {
    fn new(max: usize) -> Self {
        Self { pending: Vec::new(), overflow: false, max }
    }

    /// Feeds `data` and returns the lines it completed.
    fn push(&mut self, data: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for piece in data.split_inclusive(|byte| *byte == b'\n') {
            let (body, ended) = match piece.split_last() {
                Some((b'\n', body)) => (body, true),
                _ => (piece, false),
            };
            if !self.overflow {
                let room = self.max.saturating_sub(self.pending.len());
                self.pending.extend_from_slice(&body[..body.len().min(room)]);
                self.overflow = body.len() > room;
            }
            if ended {
                lines.push(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned());
                self.overflow = false;
            }
        }
        lines
    }

    /// What was written after the last `\n`, if anything.
    fn rest(&mut self) -> Option<String> {
        (!self.pending.is_empty()).then(|| String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned())
    }
}

/// Reads the channel of a streamed command and queues its lines until the command ends or the
/// reader goes away.
async fn pump_lines(
    mut read: ChannelReadHalf,
    // Closes the channel when this task ends, however it ends.
    _closing: CloseOnDrop,
    backlog: Vec<ChannelMsg>,
    tx: mpsc::Sender<String>,
    ending: Arc<Mutex<Ending>>,
    // Held until the channel is closed: the connection must not outlive the use of its channel.
    _connection: Connection,
    max_line: usize,
) {
    let mut lines = Lines::new(max_line);
    let mut queue: std::collections::VecDeque<ChannelMsg> = backlog.into();
    let mut reader_gone = false;
    'read: loop {
        let msg = match queue.pop_front() {
            Some(msg) => Some(msg),
            None => tokio::select! {
                // The stream was dropped: stop listening, close the channel.
                () = tx.closed() => {
                    reader_gone = true;
                    break 'read;
                }
                msg = read.wait() => msg,
            },
        };
        match msg {
            Some(ChannelMsg::Data { data }) => {
                for line in lines.push(&data) {
                    if tx.send(line).await.is_err() {
                        reader_gone = true;
                        break 'read;
                    }
                }
            }
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                let mut ending = lock(&ending);
                ending.stderr.extend_from_slice(&data);
                let excess = ending.stderr.len().saturating_sub(STDERR_TAIL);
                ending.stderr.drain(..excess);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => lock(&ending).status = Some(ExecStatus::Code(exit_status)),
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                lock(&ending).status = Some(ExecStatus::Signal(format!("{signal_name:?}")))
            }
            Some(ChannelMsg::Close) | None => break,
            Some(_) => {}
        }
    }
    if !reader_gone && let Some(rest) = lines.rest() {
        let _ = tx.send(rest).await;
    }
    // Dropping `tx` ends the stream; `_closing` closes the channel.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(max: usize, chunks: &[&[u8]]) -> (Vec<String>, Option<String>) {
        let mut lines = Lines::new(max);
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(lines.push(chunk));
        }
        (out, lines.rest())
    }

    #[test]
    fn lines_are_split_on_newlines_across_chunk_boundaries() {
        let (lines, rest) = split(100, &[b"one\ntw", b"o\n\nthree", b"\nfour"]);
        assert_eq!(lines, ["one", "two", "", "three"]);
        assert_eq!(rest.as_deref(), Some("four"), "a last line without a newline is still a line");
        assert_eq!(split(100, &[b"a\n"]), (vec!["a".to_string()], None));
    }

    #[test]
    fn a_carriage_return_stays_in_the_line() {
        assert_eq!(split(100, &[b"a\r\nb\n"]).0, ["a\r", "b"]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let (lines, _) = split(100, &[b"caf\xc3\xa9 \xff\xfe ok\n", b"\xe4\xb8", b"\xad\n"]);
        assert_eq!(lines[0], "café \u{fffd}\u{fffd} ok");
        assert_eq!(lines[1], "中", "a character split between chunks is whole again");
    }

    #[test]
    fn a_line_longer_than_the_limit_is_cut_and_the_next_line_is_fine() {
        let long = vec![b'x'; 10_000];
        let (lines, rest) = split(8, &[&long[..4000], &long[4000..], b"\nnext\n"]);
        assert_eq!(lines, ["xxxxxxxx", "next"]);
        assert_eq!(rest, None);
        // Nothing but the limit is ever kept, however long a line without a newline gets.
        let mut lines = Lines::new(8);
        for _ in 0..1000 {
            assert!(lines.push(&long).is_empty());
            assert!(lines.pending.len() <= 8);
        }
    }

    #[test]
    fn the_output_cap_keeps_the_start_and_says_so() {
        let mut collect = Collect::new(5);
        collect.feed(ChannelMsg::Data { data: b"abc"[..].into() });
        assert!(!collect.done());
        collect.feed(ChannelMsg::Data { data: b"defgh"[..].into() });
        assert_eq!(collect.stdout, b"abcde");
        assert!(collect.truncated && collect.done());
        // stderr has its own room.
        let mut collect = Collect::new(3);
        collect.feed(ChannelMsg::ExtendedData { data: b"err"[..].into(), ext: 1 });
        collect.feed(ChannelMsg::Data { data: b"out"[..].into() });
        assert_eq!(
            (collect.stdout.as_slice(), collect.stderr.as_slice(), collect.truncated),
            (&b"out"[..], &b"err"[..], false)
        );
    }
}
