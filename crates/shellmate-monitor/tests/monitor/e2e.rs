//! Tests against real OpenSSH servers (`#[ignore]`d): the throwaway Alpine container of the SSH, SFTP and
//! forwarding tests (`shellmate-e2e`: BusyBox ash, procps-ng `ps`, no systemd, no Docker) and the Ubuntu
//! 24.04 OrbStack machine `shellmate-mon` (dash, systemd 255, Docker, passwordless sudo). The tests that
//! can run on both do.
//!
//! ```text
//! SHELLMATE_E2E_KEY=/tmp/shellmate-e2e-key cargo test -p shellmate-monitor --test monitor e2e -- --ignored --nocapture
//! ```
//!
//! Settings (environment): `SHELLMATE_E2E_KEY` (the private key both servers accept); for the container
//! `SHELLMATE_E2E_PORT` (2222), `SHELLMATE_E2E_USER` (tester), `SHELLMATE_E2E_CONTAINER` (shellmate-e2e); for
//! the Ubuntu machine `SHELLMATE_MON_HOST` (shellmate-mon.orb.local), `SHELLMATE_MON_PORT` (22),
//! `SHELLMATE_MON_USER` (tester). The machines are used through SSH as the test user, like the container
//! through `docker exec`, to read `/proc` for comparison and to start and stop test processes (all removed
//! again at the end). Host keys are trusted without being recorded anywhere. The tests take turns (they
//! measure CPU and process numbers); the numbers they measure are printed.

use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use shellmate_core::AuthMethod;
use shellmate_monitor::commands::{self, Plan, Privilege, Signal};
use shellmate_monitor::probe::{ContainerAccess, RuntimeKind, Sudo, parse_probe};
use shellmate_monitor::script::{CollectorConfig, collector_command, new_token, probe_command};
use shellmate_monitor::{ActionOutcome, Monitor, MonitorEvent, Options, Phase, Snapshot};
use shellmate_ssh::{Connection, ExecOptions, Route, SshHost};

use crate::support::Env;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// A server to test against.
#[derive(Clone)]
struct Target {
    name: &'static str,
    host: String,
    port: u16,
    user: String,
    /// `Some(container)`: the machine is reached with `docker exec` (the Alpine container); otherwise with `ssh`.
    container: Option<String>,
    ubuntu: bool,
}

impl Target {
    fn alpine() -> Self {
        Self {
            name: "alpine",
            host: "127.0.0.1".into(),
            port: setting("SHELLMATE_E2E_PORT", "2222").parse().expect("SHELLMATE_E2E_PORT is a port number"),
            user: setting("SHELLMATE_E2E_USER", "tester"),
            container: Some(setting("SHELLMATE_E2E_CONTAINER", "shellmate-e2e")),
            ubuntu: false,
        }
    }

    fn ubuntu() -> Self {
        Self {
            name: "ubuntu",
            host: setting("SHELLMATE_MON_HOST", "shellmate-mon.orb.local"),
            port: setting("SHELLMATE_MON_PORT", "22").parse().expect("SHELLMATE_MON_PORT is a port number"),
            user: setting("SHELLMATE_MON_USER", "tester"),
            container: None,
            ubuntu: true,
        }
    }

