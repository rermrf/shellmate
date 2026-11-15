use std::sync::atomic::Ordering;
use std::time::Duration;

use shellmate_monitor::{ActionOutcome, MonitorEvent};

use crate::fake::*;
use crate::support::{Env, lock};

async fn report_of(
    events: &async_channel::Receiver<MonitorEvent>,
    id: shellmate_monitor::ActionId,
) -> shellmate_monitor::ActionReport {
    next_event(events, "the report of an action", |e| match e {
        MonitorEvent::Action(report) if report.id == id => Some(report.clone()),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn an_action_reports_what_was_run_and_how_it_went() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);

    // No collector is running (nobody looks): actions work all the same.
    let ok = monitor.run_action("kill -TERM 2207");
    let done = report_of(&events, ok).await;
    assert_eq!(done.command, "kill -TERM 2207");
    assert_eq!(done.outcome, ActionOutcome::Success { output: "done".into() });

    let failing = monitor.run_action("kill -KILL 1 # fail");
    let failed = report_of(&events, failing).await;
    assert_eq!(failed.outcome, ActionOutcome::Failure { code: Some(2), message: "boom: permission denied".into() });

    assert_ne!(ok, failing, "every action has its own id");
    assert_eq!(*lock(&fake.actions), ["kill -TERM 2207", "kill -KILL 1 # fail"]);
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_action_without_a_connection_or_over_time_is_an_error_not_a_hang() {
    let (server, _fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    let none = monitor.run_action("kill -TERM 1");
    assert_eq!(report_of(&events, none).await.outcome, ActionOutcome::Error("没有可用的 SSH 连接".into()));

    monitor.connection_restored(&connection);
    let slow = monitor.run_action("sleep 1000 # hang");
    let ActionOutcome::Error(why) = report_of(&events, slow).await.outcome else { panic!("expected an error") };
    assert!(why.contains("命令执行超时"), "{why}");
}

#[tokio::test]
async fn an_action_that_is_still_running_does_not_keep_the_connection_open_after_shutdown() {
    let (server, _fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let options = shellmate_monitor::Options { action_timeout: Duration::from_secs(60), ..quick() };
    let (monitor, _events) = monitor(options);
    monitor.connection_restored(&connection);
    monitor.run_action("sleep 1000 # hang");
    until("the command to start", || lock(&server.state.execs).len() == 1).await;
    monitor.shutdown();
    drop(connection);
    until("the SSH connection to close", || *lock(&server.state.open_connections) == 0).await;
}
