//! Release discovery, transfer and macOS bundle installation. No application/UI state.
pub(crate) mod check;
pub(crate) mod download;
pub(crate) mod install;
pub(crate) mod release;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A `flock` is shared with a forked child until it execs, so a lock that was just released can
/// look busy for a moment whenever the app (PTYs, helpers) or a test spawns a process. Retry for up
/// to five seconds before deciding that another Shellmate really holds it.
pub(crate) const LOCK_ATTEMPTS: u32 = 100;
pub(crate) const LOCK_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
pub const DOWNLOAD_PAGE: &str = "https://github.com/rermrf/shellmate/releases/latest";

#[cfg(test)]
mod tests;
