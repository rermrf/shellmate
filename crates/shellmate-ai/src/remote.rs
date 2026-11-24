//! Per-conversation discovery state; intentionally not persisted with the log.
use std::collections::HashSet;
#[derive(Clone, Debug, Default)]
pub struct State {
    pub retry_trust: bool,
    pub requested_cwd: Option<String>,
    pub cwd: String,
    pub prompt_cwd: Option<String>,
    pub loaded_paths: HashSet<String>,
    pub snapshot_texts: HashSet<String>,
}
