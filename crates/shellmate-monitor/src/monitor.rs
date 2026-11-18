//! The [`Monitor`] of one SSH connection: probes the server once, follows the collector while somebody
//! looks at the monitor page, and runs the actions the user asks for.
//!
//! It mirrors the forwarder of `shellmate-forward`: it is created with the SSH runtime's handle, only
//! holds the connection weakly (the exec channel of the collector holds it for as long as it is read), and
//! reports over an `async-channel`. Every method can be called from any thread. Nothing is sent after
//! [`shutdown`](Monitor::shutdown) or after the last clone is dropped.

mod pollers;
#[cfg(target_os = "macos")]
mod native;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shellmate_ssh::{Connection, ExecOptions, SshError, WeakConnection};
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::docker::{Dangling, DockerView, Usage};
use crate::engine::Engine;
use crate::frame::{FrameParser, Item};
use crate::probe::{Probe, parse_probe};
use crate::script::{CollectorConfig, collector_command, new_token, probe_command};
use crate::services::ServiceList;
use crate::snapshot::Snapshot;

/// How much of a command's output is kept in an [`ActionReport`], in characters.
const REPORT_CHARS: usize = 2000;

/// Settings; the defaults are what the app uses and the tests use shorter times.
#[derive(Clone, Debug)]
pub struct Options {
    /// How long collecting goes on after the last viewer left (so that switching between tabs does not
    /// start and stop the collector over and over).
    pub grace: Duration,
    /// How long the collector may be silent (it writes a line every second) before it counts as stuck
    /// and is started again.
    pub stall: Duration,
    /// How long the probe may take.
    pub probe_timeout: Duration,
    /// How long an action may take.
    pub action_timeout: Duration,
    /// A collector that ran this long and wrote frames was working: when it ends the failures before it are
    /// forgotten (otherwise a server that drops the channel every few minutes would run out of retries).
    pub healthy_run: Duration,
    /// How long to wait before starting again after the probe or the collector failed, one delay per
    /// attempt; when they are used up the monitor gives up ([`Phase::Failed`]) until it is asked again.
    pub retry_delays: Vec<Duration>,
    /// How often the services and the containers are asked about while their page is in front.
    pub poll_visible: Duration,
    /// And while another page is in front (the overview and the header still say how many failed).
    pub poll_hidden: Duration,
    /// How long after a container event the containers are asked again (a restart makes several events).
    pub debounce: Duration,
    /// How often the disk usage of docker (slow to work out) is asked again.
    pub usage_every: Duration,
    /// The schedule of the collector (its `token` is replaced for every run).
    pub collector: CollectorConfig,
    /// A fixed token for the markers (the in-process tests need to know it); otherwise a new one for
    /// every run.
    pub token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            stall: Duration::from_secs(20),
            probe_timeout: Duration::from_secs(25),
            action_timeout: Duration::from_secs(30),
            healthy_run: Duration::from_secs(30),
            retry_delays: [1, 3, 10].map(Duration::from_secs).to_vec(),
            poll_visible: Duration::from_secs(10),
            poll_hidden: Duration::from_secs(30),
            debounce: Duration::from_millis(300),
            usage_every: Duration::from_secs(60),
            collector: CollectorConfig::new(""),
            token: None,
        }
    }
}

/// Where the monitor stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not collecting: nobody is looking, or nothing was asked yet.
    Idle,
    /// Probing the server or starting the collector.
    Starting,
    /// The collector is running and frames come in.
    Collecting,
    /// The connection is gone; the last numbers stay and collecting goes on after the reconnect.
    WaitingReconnect,
    /// The server cannot be monitored (not Linux, no `/proc`); the text says why.
    Unsupported(String),
    /// The probe or the collector failed repeatedly; the text says why. Asking again starts over.
    Failed(String),
}

/// Which sub-page of the monitor page is in front: what is asked about depends on it (the container
/// statistics only run while the Docker page is open, the services and containers are asked about more
/// often while theirs is).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// 概览
    Overview,
    /// 进程
    Processes,
    /// Local listening ports.
    Ports,
    /// 服务
    Services,
    /// Docker
    Docker,
}

