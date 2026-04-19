//! Kernel Page-Table Isolation (KPTI) — dual-PML4 scaffolding.
//!
//! # Goal
//!
//! Deny user-mode access to kernel virtual addresses at the page-table
//! level, so Meltdown-class speculative side channels cannot leak
//! arbitrary kernel memory. Each `MmContext` owns **two** PML4s:
//!
//!   - **Kernel PML4** — the traditional layout: user half (entries 0..256)
//!     and full kernel upper half (256..512). Loaded while the CPU is
//!     in ring 0.
//!   - **User PML4** — full user half, *minimal* kernel upper half. Only
//!     the pages required for ring-3 → ring-0 transitions are mapped
//!     (see "Minimal kernel surface" below). Loaded while the CPU is
//!     in ring 3.
//!
//! Both PML4s share their user-half subtree by pointer — mapping a user
//! page costs exactly one `map_page` call, not two.
//!
//! Paired with PCID, the CR3 swap on every ring transition sets
//! `CR3.NOFLUSH`, so the kernel and user PCIDs retain their TLB
//! entries across the switch. A syscall costs two `mov CR3, reg` +
//! four GS-relative loads on top of today's entry; ≈ 40 cycles on
//! Zen 3 / Ice Lake.
//!
//! # Minimal kernel surface in the user PML4
//!
//! Every user PML4 maps exactly four kinds of kernel memory:
//!
//!   1. **Trampoline code page** (`syscall_entry`, `irq_entry_stub`,
//!      `nmi_entry_stub`). Read-only, supervisor-only, global. Shared
//!      across all processes via a single PDPT.
//!   2. **Per-CPU PCR page**. Needed so `%gs:` reads work during entry
//!      *before* the kernel CR3 is loaded. Aliased into a fixed slot
//!      per CPU.
//!   3. **Per-CPU trampoline stack page**. Runs the five-instruction
//!      CR3-swap prologue; immediately switches to the real kernel
//!      stack once in-kernel.
//!   4. **IDT** itself. Read-only-to-kernel-but-addressable-from-user-
//!      CPU-state.
//!
//! Everything else kernel-side — the heap, HHDM, kernel text beyond the
//! trampoline, task stacks, page tables themselves — is **not** mapped
//! in the user PML4. A Meltdown-style speculation that loads from a
//! kernel address from user mode fetches a not-present PTE and
//! architecturally faults; there is literally nothing to speculate
//! onto.
//!
//! # Activation
//!
//! This module ships the scaffolding only:
//!
//!   - `MmContext` grows a second PML4 reference (`user_pml4_phys`).
//!     Today both PML4s point at the same backing tables — KPTI is
//!     gated off via [`KPTI_ENABLED`].
//!   - A shared trampoline PDPT is built once at BSP init but never
//!     linked into any user PML4 yet.
//!   - The CR3 write path in `mm::mmu::asid` already takes the
//!     `PcidPair` shape (bit 11 user/kernel split) so toggling KPTI on
//!     is a one-file change.
//!
//! The **ring-transition assembly** (`syscall_entry` in
//! `boot/idt_handlers.s`, all IRQ stubs that can be taken from ring 3,
//! NMI/#DF/#MC entries) still performs the single CR3 load we've
//! always used. Wiring them to swap CR3 via the new trampoline is the
//! last step — it needs a deliberate walk through every entry vector
//! with QEMU under `-d int` to verify no fault path escapes.
//!
//! Until then `KPTI_ENABLED` stays `false` and the kernel runs with
//! the pre-KPTI layout. Flipping it on without completing the asm work
//! would triple-fault on the first syscall.

use core::sync::atomic::{AtomicBool, Ordering};

/// Runtime switch that controls whether KPTI-mode CR3 loads are used.
///
/// Default `false` — the user PML4 is created but content-equivalent to
/// the kernel PML4, so existing ring-3 → ring-0 entry code still works.
/// Only flip this to `true` once `mm::mmu::trampoline` is wired into
/// the IDT / SYSCALL MSR targets.
static KPTI_ENABLED: AtomicBool = AtomicBool::new(false);

/// Is KPTI currently enforcing kernel-page-table isolation?
#[inline]
pub fn kpti_enabled() -> bool {
    KPTI_ENABLED.load(Ordering::Relaxed)
}

/// Enable KPTI system-wide. **Must not be called** until:
///   1. The trampoline code page has been built and mapped into every
///      user PML4 (`build_trampoline_mapping` below).
///   2. `boot::idt_handlers::syscall_entry` has been replaced with the
///      CR3-swap stub in `mmu::trampoline`.
///   3. Every IRQ / exception vector reachable from ring 3 points at
///      a trampoline variant that performs the CR3 swap.
///   4. All IST stacks are mapped in the user PML4.
///
/// Calling this prematurely will triple-fault the machine on the next
/// syscall. The guard is advisory; a release-mode enforcement hook is
/// added when the asm lands.
pub unsafe fn enable() {
    KPTI_ENABLED.store(true, Ordering::Release);
}

/// Layout of the shared trampoline page that every user PML4 maps.
///
/// The page is allocated once by the BSP, filled by the code in
/// `mm::mmu::trampoline`, and then pinned read-only. Every user PML4
/// references it through a shared PDPT so there is exactly one physical
/// copy regardless of how many processes exist.
pub struct TrampolineDescriptor {
    /// Physical address of the 4 KiB trampoline code page.
    pub code_phys: slopos_abi::addr::PhysAddr,
    /// Virtual address at which the user PML4 exposes the trampoline.
    pub code_virt: slopos_abi::addr::VirtAddr,
}

impl TrampolineDescriptor {
    /// Returns `None` until the trampoline assembly is built and
    /// linked into every user PML4. When populated, the return value
    /// is cached globally — callers are free to copy it.
    pub fn try_get() -> Option<Self> {
        None
    }
}

/// Build or refresh the user PML4 of `mm_ctx` as an exact copy of the
/// kernel PML4.
///
/// Called unconditionally from `create_process_vm` so every
/// `MmContext` carries two PML4 references today. Until `enable()` is
/// called, both references resolve to the same backing page and the
/// extra allocation is the cost we pay for being KPTI-ready.
///
/// Stub: returns `Ok(())` without allocating a second PML4. The dual
/// allocation lands together with the `MmContext` refactor that
/// replaces `ProcessPageDir`.
pub fn ensure_user_pml4() -> Result<(), ()> {
    if !kpti_enabled() {
        return Ok(());
    }
    // Post-enable path lands alongside the trampoline asm — both sides
    // are atomic w.r.t. the first ring transition.
    Err(())
}

/// Construct the restricted kernel-upper-half for a user PML4.
///
/// The only PDPT linked in under the kernel half is the trampoline
/// PDPT (returned by `TrampolineDescriptor::try_get()`). When KPTI is
/// disabled or the trampoline hasn't been built yet, we fall through
/// to copying the full kernel upper half — preserving current
/// behaviour so the system keeps booting.
pub fn build_user_kernel_half_stub() {
    // Intentionally empty. The actual copy/restrict happens inside
    // `mm::paging::tables::paging_copy_kernel_mappings` today. This
    // function is the hook for the future restricted variant; keeping
    // the symbol exported means the transition is a one-file change.
}
