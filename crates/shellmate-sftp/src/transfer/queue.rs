//! The queue: items, scheduling, state transitions and events.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;

use super::control::{Control, Interrupt};
use super::plan::Plan;
use super::run::{self, RunInput, RunOutcome};
use super::speed::SpeedMeter;
use super::{
    Choice, Clash, Direction, ItemId, MAX_FAILURES_KEPT, MAX_RUNNING, PauseReason, Skipped, TransferEvent,
    TransferFailure, TransferState, TransferView, failure_summary,
};
use crate::path;
use crate::session::SftpSession;

/// Progress events per item are spaced at least this far apart.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// A temporary file left by a stopped transfer, which a resume may continue.
#[derive(Clone, Debug)]
pub(super) struct Partial {
    /// The entry of the plan it belongs to.
    pub entry: usize,
    /// The temporary file on the server (uploads).
    pub temp_remote: Option<String>,
    /// The temporary file on this machine (downloads).
    pub temp_local: Option<PathBuf>,
    /// Size and time the source had when the file was started.
    pub src_size: u64,
    pub src_mtime: Option<i64>,
    /// How far the file is known to be written without gaps: the end of the contiguous run of
    /// acknowledged writes from the start. A server may apply requests in another order, so the
    /// length of the temporary file only says how far *something* was written; a resume goes on
    /// from the smaller of the two and rewrites what is above.
    pub highwater: u64,
}

enum Selection {
    Local(Vec<PathBuf>),
    Remote(Vec<String>),
}

struct Item {
    id: ItemId,
    direction: Direction,
    name: String,
    selection: Selection,
    /// The destination directory (a remote path for uploads, a local one for downloads).
    destination: String,
    state: TransferState,
    /// No more questions to settle before the item may start.
    ready: bool,
    /// A task is checking the destination for names that exist.
    checking: bool,
    /// The generation of the session on which that check failed (it waits for the next one).
    check_failed: Option<u64>,
    choice: Option<Choice>,
    clash: Option<Clash>,
    /// Remote paths of what an upload will create at the top level.
    claims: Vec<String>,
    plan: Option<Arc<Plan>>,
    scanning: bool,
    cursor: usize,
    partial: Option<Partial>,
    completed_bytes: u64,
    current_offset: u64,
    bytes_total: u64,
    /// The size found for the entry in progress when it differs from the plan: what the total counts for it.
    counted: Option<(usize, u64)>,
    files_done: u64,
    skipped: Skipped,
    /// Files overwritten in place because the directory had no room for a temporary file.
    in_place: u32,
    /// The first [`MAX_FAILURES_KEPT`] failures; `failure_total` counts them all.
    failures: Vec<TransferFailure>,
    failure_total: usize,
    current: Option<String>,
    result: Option<PathBuf>,
    /// Local names this download holds.
    reserved: Vec<PathBuf>,
    meter: SpeedMeter,
    last_progress: Option<Instant>,
    /// Identifies the task that is running the item now; answers of older tasks are ignored.
    run: u64,
    control: Arc<Control>,
}

impl Item {
    fn bytes_done(&self) -> u64 {
        self.completed_bytes + self.current_offset
    }

    fn add_failure(&mut self, failure: TransferFailure) {
        self.failure_total += 1;
        if self.failures.len() < MAX_FAILURES_KEPT {
            self.failures.push(failure);
        }
    }

    /// What the total counts for entry `index`: its size when it was planned, unless the source
    /// turned out different.
    fn counted_size(&self, index: usize, planned: u64) -> u64 {
        self.counted.filter(|(counted, _)| *counted == index).map_or(planned, |(_, size)| size)
    }

