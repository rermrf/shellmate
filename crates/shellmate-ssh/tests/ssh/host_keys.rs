//! known_hosts handling: new keys, changed keys, accept / reject.

use std::sync::Arc;

use russh::keys::key::safe_rng;
use russh::keys::{Algorithm, PrivateKey};
use shellmate_core::{AuthMethod, FingerprintState};
use shellmate_ssh::{ConnectEvent, ConnectOptions, Connection, Route, Secret, SshError, SshHost};

use crate::support::{Env, Script, ServerOptions, TestServer, Verifier, lock};

fn host(server: &TestServer) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"))
}

async fn connect(server: &TestServer, env: &Env, verifier: Arc<Verifier>) -> Result<Connection, SshError> {
    Connection::connect(Route::direct(host(server)), env.options(verifier, Script::new())).await
}

fn entry_for(server: &TestServer, key: &PrivateKey) -> String {
    format!("[127.0.0.1]:{} {}\n", server.port, key.public_key().to_openssh().unwrap())
}

#[tokio::test]
async fn new_key_is_confirmed_then_remembered() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let verifier = Verifier::new(true);

    let first = connect(&server, &env, verifier.clone()).await.unwrap();
    let asked = verifier.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].state, FingerprintState::New);
    assert_eq!((asked[0].host.as_str(), asked[0].port), ("127.0.0.1", server.port));
    assert_eq!(asked[0].key.algorithm, "ssh-ed25519");
    assert_eq!(asked[0].key.short_algorithm(), "ED25519");
    assert_eq!(asked[0].key.fingerprint, server.fingerprint());
    assert!(asked[0].key.fingerprint.starts_with("SHA256:"));
    assert!(asked[0].recorded.is_empty());
    assert_eq!(first.target().host_key_state, FingerprintState::New);

    let text = env.known_hosts_text();
    assert!(text.starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.port)), "{text}");
    drop(first);

    // Second time known_hosts vouches for the key: the verifier is not asked.
    let second = connect(&server, &env, verifier.clone()).await.unwrap();
    assert_eq!(verifier.asked().len(), 1);
    assert_eq!(second.target().host_key_state, FingerprintState::Known);
    assert_eq!(second.target().host_key.fingerprint, server.fingerprint());
}

#[tokio::test]
async fn rejecting_a_new_key_cancels_without_writing_known_hosts() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let err = connect(&server, &env, Verifier::new(false)).await.unwrap_err();
    assert!(matches!(err, SshError::HostKeyRejected), "{err}");
    assert!(err.is_cancelled());
    assert_eq!(env.known_hosts_text(), "");
    assert_eq!(server.attempts("password"), 0, "nothing is sent to an untrusted server");
}

#[tokio::test]
async fn changed_key_is_replaced_after_confirmation() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let stale = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let other = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    std::fs::write(
        &env.known_hosts,
        format!(
            "# my hosts\n{}other.example.com {}\n",
            entry_for(&server, &stale),
            other.public_key().to_openssh().unwrap()
        ),
    )
    .unwrap();

    let verifier = Verifier::new(true);
    let connection = connect(&server, &env, verifier.clone()).await.unwrap();
    assert_eq!(connection.target().host_key_state, FingerprintState::Changed);

    let asked = verifier.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].state, FingerprintState::Changed);
    assert_eq!(asked[0].key.fingerprint, server.fingerprint());
    assert_eq!(asked[0].recorded.len(), 1);
    assert_ne!(asked[0].recorded[0].fingerprint, asked[0].key.fingerprint, "shows the key being replaced");

    // The old entry is replaced in place, not appended; unrelated lines survive.
    let text = env.known_hosts_text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(lines[0], "# my hosts");
    assert!(lines[1].starts_with(&format!("[127.0.0.1]:{} ssh-ed25519 ", server.port)));
    assert!(lines[1].contains(server.public_key().to_openssh().unwrap().split(' ').nth(1).unwrap()));
    assert!(lines[2].starts_with("other.example.com "));
    drop(connection);

    let again = connect(&server, &env, verifier.clone()).await.unwrap();
    assert_eq!(again.target().host_key_state, FingerprintState::Known);
    assert_eq!(verifier.asked().len(), 1);
}

#[tokio::test]
async fn rejecting_a_changed_key_leaves_known_hosts_untouched() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let stale = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let original = entry_for(&server, &stale);
    std::fs::write(&env.known_hosts, &original).unwrap();

    let err = connect(&server, &env, Verifier::new(false)).await.unwrap_err();
    assert!(matches!(err, SshError::HostKeyChanged), "{err}");
    assert_eq!(env.known_hosts_text(), original);
    assert_eq!(server.attempts("password"), 0);
}

