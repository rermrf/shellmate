//! ProxyJump: connecting through a jump host.

use std::sync::{Arc, Mutex};

use shellmate_core::{AuthMethod, FingerprintState, PtyDims, TerminalBackend};
use shellmate_ssh::{ConnectEvent, Connection, Route, Secret, SshError, SshHost};

use crate::support::{Env, Recorder, Script, ServerOptions, TestServer, Verifier, lock};

fn host(server: &TestServer, password: &str) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new(password))
}

fn jump_options() -> ServerOptions {
    ServerOptions { forwarding: true, ..Default::default() }
}

#[tokio::test]
async fn shell_on_the_target_through_a_jump_host() {
    let jump = TestServer::start(jump_options()).await;
    let target =
        TestServer::start(ServerOptions { passwords: vec![("deploy", "t0ps3cret")], ..Default::default() }).await;
    let env = Env::new();
    let verifier = Verifier::new(true);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let options = env.options(verifier.clone(), Script::new()).with_event_handler(move |event| lock(&sink).push(event));

    let target_host =
        SshHost::new("127.0.0.1", target.port, "deploy", AuthMethod::Password).with_password(Secret::new("t0ps3cret"));
    let route = Route::direct(target_host).via(host(&jump, "secret"));
    let connection = Connection::connect(route, options).await.unwrap();

    // Both hops were verified (the verifier saw both keys) and authenticated.
    let asked: Vec<u16> = verifier.asked().iter().map(|r| r.port).collect();
    assert_eq!(asked, [jump.port, target.port]);
    let fingerprints: Vec<String> = verifier.asked().iter().map(|r| r.key.fingerprint.clone()).collect();
    assert_eq!(fingerprints, [jump.fingerprint(), target.fingerprint()]);
    let known = env.known_hosts_text();
    assert!(
        known.contains(&format!("[127.0.0.1]:{} ", jump.port))
            && known.contains(&format!("[127.0.0.1]:{} ", target.port))
    );

    let hops = connection.hops();
    assert_eq!(hops.len(), 2);
    assert_eq!((hops[0].port, hops[0].user.as_str()), (jump.port, "alice"));
    assert_eq!((hops[1].port, hops[1].user.as_str()), (target.port, "deploy"));
    assert!(hops.iter().all(|h| h.host_key_state == FingerprintState::New && h.auth == Some(AuthMethod::Password)));
    assert_eq!(connection.target().port, target.port);

    // The target server only ever saw one (tunnelled) connection and the right user.
    assert_eq!(lock(&jump.state.forwards).clone(), [("127.0.0.1".to_string(), target.port as u32)]);
    assert_eq!(*lock(&target.state.connections), 1);
    assert_eq!(target.attempts("password:deploy"), 1);

    let events = lock(&events).clone();
    let connecting: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ConnectEvent::Connecting { port, via, .. } => Some((*port, via.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(connecting, [(jump.port, None), (target.port, Some("127.0.0.1".to_string()))]);

    // A shell on the target works end to end.
    let (recorder, sink) = Recorder::sink();
    let dims = PtyDims { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 };
    let mut shell = connection.open_shell(dims).await.unwrap().start(sink, dims);
    recorder.wait_for_text("fixture ready").await;
    shell.write(b"through the jump host").unwrap();
    recorder.wait_for_text("through the jump host").await;
    assert_eq!(*lock(&target.state.shells), 1);
    assert_eq!(*lock(&jump.state.shells), 0, "the shell runs on the target, not on the jump host");
}

#[tokio::test]
async fn two_jump_hosts_in_a_row() {
    let first = TestServer::start(jump_options()).await;
    let second = TestServer::start(jump_options()).await;
    let target = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let route = Route::direct(host(&target, "secret")).via(host(&first, "secret")).via(host(&second, "secret"));
    let connection = Connection::connect(route, env.trusting()).await.unwrap();
    assert_eq!(connection.hops().iter().map(|h| h.port).collect::<Vec<_>>(), [first.port, second.port, target.port]);
    assert_eq!(lock(&first.state.forwards).len(), 1);
    assert_eq!(lock(&second.state.forwards).len(), 1);
    let (recorder, sink) = Recorder::sink();
    let _shell = connection.open_shell(PtyDims::default()).await.unwrap().start(sink, PtyDims::default());
    recorder.wait_for_text("fixture ready").await;
}

#[tokio::test]
async fn authentication_failure_on_the_jump_host_names_it() {
    let jump = TestServer::start(jump_options()).await;
    let target = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let route = Route::direct(host(&target, "secret")).via(host(&jump, "wrong"));
    let options = env.options(Verifier::new(true), Script::new().with_passwords(&["still wrong", "nope"]));
    let err = Connection::connect(route, options).await.unwrap_err();
    match &err {
        SshError::Jump { port, source, .. } => {
            assert_eq!(*port, jump.port);
            assert!(matches!(**source, SshError::AuthRejected(AuthMethod::Password)), "{err}");
        }
        other => panic!("expected a jump error, got {other}"),
    }
    assert!(err.to_string().starts_with(&format!("跳板机 127.0.0.1:{}：认证失败", jump.port)), "{err}");
    assert_eq!(*lock(&target.state.connections), 0, "the target is never contacted");
}

#[tokio::test]
async fn target_host_key_is_checked_even_behind_a_trusted_jump_host() {
    let jump = TestServer::start(jump_options()).await;
    let target = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let route = Route::direct(host(&target, "secret")).via(host(&jump, "secret"));
    let options = env.options(Verifier::denying(&[target.port]), Script::new());
    let err = Connection::connect(route, options).await.unwrap_err();
    assert!(matches!(err, SshError::HostKeyRejected), "target errors are not attributed to the jump host: {err}");
    assert_eq!(target.attempts("password"), 0);
    assert!(env.known_hosts_text().contains(&format!("[127.0.0.1]:{} ", jump.port)));
    assert!(!env.known_hosts_text().contains(&format!("[127.0.0.1]:{} ", target.port)));
}

#[tokio::test]
async fn jump_host_that_refuses_forwarding_fails_the_connection() {
    let jump = TestServer::start(ServerOptions::default()).await;
    let target = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let route = Route::direct(host(&target, "secret")).via(host(&jump, "secret"));
    let err = Connection::connect(route, env.trusting()).await.unwrap_err();
    assert!(
        matches!(&err, SshError::Connect { port, reason, .. } if *port == target.port && reason.contains("经由")),
        "{err}"
    );
}

#[tokio::test]
async fn route_built_from_saved_hosts_connects_through_the_jump_host() {
    use shellmate_core::Host;
    use shellmate_ssh::HostSecrets;

    let jump = TestServer::start(jump_options()).await;
    let target =
        TestServer::start(ServerOptions { passwords: vec![("deploy", "t0ps3cret")], ..Default::default() }).await;
    let env = Env::new();

    let saved = |name: &str, port: u16, user: &str, auth: AuthMethod| {
        let mut host = Host::new(name, "127.0.0.1");
        host.port = port;
        host.user = Some(user.to_string());
        host.auth = auth;
        host
    };
    let chain = [
        saved("bastion", jump.port, "alice", AuthMethod::Password),
        saved("app", target.port, "deploy", AuthMethod::Password),
    ];
    let route = Route::from_hosts(&chain, "unused", |host| HostSecrets {
        password: Some(Secret::new(if host.name == "bastion" { "secret" } else { "t0ps3cret" })),
        passphrase: None,
    })
    .unwrap();

    let connection = Connection::connect(route, env.trusting()).await.unwrap();
    assert_eq!(connection.hops().iter().map(|h| h.user.as_str()).collect::<Vec<_>>(), ["alice", "deploy"]);
    assert_eq!(lock(&jump.state.forwards).len(), 1);
}
