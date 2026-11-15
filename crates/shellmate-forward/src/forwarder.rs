//! The forwarder of one SSH connection: starts and stops rules and follows the connection
//! through losses and reconnects.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shellmate_ssh::Connection;
use tokio::net::TcpListener;
use tokio::runtime::Handle;

use crate::conn::{AbortOnDrop, ConnSlot, Mode, RemoteRun, Update, spawn_listener};
use crate::listen;
use crate::retry::{GrantMemory, RetryTiming};
use crate::rule::{ForwardKind, ForwardRule, RuleId};
use crate::state::{ForwardEvent, RuleError, RuleState, RuleView};
use crate::stats::{RuleStats, StatsSnapshot};

/// How often changed numbers are reported (at most).
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// How long after a rule released a local port the same port is retried when it still looks
/// taken: the listener is closed by its task a moment after the rule is stopped.
const RELEASE_GRACE: Duration = Duration::from_secs(2);
const RELEASE_RETRY: Duration = Duration::from_millis(5);
/// How long cancelling a remote forward at the server may take.
const CANCEL_LIMIT: Duration = Duration::from_secs(3);

/// What a [`Forwarder`] can be told besides the runtime and the channel for its events.
#[derive(Clone, Debug)]
pub struct Options {
    /// How often changed numbers are reported (at most).
    pub stats_interval: Duration,
    /// What the servers granted so far; give the forwarders of all connections to a host the same
    /// memory, so that a rule that moves to another connection is still known to have worked.
    pub grants: GrantMemory,
    /// How long the engine waits before it asks for a remote port again.
    pub retry: RetryTiming,
}

impl Default for Options {
    fn default() -> Self {
        Self { stats_interval: STATS_INTERVAL, grants: GrantMemory::new(), retry: RetryTiming::default() }
    }
}

/// A started rule.
pub(crate) struct Entry {
    rule: ForwardRule,
    state: RuleState,
    stats: Arc<RuleStats>,
    /// Which run of a remote forward this entry belongs to; tasks of older runs are ignored.
    generation: u64,
    /// The accept loop of a local / dynamic forward, or the task of a remote one.
    task: Option<AbortOnDrop>,
    /// What the server listens on for a running remote forward (address as asked, actual port).
    pub(crate) remote: Option<(String, u16)>,
    /// The local port a local / dynamic forward holds.
    bound: Option<u16>,
    /// What the last statistics event said.
    reported: StatsSnapshot,
}

impl Entry {
    pub(crate) fn set_state(&mut self, state: RuleState) {
        self.state = state;
    }
}

#[derive(Default)]
struct State {
    /// After `shutdown`: nothing starts any more.
    closed: bool,
    rules: Vec<Entry>,
    next_generation: u64,
    /// Local endpoints given up recently, with the time (see [`RELEASE_GRACE`]).
    released: Vec<(String, u16, Instant)>,
}

pub(crate) struct Inner {
    runtime: Handle,
    events: async_channel::Sender<ForwardEvent>,
    grants: GrantMemory,
    retry: RetryTiming,
    slot: Arc<ConnSlot>,
    state: Mutex<State>,
    /// Stops with the forwarder.
    _stats_task: Mutex<Option<AbortOnDrop>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn emit(&self, event: ForwardEvent) {
        // A UI that stopped listening is not an error.
        let _ = self.events.try_send(event);
    }

    /// Applies `change` to the entry `id` if it still belongs to run `generation`; tells the
    /// UI if the state changed.
    pub(crate) fn update(&self, id: &RuleId, generation: u64, change: Update) {
        let mut state = lock(&self.state);
        let Some(entry) = state.rules.iter_mut().find(|entry| entry.rule.id == *id && entry.generation == generation)
        else {
            return;
        };
        let before = entry.state.clone();
        change(entry);
        if entry.state != before {
            self.emit(ForwardEvent::State { id: id.clone(), state: entry.state.clone() });
        }
    }

    fn report_stats(&self) {
        let mut state = lock(&self.state);
        for entry in &mut state.rules {
            let snapshot = entry.stats.snapshot();
            if snapshot != entry.reported {
                entry.reported = snapshot;
                self.emit(ForwardEvent::Stats { id: entry.rule.id.clone(), stats: snapshot });
            }
        }
    }
}

