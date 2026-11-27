//! Shellmate domain logic that does not depend on the UI toolkit.

pub mod ai_output;
pub mod filter;
pub mod host;
pub mod import;
pub mod io;
pub mod terminal_interaction;
pub mod program;
pub use program::Program;
#[cfg(any(test, feature = "test-support"))]
pub mod sample;
pub mod secret;
pub mod serial;
pub use serial::{Newline, SerialSettings};
pub mod ssh_config;

pub use filter::{HostFilter, HostSection, sections};
pub use host::{
    AuthMethod, FingerprintState, Group, GroupId, GroupTone, Host, HostBook, HostId, Protocol, minutes_since,
    relative_time,
};
pub use import::{ImportCandidate, ImportPlan, candidates, plan_import};
pub use io::{OutputHandler, OutputSink, ProcessInfo, PtyDims, TerminalBackend};
pub use secret::Secret;
pub use ssh_config::{ConfigForward, ForwardDirective, ParsedConfig, SshConfigEntry, parse_ssh_config};

pub mod memory;
