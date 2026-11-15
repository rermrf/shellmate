//! A remote port the server granted before and now refuses: after a network drop the old session may
//! keep a fixed port for hours, so the rule waits for it instead of failing for good.

use std::net::TcpListener;
use std::time::Duration;

use shellmate_forward::{ForwardKind, ForwardRule, Forwarder, GrantMemory, Options, RetryTiming, RuleError, RuleState};

use crate::rig::*;
use crate::support::{eventually, lock};

/// Short times, so that a few rounds of waiting fit in a test.
fn quick() -> Options {
    Options {
        retry: RetryTiming {
            quick_retries: 2,
            quick_delay: Duration::from_millis(20),
            held_delays: vec![Duration::from_millis(60), Duration::from_millis(120)],
        },
        ..Options::default()
    }
}

fn requests(rig: &Rig) -> usize {
    lock(&rig.server.state.remote_forwards).len()
}

async fn wait_held(rig: &Rig, rule: &ForwardRule) {
    eventually("the held state", || {
        matches!(rig.forwarder.state(&rule.id), RuleState::RemotePortHeld { .. }).then_some(())
    })
    .await;
}

/// Something else takes `port` on the server's side once the old session let go of it.
async fn take_port(port: u16) -> TcpListener {
    eventually("the port to be free", || TcpListener::bind(("127.0.0.1", port)).ok()).await
}

