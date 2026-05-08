//! Naked-asm trampolines required by Rust language semantics.
//!
//! Two trampolines live here:
//!
//! - [`__safestack_pointer_address`] — LLVM SafeStack runtime callback.
//!   Every Rust function compiled with `-Zsanitizer=safestack
//!   -C llvm-args=-safestack-use-pointer-address` emits a prologue call
//!   to this symbol to fetch `&current_task->unsafe_stack_sp`.  Naked
//!   to avoid self-recursion (a non-naked fn would itself emit a
//!   prologue that calls back into this address).
//!
//! - [`ap_entry`] — AP boot trampoline.  Limine jumps each AP directly
//!   here with `rdi = &MpInfo`; the trampoline installs `IA32_GS_BASE`
//!   from `AP_PCR_PTRS[slot - 1]` *before* any instrumented Rust runs
//!   on the AP, then tail-jumps into a kernel-supplied
//!   `extern "C" fn(*const ()) -> !` registered through
//!   [`install_ap_trampoline`].  Naked because the first instruction of
//!   any non-naked safestack-instrumented fn would fetch `gs:[0]` —
//!   which is still zero on the AP at this point.
//!
//! The `unsafe extern "C"` and `#[unsafe(naked)]` keywords live interior
//! to OSTD; consumers reach the trampolines exclusively through the
//! safe wrappers ([`install_safestack_runtime`], [`install_ap_trampoline`]).
//!
//! # Layout contract for `__safestack_pointer_address`
//!
//! The naked asm reads the SP-slot offset at runtime from a kernel-side
//! linker-visible static `BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64`.  The
//! kernel's `Task` struct definition computes the offset via
//! `offset_of!(Task, unsafe_stack_sp)` and stamps it into that static
//! at compile time.  Sourcing the offset at runtime through a single
//! linker symbol (rather than baking it as a `const` operand here)
//! avoids forcing OSTD to know the kernel-side `Task` layout — the
//! same scheme `boot/limine_entry.s` already uses to prime the BSP
//! bootstrap Task's slot before Rust code begins.

use core::arch::naked_asm;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::cpu::x86_64::pcr;

unsafe extern "C" {
    static BOOTSTRAP_TASK_UNSAFE_SP_OFFSET: u64;
}

/// LLVM SafeStack runtime callback.  Returns `&current_task->unsafe_stack_sp`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "sysv64" fn __safestack_pointer_address() -> *mut *mut u8 {
    naked_asm!(
        "mov rax, gs:[{off_current_task}]",
        "add rax, [rip + {sp_offset}]",
        "ret",
        off_current_task = const pcr::offsets::CURRENT_TASK,
        sp_offset = sym BOOTSTRAP_TASK_UNSAFE_SP_OFFSET,
    )
}

/// Pull [`__safestack_pointer_address`] into the link from the kernel's
/// BSP-init path.  LLVM emits direct calls to the symbol from every
/// instrumented prologue, but LTO would discard the body if no Rust
/// call site ever names it; this no-op reference keeps the symbol
/// resident.
pub fn install_safestack_runtime() {
    let _: extern "sysv64" fn() -> *mut *mut u8 = __safestack_pointer_address;
}

// ---------------------------------------------------------------------------
// AP entry trampoline
// ---------------------------------------------------------------------------

/// Offset of `MpInfo.extra_argument` in limine 0.6.x.
/// `processor_id: u32 + lapic_id: u32 + _resvd0: u64 + goto_addr: AtomicPtr<()>`
/// = 24 bytes.  The field is `pub(crate)` upstream so a `const { offset_of!(...) }`
/// probe cannot see it; the ABI is pinned by `Cargo.lock`.  A mismatched
/// limine bump would manifest as an immediate triple-fault on AP bringup.
const MP_INFO_EXTRA_OFFSET: usize = 24;
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Kernel-supplied AP entry function pointer.  Stamped by
/// [`install_ap_trampoline`] before any AP is started.  The naked
/// trampoline tail-jumps through this slot after installing GS_BASE.
///
/// `AtomicPtr<()>` is `repr(transparent)` over a single `*mut ()`, so
/// `mov rax, [rip + AP_ENTRY_RUST]` loads the 8-byte pointer value
/// directly — no atomic semantics needed in the asm because the
/// publication happens once on the BSP before any AP can observe it.
#[unsafe(no_mangle)]
static AP_ENTRY_RUST: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Payload for [`install_ap_trampoline`].
pub struct ApPayload {
    /// Kernel AP entry function.  Receives a raw pointer that is in
    /// reality `&limine::mp::MpInfo`; the consumer re-borrows on the
    /// kernel side to keep the limine dependency out of OSTD.
    pub entry_rust: unsafe extern "C" fn(*const ()) -> !,
}

/// Stamp the kernel-supplied AP entry function pointer into the
/// trampoline's tail-call slot.  Must run on the BSP before any AP is
/// bootstrapped.
pub fn install_ap_trampoline(payload: ApPayload) {
    AP_ENTRY_RUST.store(payload.entry_rust as *mut (), Ordering::Release);
}

/// AP boot trampoline.  Pass to `limine::mp::Cpu::bootstrap` as the
/// AP entry symbol.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_entry(_cpu_info: *const ()) -> ! {
    naked_asm!(
        "mov rax, [rdi + {extra_offset}]",
        "dec rax",
        "lea rcx, [rip + {ap_pcr_ptrs}]",
        "mov rax, [rcx + rax*8]",
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, {ia32_gs_base}",
        "wrmsr",
        "mov rax, [rip + {ap_entry_rust}]",
        "jmp rax",
        extra_offset = const MP_INFO_EXTRA_OFFSET,
        ia32_gs_base = const IA32_GS_BASE,
        ap_pcr_ptrs = sym pcr::AP_PCR_PTRS,
        ap_entry_rust = sym AP_ENTRY_RUST,
    )
}
