//! Naked-asm ↔ kernel-`Task` layout contract.
//!
//! Every kernel-side `Task` field that OSTD's naked asm reads via a
//! compile-time `const` offset operand lives in [`TaskAbi`]. The
//! kernel-side `Task` struct embeds a `TaskAbi` as its first field and
//! asserts the offset is zero; that single razor catches any
//! reordering of `Task` that would silently desync the asm.
//!
//! OSTD owns both the asm and the layout. Adding a new asm-readable
//! field means adding it to [`TaskAbi`] (and exporting a matching
//! `TASK_*_OFFSET` const) — never spreading the contract across
//! crates.
//!
//! Inspired by Fuchsia's `zircon/system/public/zircon/tls.h`, which
//! pins per-thread SafeStack offsets in a header owned by the
//! toolchain ABI rather than by the kernel or libc. SlopOS reifies
//! the same idea as a named Rust type so the layout drift detector
//! is a `const _: () = assert!(...)` rather than a `static_assert`
//! in a C header.

/// Layout contract for naked-asm reads against the kernel `Task`.
///
/// Every field is consumed by an OSTD-owned naked function via a
/// `const` offset operand. The kernel-side `Task` struct embeds this
/// as its first field; that placement is enforced by an `offset_of!`
/// razor inside the kernel.
///
/// Today the only consumer is `__safestack_pointer_address`, which
/// reads `unsafe_stack_sp`. Future asm-readable fields (e.g. a per-
/// task TLS slot, a fast-path syscall counter) accumulate here.
#[repr(C)]
pub struct TaskAbi {
    /// SafeStack `unsafe_sp` slot. The LLVM SafeStack pass calls
    /// `__safestack_pointer_address` on every instrumented function
    /// prologue, which returns `&current_task->abi.unsafe_stack_sp`.
    /// Heap-stable inside the running `Task`, so the cached pointer
    /// survives CPU migration.
    pub unsafe_stack_sp: u64,
}

/// Offset of `Task.abi.unsafe_stack_sp` from the `Task` base.
///
/// Equals zero by construction:
/// - `TaskAbi` contains a single field at its offset 0.
/// - The kernel-side `Task` places `abi: TaskAbi` at its offset 0
///   (enforced by `const _: () = assert!(offset_of!(Task, abi) == 0);`
///   inside `slopos-core`).
///
/// Exported as a `pub const` so OSTD's naked asm can name it via
/// `const TASK_UNSAFE_STACK_SP_OFFSET` operand. Any future drift
/// fails at the kernel's offset razor before the asm can corrupt
/// memory.
pub const TASK_UNSAFE_STACK_SP_OFFSET: usize = core::mem::offset_of!(TaskAbi, unsafe_stack_sp);
