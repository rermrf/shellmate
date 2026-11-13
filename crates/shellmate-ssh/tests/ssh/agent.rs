//! ssh-agent authentication against an in-process agent on a temp unix socket.

use std::path::Path;

use futures::StreamExt;
use russh::keys::agent::client::AgentClient;
use russh::keys::key::safe_rng;
use russh::keys::{Algorithm, PrivateKey};
use shellmate_core::AuthMethod;
use shellmate_ssh::{Connection, Route, SshError, SshHost};
use tokio::net::UnixListener;

use crate::support::{Env, Script, ServerOptions, TestServer, Verifier};

/// Starts an agent serving `socket` and loads `keys` into it.
async fn start_agent(socket: &Path, keys: &[&PrivateKey]) {
    let listener = UnixListener::bind(socket).unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        let accepted = listener.accept().await.map(|(stream, _)| stream);
        Some((accepted, listener))
    })
    .boxed();
    tokio::spawn(russh::keys::agent::server::serve(incoming, ()));
    let mut client = AgentClient::connect_uds(socket).await.unwrap();
    for key in keys {
        client.add_identity(key, &[]).await.unwrap();
    }
}

async fn connect(server: &TestServer, env: &Env, socket: &Path) -> Result<Connection, SshError> {
    let host = SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Agent);
    let options = env.options(Verifier::new(true), Script::new()).with_agent_socket(socket);
    Connection::connect(Route::direct(host), options).await
}

#[tokio::test]
async fn agent_key_authenticates() {
    let env = Env::new();
    let unrelated = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let wanted = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let server =
        TestServer::start(ServerOptions { authorized_keys: vec![wanted.public_key().clone()], ..Default::default() })
            .await;
    let socket = env.dir.path().join("agent.sock");
    start_agent(&socket, &[&unrelated, &wanted]).await;

    let connection = connect(&server, &env, &socket).await.unwrap();
    assert_eq!(connection.target().auth, Some(AuthMethod::Agent));
    // The agent lists its keys in no particular order; the unrelated one may or may not be tried first.
    assert!((1..=2).contains(&server.attempts("publickey")), "keys are offered one by one until one is accepted");
}

#[tokio::test]
async fn agent_without_an_authorized_key_is_rejected() {
    let env = Env::new();
    let loaded = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
    let server = TestServer::start(ServerOptions::default()).await;
    let socket = env.dir.path().join("agent.sock");
    start_agent(&socket, &[&loaded]).await;

    let err = connect(&server, &env, &socket).await.unwrap_err();
    assert!(matches!(err, SshError::AuthRejected(AuthMethod::Agent)), "{err}");
    assert_eq!(err.to_string(), "认证失败：服务器拒绝了 ssh-agent 中的所有密钥");
}

#[tokio::test]
async fn empty_agent_is_reported() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let socket = env.dir.path().join("agent.sock");
    start_agent(&socket, &[]).await;

    let err = connect(&server, &env, &socket).await.unwrap_err();
    assert_eq!(err.to_string(), "认证失败：ssh-agent 中没有可用的密钥");
}

#[tokio::test]
async fn missing_agent_socket_is_reported() {
    let env = Env::new();
    let server = TestServer::start(ServerOptions::default()).await;
    let err = connect(&server, &env, &env.dir.path().join("nothing-here.sock")).await.unwrap_err();
    assert!(matches!(&err, SshError::Agent(reason) if reason.starts_with("无法连接 ssh-agent")), "{err}");
}
