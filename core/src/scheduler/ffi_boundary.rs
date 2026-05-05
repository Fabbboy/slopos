#![allow(unsafe_op_in_unsafe_fn)]

//! FFI Boundary Layer for Scheduler
//!
//! Thin shim around the two cross-language symbols still needed:
//!   - `scheduler_task_exit`: callable from Rust (`kthread`).  The
//!     OSTD trampoline reaches the same impl through the registered
//!     `TaskExitHook` (see `super::scheduler::install_ostd_task_exit_hook`).
//!   - `kernel_stack_top`: a linker symbol exported by
//!     `boot/limine_entry.s` (the BSP boot stack top).  Read by
//!     `scheduler::prepare_switch_to` to seed `TSS.RSP0` for
//!     kernel-mode tasks (which don't own a per-task kernel stack).
//!
//! `slopos_ostd::task::switch` (Rust naked functions) is the sole
//! context-switch implementation; the historical extern-C helpers that
//! used to live in a separate asm file (`context_switch`,
//! `context_switch_user`, `simple_context_switch`,
//! `init_kernel_context`, `task_entry_wrapper`,
//! `context_switch_bad_target`) are gone.

/// Task exit hook called from the OSTD `task_entry_trampoline` (when a
/// kernel task's entry function returns) and from `super::kthread`.
#[unsafe(no_mangle)]
pub extern "C" fn scheduler_task_exit() -> ! {
    super::scheduler::scheduler_task_exit_impl()
}

unsafe extern "C" {
    #[link_name = "kernel_stack_top"]
    static kernel_stack_top_impl: u8;
}

/// Address of the BSP boot stack top.  Used by `prepare_switch_to`
/// for kernel-mode tasks (which don't carry their own per-task kernel
/// stack).
pub fn kernel_stack_top() -> *const u8 {
    unsafe { &kernel_stack_top_impl }
}
