//! An argv/environment launch specification. Environment values are redacted in
//! Debug output and zeroized on drop, including transport session keys.
use crate::Secret;
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Debug)]
pub struct Program {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, Secret>,
    pub cwd: Option<PathBuf>,
}
