use crate::support::{Env, ServerOptions, TestServer, eventually, exec_runner, lock};
use shellmate_core::{AuthMethod, PtyDims, Secret};
use shellmate_ssh::{Connection, Route, SshHost, mosh};
use shellmate_term::LocalPty;
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const KEY: &str = "abcdefghijklmnopqrstuw";
async fn server(installed: bool) -> TestServer {
    TestServer::start(ServerOptions {
        exec: Some(exec_runner(move |command, channel| async move {
            match command.as_str() {
                mosh::SERVER_CHECK => {
                    if installed {
                        channel.stdout("/usr/bin/mosh-server\n").await;
                        channel.exit(0).await;
                    } else {
                        channel.exit(1).await;
                    }
                }
                mosh::LOCALES => {
                    channel.stdout("C\nC.utf8\nen_US.UTF-8\n").await;
                    channel.exit(0).await;
                }
                "mosh-server new -s -c 256 -l LANG=C.utf8" => {
                    channel
                        .stderr("mosh-server 1.4.0\n[mosh-server detached, pid = 12345]\n")
                        .await;
                    channel.stdout(format!("MOSH CONNECT 60001 {KEY}\n")).await;
                    channel.exit(0).await;
                }
                cmd if cmd.contains("kill -TERM 12345") => channel.exit(0).await,
                _ => channel.exit(127).await,
            }
        })),
        ..Default::default()
    })
    .await
}
async fn connect(server: &TestServer, env: &Env) -> Connection {
    let host = SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password)
        .with_password(Secret::new("secret"));
    Connection::connect(Route::direct(host), env.trusting())
        .await
        .unwrap()
}
fn client(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("mosh-client");
    std::fs::write(&path,b"#!/bin/sh\nstty size\nprintf 'arg:%s\\n' \"$@\"\n[ -n \"$MOSH_KEY\" ] && printf 'env:MOSH_KEY\\n'\n[ -n \"$TERM\" ] && printf 'env:TERM\\n'\n[ -n \"$LANG\" ] && printf 'env:LANG\\n'\n[ -n \"$LC_ALL\" ] && printf 'env:LC_ALL\\n'\n[ -n \"$MOSH_PREDICTION_DISPLAY\" ] && printf 'env:MOSH_PREDICTION_DISPLAY\\n'\nexit 1\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
#[tokio::test]
async fn bootstrap_spawn_size_env_and_immediate_exit_cleanup() {
    let server = server(true).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    assert_eq!(
        connection.peer_addr().unwrap().ip().to_string(),
        "127.0.0.1"
    );
    let mut ready = mosh::prepare(connection, client(env.dir.path()))
        .await
        .unwrap();
    ready.program.cwd = Some(env.dir.path().into());
    let lease = ready.lease.clone();
    let start = Instant::now();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let output = Arc::new(Mutex::new(Vec::new()));
    let copy = output.clone();
    let _pty = LocalPty::spawn_program(
        &ready.program,
        PtyDims {
            cols: 100,
            rows: 30,
            ..Default::default()
        },
        move |b| copy.lock().unwrap().extend_from_slice(b),
        move |code| {
            lease.client_exited(code, start.elapsed());
            let _ = tx.send(code);
        },
    )
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap(),
        Some(1)
    );
    let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert_eq!(text.lines().next().unwrap().trim(), "30 100");
    for name in [
        "MOSH_KEY",
        "TERM",
        "LANG",
        "LC_ALL",
        "MOSH_PREDICTION_DISPLAY",
    ] {
        assert!(text.contains(&format!("env:{name}")));
    }
    assert!(!text.contains(KEY));
    assert!(text.contains("arg:127.0.0.1"));
    eventually("cleanup", || {
        lock(&server.state.execs)
            .iter()
            .any(|s| s.contains("kill -TERM 12345"))
            .then_some(())
    })
    .await;
    assert_eq!(
        &lock(&server.state.execs)[..3],
        [
            mosh::SERVER_CHECK,
            mosh::LOCALES,
            "mosh-server new -s -c 256 -l LANG=C.utf8"
        ]
    );
    assert_eq!(
        *lock(&server.state.shells),
        0,
        "bootstrap must not open an SSH shell"
    );
}
#[tokio::test]
async fn missing_client_starts_nothing_and_missing_server_is_chinese() {
    let server = server(false).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let error = mosh::prepare(connection.clone(), env.dir.path().join("absent"))
        .await
        .err()
        .unwrap();
    assert_eq!(error, mosh::MISSING_CLIENT);
    assert!(lock(&server.state.execs).is_empty());
    let error = mosh::prepare(connection, client(env.dir.path()))
        .await
        .err()
        .unwrap();
    assert_eq!(error, mosh::MISSING_SERVER);
    assert_eq!(*lock(&server.state.execs), [mosh::SERVER_CHECK]);
}
#[tokio::test]
async fn failed_local_spawn_and_abandoned_prepared_server_are_cleaned() {
    let server = server(true).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let path = client(env.dir.path());
    let ready = mosh::prepare(connection.clone(), path.clone())
        .await
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(LocalPty::spawn_program(&ready.program, PtyDims::default(), |_| {}, |_| {}).is_err());
    drop(ready);
    eventually("spawn-failure cleanup", || {
        lock(&server.state.execs)
            .iter()
            .any(|s| s.contains("kill -TERM 12345"))
            .then_some(())
    })
    .await;
    let ready = mosh::prepare(connection, client(env.dir.path()))
        .await
        .unwrap();
    drop(ready);
    eventually("abandoned bootstrap cleanup", || {
        (lock(&server.state.execs)
            .iter()
            .filter(|s| s.contains("kill -TERM 12345"))
            .count()
            == 2)
            .then_some(())
    })
    .await;
}
#[tokio::test]
async fn jump_connection_is_refused_before_bootstrap() {
    let server = server(true).await;
    let jump = TestServer::start(ServerOptions {
        forwarding: true,
        ..Default::default()
    })
    .await;
    let env = Env::new();
    let host = |port| {
        SshHost::new("127.0.0.1", port, "alice", AuthMethod::Password)
            .with_password(Secret::new("secret"))
    };
    let connection = Connection::connect(
        Route {
            via: vec![host(jump.port)],
            target: host(server.port),
        },
        env.trusting(),
    )
    .await
    .unwrap();
    assert!(connection.peer_addr().is_none());
    assert_eq!(
        mosh::prepare(connection, client(env.dir.path()))
            .await
            .err()
            .unwrap(),
        mosh::JUMP_REFUSAL
    );
    assert!(lock(&server.state.execs).is_empty());
}

#[tokio::test]
async fn cancelled_bootstrap_waiter_cleans_its_detached_server() {
    let server = server(true).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let task = tokio::spawn(mosh::prepare(connection, client(env.dir.path())));
    eventually("bootstrap started", || {
        lock(&server.state.execs)
            .iter()
            .any(|s| s.starts_with("mosh-server new"))
            .then_some(())
    })
    .await;
    task.abort();
    drop(task);
    eventually("cancelled bootstrap cleanup", || {
        lock(&server.state.execs)
            .iter()
            .any(|s| s.contains("kill -TERM 12345"))
            .then_some(())
    })
    .await;
}
