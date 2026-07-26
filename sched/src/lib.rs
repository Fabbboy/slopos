#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(feature = "test-hooks", feature(allocator_api))]
#![feature(sync_unsafe_cell)]

#[cfg(feature = "test-hooks")]
pub mod context_tests;
pub mod fate_api;
pub mod ffi_boundary;
pub mod futex;
#[cfg(feature = "test-hooks")]
pub mod inspect;
pub mod kthread;
pub mod lifecycle;
pub mod per_cpu;
pub mod runtime;
pub mod safestack_rt;
#[cfg(feature = "test-hooks")]
pub mod sched_tests;
pub mod scheduler;
pub mod sleep;
pub mod task;
pub mod task_stack;
pub mod task_struct;
#[cfg(feature = "test-hooks")]
pub mod test_fixture;
#[cfg(feature = "test-hooks")]
pub mod test_hermetic;
pub mod trap;
pub mod work_steal;

// Re-export the OSTD-owned modules under their historical kernel-side
// paths so existing `crate::{exit_info, task_state, test_reports}`
// imports inside moved files continue to resolve.
pub use slopos_ostd::task::exit_info;
pub use slopos_ostd::task::state as task_state;
pub use slopos_ostd::task::test_reports;

pub use exit_info::ExitInfo;
