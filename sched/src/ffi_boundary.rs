//! Thin shim over the task-exit path and the `kernel_stack_top` linker symbol
//! exported by `boot/limine_entry.s`. `slopos_ostd::task::switch` is the sole
//! context-switch implementation.

/// Task exit path for `super::kthread`. OSTD's `task_entry_trampoline`
/// reaches the same impl through the registered `TaskExitHook`, so this
/// needs no C linkage.
pub fn scheduler_task_exit() -> ! {
    super::scheduler::scheduler_task_exit_impl()
}

/// Address of the BSP boot stack top. `prepare_switch_to` seeds `TSS.RSP0`
/// with it for kernel-mode tasks, which carry no per-task kernel stack.
pub fn kernel_stack_top() -> *const u8 {
    slopos_ostd::arch::x86_64::linker::kernel_stack_top()
}
