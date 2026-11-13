//! A connection to an SSH server, directly or through ProxyJump hops.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::Algorithm;
use russh::{Disconnect, Preferred};
use shellmate_core::{AuthMethod, FingerprintState, PtyDims};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::runtime;

use crate::auth::{AuthContext, authenticate};
use crate::config::{ConnectEvent, ConnectOptions, Route, SshHost};
use crate::error::SshError;
use crate::handler::{ClientHandler, Shared};
use crate::interact::HostKeyInfo;
use crate::known_hosts::KnownHosts;
use crate::shell::PendingShell;

/// What is known about one hop of an established connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HopInfo {
    /// Host name or address as configured.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Login name.
    pub user: String,
    /// The host key the hop presented.
    pub host_key: HostKeyInfo,
    /// `Known` if `known_hosts` already vouched for it; `New` / `Changed` if the user had to confirm.
    pub host_key_state: FingerprintState,
    /// Method that authenticated us; `None` if the server wanted no credentials.
    pub auth: Option<AuthMethod>,
    /// The private key file that authenticated us (key authentication only).
    pub key_file: Option<std::path::PathBuf>,
}

pub(crate) struct Inner {
    pub handle: Handle<ClientHandler>,
    pub shared: Arc<Shared>,
    pub runtime: runtime::Handle,
    /// Every hop from the first jump host to this one.
    hops: Vec<HopInfo>,
    /// The previous hop; kept alive because this connection is tunnelled through it.
    _via: Option<Connection>,
}

/// An authenticated SSH connection that can host several shell channels.
///
/// Cloning is cheap and shares the connection; it closes when the last clone
/// and the last channel opened on it are dropped.
#[derive(Clone)]
pub struct Connection {
    pub(crate) inner: Arc<Inner>,
}

impl Connection {
    /// Connects along `route`: every jump host and the target get the same
    /// host key check and authentication. Must run inside the tokio runtime
    /// (see [`SshRuntime::connect`](crate::SshRuntime::connect)).
    pub async fn connect(route: Route, options: ConnectOptions) -> Result<Connection, SshError> {
        let runtime = runtime::Handle::try_current().map_err(|_| SshError::Connect {
            host: route.target.host.clone(),
            port: route.target.port,
            reason: "SSH 运行时未启动".into(),
        })?;
        let Route { via, target } = route;
        let mut previous: Option<Connection> = None;
        let total = via.len() + 1;
        for (index, hop) in via.into_iter().chain(std::iter::once(target)).enumerate() {
            let is_jump = index + 1 < total;
            previous = Some(connect_hop(&hop, previous.take(), is_jump, &options, &runtime).await?);
        }
        previous.ok_or(SshError::NoAuthMethod)
    }

    /// Hops from the first jump host to the target; the last entry is this connection.
    pub fn hops(&self) -> &[HopInfo] {
        &self.inner.hops
    }

    /// The server this connection ends at.
    pub fn target(&self) -> &HopInfo {
        self.inner.hops.last().expect("a connection has at least one hop")
    }

    /// Whether the transport is gone.
    pub fn is_closed(&self) -> bool {
        self.inner.handle.is_closed()
    }

    /// Why the transport ended, if it did.
    pub fn disconnect_reason(&self) -> Option<String> {
        self.inner.shared.disconnect_reason()
    }

    /// Opens a session channel with a PTY (`xterm-256color`) and starts the
    /// user's shell. Output only flows once [`PendingShell::start`] is called,
    /// so the caller can attach a sink first.
    pub async fn open_shell(&self, dims: PtyDims) -> Result<PendingShell, SshError> {
        PendingShell::open(self.clone(), dims).await
    }

    /// Sends a disconnect message and closes the transport.
    pub async fn disconnect(&self) {
        let _ = self.inner.handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }

    /// Resolves once the transport is closed. Polls, so it is meant for tests
    /// and shutdown paths rather than hot loops.
    pub async fn closed(&self) {
        while !self.is_closed() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let target = self.target();
        write!(f, "Connection({}@{}:{})", target.user, target.host, target.port)
    }
}

/// Opens the transport to `host` (TCP, or a direct-tcpip channel of `via`),
/// then verifies its key and authenticates.
async fn connect_hop(
    host: &SshHost,
    via: Option<Connection>,
    is_jump: bool,
    options: &ConnectOptions,
    runtime: &runtime::Handle,
) -> Result<Connection, SshError> {
    options.emit(ConnectEvent::Connecting {
        host: host.host.clone(),
        port: host.port,
        via: via.as_ref().map(|c| c.target().host.clone()),
    });

    match via {
        None => {
            let connect = TcpStream::connect((host.host.as_str(), host.port));
            let stream = match tokio::time::timeout(options.connect_timeout, connect).await {
                Err(_) => {
                    return Err(SshError::ConnectTimeout {
                        host: host.host.clone(),
                        port: host.port,
                        secs: options.connect_timeout.as_secs(),
                    });
                }
                Ok(Err(err)) => {
                    return Err(SshError::Connect {
                        host: host.host.clone(),
                        port: host.port,
                        reason: describe_io(&err),
                    });
                }
                Ok(Ok(stream)) => stream,
            };
            let _ = stream.set_nodelay(true);
            handshake(stream, host, None, options, runtime).await.map_err(|err| as_jump_error(err, host, is_jump))
        }
        Some(jump) => {
            let opened =
                jump.inner.handle.channel_open_direct_tcpip(host.host.clone(), host.port.into(), "127.0.0.1", 0);
            let channel = match tokio::time::timeout(options.connect_timeout, opened).await {
                Err(_) => {
                    return Err(SshError::ConnectTimeout {
                        host: host.host.clone(),
                        port: host.port,
                        secs: options.connect_timeout.as_secs(),
                    });
                }
                Ok(Err(err)) => {
                    return Err(SshError::Connect {
                        host: host.host.clone(),
                        port: host.port,
                        reason: format!("经由 {} 转发失败：{err}", jump.target().host),
                    });
                }
                Ok(Ok(channel)) => channel,
            };
            handshake(channel.into_stream(), host, Some(jump), options, runtime)
                .await
                .map_err(|err| as_jump_error(err, host, is_jump))
        }
    }
}