    /// Runs a shell snippet on the machine as the SSH user and returns what it printed.
    fn run(&self, script: &str) -> String {
        let output = match &self.container {
            Some(container) => Command::new("docker")
                .args(["exec", "-u", &self.user, container, "sh", "-c", script])
                .output()
                .expect("docker is installed"),
            None => Command::new("ssh").args(self.ssh_args()).arg(script).output().expect("ssh is installed"),
        };
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn ssh_args(&self) -> Vec<String> {
        let key =
            std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts");
        [
            "-i",
            &key,
            "-p",
            &self.port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "BatchMode=yes",
        ]
        .into_iter()
        .map(String::from)
        .chain([format!("{}@{}", self.user, self.host)])
        .collect()
    }

    /// Starts a command on the machine and does not wait for it (it must not hold on to stdout).
    fn detached(&self, script: &str) {
        match &self.container {
            Some(container) => {
                let status = Command::new("docker")
                    .args(["exec", "-d", "-u", &self.user, container, "sh", "-c", script])
                    .status()
                    .expect("docker is installed");
                assert!(status.success(), "{script}");
            }
            None => {
                self.run(&format!("setsid sh -c '{}' </dev/null >/dev/null 2>&1 &", script.replace('\'', "'\\''")));
            }
        }
    }

    fn collector_pids(&self) -> Vec<u32> {
        // The brackets keep pgrep from finding its own command line.
        self.run("pgrep -f '[s]hellmate-collector'").split_whitespace().filter_map(|pid| pid.parse().ok()).collect()
    }

    /// CPU ticks (own and of the children that were waited for) of a process, from its stat file.
    fn cpu_ticks(&self, pid: u32) -> Option<u64> {
        let stat = self.run(&format!("cat /proc/{pid}/stat 2>/dev/null"));
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<u64> = rest.split_whitespace().skip(11).take(4).filter_map(|word| word.parse().ok()).collect();
        (fields.len() == 4).then(|| fields.iter().sum())
    }

    fn meminfo(&self, key: &str) -> u64 {
        self.run(&format!("grep '^{key}:' /proc/meminfo"))
            .split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .expect(key)
    }

    /// The pid of the process whose command line is exactly `command`.
    fn pid_of(&self, command: &str) -> u32 {
        self.run(&format!("pgrep -f '^{command}$' | head -1"))
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("no process {command:?}"))
    }
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

async fn session(target: &Target) -> Session {
    let env = Env::new();
    let key = std::env::var("SHELLMATE_E2E_KEY").expect("set SHELLMATE_E2E_KEY to the private key the server accepts");
    let host = SshHost::new(target.host.clone(), target.port, target.user.clone(), AuthMethod::PrivateKey)
        .with_identity_file(key);
    let connection = Connection::connect(Route::direct(host), env.trusting()).await.expect("connect");
    Session { connection, _env: env }
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

macro_rules! on_both {
    ($(async fn $name:ident($t:ident: &Target) $body:block)*) => {
        $(
            mod $name {
                use super::*;

                pub async fn run($t: &Target) $body

                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                #[ignore = "needs the shellmate-e2e container (see the module docs)"]
                async fn alpine() {
                    let _turn = turn().await;
                    run(&Target::alpine()).await;
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                #[ignore = "needs the shellmate-mon machine (see the module docs)"]
                async fn ubuntu() {
                    let _turn = turn().await;
                    run(&Target::ubuntu()).await;
                }
            }
        )*
    };
}

async fn next_report(
    receiver: &async_channel::Receiver<MonitorEvent>,
    id: shellmate_monitor::ActionId,
) -> shellmate_monitor::ActionReport {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), receiver.recv()).await.expect("a report").unwrap() {
            MonitorEvent::Action(report) if report.id == id => return report,
            _ => {}
        }
    }
}

