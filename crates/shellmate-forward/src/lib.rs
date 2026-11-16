//! Port forwarding for Shellmate: local (`-L`), remote (`-R`) and dynamic SOCKS5 (`-D`)
//! rules on top of an SSH [`Connection`](shellmate_ssh::Connection).
//!
//! The crate does not depend on the UI. A [`Forwarder`] runs the rules of one connection on
//! the SSH runtime and reports [`ForwardEvent`]s over an `async-channel`; it never keeps the
//! connection open by itself. Rule and address types parse what users type and explain
//! mistakes in Chinese. Forwarded data is never logged.

#![warn(missing_docs)]

mod addr;
mod conn;
mod forwarder;
mod listen;
mod retry;
mod rule;
mod socks;
mod state;
mod stats;

pub use addr::{AddrError, Endpoint, parse_listen, parse_target};
pub use forwarder::{Forwarder, Options};
pub use listen::pick_local_port;
pub use retry::{GrantMemory, RetryTiming};
pub use rule::{ForwardKind, ForwardRule, RuleId};
pub use state::{ForwardEvent, RuleError, RuleState, RuleView};
pub use stats::StatsSnapshot;
