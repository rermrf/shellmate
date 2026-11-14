//! Test fixtures: an in-process russh server on 127.0.0.1, scripted prompts and a
//! recording terminal sink. Nothing here touches the real `~/.ssh`, agent or keychain.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use russh::keys::agent::client::AgentClient;
use russh::keys::key::safe_rng;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Response, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, ChannelStream, Disconnect, MethodKind, MethodSet};
use shellmate_core::{OutputHandler, OutputSink};
use shellmate_ssh::{
    ChallengeRequest, ConnectOptions, HostKeyRequest, HostKeyVerifier, PassphraseRequest, PasswordRequest, Prompter,
    Secret,
};
use tokio::net::{TcpListener, TcpStream, UnixListener};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Polls `check` until it returns a value; panics after 10 s.
pub async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- server -------------------------------------------------------------

/// One round of keyboard-interactive: the prompts to show and the answers that pass.
#[derive(Clone)]
pub struct KbdRound {
    pub prompts: Vec<(&'static str, bool)>,
    pub expect: Vec<&'static str>,
}

/// Runs a subsystem (`sftp`) on the byte stream of the channel that requested it.
pub type SubsystemRunner = Arc<dyn Fn(ChannelStream<Msg>) + Send + Sync>;

#[derive(Clone)]
pub struct ServerOptions {
    /// (user, password) pairs accepted by `password` auth.
    pub passwords: Vec<(&'static str, &'static str)>,
    /// Public keys accepted by `publickey` auth (any user).
    pub authorized_keys: Vec<PublicKey>,
    /// Keyboard-interactive script; `None` rejects the method.
    pub kbd: Option<Vec<KbdRound>>,
    /// Authentication methods advertised to clients.
    pub methods: Vec<MethodKind>,
    /// Require a valid public key first, then keyboard-interactive.
    pub two_factor: bool,
    /// Serve `direct-tcpip` channels (jump host).
    pub forwarding: bool,
    /// Serve `tcpip-forward` requests: listen on 127.0.0.1 and open `forwarded-tcpip` channels.
    pub remote_forwarding: bool,
    /// Never answer `direct-tcpip` requests (a target that does not respond).
    pub stall_direct_tcpip: bool,
    /// The address announced in `forwarded-tcpip` channels instead of the one that was asked
    /// for (OpenSSH may answer `127.0.0.1` for `localhost`).
    pub announce_address: Option<&'static str>,
    /// How long the server sits on a PTY request before answering.
    pub pty_delay: Duration,
    /// How long the server sits on a password check before answering.
    pub auth_delay: Duration,
    /// Subsystems served on session channels, by name; others are refused.
    pub subsystems: Vec<(&'static str, SubsystemRunner)>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            passwords: vec![("alice", "secret")],
            authorized_keys: Vec::new(),
            kbd: None,
            methods: vec![MethodKind::Password, MethodKind::PublicKey, MethodKind::KeyboardInteractive],
            two_factor: false,
            forwarding: false,
            remote_forwarding: false,
            stall_direct_tcpip: false,
            announce_address: None,
            pty_delay: Duration::ZERO,
            auth_delay: Duration::ZERO,
            subsystems: Vec::new(),
        }
    }
}

/// What the server observed.
#[derive(Default)]
pub struct ServerState {
    /// `kind:user` for every authentication attempt (`password:alice`, `publickey:alice`, ...).
    pub attempts: Mutex<Vec<String>>,
    /// (term, cols, rows) of every pty request.
    pub ptys: Mutex<Vec<(String, u32, u32)>>,
    /// (cols, rows) of every window change.
    pub resizes: Mutex<Vec<(u32, u32)>>,
    /// (host, port) of every direct-tcpip request.
    pub forwards: Mutex<Vec<(String, u32)>>,
    /// (address, port) of every `tcpip-forward` request, as sent (port 0 stays 0).
    pub remote_forwards: Mutex<Vec<(String, u32)>>,
    /// (address, port) of every `cancel-tcpip-forward` request.
    pub cancelled_forwards: Mutex<Vec<(String, u32)>>,
    /// Number of accepted TCP connections.
    pub connections: Mutex<usize>,
    /// Connections whose SSH session has not ended yet.
    pub open_connections: Mutex<usize>,
    /// Number of shell requests (one per shell channel).
    pub shells: Mutex<usize>,
    /// Names of the subsystems requested (`sftp`).
    pub subsystem_requests: Mutex<Vec<String>>,
    /// Channels the client closed.
    pub closed_channels: Mutex<usize>,
    sockets: Mutex<Vec<std::net::TcpStream>>,
}