/// What [`Monitor::refresh`] asks again at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topic {
    /// The list of services.
    Services,
    /// The list of containers (and the disk usage of docker).
    Docker,
}

/// A change the UI should hear about.
#[derive(Clone, Debug)]
pub enum MonitorEvent {
    /// The phase changed.
    Phase(Phase),
    /// The probe is done (once per [`Monitor`]).
    Probe(Arc<Probe>),
    /// New numbers (after every frame: system data every 2 s, processes every 3 s, disks every 30 s).
    Snapshot(Arc<Snapshot>),
    /// The local listener list (or its refresh error) changed.
    Ports,
    /// The list of services changed.
    Services(Arc<ServiceList>),
    /// The containers (or what is known about them) changed.
    Docker(Arc<DockerView>),
    /// An action finished.
    Action(ActionReport),
}

/// Identifies an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ActionId(pub u64);

/// How an action ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The command exited with status 0; `output` is what it printed to stdout (cut at 2000 characters).
    Success {
        /// stdout, trimmed.
        output: String,
    },
    /// The command exited with another status; `message` is its stderr, or stdout if that is empty.
    Failure {
        /// The exit status, if there was one (not after a signal).
        code: Option<u32>,
        /// What it said, trimmed and cut at 2000 characters.
        message: String,
    },
    /// The command could not be run: there is no connection, it took too long, the connection dropped.
    Error(String),
    /// A local process action was denied. Offer the sudo line in the originating terminal.
    Terminal(crate::commands::Plan),
}

/// The report of an action: what was run and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionReport {
    /// Which action.
    pub id: ActionId,
    /// The command line that was run.
    pub command: String,
    /// How it went.
    pub outcome: ActionOutcome,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct State {
    /// After `shutdown`: nothing starts and nothing is reported any more.
    closed: bool,
    /// Somebody is looking at the monitor page.
    active: bool,
    /// The collecting task is running (it ends by itself when it gives up or the connection is gone).
    run_alive: bool,
    run: Option<AbortOnDrop>,
    /// The timer that stops collecting after the last viewer left.
    grace: Option<AbortOnDrop>,
    /// Which run is current; tasks of older runs change nothing.
    generation: u64,
    probe_cell: Arc<tokio::sync::OnceCell<Arc<Probe>>>,
    phase: Phase,
    probe: Option<Arc<Probe>>,
    engine: Option<Engine>,
    snapshot: Option<Arc<Snapshot>>,
    ports: Option<Arc<crate::ports::PortList>>,
    #[cfg(target_os = "macos")]
    ports_task: Option<AbortOnDrop>,
    #[cfg(target_os = "macos")]
    services_task: Option<(Focus, AbortOnDrop)>,
    services: Option<Arc<ServiceList>>,
    docker: Option<pollers::DockerState>,
    next_action: u64,
    actions: Vec<AbortOnDrop>,
}

struct Inner {
    local: bool,
    runtime: Handle,
    events: async_channel::Sender<MonitorEvent>,
    options: Options,
    slot: Arc<Mutex<Option<WeakConnection>>>,
    state: Mutex<State>,
    /// The sub-page in front.
    focus: watch::Sender<Focus>,
    wake_services: Arc<Notify>,
    #[cfg(target_os = "macos")]
    inventory: Arc<tokio::sync::Mutex<crate::local::services::Inventory>>,
    wake_docker: Arc<Notify>,
    /// The next look at the containers also asks for the disk usage (somebody pressed a button).
    docker_full: Arc<AtomicBool>,
    disconnect: watch::Sender<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Sends `event` unless the monitor was shut down. Called with the state locked, so that nothing
    /// slips out after `shutdown` has returned.
    fn emit(&self, state: &State, event: MonitorEvent) {
        if !state.closed
            && (state.run_alive
                || state.active
                || !matches!(event, MonitorEvent::Probe(_) | MonitorEvent::Services(_) | MonitorEvent::Docker(_)))
        {
            // A UI that stopped listening is not an error.
            let _ = self.events.try_send(event);
        }
    }

