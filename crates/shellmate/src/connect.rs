//! Connecting a saved host: the bridge between the SSH layer (tokio tasks) and the UI.
//!
//! The SSH crate asks questions through async traits ([`HostKeyVerifier`], [`Prompter`]) and
//! reports progress as events. [`UiBridge`] implements both traits by forwarding everything
//! over one ordered channel as [`UiMsg`]s; the workspace receives them on the UI thread, shows
//! dialogs and answers through the oneshot channels carried in the messages. Dropping a reply
//! channel (a dialog that goes away, a closed tab) counts as "no" / "cancelled".
//!
//! Secrets only ever travel as [`Secret`]s. Credentials the user asked to remember are kept
//! pending in the bridge and written to the keychain once the connection succeeded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use shellmate_core::{AuthMethod, FingerprintState, Host, HostId, PtyDims, Secret};
use shellmate_ssh::{
    ChallengeRequest, ConnectEvent, ConnectOptions, Connection, HostKeyRequest, HostKeyVerifier, HostSecrets,
    PassphraseRequest, PasswordRequest, PendingShell, Prompter, Route, SshError,
};
use shellmate_store::{SecretKey, SecretStore};
use tokio::sync::oneshot;

/// A password or passphrase the user typed, and whether to remember it in the keychain.
pub struct Answer {
    pub secret: Secret,
    pub save: bool,
}

/// What the SSH task wants from the UI, in the order it happened.
pub enum UiMsg {
    /// Progress to print in the terminal.
    Event(ConnectEvent),
    /// Trust this host key?
    HostKey { request: HostKeyRequest, reply: oneshot::Sender<bool> },
    /// Ask for a password.
    Password { request: PasswordRequest, reply: oneshot::Sender<Option<Answer>> },
    /// Ask for a private key passphrase.
    Passphrase { request: PassphraseRequest, reply: oneshot::Sender<Option<Answer>> },
    /// Ask the questions of one keyboard-interactive round.
    Challenge { request: ChallengeRequest, reply: oneshot::Sender<Option<Vec<Secret>>> },
}

/// Identifies a server of a route the way prompts do.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
}

