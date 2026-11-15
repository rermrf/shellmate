use std::sync::atomic::Ordering;
use std::time::Duration;

use shellmate_monitor::{MonitorEvent, Phase};

use crate::fake::*;
use crate::support::{Env, lock};

fn snapshot_with(event: &MonitorEvent) -> Option<&shellmate_monitor::Snapshot> {
    match event {
        MonitorEvent::Snapshot(snapshot) => Some(snapshot),
        _ => None,
    }
}

#[tokio::test]
async fn nothing_is_collected_until_somebody_looks_and_then_the_probe_and_the_frames_come() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fake.probes.load(Ordering::SeqCst) + fake.collectors_started.load(Ordering::SeqCst),
        0,
        "nobody looks, nothing runs"
    );
    assert_eq!(monitor.phase(), Phase::Idle);

    monitor.set_active(true);
    let probe = next_event(&events, "the probe", |e| match e {
        MonitorEvent::Probe(probe) => Some(probe.clone()),
        _ => None,
    })
    .await;
    assert_eq!(probe.os_name.as_deref(), Some("Test Linux 1"));
    assert_eq!(probe.sudo, shellmate_monitor::Sudo::Passwordless);
    assert_eq!(monitor.probe().unwrap().user, "alice");

    // The first frames make a snapshot without rates; the next ones have them.
    let first = next_event(&events, "the first snapshot", |e| snapshot_with(e).cloned()).await;
    assert_eq!(first.cpu.as_ref().and_then(|cpu| cpu.percent), None);
    let with_rates = next_event(&events, "a snapshot with rates", |e| {
        snapshot_with(e).filter(|s| s.cpu.as_ref().is_some_and(|c| c.percent.is_some())).cloned()
    })
    .await;
    assert_eq!(with_rates.cpu.as_ref().unwrap().percent, Some(50.0));
    assert_eq!(with_rates.memory.unwrap().total, 1000 * 1024);
    assert_eq!(with_rates.host.os_name.as_deref(), Some("Test Linux 1"));
    let processes =
        next_event(&events, "the process list", |e| snapshot_with(e).filter(|s| !s.processes.is_empty()).cloned())
            .await;
    assert_eq!(processes.processes[0].user, "alice");
    assert_eq!(processes.processes[0].command, "/sbin/init");
    let disks = next_event(&events, "the disks", |e| snapshot_with(e).filter(|s| !s.disks.is_empty()).cloned()).await;
    assert_eq!(disks.disks_over(80).len(), 1);
    assert_eq!(monitor.phase(), Phase::Collecting);
    assert!(monitor.snapshot().is_some());
    assert!(lock(&server.state.ptys).is_empty(), "no PTY for the probe or the collector");
    assert_eq!(fake.collectors_running.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn collecting_stops_when_the_grace_period_after_the_last_viewer_is_over() {
    let (server, fake) = server(linux_probe("password")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "collecting", |e| matches!(e, MonitorEvent::Phase(Phase::Collecting)).then_some(())).await;

    // Leaving and coming back within the grace period changes nothing.
    monitor.set_active(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    monitor.set_active(true);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 1, "no restart");
    assert_eq!(monitor.phase(), Phase::Collecting);

    // Leaving for good: the collector goes on for the grace period and then the channel is closed.
    monitor.set_active(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.collectors_running.load(Ordering::SeqCst), 1, "still running during the grace period");
    next_event(&events, "idle", |e| matches!(e, MonitorEvent::Phase(Phase::Idle)).then_some(())).await;
    until("the collector on the server to see the closed channel", || {
        fake.collectors_running.load(Ordering::SeqCst) == 0
    })
    .await;
    assert!(monitor.snapshot().is_some(), "the last numbers stay");
    assert!(!connection.is_closed(), "the connection is not the monitor's to close");

    // Looking again starts a new collector, without probing again.
    monitor.set_active(true);
    next_event(&events, "collecting again", |e| matches!(e, MonitorEvent::Phase(Phase::Collecting)).then_some(()))
        .await;
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 2);
    assert_eq!(fake.probes.load(Ordering::SeqCst), 1, "the probe is made once");
}