    fn set_phase(&self, state: &mut State, phase: Phase) {
        if state.phase != phase {
            state.phase = phase.clone();
            self.emit(state, MonitorEvent::Phase(phase));
        }
    }

    fn connection(&self) -> Option<Connection> {
        lock(&self.slot).as_ref().and_then(WeakConnection::upgrade)
    }

    /// Starts collecting if somebody looks, nothing runs and there is a connection.
    fn ensure_running(self: &Arc<Self>, state: &mut State) {
        if state.closed || !state.active || state.run_alive {
            return;
        }
        if !self.local && self.connection().is_none() {
            self.set_phase(state, Phase::WaitingReconnect);
            return;
        }
        state.generation += 1;
        state.run_alive = true;
        if let Some(engine) = state.engine.as_mut() {
            // Whatever time passed since the last sample is not a rate.
            engine.reset_baselines();
        }
        self.set_phase(state, Phase::Starting);
        let task = self.task(state);
        state.run = Some(AbortOnDrop(self.runtime.spawn(task.run())));
    }

    fn task(self: &Arc<Self>, state: &State) -> Task {
        Task {
            local: self.local,
            inner: Arc::downgrade(self),
            slot: self.slot.clone(),
            generation: state.generation,
            options: self.options.clone(),
            probe_cell: state.probe_cell.clone(),
        }
    }

    /// The grace period after the last viewer left is over.
    fn grace_over(&self, generation: u64) {
        let mut state = lock(&self.state);
        if state.closed || state.active || state.generation != generation || !state.run_alive {
            return;
        }
        state.run = None;
        state.run_alive = false;
        state.grace = None;
        self.set_phase(&mut state, Phase::Idle);
    }
}

/// The task of one run: probe if needed, then follow the collector until it ends.
#[derive(Clone)]
struct Task {
    local: bool,
    inner: std::sync::Weak<Inner>,
    slot: Arc<Mutex<Option<WeakConnection>>>,
    generation: u64,
    options: Options,
    probe_cell: Arc<tokio::sync::OnceCell<Arc<Probe>>>,
}

impl Task {
    /// Runs `change` on the state if the monitor is there and this run is still the current one.
    fn with<R>(&self, change: impl FnOnce(&Arc<Inner>, &mut State) -> R) -> Option<R> {
        let inner = self.inner.upgrade()?;
        let mut state = lock(&inner.state);
        if state.closed || state.generation != self.generation {
            return None;
        }
        Some(change(&inner, &mut state))
    }

    /// The run is over (and the phase says why): nothing runs until somebody asks again.
    fn finish(&self, phase: Phase) {
        self.with(|inner, state| {
            state.run_alive = false;
            inner.set_phase(state, phase);
        });
    }

    fn connection(&self) -> Option<Connection> {
        lock(&self.slot).as_ref().and_then(WeakConnection::upgrade)
    }

