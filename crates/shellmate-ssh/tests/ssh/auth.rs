//! Authentication methods: success, failure, cancellation.

use std::sync::Arc;

use russh::MethodKind;
use russh::keys::key::safe_rng;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use shellmate_core::AuthMethod;
use shellmate_ssh::{Connection, Route, Secret, SshError, SshHost};

use crate::support::{Env, KbdRound, Script, ServerOptions, TestServer, Verifier, lock};

fn host(server: &TestServer, auth: AuthMethod) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", auth)
}

async fn connect(host: SshHost, env: &Env, script: Arc<Script>) -> Result<Connection, SshError> {
    Connection::connect(Route::direct(host), env.options(Verifier::new(true), script)).await
}

#[tokio::test]
async fn password_success() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let host = host(&server, AuthMethod::Password).with_password(Secret::new("secret"));
    let connection = connect(host, &env, Script::new()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Password));
    assert_eq!(connection.target().user, "alice");
}

#[tokio::test]
async fn password_is_prompted_when_not_stored() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let script = Script::new().with_passwords(&["secret"]);
    connect(host(&server, AuthMethod::Password), &env, script.clone()).await.unwrap();
    let requests = lock(&script.password_requests).clone();
    assert_eq!(requests.len(), 1);
    assert_eq!((requests[0].user.as_str(), requests[0].attempt), ("alice", 0));
}

#[tokio::test]
async fn wrong_stored_password_falls_back_to_a_prompt() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let script = Script::new().with_passwords(&["secret"]);
    let host = host(&server, AuthMethod::Password).with_password(Secret::new("stale"));
    connect(host, &env, script.clone()).await.unwrap();
    assert_eq!(server.attempts("password"), 2);
    assert_eq!(lock(&script.password_requests)[0].attempt, 1, "second attempt is flagged as a retry");
}

#[tokio::test]
async fn password_rejected_after_three_attempts() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let script = Script::new().with_passwords(&["a", "b", "c", "d"]);
    let err = connect(host(&server, AuthMethod::Password), &env, script).await.unwrap_err();
    assert!(matches!(err, SshError::AuthRejected(AuthMethod::Password)), "{err}");
    assert_eq!(err.to_string(), "认证失败：用户名或密码错误");
    assert_eq!(server.attempts("password"), 3);
}

#[tokio::test]
async fn cancelling_the_password_prompt_aborts() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let err = connect(host(&server, AuthMethod::Password), &env, Script::new()).await.unwrap_err();
    assert!(matches!(err, SshError::AuthCancelled), "{err}");
    assert!(err.is_cancelled());
    assert_eq!(server.attempts("password"), 0);
}

#[tokio::test]
async fn publickey_success_with_explicit_identity() {
    let env = Env::new();
    let (path, key) = env.write_key("id_test");
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![key.public_key().clone()], ..Default::default() })
            .await;
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    let connection = connect(host, &env, Script::new()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::PrivateKey));
}

#[tokio::test]
async fn publickey_rejected_when_not_authorized() {
    let env = Env::new();
    let (path, _) = env.write_key("id_test");
    let server = TestServer::start(ServerOptions::default()).await;
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    let err = connect(host, &env, Script::new()).await.unwrap_err();
    assert!(matches!(err, SshError::AuthRejected(AuthMethod::PrivateKey)), "{err}");
    assert_eq!(err.to_string(), "认证失败：服务器拒绝了私钥");
}

#[tokio::test]
async fn default_keys_are_taken_from_the_ssh_dir() {
    let env = Env::new();
    let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    std::fs::write(env.ssh_dir.join("id_ed25519"), key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![key.public_key().clone()], ..Default::default() })
            .await;
    let connection = connect(host(&server, AuthMethod::PrivateKey), &env, Script::new()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::PrivateKey));
}

#[tokio::test]
async fn missing_default_keys_are_reported() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let err = connect(host(&server, AuthMethod::PrivateKey), &env, Script::new()).await.unwrap_err();
    assert!(matches!(err, SshError::NoPrivateKey { .. }), "{err}");
}

