//! Terminal emulation for Shellmate: alacritty's grid rendered with gpui.

pub mod colors;
mod element;
mod keys;
pub mod history;
mod osc7;
mod send;
mod pty;
pub mod terminal;
pub mod view;

pub use colors::TerminalPalette;
pub use element::TerminalStyle;
pub use pty::{PtyOptions,LocalPty,nonzero_dims};
pub use shellmate_core::io::{OutputSink, ProcessInfo, PtyDims, TerminalBackend};
pub use terminal::{SessionEnd, Terminal, TerminalEvent, TerminalSize};
pub use view::{TerminalView, init};

pub mod archive;
pub mod cast;
pub mod session_log;
mod capture_output;
pub mod replay;
pub use capture_output::RecordingResult;
mod behavior;
