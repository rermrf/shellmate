//! The 文件 panel, shared by local terminals and SFTP sessions.

pub mod draw;
pub mod format;
pub(crate) mod local;
mod local_copy;
#[cfg(debug_assertions)]
pub(crate) mod local_fixture;
mod source;
pub mod model;
pub mod panel;
pub mod queue_card;
pub mod transfers;
