//! The 端口转发 panel. [`model`] holds everything the panel says and decides (form checks, warning
//! lines, the texts of states and buttons, the summary, the confirmations) and needs no window;
//! [`panel`] draws it.

#[cfg(debug_assertions)]
pub mod demo;
pub mod model;
pub mod panel;