    fn view(&mut self, now: Instant) -> TransferView {
        let speed = if self.state == TransferState::Running { self.meter.bytes_per_second(now) } else { 0.0 };
        TransferView {
            id: self.id,
            direction: self.direction,
            name: self.name.clone(),
            destination: self.destination.clone(),
            state: self.state.clone(),
            bytes_done: self.bytes_done(),
            bytes_total: self.bytes_total,
            files_done: self.files_done,
            files_total: self.plan.as_ref().map_or(0, |plan| plan.total_files),
            scanning: self.scanning,
            speed,
            current: self.current.clone(),
            skipped: self.skipped,
            in_place: self.in_place,
            failures: self.failures.clone(),
            failure_total: self.failure_total,
            clash: self.clash.clone(),
            result: self.result.clone(),
            temp: self.partial.as_ref().and_then(|partial| partial.temp_remote.clone()),
        }
    }
}

struct State {
    items: Vec<Item>,
    session: Option<Arc<SftpSession>>,
    /// Counts the sessions handed in; a task of an older one is told apart from the current.
    generation: u64,
    /// The generation for which [`TransferEvent::SessionLost`] was sent.
    lost_generation: Option<u64>,
    next_id: u64,
    /// Local names that running or paused downloads have picked.
    reserved_local: HashSet<PathBuf>,
}

pub(super) struct Inner {
    state: Mutex<State>,
    events: async_channel::Sender<TransferEvent>,
    handle: Handle,
}

/// The transfers of one connection. Cheap to clone; all clones are the same queue.
#[derive(Clone)]
pub struct TransferQueue {
    inner: Arc<Inner>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: TransferEvent) {
        let _ = self.events.try_send(event);
    }
}

impl TransferQueue {
    /// A queue running its transfers on `runtime`, and the receiver of its events. Nothing
    /// starts until a session is set ([`set_session`](Self::set_session)).
    pub fn new(runtime: Handle) -> (Self, async_channel::Receiver<TransferEvent>) {
        let (events, receiver) = async_channel::unbounded();
        let state = State {
            items: Vec::new(),
            session: None,
            generation: 0,
            lost_generation: None,
            next_id: 1,
            reserved_local: HashSet::new(),
        };
        (Self { inner: Arc::new(Inner { state: Mutex::new(state), events, handle: runtime }) }, receiver)
    }