/// A rig whose rule on a fixed remote port was granted, then lost its connection; the port is taken by
/// somebody else and the connection is back: the rule is waiting for the port.
async fn held_rule(options: Options) -> (Rig, ForwardRule, TcpListener, u16) {
    let mut rig = Rig::with_forwarder(options).await;
    let port = free_port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{port}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    assert_eq!(rig.running_port(&rule.id).await, port);
    rig.cut().await;
    let holder = take_port(port).await;
    rig.reconnect().await;
    wait_held(&rig, &rule).await;
    (rig, rule, holder, port)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_that_was_granted_and_is_refused_after_a_reconnect_is_held_until_it_is_free() {
    let (rig, rule, holder, port) = held_rule(quick()).await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::RemotePortHeld { port });
    assert_eq!(rig.forwarder.running_rules(), 0);

    // It keeps asking.
    let before = requests(&rig);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(requests(&rig) >= before + 3, "asked again and again: {} -> {}", before, requests(&rig));
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::RemotePortHeld { port }, "and says so all along");

    // The server lets go of the port: plain running again, and it carries traffic.
    drop(holder);
    assert_eq!(rig.running_port(&rule.id).await, port);
    assert_eq!(echo_through(port, b"after the wait").await, b"after the wait");
    let asked = requests(&rig);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&rig), asked, "no more asking once it runs");

    let states = states_of(&drain(&rig.events), &rule.id);
    assert_eq!(
        states,
        [
            RuleState::Starting,
            RuleState::Running { port },
            RuleState::WaitingReconnect,
            RuleState::Starting,
            RuleState::RemotePortHeld { port },
            RuleState::Running { port },
        ],
        "the quiet retries and the waiting are one state, and there is no failure on the way"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_rule_that_waits_ends_the_asking() {
    let (rig, rule, holder, _port) = held_rule(quick()).await;
    assert!(rig.forwarder.stop(&rule.id));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let asked = requests(&rig);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(requests(&rig), asked, "nobody asks any more");
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::Stopped);
    drop(holder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_down_ends_the_asking_too() {
    let (rig, _rule, holder, _port) = held_rule(quick()).await;
    rig.forwarder.shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let asked = requests(&rig);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(requests(&rig), asked);
    drop(holder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_connection_ends_the_asking_and_the_restore_starts_it_again() {
    let (mut rig, rule, holder, port) = held_rule(quick()).await;
    rig.cut().await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::WaitingReconnect);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let asked = requests(&rig);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(requests(&rig), asked, "nothing to ask without a connection");

    // Back on a new connection the port is still taken: it waits again; once it is free it runs.
    rig.reconnect().await;
    wait_held(&rig, &rule).await;
    drop(holder);
    assert_eq!(rig.running_port(&rule.id).await, port);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_asks_at_once_instead_of_waiting_for_the_next_turn() {
    // A long pause between the rounds: only the button can be quick.
    let slow = Options {
        retry: RetryTiming { held_delays: vec![Duration::from_secs(300)], ..quick().retry },
        ..Options::default()
    };
    let (rig, rule, holder, port) = held_rule(slow).await;
    drop(holder);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::RemotePortHeld { port }, "it waits for its turn");

    assert!(rig.forwarder.retry(&rule.id));
    assert_eq!(rig.running_port(&rule.id).await, port);
    let states = states_of(&drain(&rig.events), &rule.id);
    assert_eq!(
        &states[states.len() - 2..],
        [RuleState::RemotePortHeld { port }, RuleState::Running { port }],
        "no 'starting' in between: the card does not flicker"
    );
    assert!(!rig.forwarder.retry(&rule.id), "a rule that runs is not asked again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pressed_retry_that_is_refused_again_goes_on_waiting() {
    let (rig, rule, holder, port) = held_rule(quick()).await;
    assert!(rig.forwarder.retry(&rule.id));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.forwarder.state(&rule.id), RuleState::RemotePortHeld { port });
    drop(holder);
    assert_eq!(rig.running_port(&rule.id).await, port);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_never_granted_fails_as_before_and_retry_is_for_the_button() {
    let rig = Rig::with_forwarder(quick()).await;
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{port}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    let error = eventually("the failure", || match rig.forwarder.state(&rule.id) {
        RuleState::Failed(error) => Some(error),
        _ => None,
    })
    .await;
    assert_eq!(error, RuleError::RemoteDenied, "never worked here: the text about AllowTcpForwarding stays");

    // Refused again: still failed. Then the port is free and the button works.
    assert!(rig.forwarder.retry(&rule.id));
    eventually("the second failure", || {
        matches!(rig.forwarder.state(&rule.id), RuleState::Failed(RuleError::RemoteDenied)).then_some(())
    })
    .await;
    drop(holder);
    assert!(rig.forwarder.retry(&rule.id));
    assert_eq!(rig.running_port(&rule.id).await, port);
    let states = states_of(&drain(&rig.events), &rule.id);
    assert!(!states.iter().any(|state| matches!(state, RuleState::RemotePortHeld { .. })), "{states:?}");
}

/// The rules of a host move from one connection to another: what the server granted is still known.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_the_server_granted_is_known_to_the_next_forwarder_of_the_host_when_they_share_a_memory() {
    let rig = Rig::with_forwarder(quick()).await;
    let port = free_port();
    let rule = rule(ForwardKind::Remote, &format!("127.0.0.1:{port}"), &rig.echo_target());
    rig.forwarder.start(rule.clone());
    assert_eq!(rig.running_port(&rule.id).await, port);
    let shared: GrantMemory = rig.forwarder.grants();

    // The first connection goes (its forwarder with it); somebody takes the port.
    rig.cut().await;
    rig.forwarder.shutdown();
    let holder = take_port(port).await;
    let connection = rig.new_connection().await;

    let next = |memory: GrantMemory| {
        let (events, _) = async_channel::unbounded();
        let forwarder =
            Forwarder::with_options(tokio::runtime::Handle::current(), events, Options { grants: memory, ..quick() });
        forwarder.connection_restored(&connection);
        forwarder
    };
    let forgetful = next(GrantMemory::new());
    forgetful.start(rule.clone());
    eventually("a plain failure", || {
        matches!(forgetful.state(&rule.id), RuleState::Failed(RuleError::RemoteDenied)).then_some(())
    })
    .await;

    let informed = next(shared);
    informed.start(rule.clone());
    eventually("the waiting", || matches!(informed.state(&rule.id), RuleState::RemotePortHeld { .. }).then_some(()))
        .await;
    drop(holder);
    eventually("running", || informed.state(&rule.id).is_running().then_some(())).await;
}
