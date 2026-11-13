//! The connect watchdog: dead servers time out, waiting for the user does not count.

use std::time::{Duration, Instant};

use shellmate_core::{AuthMethod, PtyDims};
use shellmate_ssh::{ConnectOptions, Connection, Route, Secret, SshError, SshHost};

use crate::support::{Env, Script, ServerOptions, SilentServer, TestServer, Verifier};

const LIMIT: Duration = Duration::from_millis(300);

fn host(server: &TestServer) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"))
}

fn options(env: &Env, verifier: std::sync::Arc<Verifier>, script: std::sync::Arc<Script>) -> ConnectOptions {
    env.options(verifier, script).with_connect_watchdog(LIMIT)
}

#[test]
fn the_default_budget_is_twenty_seconds_and_the_error_says_so() {
    let options = ConnectOptions::new(Verifier::new(false), Script::new());
    assert_eq!(options.connect_watchdog, Duration::from_secs(20));
    assert_eq!(SshError::Timeout { limit: Duration::from_secs(20) }.to_string(), "连接超时（20 秒内没有响应）");
    assert_eq!(SshError::Timeout { limit: LIMIT }.to_string(), "连接超时（300 毫秒内没有响应）");
}

#[tokio::test]
async fn a_server_that_accepts_and_never_speaks_times_out() {
    let silent = SilentServer::start().await;
    let env = Env::new();
    let target = SshHost::new("127.0.0.1", silent.port, "alice", AuthMethod::Password);
    let started = Instant::now();
    let err = Connection::connect(Route::direct(target), options(&env, Verifier::new(true), Script::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, SshError::Timeout { limit } if limit == LIMIT), "{err}");
    assert!(!err.is_cancelled(), "a timeout is a failure, not the user backing out");
    assert!(started.elapsed() < Duration::from_secs(5), "gave up after about the budget");
}

#[tokio::test]
async fn a_slow_host_key_prompt_does_not_time_out() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let verifier = Verifier::slow(true, Duration::from_millis(900));
    let started = Instant::now();
    let connection =
        Connection::connect(Route::direct(host(&server)), options(&env, verifier.clone(), Script::new())).await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert!(started.elapsed() > Duration::from_millis(800), "the user really did take that long");
    assert_eq!(verifier.asked().len(), 1);
}

#[tokio::test]
async fn slow_password_prompts_do_not_time_out_either() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    // No saved password: the user is asked, twice over the budget, and then also approves the key slowly.
    let target = SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password);
    let script = Script::new().with_passwords(&["secret"]).with_delay(Duration::from_millis(700));
    let verifier = Verifier::slow(true, Duration::from_millis(700));
    let connection = Connection::connect(Route::direct(target), options(&env, verifier, script)).await;
    assert!(connection.is_ok(), "{:?}", connection.err());
}

#[tokio::test]
async fn time_spent_on_the_network_still_counts_around_a_prompt() {
    // The user takes 600 ms (free), but the server then sits on the password check for 500 ms.
    let server =
        TestServer::start(ServerOptions { auth_delay: Duration::from_millis(500), ..Default::default() }).await;
    let env = Env::new();
    let verifier = Verifier::slow(true, Duration::from_millis(600));
    let err =
        Connection::connect(Route::direct(host(&server)), options(&env, verifier, Script::new())).await.unwrap_err();
    assert!(matches!(err, SshError::Timeout { .. }), "{err}");
}

#[tokio::test]
async fn a_stalled_second_hop_times_out() {
    let jump = TestServer::start(ServerOptions { forwarding: true, ..Default::default() }).await;
    let silent = SilentServer::start().await;
    let env = Env::new();
    let dead = SshHost::new("127.0.0.1", silent.port, "alice", AuthMethod::Password);
    let route = Route::direct(dead).via(host(&jump));

    let started = Instant::now();
    let err = Connection::connect(route, options(&env, Verifier::new(true), Script::new())).await.unwrap_err();
    assert!(matches!(err, SshError::Timeout { .. }), "the first hop was fine, the second never answered: {err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(jump.attempts("password"), 1, "the jump host itself was reached and authenticated");
}

#[tokio::test]
async fn the_budget_is_shared_by_all_hops() {
    // Each hop's handshake takes about 200 ms (slow password check); two of them blow the 300 ms budget.
    let jump = TestServer::start(ServerOptions {
        forwarding: true,
        auth_delay: Duration::from_millis(200),
        ..Default::default()
    })
    .await;
    let target =
        TestServer::start(ServerOptions { auth_delay: Duration::from_millis(200), ..Default::default() }).await;
    let env = Env::new();
    let route = Route::direct(host(&target)).via(host(&jump));
    let err = Connection::connect(route, options(&env, Verifier::new(true), Script::new())).await.unwrap_err();
    assert!(matches!(err, SshError::Timeout { .. }), "{err}");
    // One hop alone fits.
    let single = Route::direct(host(&jump));
    assert!(Connection::connect(single, options(&env, Verifier::new(true), Script::new())).await.is_ok());
}

#[tokio::test]
async fn opening_the_shell_is_part_of_the_attempt() {
    let server = TestServer::start(ServerOptions { pty_delay: Duration::from_secs(2), ..Default::default() }).await;
    let env = Env::new();
    let dims = PtyDims::default();
    let started = Instant::now();
    let err = Connection::connect_with_shell(
        Route::direct(host(&server)),
        options(&env, Verifier::new(true), Script::new()),
        dims,
    )
    .await
    .err()
    .expect("the PTY request is never answered in time");
    assert!(matches!(err, SshError::Timeout { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));

    // Connecting alone is fine with the same server, and so is opening the shell once it answers quickly.
    let quick = TestServer::start(ServerOptions::default()).await;
    let (_connection, _shell) = Connection::connect_with_shell(
        Route::direct(host(&quick)),
        options(&env, Verifier::new(true), Script::new()),
        dims,
    )
    .await
    .expect("a prompt server fits the budget");
}

#[tokio::test]
async fn opening_another_shell_gets_a_fresh_budget_and_times_out_on_its_own() {
    let server = TestServer::start(ServerOptions { pty_delay: Duration::from_secs(2), ..Default::default() }).await;
    let env = Env::new();
    let connection =
        Connection::connect(Route::direct(host(&server)), options(&env, Verifier::new(true), Script::new()))
            .await
            .expect("connecting does not involve the PTY");
    let err = connection.open_shell(PtyDims::default()).await.err().expect("times out");
    assert!(matches!(err, SshError::Timeout { .. }), "{err}");
}
