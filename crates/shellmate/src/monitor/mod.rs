//! The 监控 panel. [`model`] and [`process`] hold everything the panel says and decides (the header, the
//! cards, what needs attention, the process table, the buttons and the commands behind them) and need no
//! window; [`panel`] draws it and [`charts`] draws the sparklines.

pub mod charts;
#[cfg(debug_assertions)]
pub mod demo;
pub mod format;
pub mod model;
pub mod panel;
pub mod process;
