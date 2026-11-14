//! The SFTP side of one connection: a lazily opened session and the transfer queue, kept in step
//! with the connection.
//!
//! One [`SftpLink`] belongs to one connection (and is shared by every tab on it). The session is
//! opened the first time something needs it ([`SftpLink::open`]; asking again while it is opening
//! or open changes nothing, so two tabs asking at once open it once). The link follows the
//! connection: when it is lost the session is dropped and the transfers pause; when it is back
//! (and the session had been opened before) a new session is opened; when the SFTP subsystem dies
//! while the connection lives on (the queue says [`TransferEvent::SessionLost`]) it is reopened,
//! a few times at most. Dropping the link shuts the queue down and lets go of the session: a
//! session holds the connection open, so this is what lets the connection close with its last tab.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shellmate_ssh::{Connection, WeakConnection};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::session::SftpSession;
use crate::transfer::{TransferEvent, TransferQueue};

/// How often the session may be reopened after it died, within [`REOPEN_WINDOW`].
const MAX_REOPENS: usize = 3;
/// See [`MAX_REOPENS`].
const REOPEN_WINDOW: Duration = Duration::from_secs(10);

/// Where the link stands.
#[derive(Clone)]
pub enum LinkState {
    /// No session has been asked for.
    Idle,
    /// A session is being opened.
    Opening,
    /// The session is there.
    Ready(Arc<SftpSession>),
    /// The session could not be opened (or keeps dying); the reason is for the user.
    Failed(String),
    /// The connection is gone; a session is opened again when it is back.
    Disconnected,
    /// The link was shut down.
    Closed,
}

impl std::fmt::Debug for LinkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Opening => f.write_str("Opening"),
            Self::Ready(_) => f.write_str("Ready"),
            Self::Failed(reason) => write!(f, "Failed({reason})"),
            Self::Disconnected => f.write_str("Disconnected"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

impl LinkState {
    /// The session, if there is one.
    pub fn session(&self) -> Option<&Arc<SftpSession>> {
        match self {
            Self::Ready(session) => Some(session),
            _ => None,
        }
    }
}

struct Inner {
    state: LinkState,
    /// A session has been open at some point: it is worth reopening after a reconnect.
    opened_before: bool,
    /// The connection to open sessions on (it must not be kept alive by the link).
    connection: Option<WeakConnection>,
    /// Identifies the attempt in progress; the answer of an older one is thrown away.
    attempt: u64,
    opening: Option<AbortHandle>,
    /// When the session was reopened after dying, newest last.
    reopens: VecDeque<Instant>,
}

struct Shared {
    inner: Mutex<Inner>,
    runtime: Handle,
    queue: TransferQueue,
    changed: async_channel::Sender<()>,
}

/// The session and transfer queue of one connection. See the module docs.
pub struct SftpLink {
    shared: Arc<Shared>,
}

/// Whether the session may be reopened now, and if so, notes it. A session that dies again and
/// again is not reopened for ever.
fn reopen_allowed(log: &mut VecDeque<Instant>, now: Instant) -> bool {
    while log.front().is_some_and(|at| now.saturating_duration_since(*at) >= REOPEN_WINDOW) {
        log.pop_front();
    }
    if log.len() >= MAX_REOPENS {
        return false;
    }
    log.push_back(now);
    true
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        let _ = self.changed.try_send(());
    }

    /// Starts opening a session on `connection` (the state becomes `Opening`).
    fn start_open(self: &Arc<Self>, inner: &mut Inner, connection: Connection) {
        if let Some(previous) = inner.opening.take() {
            previous.abort();
        }
        inner.attempt += 1;
        inner.state = LinkState::Opening;
        let (shared, token) = (self.clone(), inner.attempt);
        let task = self.runtime.spawn(async move {
            let result = SftpSession::open(&connection).await;
            drop(connection);
            shared.opened(token, result);
        });
        inner.opening = Some(task.abort_handle());
    }

    fn opened(&self, token: u64, result: Result<SftpSession, crate::SftpError>) {
        let session = {
            let mut inner = self.lock();
            if token != inner.attempt || matches!(inner.state, LinkState::Closed) {
                return;
            }
            inner.opening = None;
            match result {
                Ok(session) => {
                    let session = Arc::new(session);
                    inner.state = LinkState::Ready(session.clone());
                    inner.opened_before = true;
                    Some(session)
                }
                Err(err) => {
                    inner.state = LinkState::Failed(err.to_string());
                    None
                }
            }
        };
        if let Some(session) = session {
            self.queue.set_session(Some(session));
        }
        self.notify();
    }
}

impl SftpLink {
    /// A link whose work runs on `runtime`, and the receiver that is told whenever the state changes.
    pub fn new(runtime: Handle) -> (Self, async_channel::Receiver<()>, async_channel::Receiver<TransferEvent>) {
        let (queue, events) = TransferQueue::new(runtime.clone());
        let (changed, changes) = async_channel::unbounded();
        let inner = Inner {
            state: LinkState::Idle,
            opened_before: false,
            connection: None,
            attempt: 0,
            opening: None,
            reopens: VecDeque::new(),
        };
        let shared = Arc::new(Shared { inner: Mutex::new(inner), runtime, queue, changed });
        (Self { shared }, changes, events)
    }

