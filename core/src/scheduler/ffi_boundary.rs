#![allow(unsafe_op_in_unsafe_fn)]

//! FFI Boundary Layer for Scheduler
//!
//! This module contains ONLY functions that require `extern "C"` linkage because they are:
//! 1. Called from assembly code (context_switch.s)
//! 2. Defined in assembly and called from Rust
//!
//! All other Rust-to-Rust calls should use regular Rust functions without extern "C".

use super::task::TaskContext;
use slopos_utils::klog_info;

// ============================================================================
// Functions called FROM assembly (must be extern "C")
// ============================================================================

/// Task exit function called from context_switch.s (task_entry_wrapper)
/// This is called when a task returns from its entry function
#[unsafe(no_mangle)]
pub extern "C" fn scheduler_task_exit() -> ! {
    super::scheduler::scheduler_task_exit_impl()
}

/// Recovery hook called from context_switch.s when a kernel-mode switch target
/// has an invalid RIP outside kernel .text.
#[unsafe(no_mangle)]
pub extern "C" fn context_switch_bad_target(new_context: *const TaskContext) -> ! {
    let (rip, cs, rsp, cr3) = if new_context.is_null() {
        (0u64, 0u64, 0u64, 0u64)
    } else {
        unsafe {
            (
                core::ptr::read_unaligned(core::ptr::addr_of!((*new_context).rip)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*new_context).cs)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*new_context).rsp)),
                core::ptr::read_unaligned(core::ptr::addr_of!((*new_context).cr3)),
            )
        }
    };

    klog_info!(
        "SCHED: blocked invalid kernel context target cs=0x{:x} rip=0x{:x} rsp=0x{:x} cr3=0x{:x}",
        cs,
        rip,
        rsp,
        cr3
    );

    let current = super::scheduler::scheduler_get_current_task();
    if !current.is_null() && super::task::task_pointer_is_valid(current as *const _) {
        let task_id = unsafe { (*current).task_id };
        let _ = crate::task::task_terminate(task_id);
    }

    super::scheduler::clear_scheduler_current_task();
    super::scheduler::schedule();

    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

// ============================================================================
// Functions defined IN assembly (must be declared as extern "C")
// ============================================================================

// Functions defined in assembly (context_switch.s) - these are just declarations
// The assembly exports: context_switch, context_switch_user, simple_context_switch,
// init_kernel_context, task_entry_wrapper, and kernel_stack_top
unsafe extern "C" {
    #[link_name = "context_switch"]
    fn context_switch_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    #[link_name = "context_switch_user"]
    fn context_switch_user_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    #[link_name = "simple_context_switch"]
    fn simple_context_switch_impl(old_context: *mut TaskContext, new_context: *const TaskContext);
    #[link_name = "init_kernel_context"]
    fn init_kernel_context_impl(context: *mut TaskContext);
    #[link_name = "task_entry_wrapper"]
    fn task_entry_wrapper_impl();
    #[link_name = "kernel_stack_top"]
    static kernel_stack_top_impl: u8;
}

// Public wrappers for assembly functions
pub unsafe fn context_switch(old_context: *mut TaskContext, new_context: *const TaskContext) {
    context_switch_impl(old_context, new_context);
}

pub unsafe fn context_switch_user(old_context: *mut TaskContext, new_context: *const TaskContext) {
    context_switch_user_impl(old_context, new_context);
}

pub unsafe fn simple_context_switch(
    old_context: *mut TaskContext,
    new_context: *const TaskContext,
) {
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
