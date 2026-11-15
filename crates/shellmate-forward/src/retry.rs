//! When a remote forward is asked for again, and what the engine remembers about the server's answers.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::addr::Endpoint;
use crate::rule::RuleId;

/// The remote forwards (rule and the address asked for) a server has granted during this run. After a
/// network drop the old session may keep a fixed port for hours (TCP keepalive); a refusal of a port
/// that was granted before means "still held", not "forwarding is forbidden".
///
/// Clones share what they know, so one memory can serve every connection to a host: the rules of a host
/// move from one connection to another and the answer of the server is still remembered.
#[derive(Clone, Default)]
pub struct GrantMemory(Arc<Mutex<HashSet<(RuleId, Endpoint)>>>);

impl GrantMemory {
    /// A memory that knows nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn remember(&self, id: &RuleId, listen: &Endpoint) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).insert((id.clone(), listen.clone()));
    }

    /// Whether the server has granted `listen` to the rule `id` before.
    pub(crate) fn knows(&self, id: &RuleId, listen: &Endpoint) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).contains(&(id.clone(), listen.clone()))
    }
}

impl std::fmt::Debug for GrantMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GrantMemory({} grants)", self.0.lock().unwrap_or_else(PoisonError::into_inner).len())
    }
}

/// How long the engine waits between asking the server for a fixed remote port again. The tests
/// use short times.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryTiming {
    /// Right after a refusal the port may only be on its way out (a reconnect, a rule switched off and
    /// on, a rule that moved to another connection): ask again this many times, quietly.
    pub quick_retries: u32,
    /// How far apart the quiet attempts are.
    pub quick_delay: Duration,
    /// Once a port that was granted before is still refused, the state says so and the server is asked
    /// again after these delays, one after the other; the last one repeats until the connection is gone.
    pub held_delays: Vec<Duration>,
}

impl Default for RetryTiming {
    fn default() -> Self {
        Self {
            quick_retries: 5,
            quick_delay: Duration::from_millis(400),
            held_delays: [2, 5, 10, 30].map(Duration::from_secs).to_vec(),
        }
    }
}

impl RetryTiming {
    /// The wait before the `round`-th (from 0) renewed attempt while the port is held.
    pub(crate) fn held_delay(&self, round: usize) -> Duration {
        self.held_delays.get(round).or(self.held_delays.last()).copied().unwrap_or(Duration::from_secs(30))
    }
}
