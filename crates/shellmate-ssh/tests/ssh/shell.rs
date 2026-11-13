//! Shell channels: echo, resize, exit status, disconnects, several channels per connection.

use shellmate_core::{AuthMethod, PtyDims, TerminalBackend};
use shellmate_ssh::{Connection, Route, Secret, ShellChannel, SshHost};

use crate::support::{Env, Event, Recorder, ServerOptions, TestServer, eventually, lock};

const DIMS: PtyDims = PtyDims { cols: 100, rows: 30, pixel_width: 800, pixel_height: 600 };

async fn connect(server: &TestServer, env: &Env) -> Connection {
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    Connection::connect(Route::direct(host), env.trusting()).await.unwrap()
}

async fn shell(connection: &Connection, dims: PtyDims) -> (std::sync::Arc<Recorder>, ShellChannel) {
    let (recorder, sink) = Recorder::sink();
    let pending = connection.open_shell(dims).await.unwrap();
    (recorder, pending.start(sink, dims))
}

#[tokio::test]
async fn pty_is_requested_with_term_and_size() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, _shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    assert_eq!(*lock(&server.state.ptys), [("xterm-256color".to_string(), 100, 30)]);
    assert!(lock(&server.state.resizes).is_empty(), "no window change when the size did not change");
}

#[tokio::test]
async fn input_is_echoed_back_in_order() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    shell.write(b"hel").unwrap();
    shell.write(b"lo ").unwrap();
    shell.write("世界".as_bytes()).unwrap();
    recorder.wait_for_text("hello 世界").await;
}

#[tokio::test]
async fn resizing_sends_a_window_change() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    shell.resize(120, 40, 960, 800).unwrap();
    recorder.wait_for_text("[winch 120x40]").await;
    assert_eq!(*lock(&server.state.resizes), [(120, 40)]);
    shell.write(b"size\r").unwrap();
    recorder.wait_for_text("120x40").await;
}

#[tokio::test]
async fn a_size_change_before_start_is_applied_on_start() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, sink) = Recorder::sink();
    let pending = connection.open_shell(DIMS).await.unwrap();
    let resized = PtyDims { cols: 90, rows: 20, pixel_width: 0, pixel_height: 0 };
    let _shell = pending.start(sink, resized);
    recorder.wait_for_text("[winch 90x20]").await;
}

#[tokio::test]
async fn exit_status_is_reported_as_exited() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    shell.write(b"exit 3\r").unwrap();
    assert_eq!(recorder.wait_for_event().await, Event::Exited(Some(3)));
    assert!(!connection.is_closed(), "the connection outlives the shell");
    assert_eq!(lock(&recorder.events).len(), 1, "exactly one terminal event");
}

#[tokio::test]
async fn abrupt_server_drop_is_reported_as_disconnected() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, _shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    server.state.kill_connections();
    match recorder.wait_for_event().await {
        Event::Disconnected(reason) => {
            assert!(reason.starts_with("连接已断开："), "{reason}");
            assert!(reason.contains("服务器关闭了连接") || reason.contains("网络连接被中断"), "{reason}");
        }
        other => panic!("expected a disconnect, got {other:?}"),
    }
    connection.closed().await;
    assert!(connection.disconnect_reason().is_some());
}

#[tokio::test]
async fn ssh_disconnect_message_is_reported_with_its_text() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    shell.write(b"bye\r").unwrap();
    match recorder.wait_for_event().await {
        Event::Disconnected(reason) => assert_eq!(reason, "连接已断开：服务器主动断开了连接：bye from fixture"),
        other => panic!("expected a disconnect, got {other:?}"),
    }
}

#[tokio::test]
async fn writes_after_the_connection_died_do_not_panic() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    server.state.kill_connections();
    recorder.wait_for_event().await;
    // The queue may still accept the bytes or report a broken pipe; both are fine.
    let _ = shell.write(b"ignored");
    let _ = shell.resize(80, 24, 0, 0);
}

#[tokio::test]
async fn one_connection_hosts_several_independent_shells() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (first, mut shell_a) = shell(&connection, DIMS).await;
    let (second, mut shell_b) = shell(&connection, PtyDims { cols: 60, rows: 20, ..DIMS }).await;
    first.wait_for_text("fixture ready").await;
    second.wait_for_text("fixture ready").await;

    shell_a.write(b"aaa\r").unwrap();
    shell_b.write(b"bbb\r").unwrap();
    first.wait_for_text("aaa").await;
    second.wait_for_text("bbb").await;
    assert!(!first.text().contains("bbb") && !second.text().contains("aaa"), "channels do not mix");

    assert_eq!(*lock(&server.state.connections), 1, "no second TCP connection / authentication");
    assert_eq!(server.attempts("password"), 1);
    assert_eq!(*lock(&server.state.shells), 2);
    assert_eq!(lock(&server.state.ptys).iter().map(|p| (p.1, p.2)).collect::<Vec<_>>(), [(100, 30), (60, 20)]);

    // Ending one shell leaves the other running.
    shell_a.write(b"exit 0\r").unwrap();
    assert_eq!(first.wait_for_event().await, Event::Exited(Some(0)));
    shell_b.write(b"still here\r").unwrap();
    second.wait_for_text("still here").await;
    assert!(lock(&second.events).is_empty());
}

#[tokio::test]
async fn closing_a_shell_closes_its_channel_silently() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (recorder, mut shell) = shell(&connection, DIMS).await;
    recorder.wait_for_text("fixture ready").await;
    shell.close();
    eventually("the server to see the channel close", || (*lock(&server.state.closed_channels) >= 1).then_some(()))
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(lock(&recorder.events).is_empty(), "a deliberate close is not reported");
    assert!(!connection.is_closed());
    assert!(shell.write(b"x").is_err() || shell.write(b"y").is_err(), "a closed shell stops accepting input");
}
