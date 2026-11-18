//! SFTP for Shellmate: browsing and changing files on a server, and reading and saving text
//! files for the built-in editor.
//!
//! The crate does not depend on the UI. A session runs on an SSH [`Connection`]
//! ([`SftpSession::open`]); remote paths are POSIX strings (see [`path`]). Errors carry the
//! Chinese messages the UI shows ([`SftpError`]).

#![warn(missing_docs)]

mod entry;
mod error;
mod format;
pub mod link;
mod listing;
pub mod path;
mod session;
mod text;
pub mod transfer;

pub use entry::{Entry, EntryKind, Link, Revision, natural_cmp, sort_entries};
pub use error::SftpError;
pub use format::{format_size, mode_octal, mode_string};
pub use listing::Listed;
pub use session::{POSIX_RENAME, ServerInfo, ServerLimits, SftpSession};
pub use shellmate_ssh::Connection;
pub use text::{
    InPlaceReason, LineEnding, MAX_TEXT_SIZE, SaveFormat, SaveOutcome, TextFile, WriteMode, decode_text,
    detect_line_ending, encode_text, save_temp_path,
};