/// Failures after the transport is up name the jump host they happened on.
fn as_jump_error(err: SshError, host: &SshHost, is_jump: bool) -> SshError {
    if is_jump { SshError::Jump { host: host.host.clone(), port: host.port, source: Box::new(err) } } else { err }
}

fn describe_io(err: &std::io::Error) -> String {
    use std::io::ErrorKind::{ConnectionRefused, NotFound};
    match err.kind() {
        ConnectionRefused => "连接被拒绝（端口未开放或服务未运行）".into(),
        NotFound => err.to_string(),
        _ => err.to_string(),
    }
}

/// SSH handshake, host key check and authentication over an open transport.
async fn handshake<S>(
    stream: S,
    host: &SshHost,
    via: Option<Connection>,
    options: &ConnectOptions,
    runtime: &runtime::Handle,
) -> Result<Connection, SshError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let shared = Arc::new(Shared::default());
    let known_hosts = KnownHosts::new(options.known_hosts.clone());
    let recorded = known_hosts.recorded_algorithms(&host.host, host.port).unwrap_or_default();
    let config = Arc::new(client::Config {
        keepalive_interval: Some(options.keepalive_interval),
        keepalive_max: options.keepalive_max,
        preferred: Preferred {
            key: Cow::Owned(prefer_recorded(&Preferred::DEFAULT.key, &recorded)),
            ..Preferred::default()
        },
        ..Default::default()
    });
    let handler = ClientHandler {
        host: host.host.clone(),
        port: host.port,
        known_hosts,
        verifier: options.verifier.clone(),
        options: options.clone(),
        shared: shared.clone(),
    };

    let mut handle = client::connect_stream(config, stream, handler).await?;
    let ctx = AuthContext {
        prompter: options.prompter.as_ref(),
        ssh_dir: &options.ssh_dir,
        agent_socket: options.agent_socket.as_deref(),
    };
    let auth = authenticate(&mut handle, host, &ctx).await?;
    options.emit(ConnectEvent::Authenticated {
        host: host.host.clone(),
        port: host.port,
        user: host.user.clone(),
        method: auth.method,
        key_file: auth.key_file.clone(),
    });

    let record = shared.host_key().ok_or_else(|| SshError::Handshake("服务器没有提供主机密钥".into()))?;
    let mut hops = via.as_ref().map(|v| v.inner.hops.clone()).unwrap_or_default();
    hops.push(HopInfo {
        host: host.host.clone(),
        port: host.port,
        user: host.user.clone(),
        host_key: record.key,
        host_key_state: record.state,
        auth: auth.method,
        key_file: auth.key_file,
    });
    Ok(Connection { inner: Arc::new(Inner { handle, shared, runtime: runtime.clone(), hops, _via: via }) })
}

/// Moves the algorithms we already have a recorded key for to the front, so
/// the server presents a key `known_hosts` can check.
fn prefer_recorded(default: &[Algorithm], recorded: &[Algorithm]) -> Vec<Algorithm> {
    fn same_family(a: &Algorithm, b: &Algorithm) -> bool {
        match (a, b) {
            (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
            _ => a == b,
        }
    }
    let (mut front, back): (Vec<_>, Vec<_>) =
        default.iter().cloned().partition(|alg| recorded.iter().any(|known| same_family(alg, known)));
    front.extend(back);
    front
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_algorithms_are_offered_first() {
        let default = Preferred::DEFAULT.key;
        let ecdsa = Algorithm::Ecdsa { curve: russh::keys::EcdsaCurve::NistP256 };

        let order = prefer_recorded(&default, std::slice::from_ref(&ecdsa));
        assert_eq!(order[0], ecdsa);
        assert_eq!(order.len(), default.len(), "nothing is dropped, only reordered");

        // All RSA signature variants belong to one recorded key type.
        let order = prefer_recorded(&default, &[Algorithm::Rsa { hash: None }]);
        assert!(order[..3].iter().all(|alg| matches!(alg, Algorithm::Rsa { .. })), "{order:?}");

        assert_eq!(prefer_recorded(&default, &[]), default.to_vec());
    }
}
