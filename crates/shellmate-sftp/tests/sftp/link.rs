//! The SFTP link of a connection: one lazily opened session, kept in step with the connection.

use std::sync::Arc;
use std::time::Duration;

use shellmate_core::AuthMethod;
use shellmate_sftp::SftpSession;
use shellmate_sftp::link::{LinkState, SftpLink};
use shellmate_sftp::transfer::{TransferEvent, TransferState};
use shellmate_ssh::{Connection, Route, Secret, SshHost};
use tokio::runtime::Handle;

use crate::fixture::{Fixture, SftpOptions, pattern};
use crate::support::{Env, ServerOptions, TestServer, eventually, lock};

fn new_link() -> SftpLink {
    SftpLink::new(Handle::current()).0
}

fn requests(fx: &Fixture) -> usize {
    lock(&fx.server.state.subsystem_requests).len()
}

/// Waits for a session other than `not`.
async fn ready(link: &SftpLink, not: Option<&Arc<SftpSession>>) -> Arc<SftpSession> {
    eventually("the session to be ready", || {
        let state = link.state();
        let session = state.session()?;
        not.is_none_or(|old| !Arc::ptr_eq(old, session)).then(|| session.clone())
    })
    .await
}

#[tokio::test]
async fn two_askers_open_the_session_once() {
    let fx = Fixture::default().await;
    let link = new_link();
    let before = requests(&fx);
    link.open(&fx.connection);
    link.open(&fx.connection);
    assert!(matches!(link.state(), LinkState::Opening), "{:?}", link.state());
    let first = ready(&link, None).await;
    link.open(&fx.connection);
    let second = link.state().session().cloned().expect("still ready");
    assert!(Arc::ptr_eq(&first, &second), "one session for everybody");
    assert_eq!(requests(&fx) - before, 1, "the server was asked once");
    fx.file("/x/a.txt", "a", 0o644);
    assert_eq!(first.list("/x").await.unwrap().len(), 1, "and it works");
}

#[tokio::test]
async fn the_connection_closes_when_the_link_is_dropped_even_with_a_transfer_in_flight() {
    let fx = Fixture::start(SftpOptions { write_delay: Duration::from_millis(8), ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), pattern(4 << 20, 1)).unwrap();
    let link = new_link();
    link.open(&fx.connection);
    ready(&link, None).await;
    let id = link.queue().enqueue_upload(vec![dir.path().join("big.bin")], "/");
    eventually("the upload to get going", || {
        link.queue().view(id).filter(|v| v.state == TransferState::Running && v.bytes_done > 0)
    })
    .await;

    // The tabs let go of the connection; the SFTP session and the transfer still hold it.
    let (connection, session, server) = (fx.connection, fx.session, fx.server);
    let weak = connection.downgrade();
    drop(connection);
    drop(session);
    assert!(weak.upgrade().is_some(), "the link keeps the connection up");
    assert_eq!(*lock(&server.state.open_connections), 1);

    drop(link);
    eventually("the connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
    eventually("the weak handle to go dead", || weak.upgrade().is_none().then_some(())).await;
}

#[tokio::test]
async fn a_link_that_never_opened_a_session_lets_go_without_fuss() {
    let fx = Fixture::default().await;
    let link = new_link();
    let weak = fx.connection.downgrade();
    link.open(&fx.connection);
    drop(link);
    let (connection, session, server) = (fx.connection, fx.session, fx.server);
    drop(connection);
    drop(session);
    eventually("the connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn losing_the_connection_drops_the_session_and_a_restored_one_reopens_it() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    let first = ready(&link, None).await;

    link.connection_lost();
    assert!(matches!(link.state(), LinkState::Disconnected), "{:?}", link.state());
    let second_connection = fx.new_connection().await;
    link.connection_restored(&second_connection);
    let second = ready(&link, Some(&first)).await;
    assert!(second.list("/").await.is_ok(), "the new session works");

    // A link that was never asked for a session has nothing to lose and nothing to reopen.
    let idle = new_link();
    idle.connection_lost();
    assert!(matches!(idle.state(), LinkState::Idle));
    idle.connection_restored(&second_connection);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(idle.state(), LinkState::Idle));
}

#[tokio::test]
async fn losing_the_connection_while_opening_throws_the_late_session_away() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    link.connection_lost();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(link.state(), LinkState::Disconnected), "{:?}", link.state());
}

#[tokio::test]
async fn the_session_is_reopened_once_when_the_subsystem_dies_and_the_connection_lives() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    let first = ready(&link, None).await;
    let before = requests(&fx);
    link.handle_event(&TransferEvent::SessionLost);
    link.handle_event(&TransferEvent::SessionLost);
    let second = ready(&link, Some(&first)).await;
    assert_eq!(requests(&fx) - before, 1, "two messages, one reopen");
    // Other events are none of its business.
    link.handle_event(&TransferEvent::RemoteChanged("/".into()));
    assert!(Arc::ptr_eq(&second, link.state().session().unwrap()));
}

#[tokio::test]
async fn a_session_that_will_not_open_is_reported_and_not_retried_on_its_own() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    let connection: Connection = Connection::connect(Route::direct(host), env.trusting()).await.unwrap();
    let link = new_link();
    link.open(&connection);
    eventually("the failure", || matches!(link.state(), LinkState::Failed(_)).then_some(())).await;
    let LinkState::Failed(reason) = link.state() else { unreachable!() };
    assert_eq!(reason, "无法打开会话：服务器拒绝了「sftp」子系统请求");
    link.handle_event(&TransferEvent::SessionLost);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(lock(&server.state.subsystem_requests).len(), 1, "no loop");
    // Asking again (the refresh button) is allowed to try.
    link.open(&connection);
    eventually("a second try", || (lock(&server.state.subsystem_requests).len() == 2).then_some(())).await;
}

#[tokio::test]
async fn a_session_that_keeps_dying_ends_as_failed() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    let mut last = ready(&link, None).await;
    for _ in 0..3 {
        link.handle_event(&TransferEvent::SessionLost);
        last = ready(&link, Some(&last)).await;
    }
    link.handle_event(&TransferEvent::SessionLost);
    let LinkState::Failed(reason) = link.state() else { panic!("{:?}", link.state()) };
    assert!(reason.contains("反复中断"), "{reason}");
}

#[tokio::test]
async fn a_dead_connection_is_noticed_when_the_session_is_to_be_reopened() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    ready(&link, None).await;
    fx.server.state.kill_connections();
    eventually("the connection to be seen as closed", || fx.connection.is_closed().then_some(())).await;
    link.handle_event(&TransferEvent::SessionLost);
    assert!(matches!(link.state(), LinkState::Disconnected), "{:?}", link.state());
}

#[tokio::test]
async fn a_closed_link_ignores_everything() {
    let fx = Fixture::default().await;
    let link = new_link();
    link.open(&fx.connection);
    ready(&link, None).await;
    link.shutdown();
    assert!(matches!(link.state(), LinkState::Closed));
    let before = requests(&fx);
    link.open(&fx.connection);
    link.connection_restored(&fx.connection);
    link.handle_event(&TransferEvent::SessionLost);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(link.state(), LinkState::Closed));
    assert_eq!(requests(&fx), before);
}