    async fn run(self) {
        #[cfg(target_os = "macos")]
        if self.local { return self.run_local().await; }
        let mut failures = 0usize;
        // The tasks that ask about services and containers live as long as this run does.
        let mut watchers: Option<Vec<AbortOnDrop>> = None;
        loop {
            // The connection is held while a request to the server is in flight and not while waiting.
            let Some(connection) = self.connection() else { return self.finish(Phase::WaitingReconnect) };

            let probe = match self.with(|_, state| state.probe.clone()) {
                Some(Some(probe)) => probe,
                Some(None) => match self.shared_probe(&connection).await {
                    Ok(probe) => {
                        let stored = self.with(|inner, state| {
                            state.probe = Some(probe.clone());
                            state.engine = Some(Engine::new(probe.clone()));
                            inner.emit(state, MonitorEvent::Probe(probe.clone()));
                        });
                        if stored.is_none() {
                            return;
                        }
                        probe
                    }
                    Err(why) => {
                        if !self.retry(&mut failures, why).await {
                            return;
                        }
                        continue;
                    }
                },
                None => return,
            };
            if watchers.is_none() && probe.can_collect() {
                watchers = Some(self.start_pollers(&probe));
            }
            if !probe.can_collect() {
                let what = if probe.kernel.is_empty() {
                    "无法识别服务器的系统".to_string()
                } else {
                    probe.kernel.clone()
                };
                return self.finish(Phase::Unsupported(format!("暂只支持 Linux 主机的监控（{what}）")));
            }

            let mut config = self.options.collector.clone();
            config.token = self.options.token.clone().unwrap_or_else(new_token);
            let stream = connection.exec_stream(&collector_command(&config)).await;
            drop(connection);
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    if !self.retry(&mut failures, explain(&err)).await {
                        return;
                    }
                    continue;
                }
            };
            if self.with(|inner, state| inner.set_phase(state, Phase::Collecting)).is_none() {
                return;
            }

            let mut parser = FrameParser::new(&config.token);
            let started = std::time::Instant::now();
            let mut got_frame = false;
            let ended = loop {
                match tokio::time::timeout(self.options.stall, stream.next_line()).await {
                    Ok(Some(line)) => match parser.push(&line) {
                        Some(Item::Frame(frame)) => {
                            got_frame = true;
                            let applied = self.with(|inner, state| {
                                let Some(engine) = state.engine.as_mut() else { return };
                                let snapshot = engine.apply(&frame);
                                state.snapshot = Some(snapshot.clone());
                                inner.emit(state, MonitorEvent::Snapshot(snapshot));
                            });
                            if applied.is_none() {
                                return;
                            }
                        }
                        Some(Item::Tick(_)) | None => {}
                    },
                    Ok(None) => break "ended",
                    Err(_) => break "stalled",
                }
            };
            let tail = stream.stderr_tail();
            drop(stream);
            log::debug!("监控采集通道结束（{ended}，收到过帧：{got_frame}）");
            if got_frame && started.elapsed() >= self.options.healthy_run {
                failures = 0;
            }

            if self.connection().is_none() {
                return self.finish(Phase::WaitingReconnect);
            }
            let why = match (ended, tail.trim()) {
                ("stalled", _) => "采集脚本长时间没有输出".to_string(),
                (_, "") => "采集脚本意外结束".to_string(),
                (_, tail) => format!("采集脚本意外结束：{}", tail.chars().take(300).collect::<String>()),
            };
            if !self.retry(&mut failures, why).await {
                return;
            }
        }
    }

    /// Notes a failure; waits before the next attempt, or gives up (`false`).
    async fn retry(&self, failures: &mut usize, why: String) -> bool {
        let Some(delay) = self.options.retry_delays.get(*failures).copied() else {
            self.finish(Phase::Failed(why));
            return false;
        };
        *failures += 1;
        if self.with(|inner, state| inner.set_phase(state, Phase::Starting)).is_none() {
            return false;
        }
        tokio::time::sleep(delay).await;
        true
    }

    async fn shared_probe(&self, connection: &Connection) -> Result<Arc<Probe>, String> {
        self.probe_cell.get_or_try_init(|| self.probe(connection)).await.cloned()
    }

    async fn probe(&self, connection: &Connection) -> Result<Arc<Probe>, String> {
        let token = self.options.token.clone().unwrap_or_else(new_token);
        let options = ExecOptions { timeout: self.options.probe_timeout, max_output: 512 << 10 };
        let output = connection.exec_with(&probe_command(&token), options).await.map_err(|err| explain(&err))?;
        let text = output.stdout_text();
        if !text.contains(&format!("@@{token} ")) {
            return Err("探测没有得到结果（服务器的登录 shell 无法运行 sh 脚本？）".into());
        }
        Ok(Arc::new(parse_probe(&token, &text)))
    }
}

fn explain(err: &SshError) -> String {
    err.to_string()
}

