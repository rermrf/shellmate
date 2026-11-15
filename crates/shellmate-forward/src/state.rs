//! What a rule is doing, and how the outside world hears about it.

use crate::rule::{ForwardRule, RuleId};
use crate::stats::StatsSnapshot;

/// Why a rule is not running. `Display` is the text shown on the rule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// The local port is taken by something else.
    #[error("本地端口 {port} 已被占用")]
    PortInUse {
        /// Where the rule wanted to listen.
        host: String,
        /// The port that is taken.
        port: u16,
        /// A free port to offer instead ("换用 M 重试"), if one was found.
        suggestion: Option<u16>,
    },
    /// The local address could not be listened on for another reason.
    #[error("无法监听 {host}:{port}：{reason}")]
    BindFailed {
        /// The address.
        host: String,
        /// The port.
        port: u16,
        /// What went wrong.
        reason: String,
    },
    /// The server refused to listen for a remote forward.
    #[error("服务器拒绝了远程转发（可能禁用了 AllowTcpForwarding）")]
    RemoteDenied,
    /// A remote forward could not be set up for another reason.
    #[error("{0}")]
    Remote(String),
    /// The rule itself is incomplete (a local or remote forward without a target).
    #[error("规则缺少目标地址")]
    MissingTarget,
}

impl RuleError {
    /// The port to offer after [`PortInUse`](Self::PortInUse).
    pub fn suggested_port(&self) -> Option<u16> {
        match self {
            RuleError::PortInUse { suggestion, .. } => *suggestion,
            _ => None,
        }
    }
}

/// Where a started rule stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleState {
    /// Not started (or stopped).
    Stopped,
    /// A remote forward waiting for the server's answer.
    Starting,
    /// Listening; `port` is the port actually in use (what the server chose for a remote
    /// forward asked to use port 0).
    Running {
        /// The listening port.
        port: u16,
    },
    /// A remote forward on a fixed port that the server granted earlier in this run and now refuses: the
    /// port is probably still held by a session from before a network drop. The server is asked again
    /// (2 s, 5 s, 10 s, then every 30 s) while the connection is up, or at once with
    /// [`Forwarder::retry`](crate::Forwarder::retry).
    RemotePortHeld {
        /// The port the server refuses.
        port: u16,
    },
    /// The connection is down. Local and dynamic forwards keep their port and close new
    /// connections at once; remote forwards are asked for again after the reconnect.
    WaitingReconnect,
    /// The rule could not start.
    Failed(RuleError),
}

impl RuleState {
    /// Whether the rule is serving connections.
    pub fn is_running(&self) -> bool {
        matches!(self, RuleState::Running { .. })
    }
}

/// A change the UI should hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardEvent {
    /// A rule's state changed.
    State {
        /// The rule.
        id: RuleId,
        /// What it is doing now.
        state: RuleState,
    },
    /// New numbers for a rule. At most one per rule per second, and only when they changed.
    Stats {
        /// The rule.
        id: RuleId,
        /// The numbers.
        stats: StatsSnapshot,
    },
}

/// A started rule as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleView {
    /// The rule.
    pub rule: ForwardRule,
    /// What it is doing.
    pub state: RuleState,
    /// Connections and traffic since it started.
    pub stats: StatsSnapshot,
}