impl ServerState {
    /// Drops every connection abruptly (TCP shutdown without an SSH disconnect message).
    pub fn kill_connections(&self) {
        for socket in lock(&self.sockets).drain(..) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub struct TestServer {
    pub port: u16,
    pub host_key: PrivateKey,
    pub state: Arc<ServerState>,
    accept: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start(options: ServerOptions) -> Self {
        let host_key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        Self::start_with_key(options, host_key).await
    }

    pub async fn start_with_key(options: ServerOptions, host_key: PrivateKey) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut methods = MethodSet::empty();
        for kind in &options.methods {
            methods.push(*kind);
        }
        let config = Arc::new(server::Config {
            keys: vec![host_key.clone()],
            methods,
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: Some(Duration::from_secs(120)),
            ..Default::default()
        });
        let state = Arc::new(ServerState::default());

        let accept = tokio::spawn({
            let (state, options) = (state.clone(), Arc::new(options));
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    *lock(&state.connections) += 1;
                    *lock(&state.open_connections) += 1;
                    // Keep a second handle on the socket so tests can cut the connection.
                    let std_stream = stream.into_std().unwrap();
                    lock(&state.sockets).push(std_stream.try_clone().unwrap());
                    let stream = TcpStream::from_std(std_stream).unwrap();
                    let handler = FixtureHandler::new(options.clone(), state.clone());
                    let config = config.clone();
                    let closing = state.clone();
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, stream, handler).await {
                            let _ = session.await;
                        }
                        *lock(&closing.open_connections) -= 1;
                    });
                }
            }
        });
        Self { port, host_key, state, accept }
    }

    pub fn public_key(&self) -> PublicKey {
        self.host_key.public_key().clone()
    }

    /// `SHA256:...` fingerprint of the host key.
    pub fn fingerprint(&self) -> String {
        self.public_key().fingerprint(HashAlg::Sha256).to_string()
    }

    /// Server-side view of how many auth attempts of `kind` were made.
    pub fn attempts(&self, kind: &str) -> usize {
        lock(&self.state.attempts).iter().filter(|a| a.starts_with(kind)).count()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
        self.state.kill_connections();
    }
}

struct FixtureHandler {
    options: Arc<ServerOptions>,
    state: Arc<ServerState>,
    kbd_round: usize,
    key_ok: bool,
    /// Channels running the echo shell (other channels, e.g. forwarded ones, are left alone).
    shells: HashSet<ChannelId>,
    lines: HashMap<ChannelId, Vec<u8>>,
    sizes: HashMap<ChannelId, (u32, u32)>,
    /// Session channels kept until they ask for a subsystem (only when the server has any).
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// Listeners of remote forwards, by (address, actual port); stopped with the connection.
    listeners: HashMap<(String, u32), tokio::task::JoinHandle<()>>,
}

impl Drop for FixtureHandler {
    fn drop(&mut self) {
        for (_, listener) in self.listeners.drain() {
            listener.abort();
        }
    }
}

impl FixtureHandler {
    fn new(options: Arc<ServerOptions>, state: Arc<ServerState>) -> Self {
        Self {
            options,
            state,
            kbd_round: 0,
            key_ok: false,
            shells: HashSet::new(),
            lines: HashMap::new(),
            sizes: HashMap::new(),
            channels: HashMap::new(),
            listeners: HashMap::new(),
        }
    }

    /// Like OpenSSH, a rejection keeps the other methods (and this one) on offer.
    fn reject(&self) -> Auth {
        let mut methods = MethodSet::empty();
        for kind in &self.options.methods {
            methods.push(*kind);
        }
        Auth::Reject { proceed_with_methods: Some(methods), partial_success: false }
    }

    fn prompt(round: &KbdRound) -> Auth {
        Auth::Partial {
            name: "test".into(),
            instructions: "answer the questions".into(),
            prompts: round.prompts.iter().map(|(text, echo)| ((*text).into(), *echo)).collect::<Vec<_>>().into(),
        }
    }

    fn record(&self, entry: String) {
        lock(&self.state.attempts).push(entry);
    }
}