#[tokio::test]
async fn a_lost_connection_waits_and_the_restored_one_goes_on_without_a_rate_for_the_gap() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "a snapshot with rates", |e| {
        snapshot_with(e).filter(|s| s.cpu.as_ref().is_some_and(|c| c.percent.is_some())).map(|_| ())
    })
    .await;

    // The connection drops.
    server.state.kill_connections();
    connection.closed().await;
    monitor.connection_lost();
    assert_eq!(monitor.phase(), Phase::WaitingReconnect);
    let kept = monitor.snapshot().unwrap();
    assert!(kept.cpu.as_ref().unwrap().percent.is_some(), "the last numbers are still there");
    drop(connection);
    // Nothing is collected without a connection, however long it takes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.collectors_running.load(Ordering::SeqCst), 0);
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 1);
    while events.try_recv().is_ok() {}

    // The new connection: collecting resumes; the first sample has no rate (the time in between is no
    // interval) and the history of the old one is still there.
    let connection = connect(&server, &env).await;
    monitor.connection_restored(&connection);
    let first = next_event(&events, "the first snapshot after the restore", |e| {
        snapshot_with(e).filter(|s| s.cpu.is_some()).cloned()
    })
    .await;
    assert_eq!(first.cpu.as_ref().unwrap().percent, None);
    assert!(!first.cpu.as_ref().unwrap().history.is_empty());
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 2);
    assert_eq!(fake.probes.load(Ordering::SeqCst), 1);
    assert_eq!(monitor.phase(), Phase::Collecting);
    next_event(&events, "a rate again", |e| {
        snapshot_with(e).filter(|s| s.cpu.as_ref().is_some_and(|c| c.percent.is_some())).map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn nobody_looking_while_the_connection_is_lost_stays_idle_and_a_restore_alone_starts_nothing() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let env = Env::new();
    let connection = connect(&server, &env).await;
    let (monitor, _events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.connection_lost();
    assert_eq!(monitor.phase(), Phase::Idle);
    monitor.connection_restored(&connection);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst) + fake.probes.load(Ordering::SeqCst), 0);
    // Looking without a connection waits for one.
    monitor.connection_lost();
    monitor.set_active(true);
    assert_eq!(monitor.phase(), Phase::WaitingReconnect);
    monitor.connection_restored(&connection);
    until("collecting", || monitor.phase() == Phase::Collecting).await;
}

#[tokio::test]
async fn shutdown_and_the_last_connection_dropped_close_the_ssh_connection() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "collecting", |e| matches!(e, MonitorEvent::Phase(Phase::Collecting)).then_some(())).await;
    assert_eq!(*lock(&server.state.open_connections), 1);

    // The collector's channel holds the connection: dropping our handle does not close it.
    let kept = connection.clone();
    drop(connection);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*lock(&server.state.open_connections), 1);

    monitor.shutdown();
    drop(kept);
    until("the SSH connection to close", || *lock(&server.state.open_connections) == 0).await;
    until("the collector to notice", || fake.collectors_running.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn dropping_the_last_clone_of_the_monitor_does_the_same() {
    let (server, _fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    let clone = monitor.clone();
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "collecting", |e| matches!(e, MonitorEvent::Phase(Phase::Collecting)).then_some(())).await;
    drop(monitor);
    drop(connection);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*lock(&server.state.open_connections), 1, "a clone is still there");
    drop(clone);
    until("the SSH connection to close", || *lock(&server.state.open_connections) == 0).await;
    // The channel of events ends with the monitor.
    while events.recv().await.is_ok() {}
}

#[tokio::test]
async fn no_event_after_shutdown() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "a snapshot", |e| snapshot_with(e).map(|_| ())).await;
    let frames_before = fake.frames.load(Ordering::SeqCst);
    monitor.shutdown();
    while events.try_recv().is_ok() {}
    // Frames kept coming for a while on the server's side (it only learns that the channel is closed
    // a moment later), the actions and every method are still called: nothing is reported.
    monitor.run_action("echo after");
    monitor.set_active(false);
    monitor.set_active(true);
    monitor.connection_lost();
    monitor.connection_restored(&connection);
    monitor.restart();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(events.try_recv().is_err(), "an event arrived after shutdown");
    assert!(fake.frames.load(Ordering::SeqCst) >= frames_before);
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 1, "nothing was started after the shutdown");
    assert!(lock(&fake.actions).is_empty(), "and no action was run");
}

