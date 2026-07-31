//! Kernel-side alias for the OSTD-owned generic `TaskInner<K, U>`.
//!
//! The struct body relocated to `slopos_ostd::task::kernel_task`; this
//! module keeps the type alias + razor assertions so the existing
//! `Task` spelling continues to resolve at every existing call site and
//! the field-offset contract stays pinned at the kernel boundary.

use core::mem::offset_of;

use slopos_abi::signal::NSIG;
use slopos_ostd::task::kernel_task::SignalActionCell;

use crate::task_stack::{KernelStack, UnsafeStack};

pub use slopos_abi::task::{
    BlockReason, INVALID_PROCESS_ID, INVALID_TASK_ID, MAX_TASKS, TASK_FLAG_COMPOSITOR,
    TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_FPU_INITIALIZED, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE,
    TASK_NAME_MAX_LEN, TASK_STACK_SIZE, TASK_UNSAFE_STACK_SIZE, TaskExitReason, TaskExitRecord,
    TaskFaultReason, TaskPriority, TaskStatus,
};
pub use slopos_ostd::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;
pub use slopos_ostd::task::fpu::{FPU_STATE_SIZE, FXSAVE_AREA_SIZE, FpuState, MXCSR_DEFAULT};
pub use slopos_ostd::task::kernel_task::{
    SignalAction, SwitchContext, TaskContext, TaskInner, fpu_reset_in_place,
};

// The two stack-handle types this kernel builds every task from, declared so
// `CurrentTask` and the PCR publisher agree on the monomorphisation the
// current-task slot holds. `KernelStack` and `UnsafeStack` are aliases, so the
// impl heads are the local `TaskStack<_>` — which is what makes them legal to
// write here at all.
slopos_ostd::declare_pcr_stack_type!(KernelStack);
slopos_ostd::declare_pcr_stack_type!(UnsafeStack);

/// Concrete kernel `Task` type alias. Every existing call site continues
/// to spell the type as `Task`; the struct body lives in OSTD.
pub type Task = TaskInner<KernelStack, UnsafeStack>;

/// Borrow of the task running on this CPU, at the concrete kernel
/// monomorphisation. Spelled `Current::get()` rather than
/// `CurrentTask::<KernelStack, UnsafeStack>::get()` at every call site.
pub type Current = slopos_ostd::task::CurrentTask<KernelStack, UnsafeStack>;

/// Borrow of this CPU's idle task, at the concrete kernel monomorphisation.
/// Spelled `Idle::current()` at every call site.
pub type Idle = slopos_ostd::task::IdleTask<KernelStack, UnsafeStack>;

/// Exclusive access to one endpoint of a context switch, at the concrete
/// kernel monomorphisation. Minted only by `slopos_ostd::task::run_switch`.
pub type Switching<'a> = slopos_ostd::task::SwitchWindow<'a, KernelStack, UnsafeStack>;

/// Racy, lock-free, allocation-free snapshot of the running task, at the
/// concrete kernel monomorphisation.
///
/// The fault handlers in `boot/` are the callers, and they must not name the
/// stack-handle types to get one. Takes no lock, mints no handle, forms no
/// reference — see `slopos_ostd::task::diag` for what it does and does not
/// promise.
#[inline]
pub fn current_task_diag() -> Option<slopos_ostd::task::TaskDiag> {
    slopos_ostd::task::current_task_diag::<KernelStack, UnsafeStack>()
}

// =============================================================================
// Razor blocks against the concrete monomorphisation
// =============================================================================

/// Offset of `fpu_state - context` within `Task`.
pub const FPU_STATE_OFFSET: usize = 0xC8;

// The actual `fpu_state - context` delta is `size_of::<TaskContext>()`
// (200 bytes / 0xC8) plus whatever padding the compiler inserts ahead
// of `fpu_state` to satisfy its 64-byte alignment. Allow up to one
// 64-byte alignment cycle so the tripwire still fires if someone
// inserts a real field between `context` and `fpu_state`.
const _: () = {
    let diff = offset_of!(Task, fpu_state) - offset_of!(Task, context);
    assert!(diff >= FPU_STATE_OFFSET);
    assert!(diff < FPU_STATE_OFFSET + 64);
};

// Tripwire: the Task struct is inherently large (dominated by FpuState).
// Keep it bounded to a single page so its static array fits comfortably
// in `.bss` and heap callers budget a single-page allocation.
const _: () = assert!(core::mem::size_of::<Task>() <= 8192);

// Razor: the per-signal disposition table has exactly one slot per signal.
//
// `Signum` (core/src/syscall/args.rs) and `parse_signum`
// (core/src/syscall/signal.rs) both bound signal numbers at `NSIG` and hand
// `signum - 1` to the table. This measures the field's real extent rather
// than restating its declared length, so resizing the table without moving
// `NSIG` with it is a build failure instead of an out-of-range index.
//
// `signal_actions` and `switch_ctx` are adjacent and both 8-aligned, so the
// delta is exact. If this fires after a field was inserted between them, the
// span is no longer the table and the razor needs a new neighbour — not a new
// tolerance.
const _: () = {
    let span = offset_of!(Task, switch_ctx) - offset_of!(Task, signal_actions);
    assert!(span == NSIG * core::mem::size_of::<SignalActionCell>());
};

// ABI razor: `abi: TaskAbi` must be field #0 of Task so the
// OSTD-side `TASK_UNSAFE_STACK_SP_OFFSET` const (computed as
// `offset_of!(TaskAbi, unsafe_stack_sp)` inside OSTD, naturally 0)
// matches the asm-readable offset of the `unsafe_stack_sp` field
// inside Task.
const _: () = assert!(offset_of!(Task, abi) == 0);

// SwitchContext layout (size and every field offset) is pinned by
// `const _` asserts beside the canonical definition in
// `slopos-ostd/src/task/task.rs`; the switch asm derives its offsets
// via `offset_of!` at the use site. No duplicate razors here — the
// alias cannot drift from the type it names.

// =============================================================================
// FpuState compile-time checks
// =============================================================================

const _: () = {
    // XSAVE requires 64-byte alignment.
    assert!(core::mem::align_of::<FpuState>() >= 64);
    // Buffer must be large enough for the FXSAVE legacy area.
    assert!(FPU_STATE_SIZE >= FXSAVE_AREA_SIZE);
    // Buffer size should be a multiple of the alignment for clean packing.
    assert!(FPU_STATE_SIZE % core::mem::align_of::<FpuState>() == 0);
};

/// Panics at boot if the CPU's XSAVE area exceeds our compile-time maximum.
///
/// Call once from a boot step (after `xsave::init()`) to fail early rather
/// than silently corrupting adjacent task memory.
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