/// What a followed command says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowEvent {
    /// A line of its output (stdout; add `2>&1` to the command for stderr).
    Line(String),
    /// The command is over, or could not be run; the text says why (it may be empty).
    Ended(String),
}

/// A command that is followed: the lines come out of [`events`](Follow::events). Dropping it closes the
/// channel on the server.
pub struct Follow {
    /// The lines and the end.
    pub events: async_channel::Receiver<FollowEvent>,
    _task: AbortOnDrop,
}

/// The monitor of one connection. Cheap to clone; all clones are the same monitor. Dropping the last
/// clone shuts it down.
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<Inner>,
}

impl Monitor {
    /// A monitor with no connection that collects nothing yet. Events go to `events`.
    pub fn new(runtime: Handle, events: async_channel::Sender<MonitorEvent>) -> Self {
        Self::with_options(runtime, events, Options::default())
    }

    /// Like [`new`](Self::new) with other settings.
    pub fn with_options(runtime: Handle, events: async_channel::Sender<MonitorEvent>, options: Options) -> Self {
        Self::backend(runtime, events, options, false)
    }

    /// Creates a dormant macOS monitor; it never opens an SSH connection or prefetches local data.
    #[cfg(target_os = "macos")]
    pub fn local(runtime: Handle, events: async_channel::Sender<MonitorEvent>) -> Self {
        Self::backend(runtime, events, Options::default(), true)
    }

