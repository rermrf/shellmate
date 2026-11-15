//! Tests against a real OpenSSH server (`#[ignore]`d): the throwaway Alpine container of the SSH, SFTP and
//! forwarding tests (see the SSH crate's e2e docs for how to start it). Its `ps` is procps-ng, its shell
//! is BusyBox ash, it has no systemd and no Docker.
//!
//! ```text
//! SHELLMATE_E2E_KEY=/tmp/shellmate-e2e-key cargo test -p shellmate-monitor --test monitor e2e -- --ignored --nocapture
//! ```
//!
//! Settings (environment) as for the other e2e tests: `SHELLMATE_E2E_PORT` (2222), `SHELLMATE_E2E_USER`
//! (tester), `SHELLMATE_E2E_KEY`, `SHELLMATE_E2E_CONTAINER` (shellmate-e2e). The container is also used to
//! read `/proc` for comparison and to start and stop test processes (all removed again at the end).
//! The tests take turns (they measure the container's CPU and process numbers); the numbers they measure
//! are printed.

use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use shellmate_core::AuthMethod;
use shellmate_monitor::commands::{self, Plan, Privilege, Signal};
use shellmate_monitor::probe::{ContainerAccess, Sudo, parse_probe};
use shellmate_monitor::script::{CollectorConfig, collector_command, new_token, probe_command};
use shellmate_monitor::{ActionOutcome, Monitor, MonitorEvent, Options, Phase, Snapshot};
use shellmate_ssh::{Connection, ExecOptions, Route, SshHost};

use crate::support::Env;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn container() -> String {
    setting("SHELLMATE_E2E_CONTAINER", "shellmate-e2e")
}

fn user() -> String {
    setting("SHELLMATE_E2E_USER", "tester")
}

