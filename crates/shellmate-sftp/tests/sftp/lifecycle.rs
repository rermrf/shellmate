//! Opening the subsystem and letting go of it.

use shellmate_core::AuthMethod;
use shellmate_sftp::{SftpError, SftpSession};
use shellmate_ssh::{Connection, Route, Secret, SshHost};

use crate::fixture::Fixture;
use crate::support::{Env, ServerOptions, TestServer, eventually, lock};

#[tokio::test]
async fn a_server_without_sftp_refuses_the_subsystem_with_a_clear_message() {
    let server = TestServer::start(ServerOptions::default()).await;
    let env = Env::new();
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    let connection = Connection::connect(Route::direct(host), env.trusting()).await.unwrap();
    let error = match SftpSession::open(&connection).await {
        Err(error) => error,
        Ok(_) => panic!("the server has no sftp subsystem"),
    };
    assert!(matches!(error, SftpError::Ssh(_)), "{error:?}");
    assert_eq!(error.to_string(), "无法打开会话：服务器拒绝了「sftp」子系统请求");
    assert_eq!(*lock(&server.state.subsystem_requests), ["sftp"]);
    // The connection is fine, the shell still works.
    assert!(!connection.is_closed());
}

#[tokio::test]
async fn the_connection_closes_when_the_session_and_the_connection_are_dropped() {
    let fx = Fixture::default().await;
    let (session, connection, server) = (fx.session, fx.connection, fx.server);
    let weak = connection.downgrade();
    assert!(session.list("/").await.is_ok());
    drop(connection);
    assert!(weak.upgrade().is_some(), "the SFTP channel keeps the connection up");
    assert_eq!(*lock(&server.state.open_connections), 1);
    drop(session);
    eventually("the connection to close with its last channel", || {
        (*lock(&server.state.open_connections) == 0).then_some(())
    })
    .await;
    eventually("the weak handle to go dead", || weak.upgrade().is_none().then_some(())).await;
}

#[tokio::test]
async fn two_sessions_share_one_connection() {
    let fx = Fixture::default().await;
    fx.file("/shared.txt", "both see this", 0o644);
    let second = SftpSession::open(&fx.connection).await.unwrap();
    assert_eq!(second.open_text("/shared.txt").await.unwrap().text, "both see this");
    assert_eq!(fx.session.list("/").await.unwrap().len(), 2, "home and shared.txt");
    assert_eq!(*lock(&fx.server.state.connections), 1, "one TCP connection");
}
