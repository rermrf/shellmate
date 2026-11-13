//! End-to-end tests against a real OpenSSH server. They are `#[ignore]`d: start a throwaway
//! container (bound to 127.0.0.1 only) and run them on demand.
//!
//! ```text
//! ssh-keygen -q -t ed25519 -N "" -f /tmp/shellmate-e2e-key
//! docker run -d --name shellmate-e2e --label shellmate-e2e=1 -p 127.0.0.1:2222:2222 \
//!   -e PUID=1000 -e PGID=1000 -e USER_NAME=tester -e USER_PASSWORD=e2e-pass-123 \
//!   -e PASSWORD_ACCESS=true -e PUBLIC_KEY="$(cat /tmp/shellmate-e2e-key.pub)" \
//!   linuxserver/openssh-server
//! SHELLMATE_E2E_KEY=/tmp/shellmate-e2e-key cargo test -p shellmate-ssh --test ssh e2e -- --ignored
//! docker rm -f shellmate-e2e
//! ```
//!
//! Settings (environment): `SHELLMATE_E2E_PORT` (2222), `SHELLMATE_E2E_USER` (tester),
//! `SHELLMATE_E2E_PASSWORD` (e2e-pass-123), `SHELLMATE_E2E_KEY` (path of the private key).
//! Host keys are trusted into a temporary known_hosts; nothing touches `~/.ssh`.
//!
//! Keyboard-interactive is not covered: this image's sshd has no PAM, so it advertises the
//! method but never accepts it (the system `ssh` client fails the same way); the in-process
//! server tests cover that method.

use shellmate_core::{AuthMethod, FingerprintState, PtyDims, TerminalBackend};
use shellmate_ssh::{Connection, Route, Secret, SshError, SshHost};

use crate::support::{Env, Event, Recorder, Script, Verifier};

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn host(auth: AuthMethod) -> SshHost {
    let port = setting("SHELLMATE_E2E_PORT", "2222").parse().expect("SHELLMATE_E2E_PORT is a port number");
    SshHost::new("127.0.0.1", port, setting("SHELLMATE_E2E_USER", "tester"), auth)
}

fn password() -> String {
    setting("SHELLMATE_E2E_PASSWORD", "e2e-pass-123")
}

fn key_path() -> String {
    std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts")
}

const DIMS: PtyDims = PtyDims { cols: 100, rows: 30, pixel_width: 0, pixel_height: 0 };

/// Runs `command`, waits for `expect`, then exits the shell and checks the exit status.
async fn run_and_exit(connection: &Connection, command: &str, expect: &str) {
    let (recorder, sink) = Recorder::sink();
    let mut shell = connection.open_shell(DIMS).await.expect("shell").start(sink, DIMS);
    // Wait for the prompt before typing so the command is not lost to shell start-up.
    recorder.wait_for_text("$").await;
    shell.write(format!("{command}\r").as_bytes()).unwrap();
    recorder.wait_for_text(expect).await;
    shell.write(b"exit\r").unwrap();
    assert_eq!(recorder.wait_for_event().await, Event::Exited(Some(0)));
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_password_login_runs_a_command_and_exits() {
    let env = Env::new();
    let target = host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target), env.trusting()).await.expect("connect");
    assert_eq!(connection.target().auth, Some(AuthMethod::Password));
    run_and_exit(&connection, "echo shellmate-$((20+22))-ok", "shellmate-42-ok").await;
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_key_login() {
    let env = Env::new();
    let target = host(AuthMethod::PrivateKey).with_identity_file(key_path());
    let connection = Connection::connect(Route::direct(target), env.trusting()).await.expect("connect");
    assert_eq!(connection.target().auth, Some(AuthMethod::PrivateKey));
    run_and_exit(&connection, "echo key-$((6*7))-ok", "key-42-ok").await;
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_host_key_is_confirmed_once_then_remembered() {
    let env = Env::new();
    let verifier = Verifier::new(true);
    let target = || host(AuthMethod::Password).with_password(Secret::new(password()));

    let first = Connection::connect(Route::direct(target()), env.options(verifier.clone(), Script::new()))
        .await
        .expect("connect");
    assert_eq!(first.target().host_key_state, FingerprintState::New);
    assert_eq!(first.target().host_key.algorithm, "ssh-ed25519", "OpenSSH offers ed25519 and we ask for it first");
    assert!(first.target().host_key.fingerprint.starts_with("SHA256:"));
    drop(first);

    let second = Connection::connect(Route::direct(target()), env.options(verifier.clone(), Script::new()))
        .await
        .expect("connect");
    assert_eq!(second.target().host_key_state, FingerprintState::Known);
    assert_eq!(verifier.asked().len(), 1);
    assert!(env.known_hosts_text().contains("ssh-ed25519"));
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_wrong_password_is_rejected_with_a_clear_error() {
    let env = Env::new();
    let script = Script::new().with_passwords(&["wrong-1", "wrong-2", "wrong-3"]);
    let err = Connection::connect(Route::direct(host(AuthMethod::Password)), env.options(Verifier::new(true), script))
        .await
        .unwrap_err();
    assert!(matches!(err, SshError::AuthRejected(AuthMethod::Password)), "{err}");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_resize_reaches_the_remote_pty() {
    let env = Env::new();
    let target = host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target), env.trusting()).await.expect("connect");
    let (recorder, sink) = Recorder::sink();
    let mut shell = connection.open_shell(DIMS).await.unwrap().start(sink, DIMS);
    recorder.wait_for_text("$").await;
    shell.resize(120, 40, 0, 0).unwrap();
    shell.write(b"stty size\r").unwrap();
    recorder.wait_for_text("40 120").await;
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_two_shells_share_one_connection() {
    let env = Env::new();
    let target = host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target), env.trusting()).await.expect("connect");
    let (first, sink_a) = Recorder::sink();
    let (second, sink_b) = Recorder::sink();
    let mut shell_a = connection.open_shell(DIMS).await.unwrap().start(sink_a, DIMS);
    let mut shell_b = connection.open_shell(DIMS).await.unwrap().start(sink_b, DIMS);
    first.wait_for_text("$").await;
    second.wait_for_text("$").await;
    shell_a.write(b"echo A$((1+1))\r").unwrap();
    shell_b.write(b"echo B$((2+2))\r").unwrap();
    first.wait_for_text("A2").await;
    second.wait_for_text("B4").await;
    shell_a.write(b"exit\r").unwrap();
    assert_eq!(first.wait_for_event().await, Event::Exited(Some(0)));
    shell_b.write(b"echo still$((3+4))\r").unwrap();
    second.wait_for_text("still7").await;
}
