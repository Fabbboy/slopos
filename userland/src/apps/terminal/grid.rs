//! Re-export of the terminal grid model, which lives in the host-testable
//! `slopos-terminal-core` crate. Kept as a module path so existing
//! `crate::apps::terminal::grid::*` references resolve unchanged.

pub use slopos_terminal_core::grid::*;
