//! `-R`: a port on the server that leads to a target on this machine.

use std::time::Duration;

use shellmate_forward::{ForwardKind, RuleError, RuleState};

use crate::rig::*;
use crate::support::{ServerOptions, eventually, lock};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_forward_round_trips_and_reports_the_port_the_server_chose() {
    let rig = Rig::start().await;
    let rule = rig.remote_rule();
    assert_eq!(rule.listen.port, 0);
    let started = rig.forwarder.start(rule.clone());
    assert!(matches!(started, RuleState::Starting), "the server has not answered yet: {started:?}");

    let port = rig.running_port(&rule.id).await;
    assert_ne!(port, 0, "the actual port is shown, not the 0 that was asked for");
    assert_eq!(lock(&rig.server.state.remote_forwards).as_slice(), [("127.0.0.1".to_string(), 0)]);

    let message = b"from the server's side";
    assert_eq!(echo_through(port, message).await, message);
    assert_eq!(rig.echo.accepted(), 1, "the local target was connected");
    eventually("the connection to be done", || (rig.forwarder.stats(&rule.id).active == 0).then_some(())).await;
    let stats = rig.forwarder.stats(&rule.id);
    assert_eq!((stats.up, stats.down), (message.len() as u64, message.len() as u64));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_forward_on_a_given_port_keeps_it() {
    let rig = Rig::start().await;
    let wanted = free_port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{wanted}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    assert_eq!(rig.running_port(&rule.id).await, wanted);
    assert_eq!(echo_through(wanted, b"given").await, b"given");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_connections_through_a_remote_forward() {
    let rig = Rig::start().await;
    let rule = rig.remote_rule();
    rig.forwarder.start(rule.clone());
    let port = rig.running_port(&rule.id).await;
    let clients: Vec<_> = (0..16u64)
        .map(|n| {
            tokio::spawn(async move {
                let data = pattern(20_000 + n as usize, n + 40);
                (checksum(&data), checksum(&echo_through(port, &data).await))
            })
        })
        .collect();
    for client in clients {
        let (sent, got) = client.await.unwrap();
        assert_eq!(sent, got);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_refuses_remote_forwards_gives_a_clear_failure() {
    let rig = Rig::with(ServerOptions { forwarding: true, ..Default::default() }).await;
    let rule = rig.remote_rule();
    rig.forwarder.start(rule.clone());
    let state = eventually("the refusal", || match rig.forwarder.state(&rule.id) {
        RuleState::Failed(err) => Some(err),
        _ => None,
    })
    .await;
    assert_eq!(state, RuleError::RemoteDenied);
    assert_eq!(state.to_string(), "服务器拒绝了远程转发（可能禁用了 AllowTcpForwarding）");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_remote_forward_tells_the_server_to_stop_listening() {
    let rig = Rig::start().await;
    let rule = rig.remote_rule();
    rig.forwarder.start(rule.clone());
    let port = rig.running_port(&rule.id).await;

    assert!(rig.forwarder.stop(&rule.id));
    let cancelled = eventually("the cancel request", || {
        let cancelled = lock(&rig.server.state.cancelled_forwards).clone();
        (!cancelled.is_empty()).then_some(cancelled)
    })
    .await;
    assert_eq!(cancelled, [("127.0.0.1".to_string(), u32::from(port))]);
    eventually("the server's port to close", || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err().then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_target_that_is_down_closes_the_forwarded_connection_and_the_rule_carries_on() {
    let rig = Rig::start().await;
    let rule = rule(ForwardKind::Remote, "127.0.0.1:0", &format!("127.0.0.1:{}", free_port()));
    rig.forwarder.start(rule.clone());
    let port = rig.running_port(&rule.id).await;
    expect_closed(&mut dial(port).await).await;
    expect_closed(&mut dial(port).await).await;
    assert!(rig.forwarder.state(&rule.id).is_running());
}

/// The server still holds the port for a moment (the connection that had it is being closed, or the
/// rule was just switched off): the rule is asked again instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_port_the_server_still_holds_is_asked_for_again() {
    let rig = Rig::start().await;
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{port}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::Starting, "still asking, not failed");
    drop(holder);
    assert_eq!(rig.running_port(&rule.id).await, port);
    assert_eq!(echo_through(port, b"through the retried forward").await, b"through the retried forward");
    let states = states_of(&drain(&rig.events), &rule.id);
    assert_eq!(states, [RuleState::Starting, RuleState::Running { port }], "the refusals were not reported");
}

/// A refusal that goes on is still reported, after the retries (and with a port named; port 0 fails at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_port_the_server_keeps_refusing_fails_after_the_retries() {
    let rig = Rig::with(ServerOptions { forwarding: true, ..Default::default() }).await;
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{}", free_port()), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    let state = eventually("the refusal", || match rig.forwarder.state(&rule.id) {
        RuleState::Failed(err) => Some(err),
        _ => None,
    })
    .await;
    assert_eq!(state, RuleError::RemoteDenied);
}
