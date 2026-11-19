//! Release discovery, transfer and macOS bundle installation. No application/UI state.
pub(crate) mod check;
pub(crate) mod download;
pub(crate) mod install;
pub(crate) mod release;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DOWNLOAD_PAGE: &str = "https://github.com/rermrf/shellmate/releases/latest";

#[cfg(test)]
mod tests;