    /// The transfer queue of the connection.
    pub fn queue(&self) -> &TransferQueue {
        &self.shared.queue
    }

    /// Where the link stands.
    pub fn state(&self) -> LinkState {
        self.shared.lock().state.clone()
    }

    /// Something needs the session: opens it unless it is open or being opened (or the link is closed).
    /// A link that failed or lost its connection tries again.
    pub fn open(&self, connection: &Connection) {
        let mut inner = self.shared.lock();
        if matches!(inner.state, LinkState::Closed) {
            return;
        }
        inner.connection = Some(connection.downgrade());
        if matches!(inner.state, LinkState::Idle | LinkState::Failed(_) | LinkState::Disconnected) {
            self.shared.start_open(&mut inner, connection.clone());
            drop(inner);
            self.shared.notify();
        }
    }

    /// The connection is gone: the session is dropped and the transfers pause.
    pub fn connection_lost(&self) {
        {
            let mut inner = self.shared.lock();
            if matches!(inner.state, LinkState::Closed | LinkState::Idle) {
                return;
            }
            if let Some(opening) = inner.opening.take() {
                opening.abort();
            }
            inner.attempt += 1;
            inner.state = LinkState::Disconnected;
        }
        self.shared.queue.set_session(None);
        self.shared.notify();
    }

    /// The connection is back. If a session had been open before, a new one is opened; otherwise
    /// nothing happens until something asks for it.
    pub fn connection_restored(&self, connection: &Connection) {
        let mut inner = self.shared.lock();
        if matches!(inner.state, LinkState::Closed) {
            return;
        }
        inner.connection = Some(connection.downgrade());
        if inner.opened_before
            && matches!(inner.state, LinkState::Idle | LinkState::Failed(_) | LinkState::Disconnected)
        {
            self.shared.start_open(&mut inner, connection.clone());
            drop(inner);
            self.shared.notify();
        }
    }

    /// Passes on an event of the queue: [`TransferEvent::SessionLost`] while the session is the
    /// current one reopens it (if the connection is still up), unless it keeps dying.
    pub fn handle_event(&self, event: &TransferEvent) {
        if !matches!(event, TransferEvent::SessionLost) {
            return;
        }
        let inner = self.shared.lock();
        if matches!(inner.state, LinkState::Ready(_)) {
            self.reopen(inner);
        }
    }

    /// Something other than a transfer found `session` unusable (a request to list a directory,
    /// say, came back with the connection gone or a timeout: see
    /// [`SftpError::is_session_lost`](crate::SftpError::is_session_lost)). Nothing else would
    /// notice: with no transfer running the queue says nothing, and the SFTP subsystem can die
    /// while the SSH connection lives on.
    ///
    /// Acts only if `session` is still the current one (a late report about a session that was
    /// replaced already is ignored). Then it is handled like [`TransferEvent::SessionLost`]: the
    /// session is reopened, subject to the same rate limit and ending in the same
    /// [`LinkState::Failed`] when it keeps dying.
    pub fn session_lost(&self, session: &Arc<SftpSession>) {
        let inner = self.shared.lock();
        if matches!(&inner.state, LinkState::Ready(current) if Arc::ptr_eq(current, session)) {
            self.reopen(inner);
        }
    }

    /// Replaces the current (dead) session with a new one, unless that happens too often or the
    /// connection is gone as well.
    fn reopen(&self, mut inner: MutexGuard<'_, Inner>) {
        if !reopen_allowed(&mut inner.reopens, Instant::now()) {
            inner.state = LinkState::Failed("SFTP 会话反复中断，请稍后点刷新重试".into());
            drop(inner);
            self.shared.notify();
            return;
        }
        match inner.connection.as_ref().and_then(WeakConnection::upgrade) {
            Some(connection) => self.shared.start_open(&mut inner, connection),
            // The connection is gone as well: the owner finds out and says so (`connection_lost`).
            None => inner.state = LinkState::Disconnected,
        }
        drop(inner);
        self.shared.notify();
    }

    /// Stops everything: the transfers pause and the session is let go. Idempotent; also done on drop.
    pub fn shutdown(&self) {
        {
            let mut inner = self.shared.lock();
            if matches!(inner.state, LinkState::Closed) {
                return;
            }
            if let Some(opening) = inner.opening.take() {
                opening.abort();
            }
            inner.attempt += 1;
            inner.state = LinkState::Closed;
        }
        self.shared.queue.shutdown();
    }
}

impl Drop for SftpLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_that_keeps_dying_is_not_reopened_for_ever() {
        let start = Instant::now();
        let mut log = VecDeque::new();
        let at = |secs| start + Duration::from_secs(secs);
        assert!(reopen_allowed(&mut log, at(0)));
        assert!(reopen_allowed(&mut log, at(2)));
        assert!(reopen_allowed(&mut log, at(4)));
        assert!(!reopen_allowed(&mut log, at(6)), "the fourth time within ten seconds");
        assert!(!reopen_allowed(&mut log, at(9)));
        assert!(reopen_allowed(&mut log, at(11)), "the first reopen is ten seconds old by now");
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn states_have_readable_debug_output() {
        assert_eq!(format!("{:?}", LinkState::Idle), "Idle");
        assert_eq!(format!("{:?}", LinkState::Failed("x".into())), "Failed(x)");
        assert!(LinkState::Opening.session().is_none());
    }
}
