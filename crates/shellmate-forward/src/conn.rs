//! The tasks behind a started rule: the accept loops and what happens to each connection.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use shellmate_ssh::{Connection, ForwardedStream, SshError, WeakConnection};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::addr::Endpoint;
use crate::forwarder::Inner;
use crate::rule::RuleId;
use crate::socks::{self, Negotiated};
use crate::state::{RuleError, RuleState};
use crate::stats::{ActiveGuard, RuleStats, pump};

/// How long a SOCKS client may take to send its greeting and request.
const SOCKS_HANDSHAKE: Duration = Duration::from_secs(30);
/// How long a remote forward waits to reach its local target.
const LOCAL_CONNECT: Duration = Duration::from_secs(10);
/// A server that refuses a remote forward on a port we named may only be still holding it for the
/// connection that had it a moment ago (a reconnect, a rule switched off and on, a rule that moved
/// to another connection): ask again this many times, this far apart, before giving up.
const DENIED_RETRIES: u32 = 5;
const DENIED_RETRY_DELAY: Duration = Duration::from_millis(400);

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

/// Runs a remote forward: asks the server to listen, then serves what it hands back until the
/// connection is gone. Holds the SSH connection only while talking to the server.
pub(crate) async fn run_remote(
    inner: Weak<Inner>,
    slot: Arc<ConnSlot>,
    id: RuleId,
    generation: u64,
    listen: Endpoint,
    target: Endpoint,
    stats: Arc<RuleStats>,
) {
    let update = |change: Update| {
        if let Some(inner) = inner.upgrade() {
            inner.update(&id, generation, change);
        }
    };
    let Some(connection) = slot.current() else {
        update(Box::new(|entry| entry.set_state(RuleState::WaitingReconnect)));
        return;
    };
    let mut retries = 0;
    let asked = loop {
        match connection.request_remote_forward(&listen.host, listen.port).await {
            // Port 0 lets the server choose, so a refusal is about forwarding itself.
            Err(SshError::RemoteForwardDenied)
                if listen.port != 0 && retries < DENIED_RETRIES && !connection.is_closed() =>
            {
                retries += 1;
                tokio::time::sleep(DENIED_RETRY_DELAY).await;
            }
            other => break other,
        }
    };
    let mut forward = match asked {
        Ok(forward) => forward,
        Err(err) => {
            let state = if connection.is_closed() {
                RuleState::WaitingReconnect
            } else {
                RuleState::Failed(match err {
                    SshError::RemoteForwardDenied => RuleError::RemoteDenied,
                    other => RuleError::Remote(other.to_string()),
                })
            };
            log::debug!("远程转发 {listen} 未能建立：{state:?}");
            update(Box::new(move |entry| entry.set_state(state)));
            return;
        }
    };
    drop(connection);
    let (address, port) = (forward.address.clone(), forward.port);
    update(Box::new(move |entry| {
        entry.remote = Some((address, port));
        entry.set_state(RuleState::Running { port });
    }));

    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = forward.accept() => match incoming {
                Some(stream) => {
                    let active = ActiveGuard::new(&stats);
                    connections.spawn(remote_connection(stream, target.clone(), stats.clone(), active));
                }
                None => break,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    // The connection ended under us; the owner brings the rule back after the reconnect.
    update(Box::new(|entry| {
        entry.remote = None;
        entry.set_state(RuleState::WaitingReconnect);
    }));
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
