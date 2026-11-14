//! Counting connections and bytes, and the copy loop that feeds the counters.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

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

/// The live counters of one rule, shared with its connection tasks (together with the
/// forwarder's stall limit, which they need as well).
pub(crate) struct RuleStats {
    active: AtomicU32,
    down: AtomicU64,
    up: AtomicU64,
    /// Milliseconds; shared with the forwarder so changing it reaches connections to come.
    stall_ms: Arc<AtomicU64>,
}

impl RuleStats {
    pub fn new(stall_ms: Arc<AtomicU64>) -> Self {
        Self { active: AtomicU32::new(0), down: AtomicU64::new(0), up: AtomicU64::new(0), stall_ms }
    }

    fn stall_limit(&self) -> Duration {
        Duration::from_millis(self.stall_ms.load(Ordering::Relaxed))
    }

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

/// The local side of a connection, which gives up on a peer that stops reading.
///
/// The SSH library hands a channel's data to the application through a bounded queue and
/// blocks its whole session — every channel of the connection, terminals included — when that
/// queue is full, so one local peer that does not read must not be waited for forever.
struct Guarded<L> {
    inner: L,
    limit: Duration,
    /// Running while a write is waiting for the peer to take data.
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<L: AsyncRead + Unpin> AsyncRead for Guarded<L> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<L: AsyncWrite + Unpin> AsyncWrite for Guarded<L> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => {
                let limit = self.limit;
                let timer = self.stalled.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                if timer.as_mut().poll(cx).is_ready() {
                    self.stalled = None;
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "本地一侧长时间没有接收数据")));
                }
                Poll::Pending
            }
            done => {
                self.stalled = None;
                done
            }
        }
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
/// a failure on either side ends the connection at once (the caller drops both), and so does a
/// local peer that has not taken any data for the forwarder's stall limit. When `closed` (the
/// SSH connection is gone) resolves, whatever the channel had queued is still delivered, then the
/// connection ends — the local peer might otherwise keep an idle half-open connection forever.
/// Never buffers more than one buffer per direction.
pub(crate) async fn pump<L, S>(local: L, ssh: S, stats: &Arc<RuleStats>, closed: impl Future<Output = ()>)
where
    L: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut local = Guarded { inner: local, limit: stats.stall_limit(), stalled: None };
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

    fn test_stats(stall_ms: u64) -> Arc<RuleStats> {
        Arc::new(RuleStats::new(Arc::new(AtomicU64::new(stall_ms))))
    }

    #[tokio::test]
    async fn bytes_are_counted_per_direction_and_half_close_is_passed_on() {
        let stats = test_stats(60_000);
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
        let stats = test_stats(60_000);
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
    async fn a_local_peer_that_stops_reading_is_cut_off_after_the_stall_limit() {
        let stats = test_stats(300);
        let (_client, local) = tokio::io::duplex(1024); // nobody reads this end's peer
        let (mut server, ssh) = tokio::io::duplex(1024 * 1024);
        let pumping = tokio::spawn({
            let stats = stats.clone();
            async move { pump(local, ssh, &stats, std::future::pending()).await }
        });
        // The SSH side keeps sending; the local side's small buffer fills at once.
        let sender = tokio::spawn(async move {
            let chunk = [1u8; 4096];
            while server.write_all(&chunk).await.is_ok() {}
        });
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), pumping).await.expect("cut off").unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250), "not before the limit");
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn a_slow_but_reading_peer_is_not_cut_off() {
        let stats = test_stats(400);
        let (mut client, local) = tokio::io::duplex(1024);
        let (mut server, ssh) = tokio::io::duplex(1024 * 1024);
        let pumping = tokio::spawn({
            let stats = stats.clone();
            async move { pump(local, ssh, &stats, std::future::pending()).await }
        });
        let sender = tokio::spawn(async move {
            server.write_all(&vec![7u8; 16 * 1024]).await.unwrap();
            server.shutdown().await.unwrap();
        });
        // Takes a kilobyte every 100 ms: always within the limit, but far slower than the sender.
        let mut got = 0usize;
        let mut buf = [0u8; 1024];
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match client.read(&mut buf).await.unwrap() {
                0 => break,
                n => got += n,
            }
        }
        assert_eq!(got, 16 * 1024);
        sender.await.unwrap();
        // The connection ends when the client is done as well.
        client.shutdown().await.unwrap();
        pumping.await.unwrap();
    }

    #[tokio::test]
    async fn the_connection_ends_when_the_ssh_connection_is_gone_even_if_the_local_peer_stays() {
        let stats = test_stats(60_000);
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