impl Endpoint {
    fn of(host: &Host, default_user: &str) -> Self {
        Self {
            host: host.addr.clone(),
            port: host.port,
            user: host.user.clone().unwrap_or_else(|| default_user.to_string()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The UI side of one connection attempt.
pub struct UiBridge {
    tx: async_channel::Sender<UiMsg>,
    /// Saved host behind each endpoint of the route, to key secrets by host id.
    hosts: HashMap<Endpoint, HostId>,
    /// Saved passwords found in the keychain, for keyboard-interactive autofill.
    known_passwords: Mutex<HashMap<Endpoint, Secret>>,
    /// Endpoints whose saved password was already tried automatically (a retry must ask).
    autofilled: Mutex<HashSet<Endpoint>>,
    /// Answers the user wants remembered, until the hop they belong to authenticates with them.
    /// Only the last answer per hop and kind is kept.
    pending: Mutex<HashMap<Candidate, Secret>>,
    /// Secrets whose method authenticated their hop: these are written to the keychain.
    confirmed: Mutex<Vec<(SecretKey, Secret)>>,
}

/// An answer that may be saved once it is known to have worked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Candidate {
    /// The login password of a hop.
    Password(Endpoint),
    /// The passphrase that unlocked a key file.
    Passphrase(PathBuf),
}

impl UiBridge {
    /// A bridge for the hosts of one route; messages go to `tx`.
    pub fn new(tx: async_channel::Sender<UiMsg>, chain: &[Host], default_user: &str) -> Arc<Self> {
        Arc::new(Self {
            tx,
            hosts: chain.iter().map(|h| (Endpoint::of(h, default_user), h.id.clone())).collect(),
            known_passwords: Mutex::new(HashMap::new()),
            autofilled: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            confirmed: Mutex::new(Vec::new()),
        })
    }

    /// Remembers the keychain passwords used for autofill.
    pub fn set_known_passwords(&self, passwords: HashMap<Endpoint, Secret>) {
        *lock(&self.known_passwords) = passwords;
    }

    /// Sends progress to the UI. A hop that authenticated also settles which of the answers the
    /// user wanted remembered really worked.
    pub fn event(&self, event: ConnectEvent) {
        if let ConnectEvent::Authenticated { host, port, user, method, key_file } = &event {
            self.confirm(
                Endpoint { host: host.clone(), port: *port, user: user.clone() },
                *method,
                key_file.as_deref(),
            );
        }
        let _ = self.tx.try_send(UiMsg::Event(event));
    }

    /// The secrets to write to the keychain: the answers the user asked to remember whose
    /// method authenticated its hop (a password only if the password method worked, a passphrase
    /// only if the key it unlocked was the one that authenticated).
    pub fn take_confirmed_saves(&self) -> Vec<(SecretKey, Secret)> {
        std::mem::take(&mut *lock(&self.confirmed))
    }

    /// `hop` authenticated with `method` (and `key_file`): commit what belongs to that, drop the rest.
    fn confirm(&self, hop: Endpoint, method: Option<AuthMethod>, key_file: Option<&Path>) {
        let mut pending = lock(&self.pending);
        let password = pending.remove(&Candidate::Password(hop.clone()));
        let mut confirmed = lock(&self.confirmed);
        if method == Some(AuthMethod::Password)
            && let Some(secret) = password
            && let Some(id) = self.hosts.get(&hop)
        {
            confirmed.push((SecretKey::HostPassword(id.clone()), secret));
        }
        if method == Some(AuthMethod::PrivateKey)
            && let Some(file) = key_file
            && let Some(secret) = pending.remove(&Candidate::Passphrase(file.to_path_buf()))
        {
            confirmed.push((SecretKey::KeyPassphrase(file.to_path_buf()), secret));
        }
    }

    fn endpoint_of(&self, host: &str, port: u16, user: &str) -> Endpoint {
        Endpoint { host: host.to_string(), port, user: user.to_string() }
    }

    /// Notes the answer for saving, or forgets an earlier answer if this one is not to be saved.
    fn remember(&self, candidate: Candidate, answer: &Answer) {
        let mut pending = lock(&self.pending);
        if answer.save {
            pending.insert(candidate, answer.secret.clone());
        } else {
            pending.remove(&candidate);
        }
    }
}

#[async_trait]
impl HostKeyVerifier for UiBridge {
    async fn verify(&self, request: HostKeyRequest) -> bool {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(UiMsg::HostKey { request, reply }).await.is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }
}

#[async_trait]
impl Prompter for UiBridge {
    async fn password(&self, request: PasswordRequest) -> Option<Secret> {
        let endpoint = self.endpoint_of(&request.host, request.port, &request.user);
        let (reply, answer) = oneshot::channel();
        self.tx.send(UiMsg::Password { request, reply }).await.ok()?;
        let answer = answer.await.ok()??;
        self.remember(Candidate::Password(endpoint), &answer);
        Some(answer.secret)
    }

    async fn passphrase(&self, request: PassphraseRequest) -> Option<Secret> {
        let key = Candidate::Passphrase(request.key_path.clone());
        let (reply, answer) = oneshot::channel();
        self.tx.send(UiMsg::Passphrase { request, reply }).await.ok()?;
        let answer = answer.await.ok()??;
        self.remember(key, &answer);
        Some(answer.secret)
    }

    async fn challenge(&self, request: ChallengeRequest) -> Option<Vec<Secret>> {
        let endpoint = self.endpoint_of(&request.host, request.port, &request.user);
        if let [prompt] = request.prompts.as_slice()
            && request.round == 1
            && !prompt.echo
            && !looks_like_otp(&prompt.text)
            && let Some(saved) = lock(&self.known_passwords).get(&endpoint).cloned()
            && lock(&self.autofilled).insert(endpoint)
        {
            return Some(vec![saved]);
        }
        let (reply, answer) = oneshot::channel();
        self.tx.send(UiMsg::Challenge { request, reply }).await.ok()?;
        answer.await.ok()?
    }
}

/// Whether a keyboard-interactive prompt asks for a one-time code rather than a password.
pub fn looks_like_otp(prompt: &str) -> bool {
    let text = prompt.to_lowercase();
    if text.contains("password") || text.contains("密码") {
        return false;
    }
    ["code", "otp", "token", "verif", "authenticator", "验证码", "动态码", "令牌"]
        .iter()
        .any(|word| text.contains(word))
}

/// The login name used when a host has none: the local user.
pub fn default_user() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "root".into())
}

/// Builds the route for a resolved chain, filling in saved secrets from `secrets`. Also returns
/// the saved passwords by endpoint (for keyboard-interactive autofill). Keychain errors are
/// treated as "nothing saved": the user is asked instead.
pub fn build_route(
    chain: &[Host],
    secrets: &dyn SecretStore,
    default_user: &str,
) -> Result<(Route, HashMap<Endpoint, Secret>), SshError> {
    let mut known = HashMap::new();
    let route = Route::from_hosts(chain, default_user, |host| {
        let password = secrets.get(&SecretKey::HostPassword(host.id.clone())).ok().flatten();
        let passphrase = host
            .identity_file
            .as_ref()
            .and_then(|path| secrets.get(&SecretKey::KeyPassphrase(path.clone())).ok().flatten());
        if let Some(password) = &password {
            known.insert(Endpoint::of(host, default_user), password.clone());
        }
        HostSecrets { password, passphrase }
    })?;
    Ok((route, known))
}

/// What was written to the keychain at the end of a connection attempt.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Secrets that were stored.
    pub saved: Vec<SecretKey>,
    /// Keychain writes that failed (the connection itself is unaffected).
    pub errors: Vec<String>,
}

