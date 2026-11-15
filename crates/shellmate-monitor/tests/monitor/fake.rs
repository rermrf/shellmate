//! A pretend server: answers the probe, plays the collector, runs actions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::Receiver;
use shellmate_core::AuthMethod;
use shellmate_monitor::{Monitor, MonitorEvent, Options};
use shellmate_ssh::{Connection, Route, Secret, SshHost};

use crate::support::{Env, ExecChannel, ServerOptions, TestServer, eventually, exec_runner, lock};

pub const TOKEN: &str = "TESTTOK";

/// What the collector does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Collector {
    /// Frames for as long as somebody reads.
    Steady,
    /// A few frames, then it dies with a message on stderr (and the channel closes).
    DiesAfter(usize),
    /// A tick line and a frame, then nothing at all (a stuck `df`).
    Stuck,
}

pub struct FakeState {
    pub probe_text: Mutex<String>,
    pub collector: Mutex<Collector>,
    pub probes: AtomicUsize,
    pub collectors_started: AtomicUsize,
    pub collectors_running: AtomicUsize,
    /// How many frames the collectors wrote in all.
    pub frames: AtomicUsize,
    pub actions: Mutex<Vec<String>>,
}

pub fn linux_probe(sudo: &str) -> String {
    format!(
        "@@{TOKEN} os\nPRETTY_NAME=\"Test Linux 1\"\n@@{TOKEN} kernel\nLinux 6.1.0\n@@{TOKEN} procfs\nyes\n@@{TOKEN} uptime\n5000.5\n@@{TOKEN} user\n1000\nalice\n@@{TOKEN} sudo\n{sudo}\n@@{TOKEN} systemd\n\n@@{TOKEN} containers\n@@{TOKEN} page\n4096\n@@{TOKEN} clk\n100\n@@{TOKEN} passwd\nroot:x:0:0::/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\n@@{TOKEN} end\n"
    )
}

pub fn darwin_probe() -> String {
    format!(
        "@@{TOKEN} kernel\nDarwin 23.1.0\n@@{TOKEN} procfs\n@@{TOKEN} user\n501\nme\n@@{TOKEN} sudo\npassword\n@@{TOKEN} end\n"
    )
}

/// The frames of tick `n` of a steady collector: the CPU is half busy, a process uses a core.
fn frames(n: usize) -> String {
    let p = format!("@@{TOKEN} ");
    let mut text = format!("{p}tick {n}\n");
    if n.is_multiple_of(2) {
        let stamp = n as f64;
        let busy = n * 100;
        text += &format!(
            "{p}stats {stamp}\n{p}cpu\ncpu  {busy} 0 0 {busy} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {busy} 0 0 0 0 0 0\n{p}load\n0.10 0.20 0.30 1/200 5\n{p}mem\nMemTotal: 1000\nMemFree: 500\nMemAvailable: 600\nBuffers: 0\nCached: 100\nSwapTotal: 0\nSwapFree: 0\n{p}net\ndefaultif eth0\neth0: {rx} 1 0 0 0 0 0 0 {tx} 1 0 0 0 0 0 0\n{p}thermal\n{p}end\n",
            rx = n * 1000,
            tx = n * 100
        );
    }
    if n.is_multiple_of(3) {
        let stamp = n as f64;
        let ticks = n * 100;
        text += &format!(
            "{p}procs {stamp}\n{p}stat\n1 (init) S 0 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 5 1000 10 0\n{p}ps\n    1  1000 /sbin/init\n{p}end\n"
        );
    }
    if n % 30 == 1 {
        text += &format!(
            "{p}slow {n}\n{p}df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 1000 900 100 90% /\n{p}end\n",
            n = n
        );
    }
    text
}

/// A server that plays the part of a Linux (or other) machine for the monitor.
pub async fn server(probe: String) -> (TestServer, Arc<FakeState>) {
    let state = Arc::new(FakeState {
        probe_text: Mutex::new(probe),
        collector: Mutex::new(Collector::Steady),
        probes: AtomicUsize::new(0),
        collectors_started: AtomicUsize::new(0),
        collectors_running: AtomicUsize::new(0),
        frames: AtomicUsize::new(0),
        actions: Mutex::new(Vec::new()),
    });
    let fake = state.clone();
    let runner = exec_runner(move |command, channel: ExecChannel| {
        let fake = fake.clone();
        async move {
            if command.contains("# shellmate-probe") {
                fake.probes.fetch_add(1, Ordering::SeqCst);
                let text = lock(&fake.probe_text).clone();
                channel.stdout(text).await;
                channel.exit(0).await;
            } else if command.contains("# shellmate-collector") {
                let nth = fake.collectors_started.fetch_add(1, Ordering::SeqCst);
                fake.collectors_running.fetch_add(1, Ordering::SeqCst);
                let mode = *lock(&fake.collector);
                let mut n = 0;
                loop {
                    let alive = match mode {
                        Collector::Stuck if n >= 2 => {
                            // Silent from now on, until the client gives up on it.
                            channel.wait_closed().await;
                            false
                        }
                        Collector::DiesAfter(limit) if n >= limit && nth == 0 => {
                            channel.stderr("sh: ps: not found\n").await;
                            channel.exit(1).await;
                            false
                        }
                        _ => {
                            fake.frames.fetch_add(1, Ordering::SeqCst);
                            channel.stdout(frames(n)).await
                        }
                    };
                    if !alive {
                        break;
                    }
                    n += 1;
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                fake.collectors_running.fetch_sub(1, Ordering::SeqCst);
            } else {
                lock(&fake.actions).push(command.clone());
                if command.contains("hang") {
                    channel.wait_closed().await;
                } else if command.contains("fail") {
                    channel.stderr("boom: permission denied\n").await;
                    channel.exit(2).await;
                } else {
                    channel.stdout("done\n").await;
                    channel.exit(0).await;
                }
            }
        }
    });
    let server = TestServer::start(ServerOptions { exec: Some(runner), ..Default::default() }).await;
    (server, state)
}

pub async fn connect(server: &TestServer, env: &Env) -> Connection {
    let host =
        SshHost::new("127.0.0.1", server.port, "alice", AuthMethod::Password).with_password(Secret::new("secret"));
    Connection::connect(Route::direct(host), env.trusting()).await.unwrap()
}

/// Short times for the tests.
pub fn quick() -> Options {
    Options {
        grace: Duration::from_millis(300),
        stall: Duration::from_millis(400),
        probe_timeout: Duration::from_secs(5),
        action_timeout: Duration::from_millis(600),
        healthy_run: Duration::from_secs(1),
        retry_delays: vec![Duration::from_millis(50), Duration::from_millis(50)],
        token: Some(TOKEN.into()),
        ..Options::default()
    }
}

pub fn monitor(options: Options) -> (Monitor, Receiver<MonitorEvent>) {
    let (events, receiver) = async_channel::unbounded();
    (Monitor::with_options(tokio::runtime::Handle::current(), events, options), receiver)
}

/// Waits for an event the check accepts (the others are passed over).
pub async fn next_event<T>(
    events: &Receiver<MonitorEvent>,
    what: &str,
    mut check: impl FnMut(&MonitorEvent) -> Option<T>,
) -> T {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(found) = check(&event) {
                        return found;
                    }
                }
                Err(_) => panic!("the event channel closed while waiting for {what}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

pub async fn until(what: &str, check: impl FnMut() -> bool) {
    let mut check = check;
    eventually(what, || check().then_some(())).await;
}