/// Runs the forwarding rules of one SSH connection (all tabs of a host share one).
///
/// Cheap to clone; all clones are the same forwarder. It only holds the connection weakly: the
/// connection closes when the tabs and the forwarded connections are gone, and the forwarder
/// is told with [`connection_lost`](Self::connection_lost) / [`connection_restored`](Self::connection_restored).
/// Everything runs on the tokio runtime it was created with, and every method can be called
/// from any thread without blocking. Dropping the last clone stops all rules like
/// [`shutdown`](Self::shutdown).
#[derive(Clone)]
pub struct Forwarder {
    inner: Arc<Inner>,
}

impl Forwarder {
    /// A forwarder with no rules and no connection. State changes and statistics are sent to
    /// `events` (an unbounded channel; the sender never blocks).
    pub fn new(runtime: Handle, events: async_channel::Sender<ForwardEvent>) -> Self {
        Self::with_options(runtime, events, Options::default())
    }

    /// Like [`new`](Self::new) with another statistics interval (the tests use a short one).
    pub fn with_stats_interval(
        runtime: Handle,
        events: async_channel::Sender<ForwardEvent>,
        interval: Duration,
    ) -> Self {
        Self::with_options(runtime, events, Options { stats_interval: interval, ..Options::default() })
    }

    /// Like [`new`](Self::new) with `options`.
    pub fn with_options(runtime: Handle, events: async_channel::Sender<ForwardEvent>, options: Options) -> Self {
        let interval = options.stats_interval;
        let inner = Arc::new(Inner {
            runtime: runtime.clone(),
            events,
            grants: options.grants,
            retry: options.retry,
            slot: Arc::new(ConnSlot::default()),
            state: Mutex::new(State::default()),
            _stats_task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let task = runtime.spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                inner.report_stats();
            }
        });
        *inner._stats_task.lock().unwrap_or_else(PoisonError::into_inner) = Some(AbortOnDrop::new(task));
        Self { inner }
    }

    /// What the servers granted so far (see [`Options::grants`]); hand it to the forwarder of the next
    /// connection to the same host.
    pub fn grants(&self) -> GrantMemory {
        self.inner.grants.clone()
    }

    /// Starts `rule` (replacing a running rule with the same id) and returns where it stands:
    /// a local or dynamic forward has its port (or the reason it has none) at once; a remote
    /// forward is [`Starting`](RuleState::Starting) until the server answers, which is reported
    /// as an event. Without a connection the rule waits for one
    /// ([`WaitingReconnect`](RuleState::WaitingReconnect)). `rule.enabled` is not looked at: the
    /// caller decides which rules to start.
    pub fn start(&self, rule: ForwardRule) -> RuleState {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return RuleState::Stopped;
        }
        if let Some(position) = state.rules.iter().position(|entry| entry.rule.id == rule.id) {
            let old = state.rules.remove(position);
            self.retire(&mut state, old);
        }

        let connected = inner.slot.current().is_some();
        let incomplete = rule.kind != ForwardKind::Dynamic && rule.target.is_none();
        let mut entry = Entry {
            rule: rule.clone(),
            state: RuleState::Stopped,
            stats: Arc::new(RuleStats::default()),
            generation: 0,
            task: None,
            remote: None,
            bound: None,
            reported: StatsSnapshot::default(),
        };
        match rule.kind {
            _ if incomplete => entry.state = RuleState::Failed(RuleError::MissingTarget),
            ForwardKind::Local | ForwardKind::Dynamic => {
                let mode = match (&rule.kind, &rule.target) {
                    (ForwardKind::Local, Some(target)) => Mode::Forward(target.clone()),
                    _ => Mode::Socks,
                };
                match self.bind(&mut state, &rule) {
                    Ok(listener) => {
                        let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(rule.listen.port);
                        entry.bound = Some(port);
                        entry.task = Some(spawn_listener(
                            &inner.runtime,
                            listener,
                            mode,
                            inner.slot.clone(),
                            entry.stats.clone(),
                        ));
                        entry.state = if connected { RuleState::Running { port } } else { RuleState::WaitingReconnect };
                    }
                    Err(err) => entry.state = RuleState::Failed(err),
                }
            }
            ForwardKind::Remote => {
                if connected {
                    self.spawn_remote(&mut state, &mut entry, false);
                } else {
                    entry.state = RuleState::WaitingReconnect;
                }
            }
        }
        let result = entry.state.clone();
        inner.emit(ForwardEvent::State { id: rule.id.clone(), state: result.clone() });
        state.rules.push(entry);
        result
    }

    /// Binds the local port of `rule`, waiting a moment if a rule of this forwarder has only
    /// just let go of it.
    fn bind(&self, state: &mut State, rule: &ForwardRule) -> Result<TcpListener, RuleError> {
        let now = Instant::now();
        state.released.retain(|(_, _, when)| now.duration_since(*when) < RELEASE_GRACE);
        let recently = |state: &State| {
            state.released.iter().any(|(host, port, _)| *host == rule.listen.host && *port == rule.listen.port)
        };
        let mut attempts = 0;
        let std_listener = loop {
            match listen::bind(&rule.listen) {
                Err(RuleError::PortInUse { .. }) if recently(state) && attempts < 40 => {
                    attempts += 1;
                    std::thread::sleep(RELEASE_RETRY);
                }
                other => break other?,
            }
        };
        let _in_runtime = self.inner.runtime.enter();
        TcpListener::from_std(std_listener).map_err(|err| RuleError::BindFailed {
            host: rule.listen.host.clone(),
            port: rule.listen.port,
            reason: err.to_string(),
        })
    }

    /// Takes an entry out of service: notes the port it gave up and, for a remote forward the
    /// server is listening for, asks the server to stop.
    fn retire(&self, state: &mut State, entry: Entry) {
        if let Some(port) = entry.bound {
            state.released.push((entry.rule.listen.host.clone(), entry.rule.listen.port, Instant::now()));
            if port != entry.rule.listen.port {
                state.released.push((entry.rule.listen.host.clone(), port, Instant::now()));
            }
        }
        if let Some((address, port)) = entry.remote.clone()
            && let Some(connection) = self.inner.slot.current()
        {
            self.inner.runtime.spawn(async move {
                let cancel = connection.cancel_remote_forward(&address, port);
                if let Ok(Err(err)) = tokio::time::timeout(CANCEL_LIMIT, cancel).await {
                    log::debug!("取消远程转发 {address}:{port} 失败：{err}");
                }
            });
        }
        // Dropping the entry aborts its tasks and with them every forwarded connection.
        drop(entry);
    }

    /// Starts (or restarts) the task that asks the server for a remote forward. `held`: the entry waits
    /// out a refusal ([`RuleState::RemotePortHeld`]) and keeps saying so while it asks again.
    fn spawn_remote(&self, state: &mut State, entry: &mut Entry, held: bool) {
        state.next_generation += 1;
        entry.generation = state.next_generation;
        if !held {
            entry.state = RuleState::Starting;
        }
        entry.remote = None;
        let target = entry.rule.target.clone().expect("a remote forward has a target");
        let run = RemoteRun {
            inner: Arc::downgrade(&self.inner),
            slot: self.inner.slot.clone(),
            id: entry.rule.id.clone(),
            generation: entry.generation,
            listen: entry.rule.listen.clone(),
            target,
            stats: entry.stats.clone(),
            grants: self.inner.grants.clone(),
            timing: self.inner.retry.clone(),
            held,
        };
        entry.task = Some(AbortOnDrop::new(self.inner.runtime.spawn(run.run())));
    }

    /// Asks the server for a remote forward again, now: for a rule that waits out a refusal
    /// ([`RemotePortHeld`](RuleState::RemotePortHeld)) instead of waiting for its next turn, and for one that
    /// [failed](RuleState::Failed). Returns whether it was asked (`false` for other rules and states).
    pub fn retry(&self, id: &RuleId) -> bool {
        let mut state = lock(&self.inner.state);
        if state.closed {
            return false;
        }
        let Some(position) = state.rules.iter().position(|entry| entry.rule.id == *id) else { return false };
        let mut entry = state.rules.remove(position);
        let held = matches!(entry.state, RuleState::RemotePortHeld { .. });
        let retryable = entry.rule.kind == ForwardKind::Remote && (held || matches!(entry.state, RuleState::Failed(_)));
        if retryable {
            let before = entry.state.clone();
            self.spawn_remote(&mut state, &mut entry, held);
            if entry.state != before {
                self.inner.emit(ForwardEvent::State { id: id.clone(), state: entry.state.clone() });
            }
        }
        state.rules.insert(position, entry);
        retryable
    }

    /// Stops a rule: closes its listener, ends its connections and (remote forwards) tells the
    /// server to stop listening. Returns whether the rule was running.
    pub fn stop(&self, id: &RuleId) -> bool {
        let mut state = lock(&self.inner.state);
        let Some(position) = state.rules.iter().position(|entry| entry.rule.id == *id) else { return false };
        let entry = state.rules.remove(position);
        self.retire(&mut state, entry);
        self.inner.emit(ForwardEvent::State { id: id.clone(), state: RuleState::Stopped });
        true
    }

    /// The SSH connection is gone (or going). Local and dynamic forwards keep their ports and
    /// refuse new connections until [`connection_restored`](Self::connection_restored); remote
    /// forwards are dropped and asked for again then. Forwarded connections end by themselves
    /// with the transport.
    pub fn connection_lost(&self) {
        let inner = &self.inner;
        inner.slot.set(None);
        let mut state = lock(&inner.state);
        for entry in &mut state.rules {
            let waiting = match entry.rule.kind {
                ForwardKind::Local | ForwardKind::Dynamic => entry.state.is_running(),
                ForwardKind::Remote => {
                    entry.task = None;
                    entry.remote = None;
                    entry.generation = 0;
                    matches!(
                        entry.state,
                        RuleState::Running { .. } | RuleState::Starting | RuleState::RemotePortHeld { .. }
                    )
                }
            };
            if waiting {
                entry.state = RuleState::WaitingReconnect;
                inner.emit(ForwardEvent::State { id: entry.rule.id.clone(), state: RuleState::WaitingReconnect });
            }
        }
    }

    /// There is a connection (the first one, or a new one after a loss): rules waiting for it
    /// start serving, remote forwards are requested from the server again.
    pub fn connection_restored(&self, connection: &Connection) {
        let inner = &self.inner;
        let mut state = lock(&inner.state);
        if state.closed {
            return;
        }
        inner.slot.set(Some(connection.downgrade()));
        let mut rules = std::mem::take(&mut state.rules);
        for entry in &mut rules {
            let before = entry.state.clone();
            match entry.rule.kind {
                ForwardKind::Local | ForwardKind::Dynamic => {
                    if let (RuleState::WaitingReconnect, Some(port)) = (&entry.state, entry.bound) {
                        entry.state = RuleState::Running { port };
                    }
                }
                ForwardKind::Remote => {
                    if matches!(entry.state, RuleState::WaitingReconnect | RuleState::Failed(_)) {
                        self.spawn_remote(&mut state, entry, false);
                    }
                }
            }
            if entry.state != before {
                inner.emit(ForwardEvent::State { id: entry.rule.id.clone(), state: entry.state.clone() });
            }
        }
        state.rules = rules;
    }

    /// Stops everything: listeners are closed and every forwarded connection is aborted, so
    /// nothing keeps the SSH connection open any more. For when the connection is torn down
    /// (the last tab closed); the forwarder starts nothing afterwards.
    pub fn shutdown(&self) {
        let mut state = lock(&self.inner.state);
        state.closed = true;
        self.inner.slot.set(None);
        // Dropped here, under the lock, so no task can start in between.
        let rules = std::mem::take(&mut state.rules);
        drop(state);
        drop(rules);
    }

    /// Where a rule stands; [`Stopped`](RuleState::Stopped) if it was never started or was stopped.
    pub fn state(&self, id: &RuleId) -> RuleState {
        let state = lock(&self.inner.state);
        state.rules.iter().find(|entry| entry.rule.id == *id).map_or(RuleState::Stopped, |entry| entry.state.clone())
    }

    /// The numbers of a rule right now (zero if it is not started).
    pub fn stats(&self, id: &RuleId) -> StatsSnapshot {
        let state = lock(&self.inner.state);
        state.rules.iter().find(|entry| entry.rule.id == *id).map(|entry| entry.stats.snapshot()).unwrap_or_default()
    }

    /// Every started rule in the order they were started.
    pub fn views(&self) -> Vec<RuleView> {
        let state = lock(&self.inner.state);
        state
            .rules
            .iter()
            .map(|entry| RuleView {
                rule: entry.rule.clone(),
                state: entry.state.clone(),
                stats: entry.stats.snapshot(),
            })
            .collect()
    }

    /// How many rules are serving connections.
    pub fn running_rules(&self) -> usize {
        lock(&self.inner.state).rules.iter().filter(|entry| entry.state.is_running()).count()
    }

    /// How many forwarded connections are open across all rules (what closing would cut).
    pub fn active_connections(&self) -> u32 {
        lock(&self.inner.state).rules.iter().map(|entry| entry.stats.snapshot().active).sum()
    }
}

impl std::fmt::Debug for Forwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Forwarder({} rules)", lock(&self.inner.state).rules.len())
    }
}
