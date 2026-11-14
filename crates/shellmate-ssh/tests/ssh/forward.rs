//! `direct-tcpip` channels and remote forwards (`tcpip-forward` / `forwarded-tcpip`).

use std::net::SocketAddr;
use std::time::Duration;

use shellmate_core::AuthMethod;
use shellmate_ssh::{Connection, RemoteForward, Route, Secret, SshError, SshHost, TcpipFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{EchoServer, Env, ServerOptions, TestServer, closed_port, eventually, lock};

fn host(server: &TestServer) -> SshHost {
    SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"))
}

fn forwarding() -> ServerOptions {
    ServerOptions { forwarding: true, remote_forwarding: true, ..Default::default() }
}

async fn connect(server: &TestServer) -> (Connection, Env) {
    let env = Env::new();
    let connection = Connection::connect(Route::direct(host(server)), env.trusting()).await.expect("connect");
    (connection, env)
}

fn open_connections(server: &TestServer) -> usize {
    *lock(&server.state.open_connections)
}

const ORIGINATOR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40_000);

#[tokio::test]
async fn direct_tcpip_carries_bytes_both_ways_and_passes_half_close() {
    let server = TestServer::start(forwarding()).await;
    let echo = EchoServer::start().await;
    let (connection, _env) = connect(&server).await;

    let mut stream = connection.open_direct_tcpip("127.0.0.1", echo.port, ORIGINATOR).await.expect("channel");
    stream.write_all(b"ping through the tunnel").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut back = Vec::new();
    stream.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"ping through the tunnel");
    assert_eq!(lock(&server.state.forwards).as_slice(), [("127.0.0.1".to_string(), u32::from(echo.port))]);
}

#[tokio::test]
async fn direct_tcpip_failures_say_why() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let port = closed_port();
    let err = connection.open_direct_tcpip("127.0.0.1", port, ORIGINATOR).await.unwrap_err();
    assert!(
        matches!(err, SshError::Tcpip { reason: TcpipFailure::ConnectFailed, port: p, .. } if p == port),
        "{err:?}"
    );
    assert!(err.to_string().contains(&format!("127.0.0.1:{port}")), "{err}");

    let forbidding = TestServer::start(ServerOptions::default()).await;
    let (connection, _env) = connect(&forbidding).await;
    let err = connection.open_direct_tcpip("127.0.0.1", port, ORIGINATOR).await.unwrap_err();
    assert!(matches!(err, SshError::Tcpip { reason: TcpipFailure::Prohibited, .. }), "{err:?}");
    assert!(err.to_string().contains("AllowTcpForwarding"), "{err}");
}

#[tokio::test]
async fn direct_tcpip_on_a_dead_connection_fails_as_disconnected() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    server.state.kill_connections();
    connection.closed().await;
    let err = connection.open_direct_tcpip("127.0.0.1", 9, ORIGINATOR).await.unwrap_err();
    assert!(matches!(err, SshError::Tcpip { reason: TcpipFailure::Disconnected, .. }), "{err:?}");
}

#[tokio::test]
async fn a_direct_tcpip_stream_keeps_the_connection_open_and_closing_it_lets_go() {
    let server = TestServer::start(forwarding()).await;
    let echo = EchoServer::start().await;
    let (connection, _env) = connect(&server).await;
    let mut stream = connection.open_direct_tcpip("127.0.0.1", echo.port, ORIGINATOR).await.unwrap();
    let weak = connection.downgrade();
    drop(connection);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(open_connections(&server), 1, "the stream holds the connection");
    stream.write_all(b"still there").await.unwrap();
    let mut buf = [0u8; 11];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"still there");

    drop(stream);
    eventually("the connection to close with its last stream", || (open_connections(&server) == 0).then_some(())).await;
    assert!(weak.upgrade().is_none());
}

/// Connects to the server-side listener of a remote forward, as a client of the forwarded service.
async fn dial(port: u16) -> TcpStream {
    TcpStream::connect(("127.0.0.1", port)).await.expect("the server listens")
}

/// Serves one forwarded connection with an echo (what a rule's local target does).
fn echo_one(mut forward: RemoteForward) -> tokio::task::JoinHandle<RemoteForward> {
    tokio::spawn(async move {
        let stream = forward.accept().await.expect("a connection arrives");
        let (mut reader, mut writer) = tokio::io::split(stream);
        let _ = tokio::io::copy(&mut reader, &mut writer).await;
        let _ = writer.shutdown().await;
        forward
    })
}