on_both! {
    async fn e2e_the_probe_describes_the_machine(t: &Target) {
        let s = session(t).await;
        let token = new_token();
        let started = Instant::now();
        let output = s
            .connection
            .exec_with(&probe_command(&token), ExecOptions { timeout: Duration::from_secs(30), max_output: 1 << 20 })
            .await
            .unwrap();
        println!("{}: the probe took {:.2} s", t.name, started.elapsed().as_secs_f64());
        assert!(output.status.success(), "{}", output.stderr_text());
        let probe = parse_probe(&token, &output.stdout_text());
        println!("{}: probe: {probe:#?}", t.name);
        assert!(probe.is_linux && probe.can_collect());
        assert!(probe.kernel.starts_with("Linux "), "{}", probe.kernel);
        assert_eq!(probe.user, t.user);
        assert_eq!(probe.uid, Some(1000));
        assert_eq!((probe.page_size, probe.clk_tck), (4096, 100));
        assert_eq!(probe.user_name(0), "root");
        assert_eq!(probe.user_name(1000), t.user);
        assert!(probe.uptime_secs.unwrap() > 1.0);
        if t.ubuntu {
            assert_eq!(probe.os_name.as_deref().map(|name| name.starts_with("Ubuntu 24.04")), Some(true), "{:?}", probe.os_name);
            assert_eq!(probe.sudo, Sudo::Passwordless, "the test user has passwordless sudo");
            let systemd = probe.systemd.clone().expect("systemd");
            assert_eq!((systemd.version, systemd.running), (Some(255), true));
            let docker = probe.containers.iter().find(|c| c.kind == RuntimeKind::Docker).expect("docker");
            assert_eq!(docker.access, ContainerAccess::User, "the test user is in the docker group");
            assert!(docker.version.as_deref().is_some_and(|v| v.starts_with("29.")), "{docker:?}");
            assert_eq!(probe.usable_container_runtime().unwrap().kind, RuntimeKind::Docker);
        } else {
            assert_eq!(probe.os_name.as_deref().map(|name| name.starts_with("Alpine")), Some(true), "{:?}", probe.os_name);
            assert!(matches!(probe.sudo, Sudo::NeedsPassword | Sudo::Unavailable), "{:?}", probe.sudo);
            assert_eq!(probe.systemd, None, "no systemd in the container");
            assert!(probe.containers.is_empty(), "no docker or podman: {:?}", probe.containers);
        }
    }

    async fn e2e_thirty_seconds_of_sampling_match_the_machine_and_actions_work(t: &Target) {
        let s = session(t).await;
        let (events, receiver) = async_channel::unbounded();
        let monitor = Monitor::with_options(
            tokio::runtime::Handle::current(),
            events,
            Options { grace: Duration::from_secs(1), ..Options::default() },
        );
        monitor.connection_restored(&s.connection);

        // Processes of our own to find: one that sleeps, one that burns a core.
        t.detached("exec sleep 1234");
        t.detached("exec sh -c 'while :; do :; done # p5-burner'");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let sleeper_pid = t.pid_of("sleep 1234");
        let burner_pid: u32 = t.run("pgrep -f 'p5-burner' | head -1").trim().parse().expect("the burner");

        let cores: usize = t.run("nproc").trim().parse().unwrap();
        let mem_total_kb = t.meminfo("MemTotal");
        let started = Instant::now();
        monitor.set_active(true);

        let mut snapshots: Vec<std::sync::Arc<Snapshot>> = Vec::new();
        let mut collector_cpu = None;
        while started.elapsed() < Duration::from_secs(30) {
            let Some(snapshot) = next_snapshot(&receiver, Duration::from_secs(10)).await else { panic!("no snapshot for 10 s") };
            snapshots.push(snapshot.clone());
            // Once the collector shows up in the process list, start measuring what it costs.
            if collector_cpu.is_none() {
                let pids = t.collector_pids();
                if !pids.is_empty() && snapshot.processes.iter().any(|p| p.is_collector) {
                    collector_cpu = Some((Instant::now(), pids.clone(), pids.iter().map(|pid| t.cpu_ticks(*pid).unwrap_or(0)).sum::<u64>()));
                }
            }
        }
        let end = snapshots.last().unwrap().clone();
        println!("{}: {} snapshots in {:.0} s", t.name, snapshots.len(), started.elapsed().as_secs_f64());

        // What the machine itself says, now.
        let machine_available = t.meminfo("MemAvailable");
        let machine_load: f32 = t.run("cut -d' ' -f1 /proc/loadavg").trim().parse().unwrap();
        let machine_uptime: f64 = t.run("cut -d' ' -f1 /proc/uptime").trim().parse().unwrap();
        let machine_processes = t.run("ls /proc | grep -c '^[0-9]'").trim().parse::<usize>().unwrap();

        assert_eq!(end.host.cores, cores);
        let memory = end.memory.expect("memory");
        assert_eq!(memory.total, mem_total_kb * 1024, "MemTotal is exactly what /proc/meminfo says");
        let available_diff = (memory.available as f64 - machine_available as f64 * 1024.0).abs() / (mem_total_kb as f64 * 1024.0);
        assert!(available_diff < 0.02, "MemAvailable within 2 % of the total: {available_diff}");
        assert!(memory.used + memory.cache <= memory.total);
        assert!((end.host.load.unwrap().one - machine_load).abs() < 1.5, "{:?} vs {machine_load}", end.host.load);
        assert!((end.host.uptime_secs - machine_uptime).abs() < 15.0, "{} vs {machine_uptime}", end.host.uptime_secs);
        assert!(end.host.os_name.as_deref().is_some_and(|n| n.starts_with(if t.ubuntu { "Ubuntu" } else { "Alpine" })));
        assert!(end.processes.len().abs_diff(machine_processes) <= 8, "{} processes vs {machine_processes}", end.processes.len());

        // CPU: samples with rates in the valid range; the burner is one core of `cores`.
        let cpu_samples: Vec<f32> = snapshots.iter().filter_map(|s| s.cpu.as_ref().and_then(|c| c.percent)).collect();
        assert!(cpu_samples.len() >= 10, "{} CPU samples in 30 s", cpu_samples.len());
        assert!(cpu_samples.iter().all(|p| (0.0..=100.0).contains(p)), "{cpu_samples:?}");
        let average = cpu_samples.iter().sum::<f32>() / cpu_samples.len() as f32;
        let one_core = 100.0 / cores as f32;
        println!("{}: CPU: {average:.1} % on average over {} samples (one core is {one_core:.1} %)", t.name, cpu_samples.len());
        assert!(average > one_core * 0.5 && average < one_core * 2.5, "a burner on one of {cores} cores: {average}");
        let busiest_core = end.cpu.as_ref().unwrap().per_core.iter().flatten().copied().fold(0.0, f32::max);
        assert!(busiest_core > 80.0, "one core is busy: {:?}", end.cpu.as_ref().unwrap().per_core);
        assert!(end.cpu.as_ref().unwrap().history.len() >= 10);

        // Network: an interface with rates.
        let net = end.network.as_ref().expect("a network card");
        println!("{}: network: {} rx {:.0} B/s tx {:.0} B/s", t.name, net.interface, net.rx_per_sec.unwrap_or(-1.0), net.tx_per_sec.unwrap_or(-1.0));
        assert!(net.rx_history.len() >= 10 && net.rx_per_sec.is_some() && net.tx_per_sec.is_some());
        if !t.ubuntu {
            assert_eq!(net.interface, "eth0");
        }

        // Disks, as df says (a container's / is an overlay, which is not a disk; a Mac's shared files are never asked).
        println!("{}: disks: {:?}", t.name, end.disks.iter().map(|d| (&d.mount, &d.filesystem, d.percent)).collect::<Vec<_>>());
        assert!(end.disks.iter().any(|d| d.mount == if t.ubuntu { "/" } else { "/config" }), "{:?}", end.disks);
        assert!(end.disks.iter().all(|d| !["overlay", "tmpfs", "mac", "none", "orbstack"].contains(&d.filesystem.as_str())), "{:?}", end.disks);
        assert!(end.disks.iter().all(|d| !d.mount.starts_with("/Users") && !d.mount.starts_with("/mnt/mac")), "{:?}", end.disks);
        assert!(end.zombies < 5);

        // The processes we started.
        let sleeper = end.process(sleeper_pid).expect("the sleeper is in the list");
        assert_eq!((sleeper.user.as_str(), sleeper.command.as_str(), sleeper.state), (t.user.as_str(), "sleep 1234", 'S'));
        assert!(sleeper.cpu_percent.unwrap() < 1.0);
        let burner = end.process(burner_pid).expect("the burner is in the list");
        assert_eq!(burner.user, t.user);
        assert!(burner.command.starts_with("sh -c while :; do :; done"), "{}", burner.command);
        let rate = burner.cpu_percent.unwrap();
        assert!((60.0..=110.0).contains(&rate), "a burner uses about one core, not {rate} %");
        assert_eq!(end.top_by_cpu(1)[0].pid, burner_pid, "and it is the busiest");
        assert!(end.processes.iter().any(|p| p.pid == 1 && p.user == "root"));
        let parents: std::collections::HashSet<u32> = end.processes.iter().map(|p| p.pid).collect();
        let orphans = end.processes.iter().filter(|p| p.ppid != 0 && !parents.contains(&p.ppid)).count();
        assert!(orphans <= 2, "every parent is in the list, for the tree ({orphans} are not: processes that came and went)");

        // What the collector costs on the server.
        if let Some((since, pids, before)) = collector_cpu {
            let after: u64 = pids.iter().map(|pid| t.cpu_ticks(*pid).unwrap_or(0)).sum();
            let secs = since.elapsed().as_secs_f64();
            let percent = (after - before) as f64 / (secs * 100.0) * 100.0;
            println!("{}: the collector used {percent:.2} % of a core over {secs:.0} s ({} ticks)", t.name, after - before);
            assert!(percent < 3.0, "{percent}");
        }

        // TERM through the action API ends the sleeper.
        let probe = monitor.probe().unwrap();
        let Plan::Exec { line } = commands::plan(&commands::kill(sleeper_pid, Signal::Term).unwrap(), Privilege::User, &probe) else {
            panic!("a signal to your own process runs as it is")
        };
        assert_eq!(line, format!("kill -TERM {sleeper_pid}"));
        let id = monitor.run_action(line);
        let report = next_report(&receiver, id).await;
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
        assert_eq!(t.run(&format!("[ -d /proc/{sleeper_pid} ] && echo alive")).trim(), "");

        // A signal to somebody else's process, without sudo, comes back as a failure with the server's words.
        let denied = monitor.run_action("kill -TERM 1");
        let report = next_report(&receiver, denied).await;
        let ActionOutcome::Failure { code, message } = report.outcome else { panic!("{report:?}") };
        assert_ne!(code, Some(0));
        assert!(!message.is_empty());
        println!("{}: kill -TERM 1 without sudo: {message}", t.name);

        // The plan for a process of root's follows what the server allows.
        let root_plan = commands::plan(&commands::kill(1, Signal::Term).unwrap(), Privilege::Root, &probe);
        if t.ubuntu {
            assert_eq!(root_plan, Plan::Exec { line: "sudo -n kill -TERM 1".into() });
            // A root-owned sleep started with sudo, ended through the app's path (sudo -n).
            t.detached("exec sudo -n sleep 4321");
            tokio::time::sleep(Duration::from_millis(800)).await;
            let root_sleep = t.pid_of("sleep 4321");
            let root_snapshot = next_snapshot(&receiver, Duration::from_secs(10)).await.unwrap();
            let _ = root_snapshot;
            let Plan::Exec { line } = commands::plan(&commands::kill(root_sleep, Signal::Term).unwrap(), Privilege::Root, &probe) else { panic!() };
            assert_eq!(line, format!("sudo -n kill -TERM {root_sleep}"));
            let id = monitor.run_action(line);
            let report = next_report(&receiver, id).await;
            assert_eq!(report.outcome, ActionOutcome::Success { output: String::new() }, "{report:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert_eq!(t.run(&format!("[ -d /proc/{root_sleep} ] && echo alive")).trim(), "", "root's sleep is gone");
        } else {
            assert_eq!(root_plan, Plan::Terminal { line: "sudo kill -TERM 1".into() }, "no passwordless sudo: the terminal plan");
        }

        // Lowering the priority works for our own process.
        let Plan::Exec { line } = commands::plan(&commands::lower_priority(burner_pid, burner.nice).unwrap(), Privilege::User, &probe) else { panic!() };
        let id = monitor.run_action(line);
        let report = next_report(&receiver, id).await;
        assert!(matches!(report.outcome, ActionOutcome::Success { .. }), "{report:?}");
        assert_eq!(t.run(&format!("cut -d' ' -f19 /proc/{burner_pid}/stat | tr -d '\\n'")).trim().parse::<i32>().unwrap(), burner.nice + 5);
        // KILL ends the burner.
        let Plan::Exec { line } = commands::plan(&commands::kill(burner_pid, Signal::Kill).unwrap(), Privilege::User, &probe) else { panic!() };
        let id = monitor.run_action(line);
        assert!(matches!(next_report(&receiver, id).await.outcome, ActionOutcome::Success { .. }));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(t.run(&format!("[ -d /proc/{burner_pid} ] && echo alive")).trim(), "");

        // Leaving the page: the grace period (1 s here), then the channel is closed and the script ends.
        monitor.set_active(false);
        let started = Instant::now();
        loop {
            if t.collector_pids().is_empty() {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(8), "the collector is still running on the server: {:?}", t.collector_pids());
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        println!("{}: the collector was gone {:.1} s after the last viewer left (grace 1 s)", t.name, started.elapsed().as_secs_f64());
        assert_eq!(monitor.phase(), Phase::Idle);
        monitor.shutdown();
        drop(s);
    }

    async fn e2e_what_the_collector_costs_with_hundreds_of_processes(t: &Target) {
        let s = session(t).await;
        // 300 sleeping processes, as a busy server has.
        t.detached("for i in $(seq 300); do sleep 1243 & done; wait");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let processes = t.run("ls /proc | grep -c '^[0-9]'").trim().parse::<usize>().unwrap();
        println!("{}: {processes} processes on the machine", t.name);

        let config = CollectorConfig::new(new_token());
        let mut stream = s.connection.exec_stream(&collector_command(&config)).await.unwrap();
        let mut first = stream.next_line().await.expect("a first line");
        while !first.contains(" tick ") {
            first = stream.next_line().await.expect("lines");
        }
        let pids = t.collector_pids();
        assert!(!pids.is_empty());
        let (t0, ticks0) = (Instant::now(), pids.iter().map(|pid| t.cpu_ticks(*pid).unwrap_or(0)).sum::<u64>());
        let (mut bytes, mut frames_seen) = (0usize, 0usize);
        let mut parser = shellmate_monitor::frame::FrameParser::new(&config.token);
        while t0.elapsed() < Duration::from_secs(30) {
            let line = tokio::time::timeout(Duration::from_secs(10), stream.next_line()).await.expect("the collector stays alive").expect("a line");
            bytes += line.len() + 1;
            if matches!(parser.push(&line), Some(shellmate_monitor::frame::Item::Frame(_))) {
                frames_seen += 1;
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let ticks1: u64 = pids.iter().map(|pid| t.cpu_ticks(*pid).unwrap_or(0)).sum();
        let cpu = (ticks1 - ticks0) as f64 / (secs * 100.0) * 100.0;
        println!(
            "{}: collector with {processes} processes over {secs:.0} s: {cpu:.2} % of a core, {:.0} KB/min ({bytes} bytes, {frames_seen} frames)",
            t.name,
            bytes as f64 / secs * 60.0 / 1024.0,
        );
        drop(stream);

        // Stopping: nothing of it is left within 5 s.
        let started = Instant::now();
        while !t.collector_pids().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(5), "left behind: {:?}", t.collector_pids());
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        println!("{}: nothing left {:.1} s after the channel was dropped", t.name, started.elapsed().as_secs_f64());
        let _ = t.run("pkill -f '[s]leep 1243'");
        assert!(cpu < 5.0, "{cpu}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs the shellmate-e2e container (see the module docs)"]
async fn e2e_the_collector_works_with_the_busybox_ps_too() {
    let _turn = turn().await;
    let t = Target::alpine();
    let s = session(&t).await;
    // The container's `ps` is procps-ng; BusyBox is there as an applet, so a directory with a `ps` that is
    // BusyBox (found first in the PATH) shows the other output style. Removed again at the end.
    t.run("mkdir -p /tmp/p5bb && ln -sf /bin/busybox /tmp/p5bb/ps");
    assert!(!t.run("PATH=/tmp/p5bb:$PATH ps --version 2>&1 | head -1").contains("procps"), "the stand-in is BusyBox");
    t.detached("exec sleep 1236");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sleeper = t.pid_of("sleep 1236");

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
    assert_eq!((mine.user.as_str(), mine.command.as_str()), (t.user.as_str(), "sleep 1236"));
    assert!(ps_frames >= 1);
    let _ = t.run(&format!("kill {sleeper}; rm -rf /tmp/p5bb"));
}