    fn backend(runtime: Handle, events: async_channel::Sender<MonitorEvent>, options: Options, local: bool) -> Self {
        let state = State {
            closed: false,
            active: false,
            run_alive: false,
            run: None,
            grace: None,
            generation: 0,
            probe_cell: Arc::default(),
            phase: Phase::Idle,
            probe: None,
            engine: None,
            snapshot: None,
            ports: local.then(Arc::default),
            #[cfg(target_os = "macos")]
            ports_task: None,
            #[cfg(target_os = "macos")]
            services_task: None,
            services: None,
            docker: None,
            next_action: 0,
            actions: Vec::new(),
        };
        Self {
            inner: Arc::new(Inner {
                local,
                runtime,
                events,
                options,
                slot: Arc::new(Mutex::new(None)),
                state: Mutex::new(state),
                disconnect: watch::channel(()).0,
                focus: watch::channel(Focus::Overview).0,
                wake_services: Arc::new(Notify::new()),
                #[cfg(target_os = "macos")]
                inventory: Arc::default(),
                wake_docker: Arc::new(Notify::new()),
                docker_full: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Says whether somebody looks at the monitor page of this connection. While somebody does, the
    /// server is probed (once) and the collector runs; when the last viewer leaves, collecting goes on for
    /// [`Options::grace`] and then stops (the channel is closed and the script ends on the server).
    pub fn set_active(&self, active: bool) {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return;
        }
        let was_active = std::mem::replace(&mut state.active, active);
        #[cfg(target_os = "macos")]
        if inner.local { inner.sync_native(&mut state); }
        if active {
            if !was_active {
                if let Some(probe) = &state.probe {
                    inner.emit(&state, MonitorEvent::Probe(probe.clone()));
                }
                if let Some(services) = &state.services {
                    inner.emit(&state, MonitorEvent::Services(services.clone()));
                }
                if let Some(view) = state.docker.as_ref().and_then(|docker| docker.view()) {
                    inner.emit(&state, MonitorEvent::Docker(view));
                }
            }
            state.grace = None;
            inner.ensure_running(&mut state);
        } else if state.run_alive && state.grace.is_none() {
            let (weak, generation, grace) = (Arc::downgrade(inner), state.generation, inner.options.grace);
            state.grace = Some(AbortOnDrop(inner.runtime.spawn(async move {
                tokio::time::sleep(grace).await;
                if let Some(inner) = weak.upgrade() {
                    inner.grace_over(generation);
                }
            })));
        }
    }

    /// There is a connection (the first one, or a new one after a loss). Collecting starts (again) if
    /// somebody is looking.
    pub fn connection_restored(&self, connection: &Connection) {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return;
        }
        *lock(&inner.slot) = Some(connection.downgrade());
        inner.ensure_running(&mut state);
    }

    /// The SSH connection is gone (or going): collecting stops, the last numbers stay.
    pub fn connection_lost(&self) {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return;
        }
        *lock(&inner.slot) = None;
        state.generation += 1;
        state.probe_cell = Arc::default();
        inner.disconnect.send_replace(());
        state.run = None;
        state.run_alive = false;
        state.actions.clear();
        if state.active || matches!(state.phase, Phase::Starting | Phase::Collecting) {
            inner.set_phase(&mut state, Phase::WaitingReconnect);
        }
    }

    /// Asks again after the monitor gave up ([`Phase::Failed`]); does nothing otherwise.
    pub fn restart(&self) {
        let mut state = lock(&self.inner.state);
        if !state.closed && matches!(state.phase, Phase::Failed(_)) {
            self.inner.ensure_running(&mut state);
        }
    }

    /// Stops everything: the collector's channel is closed, running actions are abandoned, and nothing is
    /// reported any more, not even by tasks that were in flight. The connection is not held by the monitor
    /// afterwards, so it closes with its last tab.
    pub fn shutdown(&self) {
        let mut state = lock(&self.inner.state);
        state.closed = true;
        #[cfg(target_os = "macos")]
        { state.ports_task = None; state.services_task = None; }
        self.inner.disconnect.send_replace(());
        *lock(&self.inner.slot) = None;
        // Dropped here, under the lock, so no task can start or report in between.
        let (run, grace, actions) = (state.run.take(), state.grace.take(), std::mem::take(&mut state.actions));
        state.run_alive = false;
        drop(state);
        drop((run, grace, actions));
    }

    /// Warm the inactive monitor once, without starting a collector, events or stats streams.
    pub async fn prefetch(&self) {
        let (task, connection, mut disconnect) = {
            let state = lock(&self.inner.state);
            if state.closed || state.run_alive || state.active {
                return;
            }
            let Some(connection) = self.inner.connection() else { return };
            (self.inner.task(&state), connection, self.inner.disconnect.subscribe())
        };
        let warm = async {
            let probe = match task.with(|_, state| state.probe.clone()) {
                Some(Some(probe)) => probe,
                Some(None) => {
                    let Ok(probe) = task.shared_probe(&connection).await else { return };
                    if task
                        .with(|_, state| {
                            state.probe = Some(probe.clone());
                            state.engine = Some(Engine::new(probe.clone()));
                        })
                        .is_none()
                    {
                        return;
                    }
                    probe
                }
                None => return,
            };
            if probe.can_collect() {
                task.prefetch_pages(&probe).await;
            }
        };
        tokio::select! { biased; _ = disconnect.changed() => {}, () = warm => {} }
    }

    /// The newest numbers, if any frame came in yet.
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        lock(&self.inner.state).snapshot.clone()
    }

    /// `Some` on a local backend, including before the first visit to the page.
    pub fn ports(&self) -> Option<Arc<crate::ports::PortList>> {
        lock(&self.inner.state).ports.clone()
    }

    /// What the probe found out, once it has run.
    pub fn probe(&self) -> Option<Arc<Probe>> {
        lock(&self.inner.state).probe.clone()
    }

    /// Where the monitor stands.
    pub fn phase(&self) -> Phase {
        lock(&self.inner.state).phase.clone()
    }

    /// The newest list of services (once systemd has been asked; `None` on a server without systemd).
    pub fn services(&self) -> Option<Arc<ServiceList>> {
        lock(&self.inner.state).services.clone()
    }

    /// The newest view of the containers (`None` on a server without a usable docker or podman).
    pub fn docker(&self) -> Option<Arc<DockerView>> {
        lock(&self.inner.state).docker.as_ref().and_then(|docker| docker.view())
    }

