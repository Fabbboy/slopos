#![allow(unsafe_op_in_unsafe_fn)]

//! FFI Boundary Layer for Scheduler
//!
//! This module contains ONLY functions that require `extern "C"` linkage because they are:
//! 1. Called from assembly code (context_switch.s)
//! 2. Defined in assembly and called from Rust
//!
//! All other Rust-to-Rust calls should use regular Rust functions without extern "C".

use crate::task::TaskContext;

// ============================================================================
// Functions called FROM assembly (must be extern "C")
// ============================================================================

/// Task exit function called from context_switch.s (task_entry_wrapper)
/// This is called when a task returns from its entry function
#[unsafe(no_mangle)]
pub extern "C" fn scheduler_task_exit() -> ! {
    crate::scheduler::scheduler_task_exit_impl();
}

// ============================================================================
// Functions defined IN assembly (must be declared as extern "C")
// ============================================================================

// Functions defined in assembly (context_switch.s) - these are just declarations
unsafe extern "C" {
    fn context_switch_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn context_switch_user_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn simple_context_switch_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    fn init_kernel_context_impl(context: *mut TaskContext);
    fn task_entry_wrapper_impl();
    static kernel_stack_top_impl: u8;
}

// Public wrappers for assembly functions
pub unsafe fn context_switch(old_context: *mut TaskContext, new_context: *const TaskContext) {
    context_switch_impl(old_context, new_context);
}

pub unsafe fn context_switch_user(old_context: *mut TaskContext, new_context: *const TaskContext) {
    context_switch_user_impl(old_context, new_context);
}

pub unsafe fn simple_context_switch(old_context: *mut TaskContext, new_context: *const TaskContext) {
    simple_context_switch_impl(old_context, new_context);
}

pub unsafe fn init_kernel_context(context: *mut TaskContext) {
    init_kernel_context_impl(context);
}

pub unsafe fn task_entry_wrapper() {
    task_entry_wrapper_impl();
}

pub fn kernel_stack_top() -> *const u8 {
    unsafe { &kernel_stack_top_impl }
}

