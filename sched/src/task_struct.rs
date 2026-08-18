//! Kernel-side aliases for the OSTD-owned generic `TaskInner<K, U>`, plus the
//! razor assertions that pin the field-offset contract at the kernel boundary.

use core::mem::offset_of;

use slopos_abi::signal::NSIG;
use slopos_ostd::task::kernel_task::SignalActionCell;

use crate::task_stack::{KernelStack, UnsafeStack};

pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN, TASK_STACK_SIZE,
    TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord, TaskFaultReason, TaskPriority,
    TaskStatus,
};
pub use slopos_ostd::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;
pub use slopos_ostd::task::fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT};
pub use slopos_ostd::task::kernel_task::{
    SignalAction, SwitchContext, TaskContext, TaskInner, fpu_reset_in_place,
};

// Declared so `CurrentTask` and the PCR publisher agree on the monomorphisation
// the current-task slot holds. Both names are aliases, so the impl heads are the
// local `TaskStack<_>` — which is what makes them legal to write here at all.
slopos_ostd::declare_pcr_stack_type!(KernelStack);
slopos_ostd::declare_pcr_stack_type!(UnsafeStack);

pub type Task = TaskInner<KernelStack, UnsafeStack>;

/// Borrow of the task running on this CPU, at the concrete kernel
/// monomorphisation.
pub type Current = slopos_ostd::task::CurrentTask<KernelStack, UnsafeStack>;

/// Borrow of this CPU's idle task, at the concrete kernel monomorphisation.
pub type Idle = slopos_ostd::task::IdleTask<KernelStack, UnsafeStack>;

/// Exclusive access to one endpoint of a context switch, at the concrete
/// kernel monomorphisation. Minted only by `slopos_ostd::task::run_switch`.
pub type Switching<'a> = slopos_ostd::task::SwitchWindow<'a, KernelStack, UnsafeStack>;

/// Racy, lock-free, allocation-free snapshot of the running task, at the
/// concrete kernel monomorphisation.
///
/// Exists so the fault handlers in `boot/` can get one without naming the
/// stack-handle types. See `slopos_ostd::task::diag` for what it promises.
#[inline]
pub fn current_task_diag() -> Option<slopos_ostd::task::TaskDiag> {
    slopos_ostd::task::current_task_diag::<KernelStack, UnsafeStack>()
}

/// Offset of `fpu_state - context` within `Task`.
pub const FPU_STATE_OFFSET: usize = 0xC8;

// The real delta is `size_of::<TaskContext>()` (0xC8) plus whatever padding
// `fpu_state`'s 64-byte alignment needs, so one alignment cycle of slack still
// fires on a real field inserted between `context` and `fpu_state`.
const _: () = {
    let diff = offset_of!(Task, fpu_state) - offset_of!(Task, context);
    assert!(diff >= FPU_STATE_OFFSET);
    assert!(diff < FPU_STATE_OFFSET + 64);
};

// Bounded so the static array fits comfortably in `.bss` and heap callers
// budget a single-page allocation.
const _: () = assert!(core::mem::size_of::<Task>() <= 8192);

// The per-signal disposition table must have exactly one slot per signal:
// callers bound at `NSIG` and index with `signum - 1`. Measuring the field's
// real extent makes a resize without `NSIG` a build failure rather than an
// out-of-range index. `signal_actions` and `switch_ctx` are adjacent and both
// 8-aligned, so the delta is exact; if this fires after a field was inserted
// between them, the razor needs a new neighbour, not a new tolerance.
const _: () = {
    let span = offset_of!(Task, switch_ctx) - offset_of!(Task, signal_actions);
    assert!(span == NSIG * core::mem::size_of::<SignalActionCell>());
};

// `abi: TaskAbi` must be field #0 so OSTD's `TASK_UNSAFE_STACK_SP_OFFSET`,
// computed inside `TaskAbi`, matches the asm-readable offset of
// `unsafe_stack_sp` within `Task`.
const _: () = assert!(offset_of!(Task, abi) == 0);

// No `SwitchContext` razor here: its layout is pinned beside the canonical
// definition in OSTD, and the alias cannot drift from the type it names.

const _: () = {
    // XSAVE requires 64-byte alignment.
    assert!(core::mem::align_of::<FpuState>() >= 64);
    assert!(FPU_STATE_SIZE >= FXSAVE_AREA_SIZE);
    assert!(FPU_STATE_SIZE % core::mem::align_of::<FpuState>() == 0);
};

/// Panics at boot if the CPU's XSAVE area exceeds our compile-time maximum.
///
/// Call once from a boot step after `xsave::init()`, to fail early rather than
/// silently corrupting adjacent task memory.
pub fn validate_fpu_state_size() {
    let hw_size = slopos_arch::cpu::xsave::area_size();
    assert!(
        hw_size <= FPU_STATE_SIZE,
        "XSAVE area size ({} B) exceeds compile-time FPU_STATE_SIZE ({} B) — \
         increase FPU_STATE_SIZE in the OSTD task module",
        hw_size,
        FPU_STATE_SIZE,
    );
}
