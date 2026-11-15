//! The [`Monitor`] of one SSH connection: probes the server once, follows the collector while somebody
//! looks at the monitor page, and runs the actions the user asks for.
//!
//! It mirrors the forwarder of `shellmate-forward`: it is created with the SSH runtime's handle, only
//! holds the connection weakly (the exec channel of the collector holds it for as long as it is read), and
//! reports over an `async-channel`. Every method can be called from any thread. Nothing is sent after
//! [`shutdown`](Monitor::shutdown) or after the last clone is dropped.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shellmate_ssh::{Connection, ExecOptions, SshError, WeakConnection};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::frame::{FrameParser, Item};
use crate::probe::{Probe, parse_probe};
use crate::script::{CollectorConfig, collector_command, new_token, probe_command};
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
    /// The schedule of the collector (its `token` is replaced for every run).
    pub collector: CollectorConfig,
    /// A fixed token for the markers (the in-process tests need to know it); otherwise a new one for
    /// every run.
    pub token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(10),
            stall: Duration::from_secs(20),
            probe_timeout: Duration::from_secs(25),
            action_timeout: Duration::from_secs(30),
            healthy_run: Duration::from_secs(30),
            retry_delays: [1, 3, 10].map(Duration::from_secs).to_vec(),
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

/// A change the UI should hear about.
#[derive(Clone, Debug)]
pub enum MonitorEvent {
    /// The phase changed.
    Phase(Phase),
    /// The probe is done (once per [`Monitor`]).
    Probe(Arc<Probe>),
    /// New numbers (after every frame: system data every 2 s, processes every 3 s, disks every 30 s).
    Snapshot(Arc<Snapshot>),
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
    phase: Phase,
    probe: Option<Arc<Probe>>,
    engine: Option<Engine>,
    snapshot: Option<Arc<Snapshot>>,
    next_action: u64,
    actions: Vec<AbortOnDrop>,
}

struct Inner {
    runtime: Handle,
    events: async_channel::Sender<MonitorEvent>,
    options: Options,
    slot: Arc<Mutex<Option<WeakConnection>>>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Sends `event` unless the monitor was shut down. Called with the state locked, so that nothing
    /// slips out after `shutdown` has returned.
    fn emit(&self, state: &State, event: MonitorEvent) {
        if !state.closed {
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
        if self.connection().is_none() {
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
        let task = Task {
            inner: Arc::downgrade(self),
            slot: self.slot.clone(),
            generation: state.generation,
            options: self.options.clone(),
        };
        state.run = Some(AbortOnDrop(self.runtime.spawn(task.run())));
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
struct Task {
    inner: std::sync::Weak<Inner>,
    slot: Arc<Mutex<Option<WeakConnection>>>,
    generation: u64,
    options: Options,
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
        let mut failures = 0usize;
        loop {
            // The connection is held while a request to the server is in flight and not while waiting.
            let Some(connection) = self.connection() else { return self.finish(Phase::WaitingReconnect) };

            let probe = match self.with(|_, state| state.probe.clone()) {
                Some(Some(probe)) => probe,
                Some(None) => match self.probe(&connection).await {
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
        let state = State {
            closed: false,
            active: false,
            run_alive: false,
            run: None,
            grace: None,
            generation: 0,
            phase: Phase::Idle,
            probe: None,
            engine: None,
            snapshot: None,
            next_action: 0,
            actions: Vec::new(),
        };
        Self {
            inner: Arc::new(Inner {
                runtime,
                events,
                options,
                slot: Arc::new(Mutex::new(None)),
                state: Mutex::new(state),
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
        state.active = active;
        if active {
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
        *lock(&self.inner.slot) = None;
        // Dropped here, under the lock, so no task can start or report in between.
        let (run, grace, actions) = (state.run.take(), state.grace.take(), std::mem::take(&mut state.actions));
        state.run_alive = false;
        drop(state);
        drop((run, grace, actions));
    }

    /// The newest numbers, if any frame came in yet.
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        lock(&self.inner.state).snapshot.clone()
    }

    /// What the probe found out, once it has run.
    pub fn probe(&self) -> Option<Arc<Probe>> {
        lock(&self.inner.state).probe.clone()
    }

    /// Where the monitor stands.
    pub fn phase(&self) -> Phase {
        lock(&self.inner.state).phase.clone()
    }

    /// Runs `command` (a line for the server's shell, built by [`crate::commands`]) on the connection and
    /// reports how it went as a [`MonitorEvent::Action`]. Works whether or not the collector runs.
    pub fn run_action(&self, command: impl Into<String>) -> ActionId {
        let inner = &self.inner;
        let command = command.into();
        let mut state = lock(&inner.state);
        state.next_action += 1;
        let id = ActionId(state.next_action);
        if state.closed {
            return id;
        }
        state.actions.retain(|task| !task.0.is_finished());
        let (weak, slot, timeout) = (Arc::downgrade(inner), inner.slot.clone(), inner.options.action_timeout);
        let task = inner.runtime.spawn(async move {
            let connection = lock(&slot).as_ref().and_then(WeakConnection::upgrade);
            let outcome = match connection {
                None => ActionOutcome::Error("没有可用的 SSH 连接".into()),
                Some(connection) => {
                    let result = connection.exec_with(&command, ExecOptions { timeout, max_output: 64 << 10 }).await;
                    drop(connection);
                    match result {
                        Ok(output) if output.status.success() => {
                            ActionOutcome::Success { output: cut(&output.stdout_text()) }
                        }
                        Ok(output) => {
                            let message = match output.stderr_text().trim() {
                                "" => output.stdout_text(),
                                stderr => stderr.to_string(),
                            };
                            ActionOutcome::Failure { code: output.status.code(), message: cut(&message) }
                        }
                        Err(err) => ActionOutcome::Error(explain(&err)),
                    }
                }
            };
            if let Some(inner) = weak.upgrade() {
                let state = lock(&inner.state);
                inner.emit(&state, MonitorEvent::Action(ActionReport { id, command, outcome }));
            }
        });
        state.actions.push(AbortOnDrop(task));
        id
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
