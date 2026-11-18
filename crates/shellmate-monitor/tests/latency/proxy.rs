use std::{io, net::SocketAddr, time::Duration};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep_until, timeout};

const CHUNK: usize = 16 * 1024;

fn direction(
    tasks: &mut JoinSet<io::Result<()>>,
    mut reader: impl AsyncRead + Unpin + Send + 'static,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    delay: Duration,
) {
    let (send, mut queued) = mpsc::unbounded_channel();
    tasks.spawn(async move {
        let mut buffer = vec![0; CHUNK];
        loop {
            let size = reader.read(&mut buffer).await?;
            let due = Instant::now() + delay;
            if send.send((due, buffer[..size].to_vec())).is_err() || size == 0 {
                return Ok(());
            }
        }
    });
    tasks.spawn(async move {
        while let Some((due, bytes)) = queued.recv().await {
            if due > Instant::now() {
                sleep_until(due).await;
            }
            if bytes.is_empty() {
                return writer.shutdown().await;
            }
            writer.write_all(&bytes).await?;
        }
        Ok(())
    });
}

async fn relay(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    server: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    delay: Duration,
) -> io::Result<()> {
    let (client_read, client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server);
    let mut tasks = JoinSet::new();
    direction(&mut tasks, client_read, server_write, delay);
    direction(&mut tasks, server_read, client_write, delay);
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

/// A single-use listener. Joining it between samples drains both directions, including delayed FINs.
pub struct Proxy {
    pub addr: SocketAddr,
    task: JoinHandle<io::Result<()>>,
    delay: Duration,
}

impl Proxy {
    pub async fn start(host: &str, port: u16, delay: Duration) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let host = host.to_owned();
        let task = tokio::spawn(async move {
            let (client, _) = listener.accept().await?;
            drop(listener);
            let server = TcpStream::connect((host.as_str(), port)).await?;
            client.set_nodelay(true)?;
            server.set_nodelay(true)?;
            relay(client, server, delay).await
        });
        Ok(Self { addr, task, delay })
    }

    pub async fn finish(mut self) -> io::Result<()> {
        timeout(Duration::from_secs(10) + self.delay * 2, &mut self.task).await??
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn echo(stream: impl AsyncRead + AsyncWrite + Unpin) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    tokio::io::copy(&mut reader, &mut writer).await?;
    writer.shutdown().await
}

async fn check_echo(mut stream: impl AsyncRead + AsyncWrite + Unpin) -> io::Result<()> {
    let payload: Vec<u8> = (0..CHUNK * 6).map(|index| (index % 251) as u8).collect();
    let mut echoed = vec![0; payload.len()];
    let started = Instant::now();
    stream.write_all(&payload).await?;
    stream.shutdown().await?;
    timeout(Duration::from_secs(2), stream.read_exact(&mut echoed[..1])).await??;
    let first = started.elapsed();
    timeout(Duration::from_secs(2), stream.read_exact(&mut echoed[1..])).await??;
    let all = started.elapsed();
    // The lower bound proves the delay; the upper one only that chunks are pipelined rather than
    // delayed one after another (several hundred ms each). Shared CI machines need the headroom.
    assert!(
        first >= Duration::from_millis(200) && all < Duration::from_millis(800),
        "echo: first {first:?}, all {all:?}"
    );
    assert!(echoed == payload, "payload changed in transit");
    assert_eq!(timeout(Duration::from_secs(2), stream.read(&mut echoed[..1])).await??, 0, "expected EOF");
    Ok(())
}

#[tokio::test]
async fn tcp_echo_is_delayed_and_ordered() -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let proxy = Proxy::start("127.0.0.1", listener.local_addr()?.port(), Duration::from_millis(100)).await?;
    let server = async { echo(listener.accept().await?.0).await };
    let client = async { check_echo(TcpStream::connect(proxy.addr).await?).await };
    tokio::try_join!(server, client)?;
    proxy.finish().await
}

#[tokio::test]
async fn relay_pipelines_chunks_and_propagates_half_closes_without_sockets() -> io::Result<()> {
    let (client, inbound) = tokio::io::duplex(CHUNK * 8);
    let (outbound, server) = tokio::io::duplex(CHUNK * 8);
    let forwarding = relay(inbound, outbound, Duration::from_millis(100));
    tokio::try_join!(forwarding, echo(server), check_echo(client))?;
    Ok(())
}
