//! The services and containers of the monitor against the pretend server: what is asked, when, and how
//! the answers come out. (The real thing is in `e2e.rs`.)

use std::sync::atomic::Ordering;
use std::time::Duration;

use shellmate_monitor::docker::ContainerState;
use shellmate_monitor::services::ActiveState;
use shellmate_monitor::{Focus, FollowEvent, MonitorEvent, Options, Topic};

use crate::fake::*;
use crate::support::{Env, lock};

fn options() -> Options {
    Options {
        poll_visible: Duration::from_millis(150),
        poll_hidden: Duration::from_millis(600),
        debounce: Duration::from_millis(20),
        usage_every: Duration::from_millis(200),
        ..quick()
    }
}

/// A probe: systemd running or not, docker usable (`user`, `sudo`, `denied`) or not there, the groups of the
/// login user.
fn probe(systemd: bool, docker: Option<&str>, groups: &str) -> String {
    let t = TOKEN;
    let mut text = format!(
        "@@{t} os\nPRETTY_NAME=\"Test Linux 1\"\n@@{t} kernel\nLinux 6.1.0\n@@{t} procfs\nyes\n@@{t} uptime\n5000.5\n@@{t} user\n1000\nalice\n@@{t} groups\n{groups}\n@@{t} sudo\nnopasswd\n@@{t} systemd\nsystemd 255\n{}@@{t} containers\n",
        if systemd { "running\n" } else { "" },
    );
    if let Some(access) = docker {
        text += &format!(
            "bin docker /usr/bin/docker\naccess docker {} 29.1.3\n",
            if access == "sudo" {
                "sudo ok"
            } else if access == "user" {
                "user ok"
            } else {
                "user denied"
            }
        );
    }
    text + &format!(
        "@@{t} page\n4096\n@@{t} clk\n100\n@@{t} passwd\nalice:x:1000:1000::/home/alice:/bin/sh\n@@{t} end\n"
    )
}

const UNITS: &str = "\
cron.service loaded active running Regular background program processing daemon
shellmate-broken.service loaded failed failed Shellmate test (broken)
ssh.service loaded active running OpenBSD Secure Shell server
";
const FILES: &str = "\
cron.service enabled enabled
shellmate-broken.service disabled enabled
shellmate-demo.service disabled enabled
ssh.service enabled enabled
";
const SHOW: &str = "\
Id=cron.service
Description=Regular background program processing daemon
ActiveState=active
SubState=running
MainPID=640
MemoryCurrent=2203648

Id=shellmate-broken.service
Description=Shellmate test (broken)
ActiveState=failed
SubState=failed
MainPID=0
MemoryCurrent=[not set]
StateChangeTimestampMonotonic=57000000
Result=exit-code
ExecMainStatus=1

Id=shellmate-demo.service
Description=Shellmate demo
ActiveState=inactive
SubState=dead

Id=ssh.service
Description=OpenBSD Secure Shell server
ActiveState=active
SubState=running
MainPID=700
MemoryCurrent=9000000
";

const PS: &str = r#"{"ID":"aaaa1111","Image":"nginx:alpine","Labels":"com.docker.compose.project=shop","Names":"shop-web-1","Ports":"127.0.0.1:18080->80/tcp","State":"running","Status":"Up 2 hours"}
{"ID":"bbbb2222","Image":"alpine","Names":"oomer","Ports":"","State":"restarting","Status":"Restarting (137) 3 seconds ago"}
"#;
const INSPECT: &str = r#"{"id":"aaaa1111","name":"/shop-web-1","restarts":0,"oom":false,"restarting":false,"memory":0,"labels":{"com.docker.compose.project":"shop"},"nets":{"shop_default":{"IPAddress":"172.18.0.2"}}}
{"id":"bbbb2222","name":"/oomer","restarts":6,"oom":true,"restarting":true,"memory":33554432,"labels":null,"nets":{}}
"#;

