//! Terminal emulation for Shellmate: alacritty's grid rendered with gpui.

pub mod colors;
mod element;
mod keys;
mod pty;
pub mod terminal;
pub mod view;

pub use colors::TerminalPalette;
pub use element::TerminalStyle;
pub use pty::PtyOptions;
pub use shellmate_core::io::{OutputSink, ProcessInfo, PtyDims, TerminalBackend};
pub use terminal::{SessionEnd, Terminal, TerminalEvent, TerminalSize};
pub use view::{TerminalView, init};
