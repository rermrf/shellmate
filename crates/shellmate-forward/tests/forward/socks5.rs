//! `-D`: a SOCKS5 proxy on a local port; the client names the target.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use shellmate_forward::RuleState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::rig::*;
use crate::support::{EchoServer, ServerOptions, lock};

/// The address part of a SOCKS5 request.
pub enum Dest {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Domain(&'static str),
}

impl Dest {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Dest::V4(ip) => [&[1][..], &ip.octets()].concat(),
            Dest::V6(ip) => [&[4][..], &ip.octets()].concat(),
            Dest::Domain(name) => [&[3, name.len() as u8][..], name.as_bytes()].concat(),
        }
    }
}

/// Connects to the proxy and does the greeting (no authentication).
pub async fn greet(port: u16) -> TcpStream {
    let mut client = dial(port).await;
    client.write_all(&[5, 1, 0]).await.unwrap();
    let mut answer = [0u8; 2];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(answer, [5, 0]);
    client
}

/// Sends a request and returns the reply code.
pub async fn ask(client: &mut TcpStream, command: u8, dest: &Dest, port: u16) -> u8 {
    let mut request = vec![5, command, 0];
    request.extend(dest.bytes());
    request.extend(port.to_be_bytes());
    client.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    tokio::time::timeout(Duration::from_secs(10), client.read_exact(&mut reply)).await.expect("a reply").unwrap();
    assert_eq!(&reply[..1], &[5], "version");
    assert_eq!(reply[2], 0, "reserved");
    assert_eq!(reply[3], 1, "the bound address is an IPv4 one");
    reply[1]
}

async fn proxy(rig: &Rig) -> u16 {
    let rule = rig.socks_rule();
    let port = rule.listen.port;
    assert!(rig.forwarder.start(rule).is_running());
    port
}

pub async fn echo_over(client: &mut TcpStream, bytes: &[u8]) {
    client.write_all(bytes).await.unwrap();
    client.shutdown().await.unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).await.unwrap();
    assert_eq!(back, bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_to_an_ipv4_address() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0);
    echo_over(&mut client, b"socks over ssh").await;
    assert_eq!(lock(&rig.server.state.forwards).as_slice(), [("127.0.0.1".to_string(), u32::from(rig.echo.port))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_to_an_ipv6_address() {
    let Some(echo) = EchoServer::start_v6().await else {
        eprintln!("no IPv6 loopback on this machine, skipping");
        return;
    };
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V6(Ipv6Addr::LOCALHOST), echo.port).await, 0);
    echo_over(&mut client, b"v6 over ssh").await;
    assert_eq!(lock(&rig.server.state.forwards).as_slice(), [("::1".to_string(), u32::from(echo.port))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_domain_is_resolved_by_the_server_not_here() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::Domain("localhost"), rig.echo.port).await, 0);
    echo_over(&mut client, b"by name").await;
    assert_eq!(
        lock(&rig.server.state.forwards).as_slice(),
        [("localhost".to_string(), u32::from(rig.echo.port))],
        "the name itself went to the server"
    );

    // A name nobody can resolve is the server's problem to report.
    let mut client = greet(port).await;
    assert_ne!(ask(&mut client, 1, &Dest::Domain("no-such-host.invalid"), 80).await, 0);
    assert_eq!(lock(&rig.server.state.forwards).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_sent_right_behind_the_request_is_not_lost() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = dial(port).await;
    let mut all = vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1];
    all.extend(rig.echo.port.to_be_bytes());
    all.extend(b"eager client");
    client.write_all(&all).await.unwrap();
    client.shutdown().await.unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).await.unwrap();
    // Greeting answer, request reply, then the echo of the data sent after the request.
    assert_eq!(&back[..2], &[5, 0]);
    assert_eq!(&back[2..4], &[5, 0]);
    assert_eq!(&back[12..], b"eager client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_and_udp_associate_are_not_supported() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    for command in [2u8, 3] {
        let mut client = greet(port).await;
        assert_eq!(
            ask(&mut client, command, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await,
            0x07,
            "command {command}"
        );
        expect_closed(&mut client).await;
    }
    assert!(lock(&rig.server.state.forwards).is_empty(), "nothing was asked of the server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks4_and_other_protocols_are_closed_without_an_answer() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut socks4 = dial(port).await;
    let mut request = vec![4, 1];
    request.extend(rig.echo.port.to_be_bytes());
    request.extend([127, 0, 0, 1, 0]);
    socks4.write_all(&request).await.unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), socks4.read_to_end(&mut got)).await.expect("closed");
    assert!(got.is_empty(), "no answer: {got:?}");

    let mut http = dial(port).await;
    http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), http.read_to_end(&mut got)).await.expect("closed");
    assert!(got.is_empty());
    assert!(lock(&rig.server.state.forwards).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_insists_on_authentication_is_turned_away() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = dial(port).await;
    client.write_all(&[5, 1, 2]).await.unwrap();
    let mut answer = [0u8; 2];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(answer, [5, 0xFF]);
    expect_closed(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_that_refuses_is_answered_with_connection_refused() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), free_port()).await, 0x05);
    expect_closed(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_forbids_forwarding_is_answered_with_not_allowed() {
    let rig = Rig::with(ServerOptions { forwarding: false, ..Default::default() }).await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0x02);
    expect_closed(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_that_never_answers_is_answered_with_host_unreachable() {
    let rig =
        Rig::with_watchdog(ServerOptions { stall_direct_tcpip: true, ..forwarding() }, Duration::from_millis(400))
            .await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0x04);
    expect_closed(&mut client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_zero_is_a_general_failure() {
    let rig = Rig::start().await;
    let port = proxy(&rig).await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), 0).await, 0x01);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_is_counted_like_any_rule() {
    let rig = Rig::start().await;
    let rule = rig.socks_rule();
    let port = rule.listen.port;
    assert!(matches!(rig.forwarder.start(rule.clone()), RuleState::Running { port: p } if p == port));
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0);
    assert_eq!(rig.forwarder.stats(&rule.id).active, 1);
    echo_over(&mut client, &[9u8; 4096]).await;
    crate::support::eventually("the connection to end", || (rig.forwarder.stats(&rule.id).active == 0).then_some(()))
        .await;
    let stats = rig.forwarder.stats(&rule.id);
    assert_eq!((stats.up, stats.down), (4096, 4096), "the SOCKS exchange itself is not counted");
}
