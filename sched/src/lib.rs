#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(feature = "test-hooks", feature(allocator_api))]

#[cfg(feature = "test-hooks")]
pub mod context_tests;
pub mod fate_api;
pub mod ffi_boundary;
pub mod futex;
pub mod kconsole;
pub mod lifecycle;
pub mod per_cpu;
pub mod quota_console;
pub mod runtime;
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

pub use slopos_ostd::task::exit_info;
pub use slopos_ostd::task::state as task_state;
pub use slopos_ostd::task::test_reports;

pub use exit_info::ExitInfo;
