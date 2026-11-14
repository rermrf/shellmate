//! Shared setup: a server, a connection to it, a forwarder, and helpers to talk through rules.

#![allow(dead_code)]

use std::time::Duration;

use async_channel::Receiver;
use shellmate_core::AuthMethod;
use shellmate_forward::{ForwardEvent, ForwardKind, ForwardRule, Forwarder, RuleId, RuleState};
use shellmate_ssh::{Connection, Route, Secret, SshHost};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{EchoServer, Env, ServerOptions, TestServer, eventually, lock};

pub fn forwarding() -> ServerOptions {
    ServerOptions { forwarding: true, remote_forwarding: true, ..Default::default() }
}

pub struct Rig {
    pub server: TestServer,
    pub env: Env,
    pub echo: EchoServer,
    pub connection: Connection,
    pub forwarder: Forwarder,
    pub events: Receiver<ForwardEvent>,
    watchdog: Option<Duration>,
}

impl Rig {
    pub async fn start() -> Self {
        Self::with(forwarding()).await
    }

    pub async fn with(options: ServerOptions) -> Self {
        Self::build(options, None).await
    }

    /// A connection whose channel opens give up after `watchdog`.
    pub async fn with_watchdog(options: ServerOptions, watchdog: Duration) -> Self {
        Self::build(options, Some(watchdog)).await
    }

    async fn build(options: ServerOptions, watchdog: Option<Duration>) -> Self {
        let server = TestServer::start(options).await;
        let env = Env::new();
        let echo = EchoServer::start().await;
        let (events_tx, events) = async_channel::unbounded();
        let forwarder = Forwarder::new(tokio::runtime::Handle::current(), events_tx);
        let rig =
            Self { connection: connect(&server, &env, watchdog).await, server, env, echo, forwarder, events, watchdog };
        rig.forwarder.connection_restored(&rig.connection);
        rig
    }

    /// A new connection to the same server, not yet announced to the forwarder.
    pub async fn new_connection(&self) -> Connection {
        connect(&self.server, &self.env, self.watchdog).await
    }

    /// What the app does after a reconnect: a new connection, announced to the forwarder.
    pub async fn reconnect(&mut self) {
        let connection = self.new_connection().await;
        self.forwarder.connection_restored(&connection);
        self.connection = connection;
    }

    /// Cuts the transport (the server drops the TCP connection) and tells the forwarder.
    pub async fn cut(&self) {
        self.server.state.kill_connections();
        self.connection.closed().await;
        self.forwarder.connection_lost();
    }

    pub fn open_connections(&self) -> usize {
        *lock(&self.server.state.open_connections)
    }

    pub fn echo_target(&self) -> String {
        format!("127.0.0.1:{}", self.echo.port)
    }

    /// `-L` from a free local port to the echo service.
    pub fn local_rule(&self) -> ForwardRule {
        rule(ForwardKind::Local, &format!("127.0.0.1:{}", free_port()), &self.echo_target())
    }

    /// `-D` on a free local port.
    pub fn socks_rule(&self) -> ForwardRule {
        rule(ForwardKind::Dynamic, &format!("127.0.0.1:{}", free_port()), "")
    }

    /// `-R` from a port the server chooses to the echo service.
    pub fn remote_rule(&self) -> ForwardRule {
        rule(ForwardKind::Remote, "127.0.0.1:0", &self.echo_target())
    }

    /// Waits until `id` is running and returns the port it listens on.
    pub async fn running_port(&self, id: &RuleId) -> u16 {
        eventually("the rule to run", || match self.forwarder.state(id) {
            RuleState::Running { port } => Some(port),
            _ => None,
        })
        .await
    }
}

async fn connect(server: &TestServer, env: &Env, watchdog: Option<Duration>) -> Connection {
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    let mut options = env.trusting();
    if let Some(limit) = watchdog {
        options = options.with_connect_watchdog(limit);
    }
    Connection::connect(Route::direct(host), options).await.expect("connect")
}

pub fn rule(kind: ForwardKind, listen: &str, target: &str) -> ForwardRule {
    ForwardRule::parse(RuleId::generate(), "", kind, listen, target, true)
        .unwrap_or_else(|err| panic!("{listen}: {err}"))
}

/// A loopback port nothing listens on right now.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub async fn dial(port: u16) -> TcpStream {
    TcpStream::connect(("127.0.0.1", port)).await.unwrap_or_else(|err| panic!("nothing listens on {port}: {err}"))
}

