//! Cold panel latency against the two real OpenSSH fixtures. From the workspace root:
//!
//! ```sh
//! SHELLMATE_E2E_KEY=/tmp/shellmate-e2e-key SHELLMATE_BENCH_OUT=/tmp/shellmate-latency.json \
//!   cargo test --offline -p shellmate-monitor --test latency first_open -- --ignored --nocapture
//! ```
//!
//! `SHELLMATE_{E2E,MON}_{HOST,PORT,USER}` default to 127.0.0.1:2222 and shellmate-mon.orb.local:22,
//! both as tester. Set the target's `KEY` or, without one, `PASSWORD`; Ubuntu inherits `SHELLMATE_E2E_KEY`
//! if neither MON credential is supplied. Only use these trusting test options on the fixtures.
//! `SHELLMATE_BENCH_RTTS` (0,100,250) are added RTTs in ms; `SHELLMATE_BENCH_RUNS` (5) is per RTT/target.
//! Have at least one container (running or stopped) for a nonempty Docker measurement. No fixtures are created.
//! `files.list+stat` includes overlapping home resolution; `files.home` times a separate REALPATH
//! after first paint and is not additive. `files.cached` includes copying the cached rows for display.
//! Warm page metrics exclude prefetch and start at activation of a fresh inactive monitor.

mod proxy;
mod results;
#[path = "../../../shellmate-ssh/tests/ssh/support.rs"]
mod support;

use std::{env, io, sync::Arc, time::Duration};

use shellmate_core::AuthMethod;
use shellmate_monitor::probe::RuntimeKind;
use shellmate_monitor::{Focus, Monitor, MonitorEvent, Options, Phase, Probe};
use shellmate_sftp::SftpSession;
use shellmate_ssh::{Connection, Route, Secret, SshHost};
use tokio::time::{Instant, timeout_at};

use proxy::Proxy;
use results::{Run, Sample};