#[tokio::test]
async fn connect_events_follow_the_connection_steps() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    let options = env.trusting().with_event_handler(move |event| lock(&sink).push(event));
    Connection::connect(Route::direct(host(&server)), options).await.unwrap();

    let events = lock(&events).clone();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], ConnectEvent::Connecting { host: "127.0.0.1".into(), port: server.port, via: None });
    assert!(matches!(&events[1], ConnectEvent::HostKeyVerified { state: FingerprintState::New, saved: true, .. }));
    assert_eq!(
        events[2],
        ConnectEvent::Authenticated {
            host: "127.0.0.1".into(),
            port: server.port,
            user: "alice".into(),
            method: Some(AuthMethod::Password),
            key_file: None,
        }
    );
}

#[tokio::test]
async fn unreadable_known_hosts_is_an_error() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    // A directory in place of the file: reading fails with an error that is not "not found".
    std::fs::create_dir(&env.known_hosts).unwrap();
    let err = connect(&server, &env, Verifier::new(true)).await.unwrap_err();
    assert!(matches!(err, SshError::KnownHosts(_)), "{err}");
    assert!(err.to_string().starts_with("无法读取 known_hosts"));
}

#[cfg(unix)]
#[tokio::test]
async fn unwritable_known_hosts_does_not_block_the_connection() {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let read_only = env.dir.path().join("readonly");
    std::fs::create_dir(&read_only).unwrap();
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o500)).unwrap();
    let path = read_only.join("known_hosts");

    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    let options = env.trusting().with_known_hosts(&path).with_event_handler(move |event| lock(&sink).push(event));
    let result = Connection::connect(Route::direct(host(&server)), options).await;
    std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o700)).unwrap();

    result.expect("the user already trusted the key; failing to remember it is not fatal");
    assert!(lock(&events).iter().any(|e| matches!(e, ConnectEvent::HostKeyVerified { saved: false, .. })));
    assert!(!path.exists());
}

#[tokio::test]
async fn refused_connection_has_a_clear_message() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let env = Env::new();
    let target = SshHost::new("127.0.0.1", port, "alice", AuthMethod::Password);
    let err = Connection::connect(Route::direct(target), env.trusting()).await.unwrap_err();
    assert!(matches!(err, SshError::Connect { .. }), "{err}");
    assert!(err.to_string().contains("连接被拒绝"), "{err}");
}

#[test]
fn defaults_match_the_documented_keepalive() {
    let options = ConnectOptions::new(Verifier::new(false), Script::new());
    assert_eq!(options.keepalive_interval, std::time::Duration::from_secs(30));
    assert_eq!(options.keepalive_max, 3);
}

#[tokio::test]
async fn connection_test_authenticates_without_opening_a_session_and_once_trust_asks_again() {
    use shellmate_ssh::{HostKeyDecision, HostKeyRequest, HostKeyVerifier};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Once(AtomicUsize);
    #[async_trait::async_trait]
    impl HostKeyVerifier for Once {
        async fn verify(&self, _: HostKeyRequest) -> bool {
            panic!("decide handles trust scope")
        }
        async fn decide(&self, _: HostKeyRequest) -> HostKeyDecision {
            self.0.fetch_add(1, Ordering::SeqCst);
            HostKeyDecision::Once
        }
    }
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let verifier = Arc::new(Once(AtomicUsize::new(0)));
    for _ in 0..2 {
        let options = ConnectOptions::new(verifier.clone(), Script::new())
            .with_known_hosts(&env.known_hosts)
            .with_ssh_dir(&env.ssh_dir)
            .with_agent_socket(env.dir.path().join("no-agent.sock"));
        let method = Connection::test(Route::direct(host(&server)), options).await.unwrap();
        assert_eq!(method, Some(AuthMethod::Password));
        assert_eq!(env.known_hosts_text(), "");
    }
    assert_eq!(verifier.0.load(Ordering::SeqCst), 2);
    assert_eq!(server.attempts("password"), 2);
    assert_eq!(*lock(&server.state.session_channels), 0);
    assert_eq!(*lock(&server.state.shells), 0);
    assert!(lock(&server.state.ptys).is_empty());
    assert!(lock(&server.state.execs).is_empty());
    assert!(lock(&server.state.subsystem_requests).is_empty());
}