#[tokio::test]
async fn services_are_listed_with_their_details_while_the_monitor_collects() {
    let (server, fake) = server(probe(true, None, "alice adm")).await;
    fake.answer("systemctl list-units", UNITS);
    fake.answer("systemctl list-unit-files", FILES);
    fake.answer("systemctl show", SHOW);
    fake.answer(
        "journalctl -u shellmate-broken.service",
        "ERROR: cannot start the broken demo\nFailed to start shellmate-broken.service - Shellmate test (broken).\n",
    );
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(options());
    monitor.connection_restored(&connection);
    assert!(monitor.services().is_none());
    assert_eq!(fake.count("systemctl"), 0, "nobody looks: nothing is asked");

    monitor.set_active(true);
    let list = next_event(&events, "the services", |event| match event {
        MonitorEvent::Services(list) => Some(list.clone()),
        _ => None,
    })
    .await;
    assert_eq!(list.services.len(), 4, "the unit file whose unit is not in memory is listed too");
    assert_eq!(list.services[0].name, "shellmate-broken.service", "failed first");
    assert_eq!(list.services[0].last_error.as_deref(), Some("ERROR: cannot start the broken demo"));
    assert_eq!(list.failed(), 1);
    let cron = list.get("cron.service").unwrap();
    assert_eq!((cron.main_pid, cron.memory, cron.active.clone()), (Some(640), Some(2_203_648), ActiveState::Active));
    assert_eq!(list.get("shellmate-demo.service").unwrap().active, ActiveState::Inactive);
    assert_eq!(monitor.services().unwrap().services.len(), 4, "kept for a page that opens later");
    assert!(!lock(&fake.actions).iter().any(|c| c.contains("docker")), "no docker on this server");

    // The journal is asked about once per failure, not on every round.
    let asked = fake.count("journalctl");
    assert_eq!(asked, 1);

    // Asking again at once, after an action: a changed answer arrives without waiting for the timer.
    fake.answer("systemctl show", &SHOW.replace("MemoryCurrent=2203648", "MemoryCurrent=3000000"));
    monitor.refresh(Topic::Services);
    let changed = next_event(&events, "the changed list", |event| match event {
        MonitorEvent::Services(list) if list.get("cron.service").and_then(|s| s.memory) == Some(3_000_000) => {
            Some(list.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(changed.failed(), 1);
    assert_eq!(fake.count("journalctl"), asked, "the same failure is not looked up again");
}

#[tokio::test]
async fn the_journal_is_not_asked_when_the_user_may_not_read_it() {
    let (server, fake) = server(probe(true, None, "alice").replace("nopasswd", "password")).await;
    fake.answer("systemctl list-units", UNITS);
    fake.answer("systemctl list-unit-files", FILES);
    fake.answer("systemctl show", SHOW);
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(options());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    let list = next_event(&events, "the services", |event| match event {
        MonitorEvent::Services(list) => Some(list.clone()),
        _ => None,
    })
    .await;
    assert_eq!(list.services[0].last_error, None);
    assert_eq!(fake.count("journalctl"), 0);
}

#[tokio::test]
async fn a_server_without_systemd_and_docker_is_asked_neither() {
    let (server, fake) = server(probe(false, None, "alice")).await;
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(options());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "a snapshot", |event| matches!(event, MonitorEvent::Snapshot(_)).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(lock(&fake.actions).is_empty(), "{:?}", lock(&fake.actions));
    assert!(monitor.services().is_none() && monitor.docker().is_none());
}

#[tokio::test]
async fn containers_events_trigger_a_new_look_and_statistics_run_only_on_the_docker_page() {
    let (server, fake) = server(probe(false, Some("user"), "alice docker")).await;
    fake.answer("docker ps", PS);
    fake.answer("docker inspect", INSPECT);
    fake.answer("docker system df", "{\"Size\":\"1.33GB\",\"TotalCount\":\"14\",\"Type\":\"Images\"}\n{\"TotalCount\":\"3\",\"Type\":\"Local Volumes\"}\n");
    fake.answer("docker images", "{\"ID\":\"sha256:1\",\"Size\":\"7.8MB\"}\n");
    fake.stream("docker events");
    fake.stream("docker stats");
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(options());
    monitor.connection_restored(&connection);
    monitor.set_active(true);

    let view = next_event(&events, "the containers", |event| match event {
        MonitorEvent::Docker(view) => Some(view.clone()),
        _ => None,
    })
    .await;
    assert_eq!(view.version.as_deref(), Some("29.1.3"));
    assert_eq!(view.containers.len(), 2);
    let web = view.containers.iter().find(|c| c.name == "shop-web-1").unwrap();
    assert_eq!(
        (web.state.clone(), web.project.as_deref(), web.ip()),
        (ContainerState::Running, Some("shop"), Some("172.18.0.2"))
    );
    let oomer = view.containers.iter().find(|c| c.name == "oomer").unwrap();
    assert!(oomer.is_restart_looping() && oomer.oom_killed && oomer.memory_limit == Some(33_554_432));
    assert_eq!(view.usage.images, None, "the disk usage is only asked while the Docker page is in front");
    assert_eq!(fake.count("docker system df"), 0);
    assert_eq!(fake.count("docker stats"), 0, "no statistics on the overview");
    assert_eq!(fake.open_streams.load(Ordering::SeqCst), 1, "only the events stream");

    // A container event makes the monitor look again; a `docker exec` does not.
    let before = fake.count("docker ps");
    fake.push("docker events", r#"{"status":"exec_start: sh","Action":"exec_start: sh","Type":"container"}"#);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fake.count("docker ps"), before, "an exec session changes nothing");
    fake.push("docker events", r#"{"status":"die","Action":"die","Type":"container"}"#);
    until("a new look after the event", || fake.count("docker ps") > before).await;

    // The Docker page: statistics start, the disk usage comes.
    monitor.set_focus(Focus::Docker);
    until("the statistics stream", || fake.open_streams.load(Ordering::SeqCst) == 2).await;
    // The way docker really writes it: cursor home before the first line of a batch, erase-to-end after every line.
    fake.push("docker stats", "\u{1b}[H{\"CPUPerc\":\"12.50%\",\"MemUsage\":\"20MiB / 32MiB\",\"Name\":\"shop-web-1\",\"ID\":\"aaaa1111\"} \u{1b}[K");
    fake.push(
        "docker stats",
        "{\"CPUPerc\":\"0.00%\",\"MemUsage\":\"1MiB / 32MiB\",\"Name\":\"oomer\",\"ID\":\"bbbb2222\"} \u{1b}[K",
    );
    fake.push("docker stats", " \u{1b}[K");
    fake.push("docker stats", "\u{1b}[J\u{1b}[H{\"CPUPerc\":\"12.50%\",\"MemUsage\":\"20MiB / 32MiB\",\"Name\":\"shop-web-1\",\"ID\":\"aaaa1111\"} \u{1b}[K");
    let with_stats = next_event(&events, "the statistics", |event| match event {
        MonitorEvent::Docker(view) if view.containers.iter().any(|c| c.cpu_percent == Some(12.5)) => Some(view.clone()),
        _ => None,
    })
    .await;
    assert_eq!(with_stats.containers.iter().find(|c| c.name == "shop-web-1").unwrap().mem_used, Some(20 * 1024 * 1024));
    assert_eq!(
        with_stats.containers.iter().find(|c| c.name == "oomer").unwrap().mem_used,
        Some(1024 * 1024),
        "both containers of the batch are there, not one batch per line"
    );
    until("the disk usage", || monitor.docker().is_some_and(|view| view.usage.images == Some(14))).await;
    let with_usage = monitor.docker().unwrap();
    assert_eq!((with_usage.usage.volumes, with_usage.dangling.count), (Some(3), 1));

    // Leaving the page stops the statistics and clears their numbers.
    monitor.set_focus(Focus::Overview);
    until("the statistics stream to close", || fake.open_streams.load(Ordering::SeqCst) == 1).await;
    let cleared = next_event(&events, "the numbers to be cleared", |event| match event {
        MonitorEvent::Docker(view) if view.containers.iter().all(|c| c.cpu_percent.is_none()) => Some(view.clone()),
        _ => None,
    })
    .await;
    assert_eq!(cleared.usage.images, Some(14), "the usage is kept");

    // Nobody looks: everything stops and the connection can close.
    monitor.shutdown();
    until("the streams to close", || fake.open_streams.load(Ordering::SeqCst) == 0).await;
    drop(connection);
    until("the SSH connection to close", || *lock(&server.state.open_connections) == 0).await;
}

#[tokio::test]
async fn a_docker_that_answers_with_an_error_is_reported_and_the_containers_stay() {
    let (server, fake) = server(probe(false, Some("sudo"), "alice")).await;
    fake.answer("sudo -n docker ps", PS);
    fake.answer("sudo -n docker inspect", INSPECT);
    fake.stream("docker events");
    let connection = connect(&server, &Env::new()).await;
    let (monitor, events) = monitor(options());
    monitor.connection_restored(&connection);
    monitor.set_active(true);
    next_event(&events, "the containers", |event| {
        matches!(event, MonitorEvent::Docker(v) if v.containers.len() == 2).then_some(())
    })
    .await;
    assert!(
        fake.count("sudo -n docker") >= 2,
        "docker is run with sudo -n when only that works: {:?}",
        lock(&fake.actions)
    );

    // The next answer is a failure (the daemon went away): the page hears why, and keeps what it had.
    fake.fail(
        "sudo -n docker ps",
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n",
    );
    monitor.refresh(Topic::Docker);
    let broken = next_event(&events, "the error", |event| match event {
        MonitorEvent::Docker(view) if view.error.is_some() => Some(view.clone()),
        _ => None,
    })
    .await;
    assert_eq!(broken.containers.len(), 2, "the containers from before stay");
    assert!(broken.error.as_deref().unwrap().contains("Cannot connect to the Docker daemon"), "{:?}", broken.error);
}

#[tokio::test]
async fn a_followed_command_hands_over_its_lines_and_stops_with_the_follow() {
    let (server, fake) = server(probe(false, None, "alice")).await;
    fake.stream("journalctl -u demo.service");
    let connection = connect(&server, &Env::new()).await;
    let (monitor, _events) = monitor(options());

    // Without a connection it ends at once, with a reason.
    let nothing = monitor.follow("journalctl -u demo.service -f");
    assert_eq!(nothing.events.recv().await.unwrap(), FollowEvent::Ended("没有可用的 SSH 连接".into()));

    monitor.connection_restored(&connection);
    let follow = monitor.follow("journalctl -u demo.service -n 50 -f");
    until("the stream to open", || fake.open_streams.load(Ordering::SeqCst) == 1).await;
    fake.push("journalctl -u demo.service", "2026-11-16T09:49:11+0000 h demo[1]: one");
    fake.push("journalctl -u demo.service", "2026-11-16T09:49:12+0000 h demo[1]: two");
    assert_eq!(
        follow.events.recv().await.unwrap(),
        FollowEvent::Line("2026-11-16T09:49:11+0000 h demo[1]: one".into())
    );
    assert_eq!(
        follow.events.recv().await.unwrap(),
        FollowEvent::Line("2026-11-16T09:49:12+0000 h demo[1]: two".into())
    );
    drop(follow);
    until("the stream to close with the follow", || fake.open_streams.load(Ordering::SeqCst) == 0).await;

    // A follow does not keep the connection alive after the monitor and the tab are gone.
    let held = monitor.follow("journalctl -u demo.service -f");
    until("the stream to open again", || fake.open_streams.load(Ordering::SeqCst) == 1).await;
    drop(held);
    monitor.shutdown();
    drop(connection);
    until("the SSH connection to close", || *lock(&server.state.open_connections) == 0).await;
}
