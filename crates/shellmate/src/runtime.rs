//! The background tokio runtime for SSH, kept as a gpui global.

use gpui::{App, Global};
use shellmate_ssh::SshRuntime;

/// Holds the [`SshRuntime`] so views can reach it through `cx`.
pub struct SshRuntimeGlobal(pub SshRuntime);

impl Global for SshRuntimeGlobal {}

/// Starts the runtime (2 worker threads). Failing to create threads is not fatal:
/// the app then simply cannot connect and says so when asked to.
pub fn init(cx: &mut App) {
    match SshRuntime::new() {
        Ok(runtime) => cx.set_global(SshRuntimeGlobal(runtime)),
        Err(err) => log::error!("could not start the SSH runtime: {err}"),
    }
}

/// The SSH runtime, if it could be started.
pub fn get(cx: &App) -> Option<SshRuntime> {
    cx.try_global::<SshRuntimeGlobal>().map(|global| global.0.clone())
}