    /// Hands the queue the SFTP session of the connection, or `None` when it is gone.
    ///
    /// With `None`, running items pause with [`PauseReason::Disconnected`]. With a session, those
    /// items (and only those: what the user paused stays paused) resume by themselves. The queue
    /// never connects by itself.
    pub fn set_session(&self, session: Option<Arc<SftpSession>>) {
        {
            let mut state = self.inner.lock();
            let same = match (&state.session, &session) {
                (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                (None, None) => true,
                _ => false,
            };
            if same {
                return;
            }
            state.generation += 1;
            let lost = session.is_none();
            state.session = session;
            for item in &mut state.items {
                match &item.state {
                    // The session under a running task is gone (or replaced, which is as good as gone).
                    TransferState::Running => item.control.set(Some(Interrupt::Pause(PauseReason::Disconnected))),
                    TransferState::Paused(PauseReason::Disconnected) if !lost => item.state = TransferState::Queued,
                    _ => {}
                }
            }
            let now = Instant::now();
            for item in state.items.iter_mut().filter(|item| item.state == TransferState::Queued) {
                let view = item.view(now);
                self.inner.emit(TransferEvent::State(view));
            }
        }
        schedule(&self.inner);
    }

    /// Queues an upload of `local_paths` (files, directories, links) into the directory `remote_dir`.
    pub fn enqueue_upload(&self, local_paths: Vec<PathBuf>, remote_dir: &str) -> ItemId {
        let names: Vec<String> = local_paths
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        let claims = names.iter().filter(|name| !name.is_empty()).map(|name| path::join(remote_dir, name)).collect();
        let label = label(&names);
        self.enqueue(Direction::Upload, label, Selection::Local(local_paths), remote_dir.to_string(), false, claims)
    }

    /// Queues a download of `remote_paths` into the directory `local_dir` on this machine (which
    /// the caller makes sure exists; the application passes `~/Downloads`).
    pub fn enqueue_download(&self, remote_paths: Vec<String>, local_dir: PathBuf) -> ItemId {
        let names: Vec<String> =
            remote_paths.iter().map(|p| path::file_name(p).unwrap_or_default().to_string()).collect();
        let label = label(&names);
        self.enqueue(
            Direction::Download,
            label,
            Selection::Remote(remote_paths),
            local_dir.display().to_string(),
            true,
            Vec::new(),
        )
    }

    fn enqueue(
        &self,
        direction: Direction,
        name: String,
        selection: Selection,
        destination: String,
        ready: bool,
        claims: Vec<String>,
    ) -> ItemId {
        let view = {
            let mut state = self.inner.lock();
            let id = ItemId(state.next_id);
            state.next_id += 1;
            let mut item = Item {
                id,
                direction,
                name,
                selection,
                destination,
                state: TransferState::Queued,
                ready,
                checking: false,
                check_failed: None,
                choice: None,
                clash: None,
                claims,
                plan: None,
                scanning: false,
                cursor: 0,
                partial: None,
                completed_bytes: 0,
                current_offset: 0,
                bytes_total: 0,
                counted: None,
                files_done: 0,
                skipped: Skipped::default(),
                in_place: 0,
                failures: Vec::new(),
                failure_total: 0,
                current: None,
                result: None,
                reserved: Vec::new(),
                meter: SpeedMeter::default(),
                last_progress: None,
                run: 0,
                control: Arc::new(Control::new()),
            };
            let view = item.view(Instant::now());
            state.items.push(item);
            view
        };
        let id = view.id;
        self.inner.emit(TransferEvent::Added(view));
        schedule(&self.inner);
        id
    }

    /// Pauses a queued or running item. A running one stops at the next chunk and keeps its
    /// temporary file.
    pub fn pause(&self, id: ItemId) {
        self.with_item(id, |item, inner| match item.state {
            TransferState::Queued => {
                item.state = TransferState::Paused(PauseReason::User);
                inner.emit(TransferEvent::State(item.view(Instant::now())));
            }
            TransferState::Running => item.control.set(Some(Interrupt::Pause(PauseReason::User))),
            _ => {}
        });
    }

    /// Continues a paused item where it stopped. If it is still stopping, this takes the pause back.
    pub fn resume(&self, id: ItemId) {
        self.with_item(id, |item, inner| match item.state {
            TransferState::Paused(_) => {
                item.state = TransferState::Queued;
                inner.emit(TransferEvent::State(item.view(Instant::now())));
            }
            TransferState::Running => item.control.set(None),
            _ => {}
        });
        schedule(&self.inner);
    }

    /// Cancels an item that is not finished. Its temporary file is removed; files of a directory
    /// that were already done stay.
    pub fn cancel(&self, id: ItemId) {
        let cleanup = {
            let mut state = self.inner.lock();
            let session = state.session.clone();
            let mut released = Vec::new();
            let mut cleanup = None;
            let mut view = None;
            let mut remote_dir = None;
            if let Some(item) = state.items.iter_mut().find(|item| item.id == id) {
                match item.state {
                    TransferState::Running => item.control.set(Some(Interrupt::Cancel)),
                    ref done if done.is_finished() => {}
                    _ => {
                        item.state = TransferState::Cancelled;
                        item.control.set(Some(Interrupt::Cancel));
                        item.clash = None;
                        released = std::mem::take(&mut item.reserved);
                        cleanup = item.partial.take().map(|partial| (partial, session));
                        if item.direction == Direction::Upload {
                            remote_dir = Some(item.destination.clone());
                        }
                        view = Some(item.view(Instant::now()));
                    }
                }
            }
            for local in released {
                state.reserved_local.remove(&local);
            }
            if let Some(view) = view {
                self.inner.emit(TransferEvent::State(view));
            }
            if let Some(dir) = remote_dir {
                self.inner.emit(TransferEvent::RemoteChanged(dir));
            }
            cleanup
        };
        if let Some((partial, session)) = cleanup {
            self.inner.handle.spawn(async move { run::remove_partial(&partial, session.as_deref()).await });
        }
        schedule(&self.inner);
    }

    /// The answer to the question of an item in [`TransferState::AwaitingClash`]; it applies to
    /// every name that exists.
    pub fn answer_clash(&self, id: ItemId, choice: Choice) {
        self.with_item(id, |item, inner| {
            if item.state == TransferState::AwaitingClash {
                item.choice = Some(choice);
                item.clash = None;
                item.ready = true;
                item.state = TransferState::Queued;
                inner.emit(TransferEvent::State(item.view(Instant::now())));
            }
        });
        schedule(&self.inner);
    }

    /// Removes finished items (done, failed, cancelled) from the queue.
    pub fn clear_finished(&self) {
        let removed: Vec<ItemId> = {
            let mut state = self.inner.lock();
            let removed = state.items.iter().filter(|item| item.state.is_finished()).map(|item| item.id).collect();
            state.items.retain(|item| !item.state.is_finished());
            removed
        };
        if !removed.is_empty() {
            self.inner.emit(TransferEvent::Removed(removed));
        }
    }

    /// Every item, oldest first.
    pub fn snapshot(&self) -> Vec<TransferView> {
        let now = Instant::now();
        self.inner.lock().items.iter_mut().map(|item| item.view(now)).collect()
    }

    /// One item.
    pub fn view(&self, id: ItemId) -> Option<TransferView> {
        let now = Instant::now();
        self.inner.lock().items.iter_mut().find(|item| item.id == id).map(|item| item.view(now))
    }

    /// Stops everything at once and lets go of the session (the connection is going away).
    /// Partial files stay where they are; the queue is not meant to be used afterwards.
    pub fn shutdown(&self) {
        let mut state = self.inner.lock();
        state.session = None;
        state.generation += 1;
        for item in &state.items {
            item.control.set(Some(Interrupt::Pause(PauseReason::Disconnected)));
        }
    }

    fn with_item(&self, id: ItemId, change: impl FnOnce(&mut Item, &Inner)) {
        let mut state = self.inner.lock();
        if let Some(item) = state.items.iter_mut().find(|item| item.id == id) {
            change(item, &self.inner);
        }
    }
}

/// `report.pdf`, or `3 个项目`.
fn label(names: &[String]) -> String {
    match names {
        [only] => only.clone(),
        many => format!("{} 个项目", many.len()),
    }
}

// ---- scheduling -------------------------------------------------------------------------------

/// Starts what can start: checks for existing names, then runs items in order up to the limit.
fn schedule(inner: &Arc<Inner>) {
    let mut state = inner.lock();
    let Some(session) = state.session.clone() else { return };
    let generation = state.generation;

    // Items whose upload targets overlap those of an earlier unfinished item wait for it: both
    // would write the same temporary files.
    let blocked: Vec<bool> = (0..state.items.len())
        .map(|ix| {
            let item = &state.items[ix];
            state.items[..ix].iter().any(|earlier| {
                !earlier.state.is_finished()
                    && earlier
                        .claims
                        .iter()
                        .any(|a| item.claims.iter().any(|b| path::is_within(a, b) || path::is_within(b, a)))
            })
        })
        .collect();

    for (ix, item) in state.items.iter_mut().enumerate() {
        if item.state == TransferState::Queued
            && !item.ready
            && !item.checking
            && item.check_failed != Some(generation)
            && !blocked[ix]
        {
            item.checking = true;
            let (id, names, dir) = (item.id, top_names(item), item.destination.clone());
            let (inner, session) = (inner.clone(), session.clone());
            let handle = inner.handle.clone();
            handle.spawn(async move {
                let found = run::existing_names(&session, &dir, &names).await;
                checked(&inner, id, generation, found);
            });
        }
    }

    let mut running = state.items.iter().filter(|item| item.state == TransferState::Running).count();
    let mut started = Vec::new();
    for (ix, item) in state.items.iter_mut().enumerate() {
        if running >= MAX_RUNNING {
            break;
        }
        if item.state == TransferState::Queued && item.ready && !blocked[ix] {
            running += 1;
            item.state = TransferState::Running;
            item.run += 1;
            item.control = Arc::new(Control::new());
            item.meter.reset();
            started.push((item.id, item.run, item.control.clone()));
            inner.emit(TransferEvent::State(item.view(Instant::now())));
        }
    }
    drop(state);

    for (id, token, control) in started {
        let (inner, session) = (inner.clone(), session.clone());
        let handle = inner.handle.clone();
        handle.spawn(async move {
            let reporter = Reporter { inner: inner.clone(), id, token };
            let outcome = run::run(&reporter, &session, &control).await;
            finish(&inner, id, token, generation, outcome);
        });
    }
}

/// The names an upload creates at the top level of its destination.
fn top_names(item: &Item) -> Vec<String> {
    match &item.selection {
        Selection::Local(paths) => {
            paths.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect()
        }
        Selection::Remote(paths) => paths.iter().filter_map(|p| path::file_name(p)).map(String::from).collect(),
    }
}

/// Sends [`TransferEvent::SessionLost`], once for each session handed in, when a task found the
/// session it was given unusable and the queue still holds that very session.
fn note_session_lost(
    inner: &Inner,
    lost_generation: &mut Option<u64>,
    task_generation: u64,
    current: u64,
    holds: bool,
) {
    if holds && task_generation == current && *lost_generation != Some(current) {
        *lost_generation = Some(current);
        inner.emit(TransferEvent::SessionLost);
    }
}

/// The check for existing names is over.
fn checked(inner: &Arc<Inner>, id: ItemId, generation: u64, found: Result<Vec<String>, crate::SftpError>) {
    {
        let mut state = inner.lock();
        let (current, holds) = (state.generation, state.session.is_some());
        let State { items, lost_generation, .. } = &mut *state;
        let Some(item) = items.iter_mut().find(|item| item.id == id) else { return };
        item.checking = false;
        if item.state != TransferState::Queued {
            return;
        }
        match found {
            Ok(names) if names.is_empty() => item.ready = true,
            Ok(names) => {
                item.clash = Some(Clash {
                    dir: item.destination.clone(),
                    count: names.len(),
                    names: names.into_iter().take(5).collect(),
                });
                item.state = TransferState::AwaitingClash;
                inner.emit(TransferEvent::State(item.view(Instant::now())));
            }
            // The session went away: the check waits for the next one (and does not spin on this one).
            Err(err) if err.is_session_lost() => {
                item.check_failed = Some(generation);
                note_session_lost(inner, lost_generation, generation, current, holds);
            }
            Err(err) => {
                item.state = TransferState::Failed(err.to_string());
                item.add_failure(TransferFailure { path: item.destination.clone(), reason: err.to_string() });
                let view = item.view(Instant::now());
                let dir = item.destination.clone();
                inner.emit(TransferEvent::State(view));
                inner.emit(TransferEvent::RemoteChanged(dir));
            }
        }
    }
    schedule(inner);
}

/// A task ended: settle the item's state.
fn finish(inner: &Arc<Inner>, id: ItemId, token: u64, generation: u64, outcome: RunOutcome) {
    {
        let mut state = inner.lock();
        let current_generation = state.generation;
        let has_session = state.session.is_some();
        let State { items, reserved_local, lost_generation, .. } = &mut *state;
        let Some(item) = items.iter_mut().find(|item| item.id == id) else { return };
        if item.run != token {
            return;
        }
        item.current = None;
        item.scanning = false;
        item.current_offset = if item.partial.is_some() { item.current_offset } else { 0 };
        item.meter.reset();
        let mut release = false;
        match outcome {
            RunOutcome::Done => {
                item.state = if item.failures.is_empty() {
                    TransferState::Done
                } else {
                    TransferState::Failed(failure_summary(&item.failures, item.failure_total))
                };
                release = true;
            }
            RunOutcome::Failed(reason) => {
                item.state = TransferState::Failed(reason);
                release = true;
            }
            RunOutcome::Interrupted(Interrupt::Cancel) => {
                item.state = TransferState::Cancelled;
                release = true;
            }
            RunOutcome::Interrupted(Interrupt::Pause(PauseReason::User)) => {
                item.state = TransferState::Paused(PauseReason::User)
            }
            // Told to stop because the session was taken away or replaced.
            RunOutcome::Interrupted(Interrupt::Pause(PauseReason::Disconnected)) => {
                item.state = paused_for_session(has_session, generation, current_generation);
            }
            // The task found the session unusable by itself.
            RunOutcome::Disconnected => {
                item.state = paused_for_session(has_session, generation, current_generation);
                note_session_lost(inner, lost_generation, generation, current_generation, has_session);
            }
            RunOutcome::Clash(clash) => {
                item.clash = Some(clash);
                item.ready = false;
                item.state = TransferState::AwaitingClash;
            }
        }
        let mut freed = Vec::new();
        if release {
            freed = std::mem::take(&mut item.reserved);
            item.partial = None;
        }
        let upload_dir =
            (item.direction == Direction::Upload && item.state.is_finished()).then(|| item.destination.clone());
        let view = item.view(Instant::now());
        for local in freed {
            reserved_local.remove(&local);
        }
        inner.emit(TransferEvent::State(view));
        if let Some(dir) = upload_dir {
            inner.emit(TransferEvent::RemoteChanged(dir));
        }
    }
    schedule(inner);
}

/// What an item whose task stopped because of the session becomes: queued again if a newer session
/// was handed in meanwhile, paused (waiting for one) otherwise.
fn paused_for_session(has_session: bool, task_generation: u64, current_generation: u64) -> TransferState {
    if has_session && task_generation != current_generation {
        TransferState::Queued
    } else {
        TransferState::Paused(PauseReason::Disconnected)
    }
}

// ---- what a running task tells the queue -----------------------------------------------------

/// The channel from a running task to its item in the queue. Everything it says is ignored once
/// the item has moved on to another task.
pub(super) struct Reporter {
    inner: Arc<Inner>,
    id: ItemId,
    token: u64,
}

impl Reporter {
    fn with<R>(&self, change: impl FnOnce(&mut Item, &mut HashSet<PathBuf>, &Inner) -> R) -> Option<R> {
        let mut state = self.inner.lock();
        let State { items, reserved_local, .. } = &mut *state;
        let item = items.iter_mut().find(|item| item.id == self.id && item.run == self.token)?;
        Some(change(item, reserved_local, &self.inner))
    }