impl server::Handler for FixtureHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.record(format!("password:{user}"));
        tokio::time::sleep(self.options.auth_delay).await;
        let ok = self.options.passwords.iter().any(|(u, p)| *u == user && *p == password);
        Ok(if ok { Auth::Accept } else { self.reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        self.record(format!("publickey:{user}"));
        if !self.options.authorized_keys.iter().any(|k| k.key_data() == key.public_key.key_data()) {
            return Ok(self.reject());
        }
        if self.options.two_factor {
            self.key_ok = true;
            let mut next = MethodSet::empty();
            next.push(MethodKind::KeyboardInteractive);
            return Ok(Auth::Reject { proceed_with_methods: Some(next), partial_success: true });
        }
        Ok(Auth::Accept)
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some(rounds) = self.options.kbd.clone() else { return Ok(Auth::reject()) };
        match response {
            None => {
                self.record(format!("keyboard-interactive:{user}"));
                self.kbd_round = 0;
                Ok(Self::prompt(&rounds[0]))
            }
            Some(response) => {
                let answers: Vec<String> = response.map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
                if answers != rounds[self.kbd_round].expect {
                    return Ok(self.reject());
                }
                self.kbd_round += 1;
                if self.kbd_round < rounds.len() {
                    Ok(Self::prompt(&rounds[self.kbd_round]))
                } else if self.options.two_factor && !self.key_ok {
                    Ok(self.reject())
                } else {
                    Ok(Auth::Accept)
                }
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.options.subsystems.is_empty() {
            self.channels.insert(channel.id(), channel);
        }
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        lock(&self.state.subsystem_requests).push(name.to_string());
        let runner = self.options.subsystems.iter().find(|(known, _)| *known == name).map(|(_, run)| run.clone());
        match (runner, self.channels.remove(&channel)) {
            (Some(run), Some(chan)) => {
                session.channel_success(channel)?;
                run(chan.into_stream());
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        lock(&self.state.forwards).push((host.to_string(), port));
        if self.options.stall_direct_tcpip {
            // Keep the request open for good without holding up the session.
            tokio::spawn(async move {
                let _held = (channel, reply);
                std::future::pending::<()>().await;
            });
            return Ok(());
        }
        if !self.options.forwarding {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        }
        match TcpStream::connect((host, port as u16)).await {
            Ok(mut upstream) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }

    async fn tcpip_forward(&mut self, address: &str, port: &mut u32, session: &mut Session) -> Result<bool, Self::Error> {
        lock(&self.state.remote_forwards).push((address.to_string(), *port));
        if !self.options.remote_forwarding {
            return Ok(false);
        }
        let Ok(listener) = TcpListener::bind(("127.0.0.1", *port as u16)).await else { return Ok(false) };
        let actual = u32::from(listener.local_addr().unwrap().port());
        *port = actual;
        let announced = self.options.announce_address.map_or_else(|| address.to_string(), str::to_string);
        let handle = session.handle();
        let task = tokio::spawn(async move {
            while let Ok((socket, peer)) = listener.accept().await {
                let handle = handle.clone();
                let announced = announced.clone();
                tokio::spawn(async move {
                    let opened = handle
                        .channel_open_forwarded_tcpip(announced, actual, peer.ip().to_string(), u32::from(peer.port()))
                        .await;
                    if let Ok(channel) = opened {
                        let (mut socket, mut stream) = (socket, channel.into_stream());
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                    }
                });
            }
        });
        if let Some(old) = self.listeners.insert((address.to_string(), actual), task) {
            old.abort();
        }
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        lock(&self.state.cancelled_forwards).push((address.to_string(), port));
        match self.listeners.remove(&(address.to_string(), port)) {
            Some(listener) => {
                listener.abort();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        lock(&self.state.ptys).push((term.to_string(), cols, rows));
        self.sizes.insert(channel, (cols, rows));
        tokio::time::sleep(self.options.pty_delay).await;
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        *lock(&self.state.shells) += 1;
        self.shells.insert(channel);
        session.channel_success(channel)?;
        session.data(channel, b"fixture ready\r\n".to_vec())?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        lock(&self.state.resizes).push((cols, rows));
        self.sizes.insert(channel, (cols, rows));
        session.data(channel, format!("[winch {cols}x{rows}]\r\n").into_bytes())?;
        Ok(())
    }

    /// Echoes input; complete lines are commands: `exit N`, `bye`, `kill`, `size`.
    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        if !self.shells.contains(&channel) {
            return Ok(());
        }
        session.data(channel, data.to_vec())?;
        for &byte in data {
            if byte != b'\r' {
                self.lines.entry(channel).or_default().push(byte);
                continue;
            }
            let line = String::from_utf8_lossy(&self.lines.remove(&channel).unwrap_or_default()).into_owned();
            match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["exit", code] => {
                    session.exit_status_request(channel, code.parse().unwrap_or(0))?;
                    session.eof(channel)?;
                    session.close(channel)?;
                }
                ["bye"] => session.disconnect(Disconnect::ByApplication, "bye from fixture", "en")?,
                ["kill"] => self.state.kill_connections(),
                ["size"] => {
                    let (cols, rows) = self.sizes.get(&channel).copied().unwrap_or_default();
                    session.data(channel, format!("\r\n{cols}x{rows}\r\n").into_bytes())?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn channel_close(&mut self, _channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        *lock(&self.state.closed_channels) += 1;
        Ok(())
    }
}

// ---- client side doubles -------------------------------------------------

/// Host key verifier with a fixed answer that remembers what it was asked.
pub struct Verifier {
    pub accept: bool,
    /// How long the "user" takes to decide.
    pub delay: Duration,
    /// Ports whose keys are refused even when `accept` is true.
    pub deny_ports: Vec<u16>,
    pub requests: Mutex<Vec<HostKeyRequest>>,
}

impl Verifier {
    pub fn new(accept: bool) -> Arc<Self> {
        Arc::new(Self { accept, delay: Duration::ZERO, deny_ports: Vec::new(), requests: Mutex::new(Vec::new()) })
    }

    /// A verifier whose "user" thinks for `delay` before answering.
    pub fn slow(accept: bool, delay: Duration) -> Arc<Self> {
        Arc::new(Self { accept, delay, deny_ports: Vec::new(), requests: Mutex::new(Vec::new()) })
    }

    /// Trusts everything except the servers on `ports`.
    pub fn denying(ports: &[u16]) -> Arc<Self> {
        Arc::new(Self { accept: true, delay: Duration::ZERO, deny_ports: ports.to_vec(), requests: Mutex::new(Vec::new()) })
    }

    pub fn asked(&self) -> Vec<HostKeyRequest> {
        lock(&self.requests).clone()
    }
}

#[async_trait]
impl HostKeyVerifier for Verifier {
    async fn verify(&self, request: HostKeyRequest) -> bool {
        let port = request.port;
        lock(&self.requests).push(request);
        tokio::time::sleep(self.delay).await;
        self.accept && !self.deny_ports.contains(&port)
    }
}

/// Prompter answering from queues; an empty queue means "user cancelled".
#[derive(Default)]
pub struct Script {
    pub passwords: Mutex<VecDeque<String>>,
    pub passphrases: Mutex<VecDeque<String>>,
    pub challenges: Mutex<VecDeque<Vec<String>>>,
    pub password_requests: Mutex<Vec<PasswordRequest>>,
    pub passphrase_requests: Mutex<Vec<PassphraseRequest>>,
    pub challenge_requests: Mutex<Vec<ChallengeRequest>>,
    /// How long the "user" takes to answer any prompt.
    pub delay: Mutex<Duration>,
}

impl Script {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_delay(self: Arc<Self>, delay: Duration) -> Arc<Self> {
        *lock(&self.delay) = delay;
        self
    }

    async fn think(&self) {
        let delay = *lock(&self.delay);
        tokio::time::sleep(delay).await;
    }

    pub fn with_passwords(self: Arc<Self>, answers: &[&str]) -> Arc<Self> {
        lock(&self.passwords).extend(answers.iter().map(|s| s.to_string()));
        self
    }

    pub fn with_passphrases(self: Arc<Self>, answers: &[&str]) -> Arc<Self> {
        lock(&self.passphrases).extend(answers.iter().map(|s| s.to_string()));
        self
    }

    pub fn with_challenges(self: Arc<Self>, rounds: &[&[&str]]) -> Arc<Self> {
        lock(&self.challenges).extend(rounds.iter().map(|r| r.iter().map(|s| s.to_string()).collect()));
        self
    }
}

#[async_trait]
impl Prompter for Script {
    async fn password(&self, request: PasswordRequest) -> Option<Secret> {
        lock(&self.password_requests).push(request);
        self.think().await;
        lock(&self.passwords).pop_front().map(Secret::new)
    }

    async fn passphrase(&self, request: PassphraseRequest) -> Option<Secret> {
        lock(&self.passphrase_requests).push(request);
        lock(&self.passphrases).pop_front().map(Secret::new)
    }

    async fn challenge(&self, request: ChallengeRequest) -> Option<Vec<Secret>> {
        lock(&self.challenge_requests).push(request);
        lock(&self.challenges).pop_front().map(|answers| answers.into_iter().map(Secret::new).collect())
    }
}

/// Isolated client environment: temp known_hosts, empty ssh dir, no agent.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub known_hosts: PathBuf,
    pub ssh_dir: PathBuf,
}

impl Env {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join("ssh");
        std::fs::create_dir(&ssh_dir).unwrap();
        Self { known_hosts: dir.path().join("known_hosts"), ssh_dir, dir }
    }

    /// Options that cannot reach the real `~/.ssh` or the real agent.
    pub fn options(&self, verifier: Arc<Verifier>, prompter: Arc<Script>) -> ConnectOptions {
        ConnectOptions::new(verifier, prompter)
            .with_known_hosts(&self.known_hosts)
            .with_ssh_dir(&self.ssh_dir)
            .with_agent_socket(self.dir.path().join("no-agent.sock"))
    }

    /// Options with a verifier that trusts everything and no scripted answers.
    pub fn trusting(&self) -> ConnectOptions {
        self.options(Verifier::new(true), Script::new())
    }

    pub fn known_hosts_text(&self) -> String {
        std::fs::read_to_string(&self.known_hosts).unwrap_or_default()
    }

    /// Writes an unencrypted OpenSSH private key and returns its path and public key.
    pub fn write_key(&self, name: &str) -> (PathBuf, PrivateKey) {
        let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        let path = self.dir.path().join(name);
        std::fs::write(&path, key.to_openssh(russh::keys::ssh_key::LineEnding::LF).unwrap().as_bytes()).unwrap();
        (path, key)
    }
}

// ---- recording sink ------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Exited(Option<u32>),
    Disconnected(String),
}

/// A terminal stand-in recording everything a backend reports.
#[derive(Default)]
pub struct Recorder {
    pub output: Mutex<Vec<u8>>,
    pub events: Mutex<Vec<Event>>,
}

struct RecorderHandler(Arc<Recorder>);

impl OutputHandler for RecorderHandler {
    fn output(&self, bytes: &[u8]) {
        lock(&self.0.output).extend_from_slice(bytes);
    }

    fn exited(&self, code: Option<u32>) {
        lock(&self.0.events).push(Event::Exited(code));
    }

    fn disconnected(&self, reason: String) {
        lock(&self.0.events).push(Event::Disconnected(reason));
    }
}

impl Recorder {
    pub fn sink() -> (Arc<Self>, OutputSink) {
        let recorder = Arc::new(Self::default());
        let sink = OutputSink::new(RecorderHandler(recorder.clone()));
        (recorder, sink)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.output)).into_owned()
    }

    pub async fn wait_for_text(&self, needle: &str) {
        eventually(&format!("output containing {needle:?}"), || self.text().contains(needle).then_some(())).await;
    }

    pub async fn wait_for_event(&self) -> Event {
        eventually("a terminal event", || lock(&self.events).first().cloned()).await
    }
}

/// Starts an agent serving `socket` and loads `keys` into it.
pub async fn start_agent(socket: &Path, keys: &[&PrivateKey]) {
    let listener = UnixListener::bind(socket).unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(stream, _)| stream);
        Some((accepted, listener))
    })
    .boxed();
    tokio::spawn(russh::keys::agent::server::serve(incoming, ()));
    let mut client = AgentClient::connect_uds(socket).await.unwrap();
    for key in keys {
        client.add_identity(key, &[]).await.unwrap();
    }
}

/// A TCP server that accepts connections and never says a word (a dead SSH server).
pub struct SilentServer {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl SilentServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Self { port, task }
    }
}

impl Drop for SilentServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A TCP echo server on a loopback port: sends back everything it receives and, after the
/// client's end-of-file, closes its side (so half-closing works).
pub struct EchoServer {
    pub port: u16,
    /// Number of connections accepted so far.
    pub accepted: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl EchoServer {
    /// On `127.0.0.1`, any free port.
    pub async fn start() -> Self {
        Self::start_on("127.0.0.1").await.expect("loopback is available")
    }

    /// On `::1`; `None` if this machine has no IPv6 loopback.
    pub async fn start_v6() -> Option<Self> {
        Self::start_on("::1").await
    }

    async fn start_on(address: &str) -> Option<Self> {
        let listener = TcpListener::bind((address, 0)).await.ok()?;
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn({
            let accepted = accepted.clone();
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::spawn(async move {
                        let (mut reader, mut writer) = socket.into_split();
                        let _ = tokio::io::copy(&mut reader, &mut writer).await;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
                    });
                }
            }
        });
        Some(Self { port, accepted, task })
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A port nothing listens on (it was free a moment ago).
pub fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