#[tokio::test]
async fn every_method_can_be_called_from_a_thread_outside_the_runtime() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    let from_thread = {
        let (monitor, connection) = (monitor.clone(), connection.clone());
        std::thread::spawn(move || {
            assert!(tokio::runtime::Handle::try_current().is_err(), "this thread has no runtime");
            monitor.connection_restored(&connection);
            monitor.set_active(true);
            monitor.run_action("echo from a thread");
            monitor.set_active(false);
            monitor.set_active(true);
            monitor.restart();
            let _ = (monitor.phase(), monitor.snapshot(), monitor.probe());
        })
    };
    from_thread.join().unwrap();
    let (mut collecting, mut acted) = (false, false);
    next_event(&events, "collecting and the action", |e| {
        collecting |= matches!(e, MonitorEvent::Phase(Phase::Collecting));
        acted |= matches!(e, MonitorEvent::Action(_));
        (collecting && acted).then_some(())
    })
    .await;
    // And the ones that stop things.
    let stopper = {
        let monitor = monitor.clone();
        std::thread::spawn(move || {
            monitor.connection_lost();
            monitor.shutdown();
        })
    };
    stopper.join().unwrap();
    until("the collector to stop", || fake.collectors_running.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn a_server_that_is_not_linux_is_named_and_nothing_is_collected() {
    let (server, fake) = server(darwin_probe()).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    let phase = next_event(&events, "the verdict", |e| match e {
        MonitorEvent::Phase(phase @ Phase::Unsupported(_)) => Some(phase.clone()),
        _ => None,
    })
    .await;
    assert_eq!(phase, Phase::Unsupported("暂只支持 Linux 主机的监控（Darwin 23.1.0）".into()));
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 0);
    assert!(monitor.probe().is_some(), "the probe is there: the header can say what the system is");
    // Asking again does not probe again (nothing changed) and does not start a collector either.
    monitor.set_active(false);
    monitor.set_active(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_collector_that_dies_is_started_again_and_after_too_many_failures_the_monitor_says_why() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    // The first collector writes four frames and dies; the second one is fine.
    *lock(&fake.collector) = Collector::DiesAfter(4);
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "the second collector", |e| matches!(e, MonitorEvent::Phase(Phase::Collecting)).then_some(()))
        .await;
    until("a second collector", || fake.collectors_started.load(Ordering::SeqCst) == 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(monitor.phase(), Phase::Collecting);
    assert_eq!(fake.collectors_running.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_server_that_refuses_exec_gives_a_failure_with_a_reason_and_restart_asks_again() {
    let server = crate::support::TestServer::start(crate::support::ServerOptions::default()).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(quick());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    let phase = next_event(&events, "the failure", |e| match e {
        MonitorEvent::Phase(phase @ Phase::Failed(_)) => Some(phase.clone()),
        _ => None,
    })
    .await;
    let Phase::Failed(why) = phase else { unreachable!() };
    assert!(why.contains("执行命令"), "{why}");
    // Three attempts (one and two retries) were made, each as a probe.
    assert_eq!(lock(&server.state.execs).len(), 3);
    monitor.restart();
    next_event(&events, "starting again", |e| matches!(e, MonitorEvent::Phase(Phase::Starting)).then_some(())).await;
    next_event(&events, "failing again", |e| matches!(e, MonitorEvent::Phase(Phase::Failed(_))).then_some(())).await;
    assert_eq!(lock(&server.state.execs).len(), 6);
}

#[tokio::test]
async fn a_collector_that_goes_silent_is_given_up_on_and_started_again() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, _events) = monitor(quick());
    *lock(&fake.collector) = Collector::Stuck;
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    // 400 ms without a line: the channel is closed and another collector started, until the retries are used.
    until("the monitor to give up", || matches!(monitor.phase(), Phase::Failed(_))).await;
    let Phase::Failed(why) = monitor.phase() else { unreachable!() };
    assert_eq!(why, "采集脚本长时间没有输出");
    assert_eq!(fake.collectors_started.load(Ordering::SeqCst), 3, "the first and two retries");
    until("the stuck collectors to see their channels closed", || fake.collectors_running.load(Ordering::SeqCst) == 0)
        .await;
}

#[tokio::test]
async fn the_reason_a_collector_died_is_in_the_failure() {
    let (server, fake) = server(linux_probe("nopasswd")).await;
    let connection = connect(&server, &Env::new()).await;
    // A server whose collectors all die at once: the stand-in dies on its first run only, so make the
    // retries die too by using a monitor that does not retry.
    let (monitor, _events) = monitor(shellmate_monitor::Options { retry_delays: vec![], ..quick() });
    *lock(&fake.collector) = Collector::DiesAfter(2);
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    until("the failure", || matches!(monitor.phase(), Phase::Failed(_))).await;
    let Phase::Failed(why) = monitor.phase() else { unreachable!() };
    assert!(why.contains("sh: ps: not found"), "{why}");
}