#[tokio::test]
async fn unreadable_identity_file_is_reported() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(env.dir.path().join("missing"));
    let err = connect(host, &env, Script::new()).await.unwrap_err();
    assert!(matches!(err, SshError::KeyUnreadable { .. }), "{err}");
    assert!(err.to_string().starts_with("无法读取私钥"));
}

fn encrypted_key(env: &Env, passphrase: &str) -> (std::path::PathBuf, PrivateKey) {
    let key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let encrypted = key.encrypt(&mut safe_rng(), passphrase).unwrap();
    let path = env.dir.path().join("id_encrypted");
    std::fs::write(&path, encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    (path, key)
}

#[tokio::test]
async fn encrypted_key_asks_for_the_passphrase() {
    let env = Env::new();
    let (path, key) = encrypted_key(&env, "correct horse");
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![key.public_key().clone()], ..Default::default() })
            .await;
    let script = Script::new().with_passphrases(&["wrong", "correct horse"]);
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    connect(host, &env, script.clone()).await.unwrap();
    let requests = lock(&script.passphrase_requests).clone();
    assert_eq!(requests.iter().map(|r| r.attempt).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(requests[0].key_path, path);
}

#[tokio::test]
async fn encrypted_key_with_stored_passphrase_does_not_prompt() {
    let env = Env::new();
    let (path, key) = encrypted_key(&env, "pw");
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![key.public_key().clone()], ..Default::default() })
            .await;
    let script = Script::new();
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path).with_passphrase(Secret::new("pw"));
    connect(host, &env, script.clone()).await.unwrap();
    assert!(lock(&script.passphrase_requests).is_empty());
}

#[tokio::test]
async fn cancelling_the_passphrase_prompt_aborts() {
    let env = Env::new();
    let (path, key) = encrypted_key(&env, "pw");
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![key.public_key().clone()], ..Default::default() })
            .await;
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    let err = connect(host, &env, Script::new()).await.unwrap_err();
    assert!(matches!(err, SshError::AuthCancelled), "{err}");
    assert_eq!(server.attempts("publickey"), 0);
}

#[tokio::test]
async fn wrong_passphrase_three_times_is_an_error() {
    let env = Env::new();
    let (path, _) = encrypted_key(&env, "pw");
    let server = TestServer::start(ServerOptions::default()).await;
    let script = Script::new().with_passphrases(&["a", "b", "c"]);
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    let err = connect(host, &env, script).await.unwrap_err();
    assert!(matches!(&err, SshError::KeyUnreadable { reason, .. } if reason == "私钥口令不正确"), "{err}");
}

fn otp_rounds() -> Vec<KbdRound> {
    vec![
        KbdRound { prompts: vec![("Password: ", false)], expect: vec!["secret"] },
        KbdRound { prompts: vec![("Verification code: ", true)], expect: vec!["123456"] },
    ]
}

#[tokio::test]
async fn keyboard_interactive_runs_several_rounds() {
    let server = TestServer::start(ServerOptions { kbd: Some(otp_rounds()), ..Default::default() }).await;
    let env = Env::new();
    let script = Script::new().with_challenges(&[&["secret"], &["123456"]]);
    let connection = connect(host(&server, AuthMethod::KeyboardInteractive), &env, script.clone()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::KeyboardInteractive));
    let requests = lock(&script.challenge_requests).clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].round, 1);
    assert_eq!(requests[0].name, "test");
    assert_eq!(requests[0].instructions, "answer the questions");
    assert_eq!((requests[0].prompts[0].text.as_str(), requests[0].prompts[0].echo), ("Password: ", false));
    assert_eq!(requests[1].round, 2);
    assert!(requests.iter().all(|r| r.attempt == 0));
    assert!(requests[1].prompts[0].echo, "echo flag of the OTP prompt is passed through");
}

