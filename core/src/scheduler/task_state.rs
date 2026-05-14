//! Re-export of the OSTD-owned fused task lifecycle state.
//!
//! The body was relocated to `slopos_ostd::task::state` so the
//! atomic-word manipulation lives in the trusted-domain crate.
//! Kernel callers continue to spell the type as
//! `crate::scheduler::task_state::TaskState`.

pub use slopos_ostd::task::state::{TaskState, TaskStateView};
