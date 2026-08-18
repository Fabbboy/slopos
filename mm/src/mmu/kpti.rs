//! Kernel Page-Table Isolation (KPTI) — dual-PML4 scaffolding.
//!
//! Each `MmContext` would own a kernel PML4 (full kernel upper half) and a user
//! PML4 whose kernel half maps only what a ring-3 → ring-0 transition needs:
//! the trampoline code page, the per-CPU PCR page, the per-CPU trampoline stack
//! and the IDT. Both share the user-half subtree by pointer, so mapping a user
//! page stays one `map_page` call.
//!
//! Only the scaffolding ships. The ring-transition assembly still performs the
//! single CR3 load, so [`KPTI_ENABLED`] stays `false` and both PML4 references
//! resolve to the same backing tables.

use core::sync::atomic::{AtomicBool, Ordering};

/// Only flip to `true` once `mm::mmu::trampoline` is wired into the IDT and
/// SYSCALL MSR targets.
static KPTI_ENABLED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn kpti_enabled() -> bool {
    KPTI_ENABLED.load(Ordering::Relaxed)
}

/// Enable KPTI system-wide. Calling this prematurely triple-faults on the next
/// syscall: the trampoline code page must be mapped into every user PML4,
/// `__ostd_user_return` and every ring-3-reachable IRQ/exception vector must
/// perform the CR3 swap, and all IST stacks must be mapped in the user PML4.
pub fn enable() {
    KPTI_ENABLED.store(true, Ordering::Release);
}

/// Layout of the shared trampoline page. Every user PML4 references it through
/// one shared PDPT, so exactly one physical copy exists whatever the process
/// count.
pub struct TrampolineDescriptor {
    pub code_phys: slopos_abi::addr::PhysAddr,
    pub code_virt: slopos_abi::addr::VirtAddr,
}

impl TrampolineDescriptor {
    /// `None` until the trampoline assembly is built and linked into every
    /// user PML4.
    pub fn try_get() -> Option<Self> {
        None
    }
}

/// Stub: allocates no second PML4 and is currently unreferenced. It names the
/// shape a KPTI build fills in.
pub fn ensure_user_pml4() -> Result<(), ()> {
    if !kpti_enabled() {
        return Ok(());
    }
    Err(())
}

/// Hook for the restricted kernel upper half of a user PML4.
pub fn build_user_kernel_half_stub() {
    // Intentionally empty: a fresh address space copies PML4 indices 256..512
    // from the registered master (`VmSpace::new`).
}
