//! Subsystem channels (`sftp`) on an established connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use russh::ChannelStream;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::connection::Connection;
use crate::error::SshError;
use crate::shell::expect_reply;
use crate::watchdog::Watchdog;

/// The byte stream of a subsystem channel (for SFTP: the packets in and out).
///
/// Holds the connection open for as long as it lives, like a shell channel does; dropping it
/// closes the channel.
pub struct SubsystemStream {
    stream: ChannelStream<Msg>,
    _connection: Connection,
}

impl AsyncRead for SubsystemStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for SubsystemStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl std::fmt::Debug for SubsystemStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubsystemStream")
    }
}

impl Connection {
    /// Opens a channel on the connection and starts the subsystem `name` (`"sftp"`) on it.
    /// Like opening a shell, this has the connection's time budget and fails with
    /// [`SshError::Channel`] if the server refuses the subsystem.
    ///
    /// Must run inside the SSH runtime (the stream is driven by tasks of the connection).
    pub async fn open_subsystem(&self, name: &str) -> Result<SubsystemStream, SshError> {
        let open = async {
            let mut channel =
                self.inner.handle.channel_open_session().await.map_err(|err| SshError::Channel(err.to_string()))?;
            channel.request_subsystem(true, name).await.map_err(|err| SshError::Channel(err.to_string()))?;
            // Nothing a subsystem sends before the request is answered is of interest: SFTP speaks
            // only after the client's first packet.
            expect_reply(&mut channel, &mut Vec::new(), &format!("「{name}」子系统")).await?;
            Ok(SubsystemStream { stream: channel.into_stream(), _connection: self.clone() })
        };
        Watchdog::new(self.inner.watchdog_limit).guard(open).await
    }
}
