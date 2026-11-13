//! Terminal emulation for Shellmate: alacritty's grid rendered with gpui.

pub mod colors;
mod element;
mod keys;
mod pty;
pub mod terminal;
pub mod view;

pub use colors::TerminalPalette;
pub use element::TerminalStyle;
pub use pty::{ProcessInfo, PtyOptions};
pub use terminal::{Terminal, TerminalEvent, TerminalSize};
pub use view::{TerminalView, init};
