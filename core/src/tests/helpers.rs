//! Shared fixtures for test-hooks-gated test modules in this crate.

use core::ffi::c_void;

/// No-op task entry point usable as a placeholder for tests that
/// only care about the task struct, not the body.
///
/// `extern "C"` to match the `TaskEntry` alias the kernel scheduler
/// exports.
pub extern "C" fn dummy_task_entry(_arg: *mut c_void) {}
