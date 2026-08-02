//! Lazy Unmap Flush (LUF) — the local half of tearing down a user mapping.
//!
//! Unmapping splits into two obligations. The virtual address must stop
//! resolving on the unmapping CPU: local, one `invlpg`, done here. The frame
//! must not reach a new owner while a *peer* still caches it: global, owned by
//! [`super::quiesce`], which holds the frame back rather than interrupting
//! anyone. The laziness is in the second — no shootdown IPI is sent.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;

/// Tear down the calling CPU's cached translation for `vaddr`, and record that
/// peers have *not* been told.
///
/// The local invalidation is unconditional: a `munmap` followed by a
/// dereference on the same CPU must fault, not hit a cached entry. `INVLPG`
/// targets whatever PCID is currently loaded; if this CPU is running some other
/// address space the instruction is simply a no-op on a user address, and the
/// quiesce epoch covers that CPU when it next acks.
pub fn queue_unmap(vaddr: VirtAddr) {
    slopos_arch::cpu::tlb::invlpg(vaddr.as_u64());
    super::quiesce::note_deferred_unmap();
}

// =============================================================================
// Per-CPU active mm-context-handle tracker
// =============================================================================

/// Per-CPU storage for the `mm_ctx_handle` of the address space currently
/// installed in CR3. Written by the OSTD `CursorUnmapHook::on_activate`
/// callback at every context switch (via [`current_cpu_set_active_mm_ctx`]).
///
/// Uses `0` as the "no context bound" sentinel — matches
/// `MmContextId::INVALID.raw()` and the unset value of
/// `VmSpace::mm_ctx_handle`. Each CPU writes its own slot; cross-CPU reads only
/// happen for diagnostics.
static ACTIVE_MM_CTX_HANDLE: [AtomicU64; MAX_CPUS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_CPUS]
};

/// Record that the current CPU has just installed an address space whose
/// opaque handle is `handle`. Called from `LufHook::on_activate` immediately
/// before `VmSpace::activate` writes CR3.
#[inline]
pub fn current_cpu_set_active_mm_ctx(handle: u64) {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].store(handle, Ordering::Release);
    }
}

/// Read the address space handle this CPU last activated, or `0` if no
/// VmSpace has been activated on this CPU yet.
#[inline]
pub fn current_cpu_active_mm_ctx() -> u64 {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}