    /// What the task needs to know to run the item.
    pub fn input(&self) -> Option<RunInput> {
        self.with(|item, _, _| {
            let (local, remote) = match &item.selection {
                Selection::Local(paths) => (paths.clone(), Vec::new()),
                Selection::Remote(paths) => (Vec::new(), paths.clone()),
            };
            RunInput {
                direction: item.direction,
                local,
                remote,
                destination: item.destination.clone(),
                choice: item.choice,
                plan: item.plan.clone(),
                cursor: item.cursor,
                partial: item.partial.clone(),
            }
        })
    }

    /// The selection is being walked (or no longer).
    pub fn set_scanning(&self, scanning: bool) {
        self.with(|item, _, inner| {
            item.scanning = scanning;
            inner.emit(TransferEvent::State(item.view(Instant::now())));
        });
    }

    /// The walk is over: the plan, what it left out and what could not even be looked at.
    pub fn set_plan(&self, plan: Arc<Plan>, skipped: Skipped, failures: Vec<TransferFailure>) {
        self.with(|item, _, inner| {
            item.bytes_total = plan.total_bytes;
            item.plan = Some(plan);
            item.skipped = skipped;
            for failure in failures {
                item.add_failure(failure);
            }
            item.scanning = false;
            inner.emit(TransferEvent::State(item.view(Instant::now())));
        });
    }