/// Sends `bytes`, half-closes, and returns everything that comes back until the end. Writing and
/// reading go on at the same time, like a real client does: a client that only reads after it
/// has written everything depends on how much the sockets in between happen to buffer.
pub async fn echo_through(port: u16, bytes: &[u8]) -> Vec<u8> {
    let (mut reader, mut writer) = dial(port).await.into_split();
    let data = bytes.to_vec();
    let sender = tokio::spawn(async move {
        writer.write_all(&data).await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), reader.read_to_end(&mut back))
        .await
        .expect("the echo ends")
        .expect("read");
    sender.await.unwrap();
    back
}

/// Waits for the peer to close the connection (end of file or reset); whatever it still sends
/// before that is ignored.
pub async fn expect_closed(client: &mut TcpStream) {
    let mut buf = [0u8; 64];
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "expected the connection to be closed");
}

/// Deterministic pseudo-random bytes (xorshift), so that corruption or reordering shows.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// FNV-1a: cheap and sensitive to any change in content or order.
pub struct Checksum(u64);

impl Checksum {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

pub fn checksum(bytes: &[u8]) -> u64 {
    let mut sum = Checksum::new();
    sum.update(bytes);
    sum.finish()
}

/// Every event that has arrived so far.
pub fn drain(events: &Receiver<ForwardEvent>) -> Vec<ForwardEvent> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}

/// The states `id` went through, from the events received so far.
pub fn states_of(events: &[ForwardEvent], id: &RuleId) -> Vec<RuleState> {
    events
        .iter()
        .filter_map(|event| match event {
            ForwardEvent::State { id: event_id, state } if event_id == id => Some(state.clone()),
            _ => None,
        })
        .collect()
}

/// A service that reads until the client is done and answers with the length and checksum of
/// what it got (8 + 8 bytes, big endian) — a one-way bulk upload that cannot deadlock.
pub struct Digest {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Digest {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut reader, mut writer) = socket.into_split();
                    let (mut sum, mut length) = (Checksum::new(), 0u64);
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                sum.update(&buf[..n]);
                                length += n as u64;
                            }
                        }
                    }
                    let _ = writer.write_all(&[length.to_be_bytes(), sum.finish().to_be_bytes()].concat()).await;
                    let _ = writer.shutdown().await;
                });
            }
        });
        Self { port, task }
    }
}

impl Drop for Digest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A service that sends `size` bytes of `pattern(_, seed)` to everyone who connects and closes;
/// it never reads (a one-way bulk download).
pub struct Source {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Source {
    pub async fn start(size: usize, seed: u64) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut left = size;
                    let mut block = 0u64;
                    while left > 0 {
                        let chunk = pattern(left.min(256 * 1024), seed.wrapping_add(block));
                        if socket.write_all(&chunk).await.is_err() {
                            return;
                        }
                        left -= chunk.len();
                        block += 1;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { port, task }
    }

    /// The checksum of what a client should receive.
    pub fn expected(size: usize, seed: u64) -> u64 {
        let mut sum = Checksum::new();
        let (mut left, mut block) = (size, 0u64);
        while left > 0 {
            let chunk = pattern(left.min(256 * 1024), seed.wrapping_add(block));
            sum.update(&chunk);
            left -= chunk.len();
            block += 1;
        }
        sum.finish()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Uploads `size` bytes of pattern to a [`Digest`] reached through `port`; returns the digest it
/// reports (length, checksum) and the checksum of what was sent.
pub async fn upload(port: u16, size: usize, seed: u64) -> ((u64, u64), u64) {
    let client = dial(port).await;
    let (mut reader, mut writer) = client.into_split();
    let sender = tokio::spawn(async move {
        let mut sum = Checksum::new();
        let (mut left, mut block) = (size, 0u64);
        while left > 0 {
            let chunk = pattern(left.min(100_000), seed.wrapping_add(block));
            sum.update(&chunk);
            writer.write_all(&chunk).await.unwrap();
            left -= chunk.len();
            block += 1;
        }
        writer.shutdown().await.unwrap();
        sum.finish()
    });
    let mut answer = [0u8; 16];
    tokio::time::timeout(Duration::from_secs(120), reader.read_exact(&mut answer)).await.expect("digest").unwrap();
    let sent = sender.await.unwrap();
    let length = u64::from_be_bytes(answer[..8].try_into().unwrap());
    let sum = u64::from_be_bytes(answer[8..].try_into().unwrap());
    ((length, sum), sent)
}

/// Downloads everything from `port`; returns the length and checksum.
pub async fn download(port: u16) -> (u64, u64) {
    let mut client = dial(port).await;
    let (mut sum, mut length) = (Checksum::new(), 0u64);
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(60), client.read(&mut buf)).await.expect("progress").unwrap();
        if n == 0 {
            return (length, sum.finish());
        }
        sum.update(&buf[..n]);
        length += n as u64;
    }
}