/// Runs a shell snippet in the container as the SSH user and returns what it printed.
fn docker(script: &str) -> String {
    let output = Command::new("docker")
        .args(["exec", "-u", &user(), &container(), "sh", "-c", script])
        .output()
        .expect("docker is installed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Starts a command in the container without waiting for it.
fn docker_detached(script: &str) {
    let status = Command::new("docker")
        .args(["exec", "-d", "-u", &user(), &container(), "sh", "-c", script])
        .status()
        .expect("docker is installed");
    assert!(status.success(), "{script}");
}

/// The tests take turns.
async fn turn() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

struct Session {
    connection: Connection,
    _env: Env,
}

async fn session() -> Session {
    let env = Env::new();
    let key = std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts");
    let port = setting("SHELLMATE_E2E_PORT", "2222").parse().expect("SHELLMATE_E2E_PORT is a port number");
    let host = SshHost::new("127.0.0.1", port, user(), AuthMethod::PrivateKey).with_identity_file(key);
    let connection = Connection::connect(Route::direct(host), env.trusting()).await.expect("connect");
    Session { connection, _env: env }
}

fn collector_pids() -> Vec<u32> {
    // The brackets keep pgrep from finding its own command line.
    docker("pgrep -f '[s]hellmate-collector'").split_whitespace().filter_map(|pid| pid.parse().ok()).collect()
}

/// CPU ticks (own and of the children that were waited for) of a process, from its stat file.
fn cpu_ticks(pid: u32) -> Option<u64> {
    let stat = docker(&format!("cat /proc/{pid}/stat 2>/dev/null"));
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<u64> = rest.split_whitespace().skip(11).take(4).filter_map(|word| word.parse().ok()).collect();
    (fields.len() == 4).then(|| fields.iter().sum())
}

fn meminfo(key: &str) -> u64 {
    docker(&format!("grep '^{key}:' /proc/meminfo")).split_whitespace().nth(1).and_then(|n| n.parse().ok()).expect(key)
}

async fn next_snapshot(
    events: &async_channel::Receiver<MonitorEvent>,
    wait: Duration,
) -> Option<std::sync::Arc<Snapshot>> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(MonitorEvent::Snapshot(snapshot))) => return Some(snapshot),
            Ok(Ok(_)) => {}
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the shellmate-e2e container (see the module docs)"]
async fn e2e_the_probe_describes_the_container() {
    let _turn = turn().await;
    let s = session().await;
    let token = new_token();
    let output = s
        .connection
        .exec_with(&probe_command(&token), ExecOptions { timeout: Duration::from_secs(30), max_output: 1 << 20 })
        .await
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr_text());
    let probe = parse_probe(&token, &output.stdout_text());
    println!("probe: {probe:#?}");
    assert!(probe.is_linux && probe.can_collect());
    assert_eq!(probe.os_name.as_deref().map(|name| name.starts_with("Alpine")), Some(true), "{:?}", probe.os_name);
    assert!(probe.kernel.starts_with("Linux "), "{}", probe.kernel);
    assert_eq!(probe.user, user());
    assert_eq!(probe.uid, Some(1000));
    assert!(
        matches!(probe.sudo, Sudo::NeedsPassword | Sudo::Unavailable),
        "the test user has no passwordless sudo: {:?}",
        probe.sudo
    );
    assert_eq!(probe.systemd, None, "no systemd in the container");
    assert!(probe.containers.is_empty(), "no docker or podman: {:?}", probe.containers);
    assert_eq!((probe.page_size, probe.clk_tck), (4096, 100));
    assert_eq!(probe.user_name(0), "root");
    assert_eq!(probe.user_name(1000), user());
    assert!(probe.uptime_secs.unwrap() > 1.0);
    let _ = ContainerAccess::User;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the shellmate-e2e container (see the module docs)"]
async fn e2e_thirty_seconds_of_sampling_match_the_container_and_actions_work() {
    let _turn = turn().await;
    let s = session().await;
    let (events, receiver) = async_channel::unbounded();
    let monitor = Monitor::with_options(
        tokio::runtime::Handle::current(),
        events,
        Options { grace: Duration::from_secs(1), ..Options::default() },
    );
    monitor.connection_restored(&s.connection);

    // Two processes of our own to find: one that sleeps, one that burns a core.
    docker_detached("exec sleep 1234");
    docker_detached("exec sh -c 'while :; do :; done # p5-burner'");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sleeper_pid: u32 =
        docker("pgrep -f '^sleep 1234'").split_whitespace().next().expect("the sleeper").parse().unwrap();
    let burner_pid: u32 =
        docker("pgrep -f 'p5-burner'").split_whitespace().next().expect("the burner").parse().unwrap();

    let cores: usize = docker("nproc").trim().parse().unwrap();
    let mem_total_kb = meminfo("MemTotal");
    let started = Instant::now();
    monitor.set_active(true);

    let mut snapshots: Vec<std::sync::Arc<Snapshot>> = Vec::new();
    let mut collector_cpu = None;
    while started.elapsed() < Duration::from_secs(30) {
        let Some(snapshot) = next_snapshot(&receiver, Duration::from_secs(10)).await else {
            panic!("no snapshot for 10 s")
        };
        snapshots.push(snapshot.clone());
        // Once the collector shows up in the process list, start measuring what it costs.
        if collector_cpu.is_none() {
            let pids = collector_pids();
            if !pids.is_empty() && snapshot.processes.iter().any(|p| p.is_collector) {
                collector_cpu = Some((
                    Instant::now(),
                    pids.clone(),
                    pids.iter().map(|pid| cpu_ticks(*pid).unwrap_or(0)).sum::<u64>(),
                ));
            }
        }
    }
    let end = snapshots.last().unwrap().clone();
    println!("{} snapshots in {:.0} s", snapshots.len(), started.elapsed().as_secs_f64());

    // What the container itself says, now.
    let docker_available = meminfo("MemAvailable");
    let docker_load: f32 = docker("cut -d' ' -f1 /proc/loadavg").trim().parse().unwrap();
    let docker_uptime: f64 = docker("cut -d' ' -f1 /proc/uptime").trim().parse().unwrap();
    let docker_processes = docker("ls /proc | grep -c '^[0-9]'").trim().parse::<usize>().unwrap();

    assert_eq!(end.host.cores, cores);
    let memory = end.memory.expect("memory");
    assert_eq!(memory.total, mem_total_kb * 1024, "MemTotal is exactly what /proc/meminfo says");
    let available_diff =
        (memory.available as f64 - docker_available as f64 * 1024.0).abs() / (mem_total_kb as f64 * 1024.0);
    assert!(available_diff < 0.02, "MemAvailable within 2 % of the total: {available_diff}");
    assert!(memory.used + memory.cache <= memory.total);
    assert!((end.host.load.unwrap().one - docker_load).abs() < 1.5, "{:?} vs {docker_load}", end.host.load);
    assert!((end.host.uptime_secs - docker_uptime).abs() < 15.0, "{} vs {docker_uptime}", end.host.uptime_secs);
    assert_eq!(end.host.os_name.as_deref().map(|n| n.starts_with("Alpine")), Some(true));
    assert!(
        end.processes.len().abs_diff(docker_processes) <= 6,
        "{} processes vs {docker_processes}",
        end.processes.len()
    );

    // CPU: samples with rates in the valid range; the burner is one core of `cores`.
    let cpu_samples: Vec<f32> = snapshots.iter().filter_map(|s| s.cpu.as_ref().and_then(|c| c.percent)).collect();
    assert!(cpu_samples.len() >= 10, "{} CPU samples in 30 s", cpu_samples.len());
    assert!(cpu_samples.iter().all(|p| (0.0..=100.0).contains(p)), "{cpu_samples:?}");
    let average = cpu_samples.iter().sum::<f32>() / cpu_samples.len() as f32;
    let one_core = 100.0 / cores as f32;
    println!("CPU: {average:.1} % on average over {} samples (one core is {one_core:.1} %)", cpu_samples.len());
    assert!(average > one_core * 0.5 && average < one_core * 2.5, "a burner on one of {cores} cores: {average}");
    let busiest_core = end.cpu.as_ref().unwrap().per_core.iter().flatten().copied().fold(0.0, f32::max);
    assert!(busiest_core > 80.0, "one core is busy: {:?}", end.cpu.as_ref().unwrap().per_core);
    assert!(end.cpu.as_ref().unwrap().history.len() >= 10);

    // Network: the interface of the default route, with rates.
    let net = end.network.as_ref().expect("a network card");
    assert_eq!(net.interface, "eth0");
    assert!(net.rx_history.len() >= 10 && net.rx_per_sec.is_some() && net.tx_per_sec.is_some());
    println!("network: {} rx {:.0} B/s tx {:.0} B/s", net.interface, net.rx_per_sec.unwrap(), net.tx_per_sec.unwrap());

    // Disks, as df says (the container's / is an overlay, which is not a disk).
    assert!(end.disks.iter().any(|d| d.mount == "/config"), "{:?}", end.disks);
    assert!(end.disks.iter().all(|d| d.filesystem != "overlay" && d.filesystem != "tmpfs"), "{:?}", end.disks);
    assert!(end.zombies < 5);

    // The processes we started.
    let sleeper = end.process(sleeper_pid).expect("the sleeper is in the list");
    assert_eq!((sleeper.user.as_str(), sleeper.command.as_str(), sleeper.state), (user().as_str(), "sleep 1234", 'S'));
    assert!(sleeper.cpu_percent.unwrap() < 1.0);
    let burner = end.process(burner_pid).expect("the burner is in the list");
    assert_eq!(burner.user, user());
    assert!(burner.command.starts_with("sh -c while :; do :; done"), "{}", burner.command);
    let rate = burner.cpu_percent.unwrap();
    assert!((60.0..=110.0).contains(&rate), "a burner uses about one core, not {rate} %");
    assert_eq!(end.top_by_cpu(1)[0].pid, burner_pid, "and it is the busiest");
    assert!(end.processes.iter().any(|p| p.pid == 1 && p.user == "root"));
    let parents: std::collections::HashSet<u32> = end.processes.iter().map(|p| p.pid).collect();
    assert!(
        end.processes.iter().all(|p| p.ppid == 0 || parents.contains(&p.ppid)),
        "every parent is in the list, for the tree"
    );

    // What the collector costs on the server.
    if let Some((since, pids, before)) = collector_cpu {
        let after: u64 = pids.iter().map(|pid| cpu_ticks(*pid).unwrap_or(0)).sum();
        let secs = since.elapsed().as_secs_f64();
        let percent = (after - before) as f64 / (secs * 100.0) * 100.0;
        println!("the collector used {percent:.2} % of a core over {secs:.0} s ({} ticks)", after - before);
        assert!(percent < 3.0, "{percent}");
    }

    // TERM through the action API ends the sleeper.
    let probe = monitor.probe().unwrap();
    let Plan::Exec { line } =
        commands::plan(&commands::kill(sleeper_pid, Signal::Term).unwrap(), Privilege::User, &probe)
    else {
        panic!("a signal to your own process runs as it is")
    };
    assert_eq!(line, format!("kill -TERM {sleeper_pid}"));
    let id = monitor.run_action(line);
    let report = loop {
        match tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await.expect("a report").unwrap() {
            MonitorEvent::Action(report) if report.id == id => break report,
            _ => {}
        }
    };
    assert_eq!(report.outcome, ActionOutcome::Success { output: String::new() }, "{report:?}");
    let gone = async {
        loop {
            let snapshot = next_snapshot(&receiver, Duration::from_secs(10)).await.expect("snapshots go on");
            if snapshot.process(sleeper_pid).is_none() {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), gone).await.expect("the terminated process leaves the list");
    assert_eq!(docker(&format!("[ -d /proc/{sleeper_pid} ] && echo alive")).trim(), "");

    // A signal that the server refuses comes back as a failure with the server's words.
    let denied = monitor.run_action("kill -TERM 1");
    let report = loop {
        match tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await.expect("a report").unwrap() {
            MonitorEvent::Action(report) if report.id == denied => break report,
            _ => {}
        }
    };
    let ActionOutcome::Failure { code, message } = report.outcome else { panic!("{report:?}") };
    assert_ne!(code, Some(0));
    assert!(!message.is_empty());

    // Lowering the priority works for our own process.
    let Plan::Exec { line } =
        commands::plan(&commands::lower_priority(burner_pid, burner.nice).unwrap(), Privilege::User, &probe)
    else {
        panic!()
    };
    let id = monitor.run_action(line);
    let report = loop {
        match tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await.expect("a report").unwrap() {
            MonitorEvent::Action(report) if report.id == id => break report,
            _ => {}
        }
    };
    assert!(matches!(report.outcome, ActionOutcome::Success { .. }), "{report:?}");
    assert_eq!(
        docker(&format!("cut -d' ' -f19 /proc/{burner_pid}/stat | tr -d '\\n'")).trim().parse::<i32>().unwrap(),
        burner.nice + 5
    );
    let _ = docker(&format!("kill -KILL {burner_pid}"));

    // Leaving the page: the grace period (1 s here), then the channel is closed and the script ends.
    monitor.set_active(false);
    let started = Instant::now();
    loop {
        if collector_pids().is_empty() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the collector is still running on the server: {:?}",
            collector_pids()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!("the collector was gone {:.1} s after the last viewer left (grace 1 s)", started.elapsed().as_secs_f64());
    assert_eq!(monitor.phase(), Phase::Idle);
    monitor.shutdown();
    drop(s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the shellmate-e2e container (see the module docs)"]
async fn e2e_what_the_collector_costs_with_hundreds_of_processes() {
    let _turn = turn().await;
    let s = session().await;
    // 300 sleeping processes, as a busy server has.
    docker_detached("for i in $(seq 300); do sleep 1243 & done; wait");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let processes = docker("ls /proc | grep -c '^[0-9]'").trim().parse::<usize>().unwrap();
    println!("{processes} processes in the container");

    let idle_pids_before: u64 = docker("cat /proc/sys/kernel/ns_last_pid").trim().parse().unwrap_or(0);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let idle_pids_after: u64 = docker("cat /proc/sys/kernel/ns_last_pid").trim().parse().unwrap_or(0);
    let baseline = idle_pids_after.saturating_sub(idle_pids_before);

    let config = CollectorConfig::new(new_token());
    let mut stream = s.connection.exec_stream(&collector_command(&config)).await.unwrap();
    let mut first = stream.next_line().await.expect("a first line");
    while !first.contains(" tick ") {
        first = stream.next_line().await.expect("lines");
    }
    let pids = collector_pids();
    assert!(!pids.is_empty());
    let (t0, ticks0) = (Instant::now(), pids.iter().map(|pid| cpu_ticks(*pid).unwrap_or(0)).sum::<u64>());
    let pid_counter0: u64 = docker("cat /proc/sys/kernel/ns_last_pid").trim().parse().unwrap_or(0);
    let (mut bytes, mut frames_seen) = (0usize, 0usize);
    let mut parser = shellmate_monitor::frame::FrameParser::new(&config.token);
    while t0.elapsed() < Duration::from_secs(30) {
        let line = tokio::time::timeout(Duration::from_secs(10), stream.next_line())
            .await
            .expect("the collector stays alive")
            .expect("a line");
        bytes += line.len() + 1;
        if matches!(parser.push(&line), Some(shellmate_monitor::frame::Item::Frame(_))) {
            frames_seen += 1;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let ticks1: u64 = pids.iter().map(|pid| cpu_ticks(*pid).unwrap_or(0)).sum();
    let pid_counter1: u64 = docker("cat /proc/sys/kernel/ns_last_pid").trim().parse().unwrap_or(0);
    let cpu = (ticks1 - ticks0) as f64 / (secs * 100.0) * 100.0;
    let forks_per_sec = (pid_counter1.saturating_sub(pid_counter0)).saturating_sub(4) as f64 / secs;
    println!(
        "collector with {processes} processes over {secs:.0} s: {cpu:.2} % of a core, {:.0} KB/min ({} bytes, {frames_seen} frames), about {forks_per_sec:.2} forks/s (idle container: {:.2}/s)",
        bytes as f64 / secs * 60.0 / 1024.0,
        bytes,
        baseline as f64 / 10.0,
    );
    drop(stream);

    // Stopping: nothing of it is left within 5 s.
    let started = Instant::now();
    while !collector_pids().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(5), "left behind: {:?}", collector_pids());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("nothing left {:.1} s after the channel was dropped", started.elapsed().as_secs_f64());
    let _ = docker("pkill -f '[s]leep 1243'");
    assert!(cpu < 5.0, "{cpu}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the shellmate-e2e container (see the module docs)"]
async fn e2e_the_collector_works_with_the_busybox_ps_too() {
    let _turn = turn().await;
    let s = session().await;
    // The container's `ps` is procps-ng; BusyBox is there as an applet, so a directory with a `ps` that is
    // BusyBox (found first in the PATH) shows the other output style. Removed again at the end.
    docker("mkdir -p /tmp/p5bb && ln -sf /bin/busybox /tmp/p5bb/ps");
    assert!(!docker("PATH=/tmp/p5bb:$PATH ps --version 2>&1 | head -1").contains("procps"), "the stand-in is BusyBox");
    docker_detached("exec sleep 1236");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sleeper: u32 =
        docker("pgrep -f '^sleep 1236'").split_whitespace().next().expect("the sleeper").parse().unwrap();

    let token = new_token();
    let probe_output = s.connection.exec(&probe_command(&token)).await.unwrap();
    let probe = std::sync::Arc::new(parse_probe(&token, &probe_output.stdout_text()));
    let config = CollectorConfig::new(new_token());
    let mut stream =
        s.connection.exec_stream(&format!("PATH=/tmp/p5bb:$PATH {}", collector_command(&config))).await.unwrap();
    let mut parser = shellmate_monitor::frame::FrameParser::new(&config.token);
    let mut engine = shellmate_monitor::engine::Engine::new(probe);
    let mut snapshot = None;
    let mut ps_frames = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        let line = tokio::time::timeout(Duration::from_secs(10), stream.next_line()).await.unwrap().expect("a line");
        if let Some(shellmate_monitor::frame::Item::Frame(frame)) = parser.push(&line) {
            ps_frames += usize::from(frame.section("ps").is_some());
            snapshot = Some(engine.apply(&frame));
        }
    }
    drop(stream);
    let snapshot = snapshot.unwrap();
    println!("busybox ps: {} processes, {ps_frames} frames with a ps section", snapshot.processes.len());
    let init = snapshot.process(1).expect("pid 1");
    assert_eq!(init.user, "root");
    assert!(init.command.contains("s6-svscan"), "{}", init.command);
    let mine = snapshot.process(sleeper).expect("the sleeper");
    assert_eq!((mine.user.as_str(), mine.command.as_str()), (user().as_str(), "sleep 1236"));
    assert!(ps_frames >= 1);
    let _ = docker(&format!("kill {sleeper}; rm -rf /tmp/p5bb"));
}