    /// Picks a local name for a download that nothing else has, and keeps it for the item.
    pub fn reserve_local(&self, dir: &Path, name: &str, keep_extension: bool) -> Option<PathBuf> {
        self.with(|item, reserved, _| {
            let chosen = super::names::first_free(name, keep_extension, |candidate| {
                let path = dir.join(candidate);
                reserved.contains(&path) || path.symlink_metadata().is_ok()
            })
            .map(|name| dir.join(name))?;
            reserved.insert(chosen.clone());
            item.reserved.push(chosen.clone());
            Some(chosen)
        })
        .flatten()
    }

    /// The task starts on entry `index` (`name` is shown as the current file).
    pub fn begin_entry(&self, index: usize, name: String) {
        self.with(|item, _, _| {
            item.cursor = index;
            item.current = Some(name);
        });
    }

    /// The temporary file of the entry in progress, if there is one that a resume could continue.
    pub fn set_partial(&self, partial: Option<Partial>) {
        self.with(|item, _, _| item.partial = partial);
    }

    /// The file in progress is known to be written without gaps up to `offset`.
    pub fn set_highwater(&self, offset: u64) {
        self.with(|item, _, _| {
            if let Some(partial) = &mut item.partial {
                partial.highwater = partial.highwater.max(offset);
            }
        });
    }

