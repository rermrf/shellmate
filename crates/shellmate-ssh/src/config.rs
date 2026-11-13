//! Description of what to connect to and how.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use shellmate_core::{AuthMethod, FingerprintState};

use crate::interact::{HostKeyInfo, HostKeyVerifier, Prompter};
use crate::known_hosts::KnownHosts;
use crate::secret::Secret;

/// One SSH server and how to authenticate to it.
#[derive(Clone, Debug)]
pub struct SshHost {
    /// Host name or IP address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Login name.
    pub user: String,
    /// Authentication methods in the order they are tried. The server's
    /// supported methods are respected: methods it does not offer are skipped.
    pub auth: Vec<AuthMethod>,
    /// Private key file for [`AuthMethod::PrivateKey`]. Without it the
    /// default keys of the SSH directory (`id_ed25519`, `id_ecdsa`, `id_rsa`) are tried.
    pub identity_file: Option<PathBuf>,
    /// Password already known (for example from the keychain). Used for the
    /// first password attempt; later attempts ask the [`Prompter`].
    pub password: Option<Secret>,
    /// Passphrase of the private key already known; used for the first attempt.
    pub passphrase: Option<Secret>,
}

impl SshHost {
    /// A host authenticating with the single method `auth`.
    pub fn new(host: impl Into<String>, port: u16, user: impl Into<String>, auth: AuthMethod) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            auth: vec![auth],
            identity_file: None,
            password: None,
            passphrase: None,
        }
    }

    /// Sets the private key file.
    pub fn with_identity_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_file = Some(path.into());
        self
    }

    /// Sets a known password.
    pub fn with_password(mut self, password: Secret) -> Self {
        self.password = Some(password);
        self
    }

    /// Sets a known key passphrase.
    pub fn with_passphrase(mut self, passphrase: Secret) -> Self {
        self.passphrase = Some(passphrase);
        self
    }

    /// Also tries `method` if the previous ones fail.
    pub fn with_fallback(mut self, method: AuthMethod) -> Self {
        if !self.auth.contains(&method) {
            self.auth.push(method);
        }
        self
    }
}

/// The path to a server: optional jump hosts (ProxyJump) and the target.
#[derive(Clone, Debug)]
pub struct Route {
    /// Jump hosts in connection order. Every hop is verified and authenticated.
    pub via: Vec<SshHost>,
    /// The server the session finally runs on.
    pub target: SshHost,
}

impl Route {
    /// Connects to `target` directly.
    pub fn direct(target: SshHost) -> Self {
        Self { via: Vec::new(), target }
    }

    /// Goes through `jump` first (appended after earlier jump hosts).
    pub fn via(mut self, jump: SshHost) -> Self {
        self.via.push(jump);
        self
    }
}

/// Progress of [`Connection::connect`](crate::Connection::connect), one event
/// per step and per hop, for the "connecting..." lines of the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // the fields of the struct variants are named after what they hold
pub enum ConnectEvent {
    /// Opening the transport to a hop; `via` is the previous hop, if any.
    Connecting { host: String, port: u16, via: Option<String> },
    /// The hop's host key was accepted. `saved` is false if `known_hosts`
    /// could not be written (the connection continues anyway).
    HostKeyVerified { host: String, port: u16, key: HostKeyInfo, state: FingerprintState, saved: bool },
    /// The hop authenticated us. `method` is `None` if the server needed no credentials.
    Authenticated { host: String, port: u16, user: String, method: Option<AuthMethod> },
}

/// Callback receiving [`ConnectEvent`]s; may be called from any runtime thread.
pub type EventCallback = Arc<dyn Fn(ConnectEvent) + Send + Sync>;

/// Environment and callbacks of a connection attempt.
#[derive(Clone)]
pub struct ConnectOptions {
    /// Asked about new and changed host keys.
    pub verifier: Arc<dyn HostKeyVerifier>,
    /// Asked for passwords, passphrases and keyboard-interactive answers.
    pub prompter: Arc<dyn Prompter>,
    /// `known_hosts` file; default `~/.ssh/known_hosts`.
    pub known_hosts: PathBuf,
    /// Directory searched for default private keys; default `~/.ssh`.
    pub ssh_dir: PathBuf,
    /// ssh-agent socket; `None` reads `SSH_AUTH_SOCK` when the agent is needed.
    pub agent_socket: Option<PathBuf>,
    /// Limit for establishing each TCP connection (the handshake, which may
    /// wait for the user, is not covered).
    pub connect_timeout: Duration,
    /// Keepalive probe interval once connected.
    pub keepalive_interval: Duration,
    /// Unanswered probes before the connection is declared dead.
    pub keepalive_max: usize,
    /// Progress callback.
    pub on_event: Option<EventCallback>,
}

impl ConnectOptions {
    /// Options using the user's real `~/.ssh` and `SSH_AUTH_SOCK`, keepalive
    /// 30 s × 3 and a 15 s TCP connect timeout.
    pub fn new(verifier: Arc<dyn HostKeyVerifier>, prompter: Arc<dyn Prompter>) -> Self {
        let ssh_dir = std::env::home_dir().unwrap_or_default().join(".ssh");
        Self {
            verifier,
            prompter,
            known_hosts: KnownHosts::default_path(),
            ssh_dir,
            agent_socket: None,
            connect_timeout: Duration::from_secs(15),
            keepalive_interval: Duration::from_secs(30),
            keepalive_max: 3,
            on_event: None,
        }
    }

    /// Uses another `known_hosts` file.
    pub fn with_known_hosts(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts = path.into();
        self
    }

    /// Searches another directory for default private keys.
    pub fn with_ssh_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.ssh_dir = dir.into();
        self
    }

    /// Talks to the ssh-agent at `socket` instead of `SSH_AUTH_SOCK`.
    pub fn with_agent_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.agent_socket = Some(socket.into());
        self
    }

    /// Receives progress events.
    pub fn with_event_handler(mut self, handler: impl Fn(ConnectEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Arc::new(handler));
        self
    }

    pub(crate) fn emit(&self, event: ConnectEvent) {
        if let Some(callback) = &self.on_event {
            callback(event);
        }
    }
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("known_hosts", &self.known_hosts)
            .field("ssh_dir", &self.ssh_dir)
            .field("agent_socket", &self.agent_socket)
            .field("connect_timeout", &self.connect_timeout)
            .finish_non_exhaustive()
    }
}
