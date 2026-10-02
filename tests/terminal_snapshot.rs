//! Terminal snapshot tests for sticky footer rendering
//!
//! Enabled with the Unix-only terminal-snapshot feature.

#![cfg(all(unix, feature = "terminal-snapshot"))]

#[path = "terminal_snapshot/mod.rs"]
mod terminal_snapshot;