    /// Where the entry in progress stands (after a resume, or a restart from 0).
    pub fn set_offset(&self, offset: u64) {
        self.with(|item, _, _| {
            item.current_offset = offset;
            item.meter.record(Instant::now(), item.bytes_done());
        });
    }

    /// `bytes` more of the entry in progress are done.
    pub fn progress(&self, bytes: u64) {
        self.with(|item, _, inner| {
            item.current_offset += bytes;
            let now = Instant::now();
            item.meter.record(now, item.bytes_done());
            if item.last_progress.is_none_or(|at| now.duration_since(at) >= PROGRESS_INTERVAL) {
                item.last_progress = Some(now);
                inner.emit(TransferEvent::Progress(item.view(now)));
            }
        });
    }

    /// The file now being written goes straight to the existing file (the directory is unwritable).
    pub fn note_in_place(&self) {
        self.with(|item, _, _| item.in_place += 1);
    }

    /// The source of entry `index` has `actual` bytes, not the `planned` ones it had when it was
    /// walked: the total follows (once, however often the entry is started).
    pub fn size_found(&self, index: usize, planned: u64, actual: u64) {
        self.with(|item, _, _| {
            let counted = item.counted_size(index, planned);
            item.bytes_total = (item.bytes_total + actual).saturating_sub(counted);
            item.counted = Some((index, actual));
        });
    }

