//! The russh client handler: host key verification and disconnect bookkeeping.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use russh::Channel;
use russh::client::{ChannelOpenHandle, DisconnectReason, Handler, Msg, Session};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use shellmate_core::FingerprintState;

use crate::config::{ConnectEvent, ConnectOptions};
use crate::error::{SshError, describe_russh_error};
use crate::forward::RemoteRegistry;
use crate::interact::{HostKeyInfo, HostKeyRequest, HostKeyVerifier};
use crate::known_hosts::{KnownHosts, Lookup};
use crate::watchdog::Watchdog;

/// State shared between a connection's handler (running in russh's session
/// task) and the code that owns the connection.
#[derive(Default)]
pub(crate) struct Shared {
    host_key: Mutex<Option<HostKeyRecord>>,
    disconnect: Mutex<Option<String>>,
    /// Who the server's `forwarded-tcpip` channels go to.
    pub remote: Arc<RemoteRegistry>,
    /// Set once the transport has ended.
    pub closed: ClosedFlag,
}

/// A flag that goes up once, when the transport ends, and that anybody can wait for.
pub(crate) struct ClosedFlag(watch::Sender<bool>);

impl Default for ClosedFlag {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl ClosedFlag {
    fn raise(&self) {
        self.0.send_replace(true);
    }

    pub fn watch(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// What was decided about the host key of a hop.
#[derive(Clone, Debug)]
pub(crate) struct HostKeyRecord {
    pub key: HostKeyInfo,
    pub state: FingerprintState,
    pub saved: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub fn host_key(&self) -> Option<HostKeyRecord> {
        lock(&self.host_key).clone()
    }

    /// User-facing reason the transport ended, once it did.
    pub fn disconnect_reason(&self) -> Option<String> {
        lock(&self.disconnect).clone()
    }
}

/// Errors the handler can hand back to russh.
#[derive(Debug)]
pub(crate) enum HandlerError {
    Russh(russh::Error),
    Ssh(SshError),
}

impl From<russh::Error> for HandlerError {
    fn from(err: russh::Error) -> Self {
        HandlerError::Russh(err)
    }
}

impl From<HandlerError> for SshError {
    fn from(err: HandlerError) -> Self {
        match err {
            HandlerError::Russh(err) => SshError::from(err),
            HandlerError::Ssh(err) => err,
        }
    }
}

pub(crate) struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub known_hosts: KnownHosts,
    pub verifier: Arc<dyn HostKeyVerifier>,
    pub options: ConnectOptions,
    pub shared: Arc<Shared>,
    pub watchdog: Watchdog,
}

pub(crate) fn host_key_info(key: &PublicKey) -> HostKeyInfo {
    HostKeyInfo {
        algorithm: key.algorithm().as_str().to_string(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    }
}

impl Handler for ClientHandler {
    type Error = HandlerError;

    async fn check_server_key(&mut self, server_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            return Err(HandlerError::Ssh(SshError::UnsupportedHostKey));
        };
        let lookup = self
            .known_hosts
            .lookup(&self.host, self.port, key)
            .map_err(|err| HandlerError::Ssh(SshError::KnownHosts(err.to_string())))?;
        let info = host_key_info(key);

        let record = match lookup {
            Lookup::Known => HostKeyRecord { key: info, state: FingerprintState::Known, saved: true },
            Lookup::New { other } => {
                let recorded = other.iter().map(host_key_info).collect();
                let request = HostKeyRequest {
                    host: self.host.clone(),
                    port: self.port,
                    state: FingerprintState::New,
                    key: info.clone(),
                    recorded,
                };
                let trusted = {
                    // Waiting for the user does not count against the watchdog.
                    let _pause = self.watchdog.pause();
                    self.verifier.decide(request).await
                };
                if trusted == crate::HostKeyDecision::Reject {
                    return Err(HandlerError::Ssh(SshError::HostKeyRejected));
                }
                let saved = self.known_hosts.accept(trusted, false, &self.host, self.port, key);
                HostKeyRecord {
                    key: info,
                    state: FingerprintState::New,
                    saved: match saved {
                        Ok(saved) => saved,
                        Err(err) => self.log_save(Err(err)),
                    },
                }
            }
            Lookup::Changed { recorded } => {
                let request = HostKeyRequest {
                    host: self.host.clone(),
                    port: self.port,
                    state: FingerprintState::Changed,
                    key: info.clone(),
                    recorded: recorded.iter().map(host_key_info).collect(),
                };
                let trusted = {
                    let _pause = self.watchdog.pause();
                    self.verifier.decide(request).await
                };
                if trusted == crate::HostKeyDecision::Reject {
                    return Err(HandlerError::Ssh(SshError::HostKeyChanged));
                }
                let saved = self.known_hosts.accept(trusted, true, &self.host, self.port, key);
                HostKeyRecord {
                    key: info,
                    state: FingerprintState::Changed,
                    saved: match saved {
                        Ok(saved) => saved,
                        Err(err) => self.log_save(Err(err)),
                    },
                }
            }
        };

        self.options.emit(ConnectEvent::HostKeyVerified {
            host: self.host.clone(),
            port: self.port,
            key: record.key.clone(),
            state: record.state,
            saved: record.saved,
        });
        *lock(&self.shared.host_key) = Some(record);
        Ok(true)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shared
            .remote
            .dispatch(channel, connected_address, connected_port, originator_address, originator_port, reply)
            .await;
        Ok(())
    }

    async fn disconnected(&mut self, reason: DisconnectReason<Self::Error>) -> Result<(), Self::Error> {
        let text = match &reason {
            DisconnectReason::ReceivedDisconnect(info) if info.message.trim().is_empty() => {
                "服务器主动断开了连接".to_string()
            }
            DisconnectReason::ReceivedDisconnect(info) => {
                format!("服务器主动断开了连接：{}", info.message.trim())
            }
            DisconnectReason::Error(HandlerError::Russh(err)) => describe_russh_error(err),
            DisconnectReason::Error(HandlerError::Ssh(err)) => err.to_string(),
        };
        *lock(&self.shared.disconnect) = Some(text);
        self.shared.remote.close();
        self.shared.closed.raise();
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(err) => Err(err),
        }
    }
}

impl ClientHandler {
    /// Whether `known_hosts` was updated; failures only produce a warning
    /// because the user already trusted the key for this session.
    fn log_save(&self, result: std::io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(err) => {
                log::warn!("could not update {}: {err}", self.known_hosts.path().display());
                false
            }
        }
    }
}
