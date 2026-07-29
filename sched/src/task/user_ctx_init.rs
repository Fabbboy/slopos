//! User-context seeding helpers for newly-created and forked tasks.
//!
//! Lives in `sched/` alongside `task_lifecycle.rs` so neither
//! `sched/` nor `core/` has to depend on the other. The exec path
//! (in `core::exec`) calls [`init_user_ctx_for_new_task`] for the
//! freshly-loaded ELF entry; the fork/clone path in
//! `task_lifecycle.rs` calls [`init_user_ctx_from_parent_frame`].

use slopos_arch::InterruptFrame;
use slopos_ostd::user::context::{UserContext, UserRegs};

/// Seed a freshly-created user task's [`UserContext`] from
/// (entry_point, stack_pointer, entry_arg) the legacy task-create
/// path used to encode in a synthetic `InterruptFrame`.
pub fn init_user_ctx_for_new_task(
    ctx: &UserContext,
    entry_point: u64,
    stack_pointer: u64,
    entry_arg: u64,
) {
    let mut regs = UserRegs::default();
    regs.rip = entry_point;
    regs.rsp = stack_pointer;
    regs.rdi = entry_arg;
    regs.rflags_user_subset = 0x202;
    ctx.set_regs(regs);
}

/// Seed a forked / cloned child's [`UserContext`] from the parent's
/// syscall-time `InterruptFrame`. Caller guarantees `frame` is the
/// parent's frame at SYSCALL exit. `force_rax` is the value to install
/// in the child's RAX (typically 0 for fork's child return).
pub fn init_user_ctx_from_parent_frame(ctx: &UserContext, frame: &InterruptFrame, force_rax: u64) {
    let mut regs = UserRegs::default();
    regs.r15 = frame.r15;
    regs.r14 = frame.r14;
    regs.r13 = frame.r13;
    regs.r12 = frame.r12;
    regs.r11 = frame.r11;
    regs.r10 = frame.r10;
    regs.r9 = frame.r9;
    regs.r8 = frame.r8;
    regs.rbp = frame.rbp;
    regs.rdi = frame.rdi;
    regs.rsi = frame.rsi;
    regs.rdx = frame.rdx;
    regs.rcx = frame.rcx;
    regs.rbx = frame.rbx;
    regs.rax = force_rax;
    regs.rip = frame.rip;
    regs.rsp = frame.rsp;
    regs.rflags_user_subset = frame.rflags;
    ctx.set_regs(regs);
}