fn setting(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

struct Target {
    name: &'static str,
    host: String,
    port: u16,
    login: SshHost,
}

impl Target {
    fn new(name: &'static str, prefix: &str, default_host: &str, default_port: &str) -> Self {
        let field = |suffix| format!("SHELLMATE_{prefix}_{suffix}");
        let host = setting(&field("HOST"), default_host);
        let port = setting(&field("PORT"), default_port).parse().expect("target port must be a u16");
        let user = setting(&field("USER"), "tester");
        let credential = |key: &str| env::var(key).ok().filter(|value| !value.is_empty());
        let login = if let Some(key) = credential(&field("KEY")) {
            SshHost::new("127.0.0.1", 0, user, AuthMethod::PrivateKey).with_identity_file(key)
        } else if let Some(password) = credential(&field("PASSWORD")) {
            SshHost::new("127.0.0.1", 0, user, AuthMethod::Password).with_password(Secret::new(password))
        } else if prefix == "MON"
            && let Some(key) = credential("SHELLMATE_E2E_KEY")
        {
            SshHost::new("127.0.0.1", 0, user, AuthMethod::PrivateKey).with_identity_file(key)
        } else {
            panic!("set {} or {} for {name}", field("KEY"), field("PASSWORD"));
        };
        Self { name, host, port, login }
    }
}

async fn files(connection: &Connection, connected: Instant, run: &mut Run) {
    let session = SftpSession::open(connection).await.expect("open SFTP");
    let opened = Instant::now();
    let listing = session.listing(".").await;
    let listed = Instant::now();
    listing.entries.as_ref().expect("list home");
    let cached = Instant::now();
    std::hint::black_box((*session.cached(".").expect("cached home")).clone());
    run.insert("files.cached", Ok(cached.elapsed()));
    let resolving = Instant::now();
    let home = session.home().await.expect("resolve home");
    run.extend([
        ("files.first", Ok(listed - connected)),
        ("files.open", Ok(opened - connected)),
        ("files.home", Ok(resolving.elapsed())),
        ("files.list+stat", Ok(listed - opened)),
    ]);
    let dir = if home == "/etc" { "/" } else { "/etc" };
    let start = Instant::now();
    let listing = session.listing(dir).await;
    let elapsed = start.elapsed();
    listing.entries.as_ref().expect("list next directory");
    run.insert("files.next", Ok(elapsed));
}

struct ActiveMonitor {
    monitor: Monitor,
    receiver: async_channel::Receiver<MonitorEvent>,
    start: Instant,
    focus: Focus,
}

impl ActiveMonitor {
    async fn new(connection: &Connection, focus: Focus, warm: bool) -> Self {
        let (events, receiver) = async_channel::unbounded();
        let monitor = Monitor::with_options(tokio::runtime::Handle::current(), events, Options::default());
        monitor.connection_restored(connection);
        if warm {
            monitor.prefetch().await;
            assert!(receiver.is_empty(), "no pre-activation events");
            match focus {
                Focus::Services => assert!(monitor.services().is_some(), "services prefetch failed"),
                Focus::Docker => assert!(monitor.docker().is_some(), "docker prefetch failed"),
                _ => (),
            }
        }
        let start = Instant::now();
        monitor.set_active(true);
        monitor.set_focus(focus);
        Self { monitor, receiver, start, focus }
    }

    async fn next(&self) -> Option<(MonitorEvent, Duration)> {
        let event = timeout_at(self.start + Duration::from_secs(90), self.receiver.recv())
            .await
            .ok()?
            .expect("monitor channel closed");
        let elapsed = self.start.elapsed();
        match event {
            MonitorEvent::Phase(Phase::Failed(_) | Phase::Unsupported(_) | Phase::WaitingReconnect) => {
                panic!("{:?} monitor stopped: {event:?}", self.focus)
            }
            _ => Some((event, elapsed)),
        }
    }
}

impl Drop for ActiveMonitor {
    fn drop(&mut self) {
        self.monitor.shutdown();
    }
}

async fn overview(connection: &Connection, run: &mut Run) -> Arc<Probe> {
    let active = ActiveMonitor::new(connection, Focus::Overview, false).await;
    let mut probe = None;
    while let Some((event, elapsed)) = active.next().await {
        match event {
            MonitorEvent::Probe(found) => {
                run.entry("monitor.probe").or_insert(Ok(elapsed));
                probe = Some(found);
            }
            MonitorEvent::Snapshot(snapshot) => {
                run.entry("monitor.snapshot").or_insert(Ok(elapsed));
                if snapshot.cpu.as_ref().and_then(|cpu| cpu.percent).is_some() {
                    run.entry("monitor.cpu").or_insert(Ok(elapsed));
                }
                if snapshot.processes.iter().any(|process| process.cpu_percent.is_some()) {
                    run.entry("monitor.process_cpu").or_insert(Ok(elapsed));
                }
                if run.contains_key("monitor.cpu") && run.contains_key("monitor.process_cpu") {
                    return probe.expect("probe precedes snapshots");
                }
            }
            _ => (),
        }
    }
    panic!("Overview timed out; reached {:?}", run.keys());
}

async fn first_page(connection: &Connection, focus: Focus, warm: bool) -> Sample {
    let active = ActiveMonitor::new(connection, focus, warm).await;
    let mut saw_empty = false;
    while let Some((event, elapsed)) = active.next().await {
        match (focus, event) {
            (Focus::Services, MonitorEvent::Services(_)) => return Ok(elapsed),
            (Focus::Docker, MonitorEvent::Docker(view)) => {
                if !view.containers.is_empty() {
                    return Ok(elapsed);
                }
                assert!(view.error.is_none(), "Docker listing failed: {:?}", view.error);
                saw_empty = true;
            }
            _ => (),
        }
    }
    if saw_empty {
        return Err("Docker returned empty views; no containers seen within 90 s");
    }
    panic!("timed out waiting for first {focus:?} event");
}

async fn measure(target: &Target, rtt_ms: u64) -> Run {
    let proxy = Proxy::start(&target.host, target.port, Duration::from_millis(rtt_ms) / 2).await.expect("start proxy");
    let mut login = target.login.clone();
    login.port = proxy.addr.port();
    let route = Route::direct(login);
    let env = support::Env::new();
    let options = env.trusting();
    let mut run = Run::new();
    let start = Instant::now();
    let connection = Connection::connect(route, options).await.expect("authenticate through proxy");
    let connected = Instant::now();
    files(&connection, connected, &mut run).await;
    run.insert("connect", Ok(connected - start));
    let probe = overview(&connection, &mut run).await;
    for (warm, services, docker) in
        [(false, "services.first", "docker.first"), (true, "services.prefetched", "docker.prefetched")]
    {
        run.insert(
            services,
            if probe.systemd.as_ref().is_some_and(|systemd| systemd.running) {
                first_page(&connection, Focus::Services, warm).await
            } else {
                Err("no running systemd")
            },
        );
        run.insert(
            docker,
            if probe.usable_container_runtime().is_some_and(|runtime| runtime.kind == RuntimeKind::Docker) {
                first_page(&connection, Focus::Docker, warm).await
            } else {
                Err("no usable Docker runtime")
            },
        );
    }
    connection.disconnect().await;
    drop(connection);
    if let Err(err) = proxy.finish().await {
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
            ),
            "drain proxy: {err}"
        );
    }
    run
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs shellmate-e2e and shellmate-mon; see module docs"]
async fn first_open() {
    let rtts: Vec<u64> = setting("SHELLMATE_BENCH_RTTS", "0,100,250")
        .split(',')
        .map(|rtt| rtt.trim().parse().expect("SHELLMATE_BENCH_RTTS: expected comma-separated milliseconds"))
        .collect();
    let runs: usize = setting("SHELLMATE_BENCH_RUNS", "5").parse().expect("SHELLMATE_BENCH_RUNS: expected a count");
    assert!(runs > 0, "SHELLMATE_BENCH_RUNS must be positive");
    let mut rows = Vec::new();
    for target in [
        Target::new("alpine", "E2E", "127.0.0.1", "2222"),
        Target::new("ubuntu", "MON", "shellmate-mon.orb.local", "22"),
    ] {
        for &rtt in &rtts {
            let mut samples = Vec::new();
            for index in 1..=runs {
                eprintln!("{}: added RTT {rtt} ms, run {index}/{runs}", target.name);
                samples.push(measure(&target, rtt).await);
            }
            eprintln!("target   RTT ms  metric                   median ms      min ms  samples");
            rows.extend(results::report(target.name, rtt, &samples));
        }
    }
    if let Some(path) = env::var_os("SHELLMATE_BENCH_OUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&rows).unwrap()).expect("write SHELLMATE_BENCH_OUT");
    }
}
