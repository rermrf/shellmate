//! The tasks behind a started rule: the accept loops and what happens to each connection.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use shellmate_ssh::{Connection, ForwardedStream, RemoteForward, SshError, WeakConnection};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::addr::Endpoint;
use crate::forwarder::Inner;
use crate::retry::{GrantMemory, RetryTiming};
use crate::rule::RuleId;
use crate::socks::{self, Negotiated};
use crate::state::{RuleError, RuleState};
use crate::stats::{ActiveGuard, RuleStats, pump};

/// How long a SOCKS client may take to send its greeting and request.
const SOCKS_HANDSHAKE: Duration = Duration::from_secs(30);
/// How long a remote forward waits to reach its local target.
const LOCAL_CONNECT: Duration = Duration::from_secs(10);

/// A spawned task that is aborted when this is dropped (and with it the connection tasks it
/// owns, and their hold on the SSH connection).
pub(crate) struct AbortOnDrop(JoinHandle<()>);

impl AbortOnDrop {
    pub fn new(task: JoinHandle<()>) -> Self {
        Self(task)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The connection the rules currently use. Weak: the forwarder never keeps a connection open.
#[derive(Default)]
pub(crate) struct ConnSlot(Mutex<Option<WeakConnection>>);

impl ConnSlot {
    fn guard(&self) -> MutexGuard<'_, Option<WeakConnection>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, connection: Option<WeakConnection>) {
        *self.guard() = connection;
    }

    /// The connection, if there is one and it is up.
    pub fn current(&self) -> Option<Connection> {
        self.guard().as_ref().and_then(WeakConnection::upgrade)
    }
}

/// What a local listener does with the connections it accepts.
#[derive(Clone)]
pub(crate) enum Mode {
    /// `-L`: everything goes to this target.
    Forward(Endpoint),
    /// `-D`: the SOCKS client names the target.
    Socks,
}

/// Accepts connections on `listener` until aborted. While no connection to the server is up,
/// connections are closed as soon as they are accepted — the port stays taken, so nobody else
/// can pick it up in the meantime. Spawned on `runtime`: the caller (the UI thread) is not inside it.
pub(crate) fn spawn_listener(
    runtime: &tokio::runtime::Handle,
    listener: TcpListener,
    mode: Mode,
    slot: Arc<ConnSlot>,
    stats: Arc<RuleStats>,
) -> AbortOnDrop {
    AbortOnDrop::new(runtime.spawn(accept_loop(listener, mode, slot, stats)))
}

async fn accept_loop(listener: TcpListener, mode: Mode, slot: Arc<ConnSlot>, stats: Arc<RuleStats>) {
    // Owned by this task: aborting the loop drops the set, which aborts every connection.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => {
                    let Some(connection) = slot.current() else {
                        // Waiting for the reconnect: refuse at once instead of leaving it hanging.
                        drop(socket);
                        continue;
                    };
                    let _ = socket.set_nodelay(true);
                    let active = ActiveGuard::new(&stats);
                    let stats = stats.clone();
                    match mode.clone() {
                        Mode::Forward(target) => {
                            connections.spawn(forward_connection(socket, peer, connection, target, stats, active));
                        }
                        Mode::Socks => {
                            connections.spawn(socks_connection(socket, peer, connection, stats, active));
                        }
                    }
                }
                Err(err) => {
                    // Out of file descriptors and the like: back off instead of spinning.
                    log::warn!("接受本地连接失败：{err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn forward_connection(
    socket: TcpStream,
    peer: SocketAddr,
    connection: Connection,
    target: Endpoint,
    stats: Arc<RuleStats>,
    _active: ActiveGuard,
) {
    match connection.open_direct_tcpip(&target.host, target.port, peer).await {
        Ok(ssh) => {
            let closed = ssh.connection_closed();
            pump(socket, ssh, &stats, closed).await
        }
        Err(err) => log::debug!("经 SSH 转发到 {target} 失败：{err}"),
    }
}

async fn socks_connection(
    mut socket: TcpStream,
    peer: SocketAddr,
    connection: Connection,
    stats: Arc<RuleStats>,
    _active: ActiveGuard,
) {
    let request = match tokio::time::timeout(SOCKS_HANDSHAKE, socks::negotiate(&mut socket)).await {
        Ok(Ok(Negotiated::Connect(request))) => request,
        // Refused (already answered where the protocol says so), failed or too slow.
        _ => {
            let _ = socket.shutdown().await;
            return;
        }
    };
    match connection.open_direct_tcpip(&request.host, request.port, peer).await {
        Ok(ssh) => {
            if socks::send_reply(&mut socket, socks::reply::SUCCEEDED).await.is_ok() {
                let closed = ssh.connection_closed();
                pump(socket, ssh, &stats, closed).await;
            }
        }
        Err(err) => {
            log::debug!("SOCKS5 经 SSH 连接 {}:{} 失败：{err}", request.host, request.port);
            let _ = socks::send_reply(&mut socket, socks::code_for(&err)).await;
            let _ = socket.shutdown().await;
        }
    }
}

/// The state change a task makes to its own entry; ignored if the entry has moved on.
pub(crate) type Update = Box<dyn FnOnce(&mut crate::forwarder::Entry) + Send>;

/// Everything a task needs to run one remote forward.
pub(crate) struct RemoteRun {
    pub inner: Weak<Inner>,
    pub slot: Arc<ConnSlot>,
    pub id: RuleId,
    pub generation: u64,
    pub listen: Endpoint,
    pub target: Endpoint,
    pub stats: Arc<RuleStats>,
    pub grants: GrantMemory,
    pub timing: RetryTiming,
    /// The entry already says [`RemotePortHeld`](RuleState::RemotePortHeld): ask at once and, if the
    /// answer is still no, go on waiting instead of retrying quietly again.
    pub held: bool,
}

impl RemoteRun {
    fn update(&self, change: Update) {
        if let Some(inner) = self.inner.upgrade() {
            inner.update(&self.id, self.generation, change);
        }
    }

    fn set_state(&self, state: RuleState) {
        self.update(Box::new(move |entry| entry.set_state(state)));
    }

    /// Asks the server to listen until it does or the attempt is over (then the state says why).
    ///
    /// A refusal of a fixed port is first retried quietly a few times. If the server has granted this
    /// very port to this rule before, a refusal that goes on means the old session still holds it
    /// ([`RemotePortHeld`](RuleState::RemotePortHeld)) and the server is asked again with growing
    /// pauses for as long as the connection is up. Otherwise it is a plain failure. The SSH
    /// connection is only held while talking to the server, never during a pause.
    async fn ask(&self) -> Option<RemoteForward> {
        let known = self.grants.knows(&self.id, &self.listen);
        let (mut quiet, mut rounds, mut held) = (0, 0, self.held);
        loop {
            let Some(connection) = self.slot.current() else {
                self.set_state(RuleState::WaitingReconnect);
                return None;
            };
            let asked = connection.request_remote_forward(&self.listen.host, self.listen.port).await;
            let closed = connection.is_closed();
            drop(connection);
            let err = match asked {
                Ok(forward) => return Some(forward),
                Err(err) => err,
            };
            if closed {
                self.set_state(RuleState::WaitingReconnect);
                return None;
            }
            // Port 0 lets the server choose, so a refusal is about forwarding itself.
            let denied = matches!(err, SshError::RemoteForwardDenied);
            let delay = if denied && self.listen.port != 0 && known && (held || quiet >= self.timing.quick_retries) {
                if !held {
                    held = true;
                    self.set_state(RuleState::RemotePortHeld { port: self.listen.port });
                    log::debug!("远程转发 {}：服务器仍在占用这个端口，稍后再试", self.listen);
                }
                rounds += 1;
                self.timing.held_delay(rounds - 1)
            } else if denied && self.listen.port != 0 && !held && quiet < self.timing.quick_retries {
                quiet += 1;
                self.timing.quick_delay
            } else {
                let state = RuleState::Failed(match err {
                    SshError::RemoteForwardDenied => RuleError::RemoteDenied,
                    other => RuleError::Remote(other.to_string()),
                });
                log::debug!("远程转发 {} 未能建立：{state:?}", self.listen);
                self.set_state(state);
                return None;
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// Runs the forward: asks the server to listen, then serves what it hands back until the connection
    /// is gone.
    pub async fn run(self) {
        let Some(mut forward) = self.ask().await else { return };
        self.grants.remember(&self.id, &self.listen);
        let (address, port) = (forward.address.clone(), forward.port);
        self.update(Box::new(move |entry| {
            entry.remote = Some((address, port));
            entry.set_state(RuleState::Running { port });
        }));

        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                incoming = forward.accept() => match incoming {
                    Some(stream) => {
                        let active = ActiveGuard::new(&self.stats);
                        connections.spawn(remote_connection(
                            stream,
                            self.target.clone(),
                            self.stats.clone(),
                            active,
                        ));
                    }
                    None => break,
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        // The connection ended under us; the owner brings the rule back after the reconnect.
        self.update(Box::new(|entry| {
            entry.remote = None;
            entry.set_state(RuleState::WaitingReconnect);
        }));
    }
}

async fn remote_connection(stream: ForwardedStream, target: Endpoint, stats: Arc<RuleStats>, _active: ActiveGuard) {
    let connecting = TcpStream::connect((target.host.as_str(), target.port));
    match tokio::time::timeout(LOCAL_CONNECT, connecting).await {
        Ok(Ok(local)) => {
            let _ = local.set_nodelay(true);
            let closed = stream.connection_closed();
            pump(local, stream, &stats, closed).await;
        }
        Ok(Err(err)) => log::debug!("远程转发无法连接本机目标 {target}：{err}"),
        Err(_) => log::debug!("远程转发连接本机目标 {target} 超时"),
    }
}