#[tokio::test]
async fn keyboard_interactive_wrong_answers_are_rejected() {
    let server = TestServer::start(ServerOptions { kbd: Some(otp_rounds()), ..Default::default() }).await;
    let env = Env::new();
    let script =
        Script::new().with_challenges(&[&["secret"], &["000000"], &["secret"], &["111111"], &["secret"], &["222222"]]);
    let err = connect(host(&server, AuthMethod::KeyboardInteractive), &env, script).await.unwrap_err();
    assert!(matches!(err, SshError::AuthRejected(AuthMethod::KeyboardInteractive)), "{err}");
    assert_eq!(err.to_string(), "认证失败：键盘交互验证未通过");
    assert_eq!(server.attempts("keyboard-interactive"), 3);
}

#[tokio::test]
async fn keyboard_interactive_retries_restart_the_rounds_and_count_attempts() {
    let server = TestServer::start(ServerOptions { kbd: Some(otp_rounds()), ..Default::default() }).await;
    let env = Env::new();
    // First attempt: right password, wrong code. Second attempt: everything right.
    let script = Script::new().with_challenges(&[&["secret"], &["000000"], &["secret"], &["123456"]]);
    connect(host(&server, AuthMethod::KeyboardInteractive), &env, script.clone()).await.unwrap();
    let seen: Vec<(u32, u32)> = lock(&script.challenge_requests).iter().map(|r| (r.attempt, r.round)).collect();
    assert_eq!(seen, [(0, 1), (0, 2), (1, 1), (1, 2)]);
}

#[tokio::test]
async fn keyboard_interactive_can_be_cancelled_midway() {
    let server = TestServer::start(ServerOptions { kbd: Some(otp_rounds()), ..Default::default() }).await;
    let env = Env::new();
    let script = Script::new().with_challenges(&[&["secret"]]);
    let err = connect(host(&server, AuthMethod::KeyboardInteractive), &env, script).await.unwrap_err();
    assert!(matches!(err, SshError::AuthCancelled), "{err}");
}

#[tokio::test]
async fn unsupported_method_names_what_the_server_offers() {
    let server = TestServer::start(ServerOptions { methods: vec![MethodKind::PublicKey], ..Default::default() }).await;
    let env = Env::new();
    let host = host(&server, AuthMethod::Password).with_password(Secret::new("secret"));
    let err = connect(host, &env, Script::new()).await.unwrap_err();
    assert!(matches!(&err, SshError::AuthMethodUnavailable { offered, .. } if offered == &["publickey"]), "{err}");
    assert_eq!(err.to_string(), "认证失败：服务器不支持密码认证（服务器提供：publickey）");
    assert_eq!(server.attempts("password"), 0, "an unsupported method is not even tried");
}

#[tokio::test]
async fn fallback_method_is_used_when_the_first_is_rejected() {
    let env = Env::new();
    let (path, _unauthorized) = env.write_key("id_test");
    let server = TestServer::start(ServerOptions::default()).await;
    let host = host(&server, AuthMethod::PrivateKey)
        .with_identity_file(&path)
        .with_fallback(AuthMethod::Password)
        .with_password(Secret::new("secret"));
    let connection = connect(host, &env, Script::new()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Password));
}

#[tokio::test]
async fn two_factor_publickey_then_keyboard_interactive() {
    let env = Env::new();
    let (path, key) = env.write_key("id_test");
    let server = TestServer::start(ServerOptions {
        authorized_keys: vec![key.public_key().clone()],
        kbd: Some(vec![KbdRound { prompts: vec![("Code: ", true)], expect: vec!["424242"] }]),
        two_factor: true,
        methods: vec![MethodKind::PublicKey, MethodKind::KeyboardInteractive],
        ..Default::default()
    })
    .await;
    let script = Script::new().with_challenges(&[&["424242"]]);
    let host = host(&server, AuthMethod::PrivateKey).with_identity_file(&path);
    let connection = connect(host, &env, script).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::KeyboardInteractive), "last factor is reported");
    assert_eq!(server.attempts("publickey"), 1);
}

// ---- fallback methods (hosts built the way the app builds them) --------------------

fn saved_host(server: &TestServer, auth: AuthMethod) -> shellmate_core::Host {
    let mut host = shellmate_core::Host::new("saved", "127.0.0.1");
    host.port = server.port;
    host.user = Some("alice".into());
    host.auth = auth;
    host
}

