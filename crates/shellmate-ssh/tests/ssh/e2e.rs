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

/// The numbers following `marker` and ending at `end` in `text` (`SESSION-461-END` → 461).
fn number_between(text: &str, marker: &str, end: &str) -> Option<u32> {
    text.match_indices(marker)
        .filter_map(|(at, _)| {
            let rest = &text[at + marker.len()..];
            rest[..rest.find(end)?].parse().ok()
        })
        .next()
}

/// Kills one sshd session process inside the server's container (`SHELLMATE_E2E_CONTAINER`,
/// default `shellmate-e2e`) by pid: the server side drops that one connection, as a crash, a
/// restart or a firewall would. Nothing else in the container is touched.
fn kill_session_process(pid: u32) {
    let container = setting("SHELLMATE_E2E_CONTAINER", "shellmate-e2e");
    let status = std::process::Command::new("docker")
        .args(["exec", &container, "kill", "-9", &pid.to_string()])
        .status()
        .expect("docker is installed");
    assert!(status.success(), "could not kill sshd session {pid} in {container}");
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server (see the module docs)"]
async fn e2e_closing_one_channel_keeps_the_other_and_the_last_close_ends_the_connection() {
    let env = Env::new();
    let target = host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target), env.trusting()).await.expect("connect");
    let weak = connection.downgrade();
    let (first, sink_a) = Recorder::sink();
    let (second, sink_b) = Recorder::sink();
    let mut shell_a = connection.open_shell(DIMS).await.unwrap().start(sink_a, DIMS);
    let mut shell_b = connection.open_shell(DIMS).await.unwrap().start(sink_b, DIMS);
    // The pool holds weak handles: the channels alone keep the connection up.
    drop(connection);
    first.wait_for_text("$").await;
    second.wait_for_text("$").await;

    shell_a.close();
    drop(shell_a);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(weak.upgrade().is_some(), "the second channel keeps the connection");
    shell_b.write(b"echo survivor$((40+2))\r").unwrap();
    second.wait_for_text("survivor42").await;

    shell_b.close();
    drop(shell_b);
    crate::support::eventually("the connection to close with its last channel", || weak.upgrade().is_none().then_some(()))
        .await;
}

#[tokio::test]
#[ignore = "needs a real OpenSSH server and docker access to its container (see the module docs)"]
async fn e2e_a_session_killed_on_the_server_is_reported_and_a_new_connection_works() {
    let env = Env::new();
    let target = || host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target()), env.trusting()).await.expect("connect");
    let (recorder, sink) = Recorder::sink();
    let mut shell = connection.open_shell(DIMS).await.unwrap().start(sink, DIMS);
    recorder.wait_for_text("$").await;

    // The login shell's parent is the sshd process serving exactly this connection.
    shell.write(b"echo SESSION-$PPID-END\r").unwrap();
    let pid = crate::support::eventually("the session's process id", || {
        number_between(&recorder.text(), "SESSION-", "-END")
    })
    .await;
    kill_session_process(pid);

    let event = recorder.wait_for_event().await;
    assert!(matches!(event, Event::Disconnected(_)), "expected the loss to be reported, got {event:?}");
    crate::support::eventually("the connection to be seen as closed", || connection.is_closed().then_some(())).await;
    shell.close();
    drop(shell);
    drop(connection);

    // The server is fine; a fresh connection (what the app's reconnect makes) works like the first.
    let again = Connection::connect(Route::direct(target()), env.trusting()).await.expect("reconnect");
    run_and_exit(&again, "echo back-$((50+8))-again", "back-58-again").await;
}

#[tokio::test]
#[ignore = "needs shellmate-e2e Docker OpenSSH on 127.0.0.1:2222; never run in sandbox"]
async fn e2e_spooled_megabyte_timeout_keeps_channel_and_drop_stops_writer() {
    use std::time::Duration;
    let env = Env::new();
    let target = host(AuthMethod::Password).with_password(Secret::new(password()));
    let connection = Connection::connect(Route::direct(target), env.trusting())
        .await
        .unwrap();
    let mut stream = connection
        .exec_spooled("printf HEAD; head -c 1048576 /dev/zero; printf TAIL; printf problem >&2")
        .await
        .unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(chunk) = stream.next_chunk().await.unwrap() {
        if chunk.stderr {
            stderr.extend(chunk.bytes);
        } else {
            stdout.extend(chunk.bytes);
        }
    }
    assert_eq!(stdout.len(), 1048584);
    assert!(stdout.starts_with(b"HEAD") && stdout.ends_with(b"TAIL"));
    assert_eq!(stderr, b"problem");
    assert_eq!(stream.status(), Some(shellmate_ssh::ExecStatus::Code(0)));
    let mut stream = connection
        .exec_spooled("sleep 1; printf completed")
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stream.next_chunk())
            .await
            .is_err()
    );
    // The caller can move the same still-open handle into its background task registry.
    assert_eq!(
        stream.next_chunk().await.unwrap().unwrap().bytes,
        b"completed"
    );
    while stream.next_chunk().await.unwrap().is_some() {}
    assert_eq!(stream.status(), Some(shellmate_ssh::ExecStatus::Code(0)));
    let mut stream = connection
        .exec_spooled("echo $$; while :; do printf tick; sleep 0.1; done")
        .await
        .unwrap();
    let mut first = Vec::new();
    while !first.contains(&b'\n') {
        first.extend(stream.next_chunk().await.unwrap().unwrap().bytes);
    }
    let pid: u32 = std::str::from_utf8(first.split(|b| *b == b'\n').next().unwrap())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // stop_task and caller drop both release this handle, closing the channel.
    drop(stream);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let alive = connection
            .exec(&format!("kill -0 {pid} 2>/dev/null"))
            .await
            .unwrap()
            .status
            .success();
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "channel drop left remote writer running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut stream = connection
        .exec_spooled("echo $$; exec sleep 30")
        .await
        .unwrap();
    let mut first = Vec::new();
    while !first.contains(&b'\n') {
        first.extend(stream.next_chunk().await.unwrap().unwrap().bytes);
    }
    let pid: u32 = std::str::from_utf8(first.split(|b| *b == b'\n').next().unwrap())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // stop_task and caller drop both release this handle, closing the channel.
    drop(stream);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let alive = connection
            .exec(&format!("kill -0 {pid} 2>/dev/null"))
            .await
            .unwrap()
            .status
            .success();
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "channel drop left silent remote command running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
