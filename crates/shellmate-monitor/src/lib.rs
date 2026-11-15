//! The monitor behind Shellmate's 监控 view: probes a Linux server over an SSH connection, follows a
//! collector script on a long-lived exec channel, and turns what it writes into [`Snapshot`]s with the
//! rates worked out (CPU, network, per process).
//!
//! The crate does not depend on the UI. A [`Monitor`] runs on the SSH runtime and reports
//! [`MonitorEvent`]s over an `async-channel`; it only holds the connection weakly (and, while it
//! collects, through the exec channel it reads), and it installs nothing on the server. Everything that
//! can be a pure function is one: the parsers, the rates, the command builders and the shell quoting.
//! Process command lines and data from the server are never logged.

#![warn(missing_docs)]

pub mod commands;
pub mod engine;
pub mod frame;
mod monitor;
pub mod parse;
pub mod probe;
pub mod quote;
pub mod script;
pub mod snapshot;

pub use monitor::{ActionId, ActionOutcome, ActionReport, Monitor, MonitorEvent, Options, Phase};
pub use probe::{ContainerAccess, ContainerRuntime, Probe, RuntimeKind, Sudo, Systemd};
pub use snapshot::{CpuCard, Disk, HostFacts, MemoryCard, NetworkCard, Process, Snapshot};
