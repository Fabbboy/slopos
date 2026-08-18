//! Context-switch register snapshot.
//!
//! The switch assembly in [`super::switch`] reads [`TaskContext`]'s register
//! offsets via `offset_of!`, so the field order is an ABI contract pinned by
//! the `const _` asserts below and must not change without the asm.

use core::mem::offset_of;

/// Callee-saved register snapshot for software context switch.
///
/// `preempt_count` is a property of the *task*, cached in the per-CPU PCR for
/// cheap guard inc/dec; every switch swaps it with the PCR so a guard's
/// increment and decrement stay balanced across a migration.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskContext {
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rflags: u64,
    pub rip: u64,
    /// Saved per-task preemption-disable count, swapped with the PCR by
    /// `switch_context`.
    pub preempt_count: u64,
}

impl TaskContext {
    /// All-zero context with `rflags` set to a sensible default
    /// (IF=1, IOPL=0).
    pub const fn zero() -> Self {
        Self {
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rflags: 0x202,
            rip: 0,
            preempt_count: 0,
        }
    }

    /// Build a context that first resumes at `trampoline` with `entry_point` in
    /// `r12` and `arg` in `r13`; the trampoline calls `entry_point(arg)` and
    /// then the task-exit hook.
    pub const fn new_for_task(entry_point: u64, arg: u64, stack_top: u64, trampoline: u64) -> Self {
        Self {
            rbx: 0,
            r12: entry_point,
            r13: arg,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: stack_top - 8,
            rflags: 0x202,
            rip: trampoline,
            preempt_count: 0,
        }
    }
}

const _: () = assert!(core::mem::size_of::<TaskContext>() == 80);
const _: () = assert!(offset_of!(TaskContext, rbx) == 0);
const _: () = assert!(offset_of!(TaskContext, r12) == 8);
const _: () = assert!(offset_of!(TaskContext, r13) == 16);
const _: () = assert!(offset_of!(TaskContext, r14) == 24);
const _: () = assert!(offset_of!(TaskContext, r15) == 32);
const _: () = assert!(offset_of!(TaskContext, rbp) == 40);
const _: () = assert!(offset_of!(TaskContext, rsp) == 48);
const _: () = assert!(offset_of!(TaskContext, rflags) == 56);
const _: () = assert!(offset_of!(TaskContext, rip) == 64);
// `preempt_count` lives past the asm-visible register block; the switch
// asm never touches it, so its offset is not part of the asm contract.
const _: () = assert!(offset_of!(TaskContext, preempt_count) == 72);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_context_zero_has_default_rflags() {
        let ctx = TaskContext::zero();
        assert_eq!(ctx.rflags, 0x202);
    }

    #[test]
    fn task_context_new_for_task_layout() {
        let ctx = TaskContext::new_for_task(0xAAAA, 0xBBBB, 0x10000, 0xCCCC);
        assert_eq!(ctx.r12, 0xAAAA);
        assert_eq!(ctx.r13, 0xBBBB);
        assert_eq!(ctx.rsp, 0x10000 - 8);
        assert_eq!(ctx.rip, 0xCCCC);
        assert_eq!(ctx.rflags, 0x202);
    }
}
