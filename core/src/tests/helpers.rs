//! Shared fixtures for test-hooks-gated test modules in this crate.

use core::ffi::c_void;

/// No-op task body; `extern "C"` to match the scheduler's `TaskEntry` alias.
pub extern "C" fn dummy_task_entry(_arg: *mut c_void) {}