fn route_for(host: &shellmate_core::Host, password: Option<&str>) -> Route {
    Route::from_hosts(std::slice::from_ref(host), "unused", |_| shellmate_ssh::HostSecrets {
        password: password.map(Secret::new),
        passphrase: None,
    })
    .unwrap()
}

#[tokio::test]
async fn a_rejected_key_falls_back_to_the_agent() {
    use crate::support::start_agent;

    let env = Env::new();
    let (unauthorized_path, _) = env.write_key("id_unauthorized");
    let agent_key = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let server = TestServer::start(ServerOptions {
        authorized_keys: vec![agent_key.public_key().clone()],
        ..Default::default()
    })
    .await;
    let socket = env.dir.path().join("agent.sock");
    start_agent(&socket, &[&agent_key]).await;

    let mut host = saved_host(&server, AuthMethod::PrivateKey);
    host.identity_file = Some(unauthorized_path);
    let options = env.options(Verifier::new(true), Script::new()).with_agent_socket(&socket);
    let connection = Connection::connect(route_for(&host, None), options).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Agent), "configured key rejected, the agent's key worked");
}

#[tokio::test]
async fn the_configured_agent_failing_falls_back_to_the_password() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    // No agent at all (the options point at a missing socket) and no default keys.
    let host = saved_host(&server, AuthMethod::Agent);
    let connection = Connection::connect(route_for(&host, Some("secret")), env.trusting()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Password));
}

#[tokio::test]
async fn the_error_describes_the_configured_method_not_the_last_fallback() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let host = saved_host(&server, AuthMethod::Agent);
    // Agent unavailable; fallbacks: no key files, keyboard-interactive rejected, wrong password.
    let script = Script::new().with_passwords(&["no", "nope"]);
    let err = Connection::connect(route_for(&host, Some("wrong")), env.options(Verifier::new(true), script))
        .await
        .unwrap_err();
    assert!(matches!(&err, SshError::Agent(reason) if reason.starts_with("无法连接 ssh-agent")), "{err}");
}

#[tokio::test]
async fn cancelling_a_fallback_prompt_reports_the_configured_failure() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let host = saved_host(&server, AuthMethod::Agent);
    // No stored password, nothing scripted: the password fallback prompt is cancelled.
    let err = Connection::connect(route_for(&host, None), env.trusting()).await.unwrap_err();
    assert!(matches!(&err, SshError::Agent(_)), "not AuthCancelled: {err}");
    assert!(!err.is_cancelled());
}

#[tokio::test]
async fn cancelling_the_configured_methods_prompt_is_still_a_cancel() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let host = saved_host(&server, AuthMethod::Password);
    let err = Connection::connect(route_for(&host, None), env.trusting()).await.unwrap_err();
    assert!(matches!(err, SshError::AuthCancelled), "{err}");
}

#[tokio::test]
async fn a_broken_default_key_does_not_stop_the_fallbacks() {
    let env = Env::new();
    std::fs::write(env.ssh_dir.join("id_ed25519"), "this is not a private key").unwrap();
    let server = TestServer::start(ServerOptions::default()).await;
    // Agent configured and absent; the PrivateKey fallback finds a broken default key; the password still works.
    let host = saved_host(&server, AuthMethod::Agent);
    let connection = Connection::connect(route_for(&host, Some("secret")), env.trusting()).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Password));
}

#[tokio::test]
async fn password_hosts_fall_back_to_keyboard_interactive() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions {
        methods: vec![MethodKind::KeyboardInteractive],
        kbd: Some(vec![KbdRound { prompts: vec![("Password: ", false)], expect: vec!["secret"] }]),
        ..Default::default()
    })
    .await;
    let host = saved_host(&server, AuthMethod::Password);
    let script = Script::new().with_challenges(&[&["secret"]]);
    let connection =
        Connection::connect(route_for(&host, None), env.options(Verifier::new(true), script)).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::KeyboardInteractive));
}
