//! Shellmate domain logic that does not depend on the UI toolkit.

pub mod filter;
pub mod host;
pub mod sample;

pub use filter::{HostFilter, HostSection, sections};
pub use host::{AuthMethod, FingerprintState, Group, GroupTone, Host, HostBook, Protocol, relative_time};
