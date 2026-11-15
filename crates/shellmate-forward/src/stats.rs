//! Counting connections and bytes, and the copy loop that feeds the counters.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Numbers of one rule since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Connections being served right now.
    pub active: u32,
    /// Bytes received from the SSH side (↓).
    pub down: u64,
    /// Bytes sent to the SSH side (↑).
    pub up: u64,
}

/// The live counters of one rule, shared with its connection tasks.
#[derive(Default)]
pub(crate) struct RuleStats {
    active: AtomicU32,
    down: AtomicU64,
    up: AtomicU64,
}

impl RuleStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            active: self.active.load(Ordering::Relaxed),
            down: self.down.load(Ordering::Relaxed),
            up: self.up.load(Ordering::Relaxed),
        }
    }
}

/// Counts one connection as active for as long as it lives.
pub(crate) struct ActiveGuard(Arc<RuleStats>);

impl ActiveGuard {
    pub fn new(stats: &Arc<RuleStats>) -> Self {
        stats.active.fetch_add(1, Ordering::Relaxed);
        Self(stats.clone())
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The SSH side of a connection, counting what passes through: reads are ↓, writes are ↑.
struct Counted<S> {
    inner: S,
    stats: Arc<RuleStats>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            self.stats.down.fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = polled {
            self.stats.up.fetch_add(written as u64, Ordering::Relaxed);
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Size of the two copy buffers of a connection. Together with the flow control of the SSH
/// channel this bounds what a connection holds in memory.
const COPY_BUFFER: usize = 64 * 1024;

/// How long what the SSH connection still had queued for the local side may take to be
/// delivered once the connection is gone.
const DRAIN_AFTER_CLOSE: Duration = Duration::from_secs(1);

/// Copies between a local socket and an SSH channel in both directions until both are done.
///
/// An end-of-file on one side is passed on as a half-close and the other direction keeps going;
/// a failure on either side ends the connection at once (the caller drops both). When `closed`
/// (the SSH connection is gone) resolves, whatever the channel had queued is still delivered, then
/// the connection ends — the local peer might otherwise keep an idle half-open connection forever.
/// Never buffers more than one buffer per direction: a local peer that stops reading makes the
/// copy wait, which holds the SSH channel's window closed and so pauses only this connection.
pub(crate) async fn pump<L, S>(mut local: L, ssh: S, stats: &Arc<RuleStats>, closed: impl Future<Output = ()>)
where
    L: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ssh = Counted { inner: ssh, stats: stats.clone() };
    let copying = tokio::io::copy_bidirectional_with_sizes(&mut local, &mut ssh, COPY_BUFFER, COPY_BUFFER);
    tokio::pin!(copying);
    let outcome = tokio::select! {
        outcome = &mut copying => Some(outcome),
        () = closed => tokio::time::timeout(DRAIN_AFTER_CLOSE, &mut copying).await.ok(),
    };
    if let Some(Err(err)) = outcome {
        log::debug!("转发连接中断：{err}");
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn test_stats() -> Arc<RuleStats> {
        Arc::new(RuleStats::default())
    }

    #[tokio::test]
    async fn bytes_are_counted_per_direction_and_half_close_is_passed_on() {
        let stats = test_stats();
        let (mut client, local) = tokio::io::duplex(1024);
        let (mut server, ssh) = tokio::io::duplex(1024);
        let guard = ActiveGuard::new(&stats);
        let pumping = tokio::spawn({
            let stats = stats.clone();
            async move { pump(local, ssh, &stats, std::future::pending()).await }
        });

        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        let mut got = Vec::new();
        server.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"hello", "the client's end-of-file reaches the server as a half-close");

        // The other direction still works after the half-close.
        server.write_all(b"world!!").await.unwrap();
        server.shutdown().await.unwrap();
        let mut back = Vec::new();
        client.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"world!!");

        pumping.await.unwrap();
        assert_eq!(stats.snapshot(), StatsSnapshot { active: 1, down: 7, up: 5 });
        drop(guard);
        assert_eq!(stats.snapshot().active, 0);
    }

    #[tokio::test]
    async fn an_error_on_one_side_ends_the_connection() {
        let stats = test_stats();
        let (client, local) = tokio::io::duplex(64);
        let (server, ssh) = tokio::io::duplex(64);
        let pumping = tokio::spawn({
            let stats = stats.clone();
            async move { pump(local, ssh, &stats, std::future::pending()).await }
        });
        drop(server);
        let mut client = client;
        // Writing towards a dead peer fails; the pump must not wait for the client to finish.
        client.write_all(b"x").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), pumping).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_connection_ends_when_the_ssh_connection_is_gone_even_if_the_local_peer_stays() {
        let stats = test_stats();
        let (mut client, local) = tokio::io::duplex(1024);
        let (mut server, ssh) = tokio::io::duplex(1024);
        let (gone_tx, gone_rx) = tokio::sync::oneshot::channel::<()>();
        let pumping = tokio::spawn({
            let stats = stats.clone();
            async move {
                pump(local, ssh, &stats, async {
                    let _ = gone_rx.await;
                })
                .await
            }
        });
        // Data that was queued before the loss still arrives.
        server.write_all(b"last words").await.unwrap();
        drop(server);
        gone_tx.send(()).unwrap();
        let mut got = [0u8; 10];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"last words");
        // The client neither writes nor closes, and the pump is over anyway.
        tokio::time::timeout(Duration::from_secs(5), pumping).await.expect("ended").unwrap();
    }
}