    /// Entry `index` is finished.
    pub fn entry_done(&self, index: usize, planned: u64, file: bool) {
        self.with(|item, _, inner| {
            item.completed_bytes += item.counted_size(index, planned);
            item.current_offset = 0;
            item.partial = None;
            item.cursor = index + 1;
            if file {
                item.files_done += 1;
            }
            item.last_progress = Some(Instant::now());
            inner.emit(TransferEvent::Progress(item.view(Instant::now())));
        });
    }

    /// Entry `index` could not be transferred; the item goes on.
    pub fn entry_failed(&self, index: usize, failure: TransferFailure, planned: u64) {
        self.with(|item, _, inner| {
            item.add_failure(failure);
            item.completed_bytes += item.counted_size(index, planned);
            item.current_offset = 0;
            item.partial = None;
            item.cursor = index + 1;
            inner.emit(TransferEvent::Progress(item.view(Instant::now())));
        });
    }

    /// Entry `index` is left out because something above it failed.
    pub fn entry_skipped(&self, index: usize, size: u64) {
        self.with(|item, _, _| {
            item.completed_bytes += size;
            item.cursor = index + 1;
        });
    }

    /// Where a finished download ended up.
    pub fn set_result(&self, path: PathBuf) {
        self.with(|item, _, _| {
            if item.result.is_none() {
                item.result = Some(path);
            }
        });
    }
}
