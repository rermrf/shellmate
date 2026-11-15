//! Rules over time: losing the connection and getting it back, taken ports, replacing and
//! stopping rules, shutting down without leaking the SSH connection.

use std::net::Ipv4Addr;
use std::time::Duration;

use shellmate_forward::{ForwardEvent, ForwardKind, Forwarder, RuleError, RuleState};
use tokio::io::AsyncWriteExt;

use crate::rig::*;
use crate::socks5::{Dest, ask, echo_over, greet};
use crate::support::{eventually, lock};

fn port_is_taken(port: u16) -> bool {
    matches!(std::net::TcpListener::bind(("127.0.0.1", port)), Err(err) if err.kind() == std::io::ErrorKind::AddrInUse)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_forward_holds_its_port_and_refuses_new_connections_while_the_connection_is_down() {
    let mut rig = Rig::start().await;
    let rule = rig.local_rule();
    let port = rule.listen.port;
    assert!(rig.forwarder.start(rule.clone()).is_running());
    assert_eq!(echo_through(port, b"before").await, b"before");

    // A connection that is open when the transport dies ends with it.
    let mut open = dial(port).await;
    open.write_all(b"x").await.unwrap();
    eventually("it to be forwarded", || (rig.forwarder.stats(&rule.id).active == 1).then_some(())).await;

    rig.cut().await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::WaitingReconnect);
    expect_closed(&mut open).await;
    eventually("the forwarded connection to be gone", || (rig.forwarder.stats(&rule.id).active == 0).then_some(()))
        .await;
    assert!(port_is_taken(port), "nobody else can take the port in the meantime");
    for _ in 0..3 {
        expect_closed(&mut dial(port).await).await;
    }
    assert_eq!(rig.forwarder.running_rules(), 0);

    rig.reconnect().await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::Running { port });
    assert_eq!(echo_through(port, b"after").await, b"after");
    assert_eq!(rig.forwarder.running_rules(), 1);

    let states = states_of(&drain(&rig.events), &rule.id);
    assert_eq!(
        states,
        [RuleState::Running { port }, RuleState::WaitingReconnect, RuleState::Running { port }],
        "the UI hears about every change once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socks_proxy_survives_a_reconnect_like_a_local_forward() {
    let mut rig = Rig::start().await;
    let rule = rig.socks_rule();
    let port = rule.listen.port;
    assert!(rig.forwarder.start(rule.clone()).is_running());
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0);
    echo_over(&mut client, b"one").await;

    rig.cut().await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::WaitingReconnect);
    assert!(port_is_taken(port));
    // The proxy does not even start a conversation.
    let mut client = dial(port).await;
    client.write_all(&[5, 1, 0]).await.unwrap();
    expect_closed(&mut client).await;

    rig.reconnect().await;
    let mut client = greet(port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0);
    echo_over(&mut client, b"two").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_forward_is_requested_again_after_the_reconnect() {
    let mut rig = Rig::start().await;
    let rule = rig.remote_rule();
    rig.forwarder.start(rule.clone());
    let first = rig.running_port(&rule.id).await;
    assert_eq!(echo_through(first, b"before").await, b"before");

    rig.cut().await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::WaitingReconnect);
    eventually("the server's listener to go with the connection", || {
        std::net::TcpStream::connect(("127.0.0.1", first)).is_err().then_some(())
    })
    .await;

    rig.reconnect().await;
    let second = rig.running_port(&rule.id).await;
    assert_eq!(lock(&rig.server.state.remote_forwards).len(), 2, "asked for again");
    assert_eq!(echo_through(second, b"after").await, b"after");
    let states = states_of(&drain(&rig.events), &rule.id);
    assert_eq!(
        states,
        [
            RuleState::Starting,
            RuleState::Running { port: first },
            RuleState::WaitingReconnect,
            RuleState::Starting,
            RuleState::Running { port: second },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_forward_on_a_fixed_port_gets_the_same_port_back() {
    let mut rig = Rig::start().await;
    let wanted = free_port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{wanted}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    assert_eq!(rig.running_port(&rule.id).await, wanted);
    rig.cut().await;
    rig.reconnect().await;
    assert_eq!(rig.running_port(&rule.id).await, wanted);
    assert_eq!(echo_through(wanted, b"same port").await, b"same port");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_started_without_a_connection_wait_for_one() {
    let mut rig = Rig::start().await;
    rig.cut().await;
    let local = rig.local_rule();
    let remote = rig.remote_rule();
    assert_eq!(rig.forwarder.start(local.clone()), RuleState::WaitingReconnect);
    assert_eq!(rig.forwarder.start(remote.clone()), RuleState::WaitingReconnect);
    assert!(port_is_taken(local.listen.port), "the local port is held from the start");

    rig.reconnect().await;
    assert!(rig.forwarder.state(&local.id).is_running());
    let port = rig.running_port(&remote.id).await;
    assert_eq!(echo_through(local.listen.port, b"l").await, b"l");
    assert_eq!(echo_through(port, b"r").await, b"r");
}

/// Takes a port in the 21000–29999 range on `host` (below the ports the system hands out at random,
/// so that other tests cannot grab the ones around it) whose "ten thousand above" neighbours are
/// free on both loopbacks.
fn occupy_a_quiet_port(host: &str) -> (u16, std::net::TcpListener) {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let start = 21_000 + (std::process::id() % 4_000) as u16 + NEXT.fetch_add(37, std::sync::atomic::Ordering::SeqCst);
    for port in (start..30_000).chain(21_000..start) {
        let free = |p: u16| ["127.0.0.1", "::1"].iter().all(|host| std::net::TcpListener::bind((*host, p)).is_ok());
        if free(port + 10_000)
            && free(port + 10_001)
            && let Ok(listener) = std::net::TcpListener::bind((host, port))
        {
            return (port, listener);
        }
    }
    panic!("no quiet port found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_port_is_reported_with_a_free_port_to_use_instead() {
    let rig = Rig::start().await;
    let (taken, occupant) = occupy_a_quiet_port("127.0.0.1");
    let rule = rule(ForwardKind::Local, &format!("127.0.0.1:{taken}"), &rig.echo_target());

    let state = rig.forwarder.start(rule.clone());
    let RuleState::Failed(error) = state.clone() else { panic!("{state:?}") };
    assert_eq!(error.to_string(), format!("本地端口 {taken} 已被占用"));
    assert_eq!(error.suggested_port(), Some(taken + 10_000), "the first free port from ten thousand above");
    assert_eq!(rig.forwarder.state(&rule.id), state);
    assert_eq!(rig.forwarder.views()[0].state, state);
    assert!(matches!(error, RuleError::PortInUse { port, .. } if port == taken));

    // With that one taken as well, the next one is offered.
    let also_taken = std::net::TcpListener::bind(("127.0.0.1", taken + 10_000)).unwrap();
    let RuleState::Failed(error) = rig.forwarder.start(rule.clone()) else { panic!("still taken") };
    assert_eq!(error.suggested_port(), Some(taken + 10_001));
    drop(also_taken);

    // "换用 M 重试": the rule with the new port starts.
    let mut retry = rule.clone();
    retry.listen.port = taken + 10_000;
    assert_eq!(rig.forwarder.start(retry.clone()), RuleState::Running { port: taken + 10_000 });
    assert_eq!(echo_through(taken + 10_000, b"moved").await, b"moved");
    drop(occupant);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_a_rule_again_replaces_it_and_frees_its_old_port() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    let old = rule.listen.port;
    assert!(rig.forwarder.start(rule.clone()).is_running());
    let mut moved = rule.clone();
    moved.listen.port = free_port();
    assert!(rig.forwarder.start(moved.clone()).is_running());
    assert_eq!(rig.forwarder.views().len(), 1);
    eventually("the old port to be free", || (!port_is_taken(old)).then_some(())).await;
    assert_eq!(echo_through(moved.listen.port, b"new").await, b"new");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_summary_counts_running_rules_and_open_connections() {
    let rig = Rig::start().await;
    let (a, b) = (rig.local_rule(), rig.socks_rule());
    rig.forwarder.start(a.clone());
    rig.forwarder.start(b.clone());
    let off = rule(ForwardKind::Local, &format!("127.0.0.1:{}", crate::support::closed_port()), &rig.echo_target());
    rig.forwarder.start(off.clone());
    rig.forwarder.stop(&off.id);
    assert_eq!((rig.forwarder.running_rules(), rig.forwarder.active_connections()), (2, 0));

    let mut one = dial(a.listen.port).await;
    let mut two = dial(a.listen.port).await;
    one.write_all(b"1").await.unwrap();
    two.write_all(b"2").await.unwrap();
    eventually("two connections", || (rig.forwarder.active_connections() == 2).then_some(())).await;
    let views = rig.forwarder.views();
    assert_eq!(views.iter().map(|v| v.rule.id.clone()).collect::<Vec<_>>(), [a.id.clone(), b.id.clone()]);
    assert_eq!(views[0].stats.active, 2);
}

/// Leaves a rule of each kind running with a forwarded connection open on every one.
async fn busy_rules(rig: &Rig) -> Vec<tokio::net::TcpStream> {
    let (local, socks, remote) = (rig.local_rule(), rig.socks_rule(), rig.remote_rule());
    for rule in [&local, &socks, &remote] {
        rig.forwarder.start(rule.clone());
    }
    let remote_port = rig.running_port(&remote.id).await;
    let mut held = Vec::new();

    let mut client = dial(local.listen.port).await;
    client.write_all(b"local").await.unwrap();
    held.push(client);

    let mut client = greet(socks.listen.port).await;
    assert_eq!(ask(&mut client, 1, &Dest::V4(Ipv4Addr::LOCALHOST), rig.echo.port).await, 0);
    client.write_all(b"socks").await.unwrap();
    held.push(client);

    let mut client = dial(remote_port).await;
    client.write_all(b"remote").await.unwrap();
    held.push(client);

    eventually("three forwarded connections", || (rig.forwarder.active_connections() == 3).then_some(())).await;
    held
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_shutdown_and_the_last_connection_handle_the_ssh_connection_closes_even_with_active_forwards() {
    let rig = Rig::start().await;
    let mut held = busy_rules(&rig).await;
    let ports: Vec<u16> = rig.forwarder.views().iter().map(|view| view.rule.listen.port).collect();
    assert_eq!(rig.open_connections(), 1);

    let Rig { server, forwarder, connection, .. } = rig;
    forwarder.shutdown();
    drop(connection);
    eventually("the SSH connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
    for client in &mut held {
        expect_closed(client).await;
    }
    for port in ports.iter().take(2) {
        eventually("the local port to close", || {
            std::net::TcpStream::connect(("127.0.0.1", *port)).is_err().then_some(())
        })
        .await;
    }
    assert_eq!(forwarder.views().len(), 0);
    assert_eq!(forwarder.start(crate::rig::rule(ForwardKind::Dynamic, "127.0.0.1:1080", "")), RuleState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_alone_do_not_keep_an_idle_connection_open() {
    let rig = Rig::start().await;
    for rule in [rig.local_rule(), rig.socks_rule(), rig.remote_rule()] {
        rig.forwarder.start(rule);
    }
    let Rig { server, forwarder, connection, .. } = rig;
    drop(connection);
    eventually("the idle SSH connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
    drop(forwarder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_forwarder_stops_everything_too() {
    let rig = Rig::start().await;
    let mut held = busy_rules(&rig).await;
    let Rig { server, forwarder, connection, .. } = rig;
    let clone = forwarder.clone();
    drop(forwarder);
    assert_eq!(clone.views().len(), 3, "a clone is the same forwarder");
    drop(clone);
    drop(connection);
    eventually("the SSH connection to close", || (*lock(&server.state.open_connections) == 0).then_some(())).await;
    for client in &mut held {
        expect_closed(client).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_comes_back_to_life_after_shutdown() {
    let mut rig = Rig::start().await;
    let rule = rig.local_rule();
    rig.forwarder.start(rule.clone());
    rig.forwarder.shutdown();
    rig.forwarder.shutdown();
    let connection = rig.new_connection().await;
    rig.forwarder.connection_restored(&connection);
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::Stopped);
    assert_eq!(rig.forwarder.start(rule.clone()), RuleState::Stopped);
    rig.connection = connection;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!port_is_taken(rule.listen.port));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_arrive_in_order_and_stopping_is_one_of_them() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    rig.forwarder.start(rule.clone());
    rig.forwarder.stop(&rule.id);
    let events = drain(&rig.events);
    assert_eq!(
        events,
        [
            ForwardEvent::State { id: rule.id.clone(), state: RuleState::Running { port: rule.listen.port } },
            ForwardEvent::State { id: rule.id.clone(), state: RuleState::Stopped },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forwarder_without_a_listener_for_its_events_still_works() {
    let rig = Rig::start().await;
    let (tx, rx) = async_channel::unbounded();
    let forwarder = Forwarder::with_stats_interval(tokio::runtime::Handle::current(), tx, Duration::from_millis(50));
    forwarder.connection_restored(&rig.connection);
    drop(rx);
    let rule = rig.local_rule();
    assert!(forwarder.start(rule.clone()).is_running());
    assert_eq!(echo_through(rule.listen.port, b"quiet").await, b"quiet");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_without_its_target_does_not_start() {
    let rig = Rig::start().await;
    for kind in [ForwardKind::Local, ForwardKind::Remote] {
        let mut broken = rule(kind, &format!("127.0.0.1:{}", free_port()), "h:1");
        broken.target = None;
        assert_eq!(rig.forwarder.start(broken.clone()), RuleState::Failed(RuleError::MissingTarget));
        assert!(!port_is_taken(broken.listen.port), "nothing was bound");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_through_a_remote_forward_end_when_the_transport_dies_even_if_nobody_closes() {
    let rig = Rig::start().await;
    let rule = rig.remote_rule();
    rig.forwarder.start(rule.clone());
    let port = rig.running_port(&rule.id).await;
    let mut client = dial(port).await;
    client.write_all(b"hold on").await.unwrap();
    eventually("it to be forwarded", || (rig.forwarder.stats(&rule.id).active == 1).then_some(())).await;

    rig.cut().await;
    // Neither the client nor the local target closes anything; the forwarded connection ends anyway.
    eventually("the forwarded connection to be gone", || (rig.forwarder.stats(&rule.id).active == 0).then_some(()))
        .await;
    expect_closed(&mut client).await;
}

fn ipv6_loopback_works() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

/// `localhost` may mean either loopback, so a program that listens on one of them must not be
/// shadowed by a rule on the other: its local clients would end up in the tunnel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listener_on_the_other_loopback_makes_the_port_taken() {
    if !ipv6_loopback_works() {
        eprintln!("no IPv6 loopback on this machine, skipping");
        return;
    }
    let rig = Rig::start().await;
    for (occupied, rule_host, rule_text) in [("127.0.0.1", "::1", "[::1]"), ("::1", "127.0.0.1", "127.0.0.1")] {
        let (port, occupant) = occupy_a_quiet_port(occupied);
        let rule = rule(ForwardKind::Local, &format!("{rule_text}:{port}"), &rig.echo_target());
        let state = rig.forwarder.start(rule.clone());
        let RuleState::Failed(error) = state else {
            panic!("a rule on {rule_host}:{port} started beside {occupied}: {state:?}")
        };
        assert!(
            matches!(&error, RuleError::PortInUse { host, port: p, .. } if host == rule_host && *p == port),
            "{error:?}"
        );
        assert_eq!(error.to_string(), format!("本地端口 {port} 已被占用"));
        assert_eq!(error.suggested_port(), Some(port + 10_000), "the suggestion is free on both loopbacks");

        // With the program gone, the same rule starts.
        drop(occupant);
        assert!(rig.forwarder.start(rule.clone()).is_running());
        assert!(rig.forwarder.stop(&rule.id));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_on_a_port_that_nobody_listens_on_is_not_mistaken_for_a_shadow() {
    let rig = Rig::start().await;
    let rule = rig.local_rule();
    assert!(rig.forwarder.start(rule.clone()).is_running());
    assert!(rig.forwarder.stop(&rule.id));
    // The probes of the previous start left nothing behind.
    assert!(rig.forwarder.start(rule.clone()).is_running());
    assert_eq!(echo_through(rule.listen.port, b"free").await, b"free");
}