/// A connection with its shell channel opened but not yet attached to a terminal. The shell
/// keeps the connection alive; the connection closes with its last channel.
pub struct Connected {
    pub shell: PendingShell,
    pub saves: SaveReport,
}

/// A failed attempt. Hops that authenticated before the failure still get their secrets saved.
pub struct ConnectFailure {
    pub error: SshError,
    pub saves: SaveReport,
}

/// Runs a whole connection attempt on the SSH runtime: route, connect, open the shell, and
/// remember the credentials the user asked to keep for the hops that authenticated with them.
pub async fn connect_and_open(
    chain: Vec<Host>,
    secrets: Arc<dyn SecretStore>,
    bridge: Arc<UiBridge>,
    dims: PtyDims,
) -> Result<Connected, ConnectFailure> {
    let attempt = async {
        let user = default_user();
        let (route, known) = {
            let (chain, secrets, user) = (chain.clone(), secrets.clone(), user.clone());
            tokio::task::spawn_blocking(move || build_route(&chain, secrets.as_ref(), &user))
                .await
                .map_err(|err| SshError::Unsupported(err.to_string()))??
        };
        bridge.set_known_passwords(known);

        let events = bridge.clone();
        let options =
            ConnectOptions::new(bridge.clone(), bridge.clone()).with_event_handler(move |event| events.event(event));
        let connection = Connection::connect(route, options).await?;
        connection.open_shell(dims).await
    };
    let outcome = attempt.await;

    let saves = bridge.take_confirmed_saves();
    let saves = if saves.is_empty() {
        SaveReport::default()
    } else {
        tokio::task::spawn_blocking(move || {
            let mut report = SaveReport::default();
            for (key, secret) in saves {
                match secrets.set(&key, &secret) {
                    Ok(()) => report.saved.push(key),
                    Err(err) => report.errors.push(err.to_string()),
                }
            }
            report
        })
        .await
        .unwrap_or_default()
    };
    match outcome {
        Ok(shell) => Ok(Connected { shell, saves }),
        Err(error) => Err(ConnectFailure { error, saves }),
    }
}

// ---- wording ---------------------------------------------------------------------------

const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

/// Display names of the hosts of a route, by address, for "经由 bastion" style wording.
pub fn names_by_address(chain: &[Host]) -> HashMap<String, String> {
    chain.iter().map(|h| (h.addr.clone(), h.name.clone())).collect()
}

/// The terminal line for a connection event, in the prototype's wording.
pub fn format_event(event: &ConnectEvent, names: &HashMap<String, String>) -> String {
    match event {
        ConnectEvent::Connecting { host, port, via } => {
            let via = via
                .as_ref()
                .map(|via| format!(" · 经由 {}", names.get(via).map_or(via.as_str(), String::as_str)))
                .unwrap_or_default();
            format!("{DIM}→ 连接 {host}:{port}{via}")
        }
        ConnectEvent::HostKeyVerified { key, state, saved, .. } => {
            let kind = key.short_algorithm();
            match (state, saved) {
                (FingerprintState::Known, _) => format!("{GREEN}✓ 主机指纹已校验 {kind} · known_hosts"),
                (FingerprintState::New, true) => format!("{GREEN}✓ 已信任新主机指纹 {kind} · 已写入 known_hosts"),
                (FingerprintState::Changed, true) => format!("{GREEN}✓ 已更新 known_hosts 中的主机指纹 {kind}"),
                (_, false) => format!("{YELLOW}! 已信任主机指纹 {kind}，但无法写入 known_hosts（下次仍会询问）"),
            }
        }
        ConnectEvent::Authenticated { method, .. } => match method {
            Some(method) => format!("{GREEN}✓ 认证成功 · {}", method_label(*method)),
            None => format!("{GREEN}✓ 认证成功 · 无需凭据"),
        },
    }
}