    /// Says which sub-page is in front. The services and containers are asked about every
    /// [`Options::poll_visible`] seconds while theirs is, and the container statistics run only while the
    /// Docker page is.
    pub fn set_focus(&self, focus: Focus) {
        if self.inner.focus.send_if_modified(|current| std::mem::replace(current, focus) != focus) {
            #[cfg(target_os = "macos")]
            if self.inner.local { self.inner.sync_native(&mut lock(&self.inner.state)); }
        }
    }

    /// Asks again at once (after an action, when a page is opened).
    pub fn refresh(&self, topic: Topic) {
        match topic {
            Topic::Services => self.inner.wake_services.notify_one(),
            Topic::Docker => {
                self.inner.docker_full.store(true, std::sync::atomic::Ordering::Relaxed);
                self.inner.wake_docker.notify_one();
            }
        }
    }

    /// Runs `command` on its own exec channel and hands over its output line by line (a followed log). The
    /// channel closes when the [`Follow`] is dropped; if there is no connection it ends at once.
    pub fn follow(&self, command: impl Into<String>) -> Follow {
        pollers::follow(&self.inner, command.into())
    }

    /// Runs `command` (a line for the server's shell, built by [`crate::commands`]) on the connection and
    /// reports how it went as a [`MonitorEvent::Action`]. Works whether or not the collector runs.
    pub fn run_action(&self, command: impl Into<String>) -> ActionId {
        let command = command.into();
        let timeout = self.inner.options.action_timeout;
        let slot = self.inner.slot.clone();
        self.spawn_action(command.clone(), {
            #[cfg(target_os = "macos")]
            let local = self.inner.local;
            async move {
                #[cfg(target_os = "macos")]
                if local {
                    use crate::local::runner;
                    return outcome(runner::output(&mut runner::shell(&command), timeout, 64 << 10).await.map_err(|err| err.to_string()));
                }
                let connection = lock(&slot).as_ref().and_then(WeakConnection::upgrade);
                match connection {
                    Some(connection) => outcome(connection.exec_with(&command, ExecOptions { timeout, max_output: 64 << 10 }).await.map_err(|err| explain(&err))),
                    None => ActionOutcome::Error("没有可用的 SSH 连接".into()),
                }
            }
        })
    }

    /// Runs validated process-action words directly on macOS (never via a shell).
    #[cfg(target_os = "macos")]
    pub fn run_local_process(&self, words: crate::commands::Command) -> ActionId {
        let timeout = self.inner.options.action_timeout;
        self.spawn_action(words.line(), crate::local::runner::process_action(words, move |mut cmd| async move {
            crate::local::runner::output(&mut cmd, timeout, 64 << 10).await
        }))
    }

    fn spawn_action(&self, command: String, action: impl Future<Output = ActionOutcome> + Send + 'static) -> ActionId {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        state.next_action += 1;
        let id = ActionId(state.next_action);
        if state.closed {
            return id;
        }
        state.actions.retain(|task| !task.0.is_finished());
        let weak = Arc::downgrade(inner);
        let task = inner.runtime.spawn(async move {
            let outcome = action.await;
            if let Some(inner) = weak.upgrade() {
                let state = lock(&inner.state);
                inner.emit(&state, MonitorEvent::Action(ActionReport { id, command, outcome }));
            }
        });
        state.actions.push(AbortOnDrop(task));
        id
    }
}

pub(crate) fn outcome(result: Result<shellmate_ssh::ExecOutput, String>) -> ActionOutcome {
    match result {
        Ok(out) if out.status.success() => ActionOutcome::Success { output: cut(&out.stdout_text()) },
        Ok(out) => {
            let stderr = out.stderr_text();
            let message = if stderr.trim().is_empty() { out.stdout_text() } else { stderr };
            ActionOutcome::Failure { code: out.status.code(), message: cut(&message) }
        }
        Err(err) => ActionOutcome::Error(err),
    }
}

/// Trimmed, and at most [`REPORT_CHARS`] characters.
fn cut(text: &str) -> String {
    text.trim().chars().take(REPORT_CHARS).collect()
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Monitor({:?})", lock(&self.inner.state).phase)
    }
}
