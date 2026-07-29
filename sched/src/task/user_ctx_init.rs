//! User-context seeding for newly-created tasks.
//!
//! Lives in `sched/` alongside `task_lifecycle.rs` so neither `sched/`
//! nor `core/` has to depend on the other. The exec path (in
//! `core::exec`) calls [`init_user_ctx_for_new_task`] for the
//! freshly-loaded ELF entry; fork and clone seed their child from the
//! parent's live `UserContext` in `task_lifecycle.rs`.

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