fn method_label(method: AuthMethod) -> &'static str {
    method.label()
}

#[cfg(test)]
mod tests {
    use shellmate_ssh::HostKeyInfo;
    use shellmate_store::MemoryStore;

    use super::*;

    fn host(name: &str, addr: &str, port: u16, user: Option<&str>) -> Host {
        let mut host = Host::new(name, addr);
        host.port = port;
        host.user = user.map(String::from);
        host
    }

    #[test]
    fn otp_prompts_are_recognised() {
        for prompt in [
            "Verification code:",
            "Enter OTP: ",
            "Authenticator token",
            "Passcode:",
            "请输入验证码",
            "Duo passcode or option (1-3):",
        ] {
            assert!(looks_like_otp(prompt), "{prompt}");
        }
        for prompt in ["Password:", "alice@host's password: ", "密码：", "Password + OTP code:", "Enter your secret:"]
        {
            assert!(!looks_like_otp(prompt), "{prompt}");
        }
    }

    #[test]
    fn route_gets_saved_secrets_and_remembers_passwords() {
        let store = MemoryStore::new();
        let (jump, mut target) =
            (host("bastion", "bastion.example.com", 2222, Some("ops")), host("db", "10.0.2.8", 22, None));
        target.identity_file = Some("/keys/db".into());
        store.set(&SecretKey::HostPassword(jump.id.clone()), &Secret::new("jump-pw")).unwrap();
        store.set(&SecretKey::KeyPassphrase("/keys/db".into()), &Secret::new("phrase")).unwrap();

        let (route, known) = build_route(&[jump.clone(), target], &store, "me").unwrap();
        assert_eq!(route.via[0].password.as_ref().map(Secret::expose), Some("jump-pw"));
        assert!(route.target.password.is_none());
        assert_eq!(route.target.passphrase.as_ref().map(Secret::expose), Some("phrase"));
        assert_eq!(route.target.user, "me");
        assert_eq!(known.len(), 1);
        let endpoint = Endpoint { host: "bastion.example.com".into(), port: 2222, user: "ops".into() };
        assert_eq!(known[&endpoint].expose(), "jump-pw");
    }

    #[test]
    fn route_building_without_secrets_is_fine() {
        let (route, known) =
            build_route(&[host("web", "w.example.com", 22, Some("deploy"))], &MemoryStore::new(), "me").unwrap();
        assert!(known.is_empty() && route.target.password.is_none() && route.target.passphrase.is_none());
    }

    #[test]
    fn event_lines_follow_the_prototype_wording() {
        let chain = [host("bastion", "bastion.example.com", 22, None), host("db", "10.0.2.8", 22, None)];
        let names = names_by_address(&chain);
        let line = |event: ConnectEvent| format_event(&event, &names);
        let key = HostKeyInfo { algorithm: "ssh-ed25519".into(), fingerprint: "SHA256:x".into() };

        assert_eq!(
            line(ConnectEvent::Connecting { host: "10.0.1.21".into(), port: 22, via: None }),
            "\x1b[2m→ 连接 10.0.1.21:22"
        );
        assert_eq!(
            line(ConnectEvent::Connecting {
                host: "10.0.2.8".into(),
                port: 22,
                via: Some("bastion.example.com".into())
            }),
            "\x1b[2m→ 连接 10.0.2.8:22 · 经由 bastion"
        );
        let verified =
            |state, saved| ConnectEvent::HostKeyVerified { host: "h".into(), port: 22, key: key.clone(), state, saved };
        assert_eq!(line(verified(FingerprintState::Known, true)), "\x1b[32m✓ 主机指纹已校验 ED25519 · known_hosts");
        assert!(line(verified(FingerprintState::New, true)).contains("已写入 known_hosts"));
        assert!(line(verified(FingerprintState::Changed, true)).contains("已更新 known_hosts"));
        assert!(line(verified(FingerprintState::New, false)).contains("无法写入 known_hosts"));
        let authed = |method| ConnectEvent::Authenticated {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            method,
            key_file: None,
        };
        assert_eq!(line(authed(Some(AuthMethod::PrivateKey))), "\x1b[32m✓ 认证成功 · 私钥");
        assert_eq!(line(authed(Some(AuthMethod::Agent))), "\x1b[32m✓ 认证成功 · ssh-agent");
        assert_eq!(line(authed(None)), "\x1b[32m✓ 认证成功 · 无需凭据");
    }

