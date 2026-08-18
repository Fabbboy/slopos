//! Naked-asm ↔ kernel-`Task` layout contract.
//!
//! Every kernel-side `Task` field that OSTD's naked asm reads via a
//! compile-time `const` offset operand lives in [`TaskAbi`]. The kernel-side
//! `Task` embeds it as its first field and asserts the offset is zero; that
//! razor catches any reordering of `Task` that would silently desync the asm.
//! A new asm-readable field goes here with a matching `TASK_*_OFFSET` const,
//! never spread across crates.

/// Layout contract for naked-asm reads against the kernel `Task`.
///
/// Today the only consumer is `__safestack_pointer_address`, which reads
/// `unsafe_stack_sp`.
#[repr(C)]
pub struct TaskAbi {
    /// SafeStack `unsafe_sp` slot: every instrumented prologue calls
    /// `__safestack_pointer_address`, which returns a pointer to it. Living
    /// inside the `Task` keeps that pointer valid across CPU migration.
    pub unsafe_stack_sp: u64,
}

/// Offset of `Task.abi.unsafe_stack_sp` from the `Task` base, named by the
/// naked asm as a `const` operand. Zero by construction: `slopos-core` asserts
/// `offset_of!(Task, abi) == 0`.
pub const TASK_UNSAFE_STACK_SP_OFFSET: usize = core::mem::offset_of!(TaskAbi, unsafe_stack_sp);

// The asm operand collapses to a literal zero, and the asm reads and writes
// exactly 8 naturally-aligned bytes through the slot.
const _: () = assert!(TASK_UNSAFE_STACK_SP_OFFSET == 0);
const _: () = assert!(core::mem::size_of::<TaskAbi>() == 8);
const _: () = assert!(core::mem::align_of::<TaskAbi>() == 8);