#[tokio::test]
async fn remote_forward_with_port_zero_reports_the_servers_port_and_delivers_connections() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let forward = connection.request_remote_forward("127.0.0.1", 0).await.expect("accepted");
    assert_ne!(forward.port, 0);
    assert_eq!(forward.address, "127.0.0.1");
    assert_eq!(lock(&server.state.remote_forwards).as_slice(), [("127.0.0.1".to_string(), 0)]);

    let port = forward.port;
    let serving = echo_one(forward);
    let mut client = dial(port).await;
    client.write_all(b"from the far side").await.unwrap();
    client.shutdown().await.unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, b"from the far side");
    drop(serving.await.unwrap());
}

#[tokio::test]
async fn remote_forward_on_a_given_port_and_the_originator_of_each_connection() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let wanted = closed_port();
    let mut forward = connection.request_remote_forward("127.0.0.1", wanted).await.expect("accepted");
    assert_eq!(forward.port, wanted, "a given port is the port");

    let client = dial(wanted).await;
    let peer = client.local_addr().unwrap();
    let stream = forward.accept().await.expect("delivered");
    assert_eq!(stream.originator_address, "127.0.0.1");
    assert_eq!(stream.originator_port, peer.port());
}

#[tokio::test]
async fn a_server_that_refuses_remote_forwards_is_reported_as_such() {
    let server = TestServer::start(ServerOptions { forwarding: true, ..Default::default() }).await;
    let (connection, _env) = connect(&server).await;
    let err = connection.request_remote_forward("127.0.0.1", 0).await.unwrap_err();
    assert!(matches!(err, SshError::RemoteForwardDenied), "{err:?}");
    assert!(err.to_string().contains("AllowTcpForwarding"));
}

#[tokio::test]
async fn forwards_on_different_ports_get_their_own_connections() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let mut first = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    let mut second = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    assert_ne!(first.port, second.port);

    let _to_second = dial(second.port).await;
    let _to_first = dial(first.port).await;
    let on_second = tokio::time::timeout(Duration::from_secs(5), second.accept()).await.unwrap().unwrap();
    let on_first = tokio::time::timeout(Duration::from_secs(5), first.accept()).await.unwrap().unwrap();
    drop((on_first, on_second));
    assert!(tokio::time::timeout(Duration::from_millis(200), first.accept()).await.is_err(), "nothing else arrived");
}

#[tokio::test]
async fn the_address_the_server_reports_may_differ_from_the_one_asked_for() {
    let server = TestServer::start(ServerOptions { announce_address: Some("127.0.0.1"), ..forwarding() }).await;
    let (connection, _env) = connect(&server).await;
    let mut forward = connection.request_remote_forward("localhost", 0).await.unwrap();
    let _client = dial(forward.port).await;
    let stream = tokio::time::timeout(Duration::from_secs(5), forward.accept()).await.unwrap();
    assert!(stream.is_some(), "matched by port although the address came back as 127.0.0.1");
}

#[tokio::test]
async fn connections_nobody_registered_for_are_refused() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let forward = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    let port = forward.port;
    // The registration goes away, the server keeps listening.
    drop(forward);

    let mut client = dial(port).await;
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf)).await.expect("the server answers");
    assert!(matches!(read, Ok(0) | Err(_)), "the refused connection is closed: {read:?}");
}

#[tokio::test]
async fn cancelling_a_remote_forward_stops_the_servers_listener() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let forward = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    let port = forward.port;
    connection.cancel_remote_forward("127.0.0.1", port).await.expect("cancelled");
    assert_eq!(lock(&server.state.cancelled_forwards).as_slice(), [("127.0.0.1".to_string(), u32::from(port))]);
    eventually("the listener to go away", || std::net::TcpStream::connect(("127.0.0.1", port)).is_err().then_some(()))
        .await;
    drop(forward);
}

#[tokio::test]
async fn only_a_bounded_number_of_unserved_connections_is_queued() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let mut forward = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    let mut clients = Vec::new();
    for _ in 0..80 {
        clients.push(dial(forward.port).await);
    }
    // Let the server hand them all over (or be refused), without anybody accepting.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut delivered = 0;
    while tokio::time::timeout(Duration::from_millis(300), forward.accept()).await.is_ok_and(|s| s.is_some()) {
        delivered += 1;
    }
    assert!((60..80).contains(&delivered), "a bounded queue held {delivered} of 80");
}

#[tokio::test]
async fn forwarded_streams_end_with_the_connection() {
    let server = TestServer::start(forwarding()).await;
    let (connection, _env) = connect(&server).await;
    let mut forward = connection.request_remote_forward("127.0.0.1", 0).await.unwrap();
    let _client = dial(forward.port).await;
    let mut stream = forward.accept().await.unwrap();

    server.state.kill_connections();
    connection.closed().await;
    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await.expect("ends");
    assert!(matches!(read, Ok(0) | Err(_)));
    drop(connection);
    assert!(forward.accept().await.is_none(), "no more connections once the connection is gone");
}