    // ---- the bridge, with a scripted "UI" answering on the other end of the channel ----

    fn bridge_for(chain: &[Host]) -> (Arc<UiBridge>, async_channel::Receiver<UiMsg>) {
        let (tx, rx) = async_channel::unbounded();
        (UiBridge::new(tx, chain, "me"), rx)
    }

    fn challenge(host: &Host, round: u32, prompts: &[(&str, bool)]) -> ChallengeRequest {
        ChallengeRequest {
            host: host.addr.clone(),
            port: host.port,
            user: host.user.clone().unwrap_or_else(|| "me".into()),
            name: String::new(),
            instructions: String::new(),
            prompts: prompts
                .iter()
                .map(|(text, echo)| shellmate_ssh::ChallengePrompt { text: text.to_string(), echo: *echo })
                .collect(),
            round,
            attempt: 0,
        }
    }

    #[tokio::test]
    async fn host_key_question_is_forwarded_and_dropped_replies_mean_no() {
        let (bridge, rx) = bridge_for(&[]);
        let request = HostKeyRequest {
            host: "h".into(),
            port: 22,
            state: FingerprintState::New,
            key: HostKeyInfo { algorithm: "ssh-ed25519".into(), fingerprint: "SHA256:x".into() },
            recorded: vec![],
        };

        let asking = tokio::spawn({
            let (bridge, request) = (bridge.clone(), request.clone());
            async move { bridge.verify(request).await }
        });
        let Ok(UiMsg::HostKey { request: seen, reply }) = rx.recv().await else {
            panic!("expected a host key question")
        };
        assert_eq!(seen, request);
        reply.send(true).unwrap();
        assert!(asking.await.unwrap());

        // The dialog goes away without answering: that is a refusal.
        let asking = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.verify(request).await }
        });
        let Ok(UiMsg::HostKey { reply, .. }) = rx.recv().await else { panic!("expected a host key question") };
        drop(reply);
        assert!(!asking.await.unwrap());

        // And so does a UI that is gone altogether.
        drop(rx);
        let gone = HostKeyRequest {
            host: "h".into(),
            port: 22,
            state: FingerprintState::New,
            key: HostKeyInfo { algorithm: "x".into(), fingerprint: "y".into() },
            recorded: vec![],
        };
        assert!(!bridge.verify(gone).await);
    }

    // ---- keychain saves: only what the authenticating method used --------------------------

    fn user_of(host: &Host) -> String {
        host.user.clone().unwrap_or_else(|| "me".into())
    }

    /// The user types `password` into the password dialog of `host`.
    async fn type_password(
        bridge: &Arc<UiBridge>,
        rx: &async_channel::Receiver<UiMsg>,
        host: &Host,
        password: &str,
        save: bool,
    ) {
        let request = PasswordRequest { host: host.addr.clone(), port: host.port, user: user_of(host), attempt: 0 };
        let asking = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.password(request).await }
        });
        loop {
            match rx.recv().await {
                Ok(UiMsg::Password { reply, .. }) => {
                    reply.send(Some(Answer { secret: Secret::new(password), save })).ok().unwrap();
                    break;
                }
                Ok(_) => continue,
                Err(_) => panic!("the bridge closed"),
            }
        }
        asking.await.unwrap().unwrap();
    }

    /// The user types `passphrase` into the passphrase dialog of `key`.
    async fn type_passphrase(
        bridge: &Arc<UiBridge>,
        rx: &async_channel::Receiver<UiMsg>,
        key: &str,
        passphrase: &str,
        save: bool,
    ) {
        let request = PassphraseRequest { key_path: key.into(), attempt: 0 };
        let asking = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.passphrase(request).await }
        });
        loop {
            match rx.recv().await {
                Ok(UiMsg::Passphrase { reply, .. }) => {
                    reply.send(Some(Answer { secret: Secret::new(passphrase), save })).ok().unwrap();
                    break;
                }
                Ok(_) => continue,
                Err(_) => panic!("the bridge closed"),
            }
        }
        asking.await.unwrap().unwrap();
    }

    /// The SSH layer reports that `host` authenticated.
    fn authenticated(bridge: &UiBridge, host: &Host, method: Option<AuthMethod>, key_file: Option<&str>) {
        bridge.event(ConnectEvent::Authenticated {
            host: host.addr.clone(),
            port: host.port,
            user: user_of(host),
            method,
            key_file: key_file.map(PathBuf::from),
        });
    }

    fn saved(bridge: &UiBridge) -> Vec<(SecretKey, String)> {
        bridge.take_confirmed_saves().into_iter().map(|(key, secret)| (key, secret.expose().to_string())).collect()
    }

    #[tokio::test]
    async fn a_password_is_saved_once_the_password_method_authenticated() {
        let web = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        type_password(&bridge, &rx, &web, "oops", true).await;
        assert!(saved(&bridge).is_empty(), "nothing is saved before the server accepted it");
        type_password(&bridge, &rx, &web, "right", true).await;
        authenticated(&bridge, &web, Some(AuthMethod::Password), None);
        assert_eq!(
            saved(&bridge),
            [(SecretKey::HostPassword(web.id.clone()), "right".to_string())],
            "the last answer, not the wrong one"
        );
        assert!(saved(&bridge).is_empty(), "taking the saves empties them");
    }

    #[tokio::test]
    async fn wrong_passwords_followed_by_a_keyboard_interactive_login_save_nothing() {
        let web = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        for wrong in ["a", "b", "c"] {
            type_password(&bridge, &rx, &web, wrong, true).await;
        }
        authenticated(&bridge, &web, Some(AuthMethod::KeyboardInteractive), None);
        assert!(saved(&bridge).is_empty());
        // And the wrong password does not linger for a later confirmation either.
        authenticated(&bridge, &web, Some(AuthMethod::Password), None);
        assert!(saved(&bridge).is_empty());
    }

    #[tokio::test]
    async fn not_ticking_the_box_on_the_last_answer_forgets_the_earlier_one() {
        let web = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        type_password(&bridge, &rx, &web, "ticked", true).await;
        type_password(&bridge, &rx, &web, "unticked", false).await;
        authenticated(&bridge, &web, Some(AuthMethod::Password), None);
        assert!(saved(&bridge).is_empty());
    }

    #[tokio::test]
    async fn a_jump_hosts_password_is_saved_only_for_the_hop_that_used_it() {
        let jump = host("bastion", "b.example.com", 2222, Some("ops"));
        let target = host("db", "d.example.com", 22, Some("dba"));
        let chain = [jump.clone(), target.clone()];

        // Both hops logged in with their passwords: each is saved under its own host.
        let (bridge, rx) = bridge_for(&chain);
        type_password(&bridge, &rx, &jump, "jump-pw", true).await;
        authenticated(&bridge, &jump, Some(AuthMethod::Password), None);
        type_password(&bridge, &rx, &target, "target-pw", true).await;
        authenticated(&bridge, &target, Some(AuthMethod::Password), None);
        assert_eq!(
            saved(&bridge),
            [
                (SecretKey::HostPassword(jump.id.clone()), "jump-pw".to_string()),
                (SecretKey::HostPassword(target.id.clone()), "target-pw".to_string())
            ]
        );

        // The target ended up authenticating by other means: only the jump host's password counts.
        let (bridge, rx) = bridge_for(&chain);
        type_password(&bridge, &rx, &jump, "jump-pw", true).await;
        authenticated(&bridge, &jump, Some(AuthMethod::Password), None);
        type_password(&bridge, &rx, &target, "typo", true).await;
        authenticated(&bridge, &target, Some(AuthMethod::Agent), None);
        assert_eq!(saved(&bridge), [(SecretKey::HostPassword(jump.id.clone()), "jump-pw".to_string())]);

        // A password typed for one hop is never credited to the other.
        let (bridge, rx) = bridge_for(&chain);
        type_password(&bridge, &rx, &target, "target-pw", true).await;
        authenticated(&bridge, &jump, Some(AuthMethod::Password), None);
        assert!(saved(&bridge).is_empty());
    }

    #[tokio::test]
    async fn a_passphrase_is_saved_only_when_that_key_authenticated() {
        let web = host("web", "w.example.com", 22, Some("deploy"));

        // The key the passphrase unlocked authenticated: saved, keyed by the file.
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        type_passphrase(&bridge, &rx, "/keys/work", "phrase", true).await;
        authenticated(&bridge, &web, Some(AuthMethod::PrivateKey), Some("/keys/work"));
        assert_eq!(saved(&bridge), [(SecretKey::KeyPassphrase("/keys/work".into()), "phrase".to_string())]);

        // The server took a different key, or the agent, or a password: not saved.
        for (method, file) in [
            (Some(AuthMethod::PrivateKey), Some("/keys/other")),
            (Some(AuthMethod::Agent), None),
            (Some(AuthMethod::Password), None),
            (Some(AuthMethod::PrivateKey), None),
        ] {
            let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
            type_passphrase(&bridge, &rx, "/keys/work", "phrase", true).await;
            authenticated(&bridge, &web, method, file);
            assert!(saved(&bridge).is_empty(), "{method:?} {file:?}");
        }

        // Unticked: never saved.
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        type_passphrase(&bridge, &rx, "/keys/work", "phrase", false).await;
        authenticated(&bridge, &web, Some(AuthMethod::PrivateKey), Some("/keys/work"));
        assert!(saved(&bridge).is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_prompt_saves_nothing() {
        let web = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        let request = PasswordRequest { host: web.addr.clone(), port: 22, user: "deploy".into(), attempt: 0 };
        let asking = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.password(request).await }
        });
        let Ok(UiMsg::Password { reply, .. }) = rx.recv().await else { panic!() };
        reply.send(None).ok().unwrap();
        assert!(asking.await.unwrap().is_none());
        authenticated(&bridge, &web, Some(AuthMethod::Password), None);
        assert!(saved(&bridge).is_empty());
    }

    #[tokio::test]
    async fn the_event_is_still_forwarded_to_the_ui() {
        let web = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&web));
        authenticated(&bridge, &web, Some(AuthMethod::Password), None);
        assert!(matches!(rx.recv().await, Ok(UiMsg::Event(ConnectEvent::Authenticated { .. }))));
    }

    #[tokio::test]
    async fn keyboard_interactive_first_round_is_autofilled_once_from_the_keychain() {
        let target = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&target));
        let endpoint = Endpoint { host: "w.example.com".into(), port: 22, user: "deploy".into() };
        bridge.set_known_passwords(HashMap::from([(endpoint, Secret::new("saved"))]));

        // A single masked password prompt in round 1: answered without any dialog.
        let first = bridge.challenge(challenge(&target, 1, &[("Password: ", false)])).await.unwrap();
        assert_eq!(first[0].expose(), "saved");
        assert!(rx.try_recv().is_err(), "no dialog for the autofilled answer");

        // The server asks again (the saved password was wrong): now the user is asked.
        let asking = tokio::spawn({
            let (bridge, target) = (bridge.clone(), target.clone());
            async move { bridge.challenge(challenge(&target, 1, &[("Password: ", false)])).await }
        });
        let Ok(UiMsg::Challenge { reply, .. }) = rx.recv().await else { panic!("expected a dialog") };
        reply.send(Some(vec![Secret::new("typed")])).ok().unwrap();
        assert_eq!(asking.await.unwrap().unwrap()[0].expose(), "typed");
    }

    #[tokio::test]
    async fn autofill_leaves_other_prompts_to_the_user() {
        let target = host("web", "w.example.com", 22, Some("deploy"));
        let (bridge, rx) = bridge_for(std::slice::from_ref(&target));
        bridge.set_known_passwords(HashMap::from([(
            Endpoint { host: "w.example.com".into(), port: 22, user: "deploy".into() },
            Secret::new("saved"),
        )]));

        for (round, prompts) in [
            (2u32, vec![("Password: ", false)]),
            (1, vec![("Verification code: ", false)]),
            (1, vec![("Password: ", true)]),
            (1, vec![("Password: ", false), ("Code: ", false)]),
        ] {
            let asking = tokio::spawn({
                let (bridge, target) = (bridge.clone(), target.clone());
                async move { bridge.challenge(challenge(&target, round, &prompts)).await }
            });
            let Ok(UiMsg::Challenge { reply, .. }) = rx.recv().await else {
                panic!("round {round} should ask the user")
            };
            reply.send(None).ok().unwrap();
            assert!(asking.await.unwrap().is_none());
        }
    }
}
