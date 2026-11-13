//! Connections close with their last channel; a weak handle never keeps one alive.

use std::time::Duration;

use shellmate_core::{AuthMethod, PtyDims, TerminalBackend};
use shellmate_ssh::{Connection, Route, Secret, SshHost};

use crate::support::{Env, Event, Recorder, ServerOptions, TestServer, eventually, lock};

fn host(server: &TestServer) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"))
}

fn open_connections(server: &TestServer) -> usize {
    *lock(&server.state.open_connections)
}

#[tokio::test]
async fn the_connection_closes_when_its_last_channel_closes() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = Connection::connect(Route::direct(host(&server)), env.trusting()).await.unwrap();
    let weak = connection.downgrade();
    let dims = PtyDims::default();

    let (first, sink_a) = Recorder::sink();
    let (second, sink_b) = Recorder::sink();
    let mut shell_a = connection.open_shell(dims).await.unwrap().start(sink_a, dims);
    let mut shell_b = connection.open_shell(dims).await.unwrap().start(sink_b, dims);
    first.wait_for_text("fixture ready").await;
    second.wait_for_text("fixture ready").await;

    // The pool only holds a weak handle: dropping the owner leaves the channels in charge.
    drop(connection);
    assert!(weak.upgrade().is_some(), "two open channels keep the connection up");
    assert_eq!(open_connections(&server), 1);

    shell_a.close();
    drop(shell_a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(weak.upgrade().is_some(), "one channel is left");
    assert_eq!(open_connections(&server), 1, "closing one channel does not end the session");
    shell_b.write(b"still alive\r").unwrap();
    second.wait_for_text("still alive").await;

    shell_b.close();
    drop(shell_b);
    eventually("the server to see the connection end", || (open_connections(&server) == 0).then_some(())).await;
    eventually("the weak handle to go dead", || weak.upgrade().is_none().then_some(())).await;
}

#[tokio::test]
async fn a_weak_handle_does_not_keep_an_idle_connection_alive() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = Connection::connect(Route::direct(host(&server)), env.trusting()).await.unwrap();
    let weak = connection.downgrade();
    assert!(weak.upgrade().is_some());
    drop(connection);
    eventually("the idle connection to close", || (open_connections(&server) == 0).then_some(())).await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn a_dropped_transport_makes_the_weak_handle_useless_even_if_clones_exist() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let connection = Connection::connect(Route::direct(host(&server)), env.trusting()).await.unwrap();
    let weak = connection.downgrade();
    let dims = PtyDims::default();
    let (recorder, sink) = Recorder::sink();
    let _shell = connection.open_shell(dims).await.unwrap().start(sink, dims);
    recorder.wait_for_text("fixture ready").await;

    server.state.kill_connections();
    assert!(matches!(recorder.wait_for_event().await, Event::Disconnected(_)));
    connection.closed().await;
    assert!(weak.upgrade().is_none(), "a dead connection is not offered for reuse, whoever still holds it");
}
