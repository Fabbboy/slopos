//! Kernel-half address translation.
//!
//! Read-only. Every kernel-half page-table *write* goes through
//! `slopos_mm::kernel_mappings`, which drives OSTD's `CursorMut` under
//! the `KERNEL_VM_SPACE` lock; this module answers the question the
//! hardware page walker answers, over the same physical PML4, and
//! answers it the same way — one relaxed atomic load per level, no
//! lock, no allocation, no reference into a page-table frame.
//!
//! That is why translation does not route through
//! `kernel_mappings::kernel_virt_to_phys`: taking an IRQ-disabling
//! spinlock behind what is conceptually an address translation would
//! make every `virt_to_phys` a synchronisation event, and reads never
//! had a synchronisation problem to begin with.

use core::ffi::c_int;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::klog_debug;

use super::walker::walk_phys;
use crate::hhdm::{self, PhysAddrHhdm};
use crate::memory_layout_defs::KERNEL_VIRTUAL_BASE;

/// The PML4 frame CR3 holds — the root of the kernel-half tree the
/// walks in this module descend. `0` until [`init_paging`] records
/// it. Written once on the BSP before any AP exists; the `Release`
/// store pairs with the `Acquire` load in [`kernel_pml4_phys`] so the
/// frame is visible before any descent rooted on it.
static KERNEL_PML4_PHYS: AtomicU64 = AtomicU64::new(0);

/// The PML4 frame backing the kernel-half tree.
/// `PhysAddr::NULL` before [`init_paging`] records it.
#[inline]
pub fn kernel_pml4_phys() -> PhysAddr {
    PhysAddr::new(KERNEL_PML4_PHYS.load(Ordering::Acquire))
}

#[inline(always)]
fn get_cr3() -> PhysAddr {
    crate::mmu::read_cr3_value().pml4_phys()
}

/// Walk the kernel half (PML4 indices 256..512) via the OSTD cursor
/// and stamp `GLOBAL` onto every present leaf — handles 4 KiB / 2 MiB /
/// 1 GiB leaves uniformly via the cursor's `protect::<S>` API.
/// Idempotent. Used by the early boot init step that wraps the live
/// kernel-master PML4.
pub fn paging_mark_kernel_global() {
    crate::kernel_mappings::mark_kernel_global();
}

pub fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
    let pml4_phys = kernel_pml4_phys();
    if pml4_phys.is_null() {
        return PhysAddr::NULL;
    }
    match walk_phys(pml4_phys, vaddr) {
        Ok(result) => result.phys_addr,
        Err(_) => PhysAddr::NULL,
    }
}

pub fn init_paging() {
    let cr3 = get_cr3();
    // `to_virt` panics on its own if the HHDM is not up, so what is left
    // to catch here is a null CR3 — a root the descent cannot start from.
    if cr3.to_virt().is_null() {
        panic!("Failed to translate kernel PML4 physical address");
    }
    KERNEL_PML4_PHYS.store(cr3.as_u64(), Ordering::Release);

    let kernel_phys = virt_to_phys(VirtAddr::new(KERNEL_VIRTUAL_BASE));
    if kernel_phys.is_null() {
        panic!("Higher-half kernel mapping not found");
    }

    klog_debug!(
        "Higher-half kernel mapping verified at 0x{:x}",
        kernel_phys.as_u64()
    );

    let identity_phys = virt_to_phys(VirtAddr::new(0x100000));
    if identity_phys == PhysAddr::new(0x100000) || hhdm::is_available() {
        klog_debug!("Identity mapping verified");
    } else {
        klog_debug!("Identity mapping not found (may be normal after early boot)");
    }

    klog_debug!("Paging system initialized successfully");
}

pub fn is_mapped(vaddr: VirtAddr) -> c_int {
    (!virt_to_phys(vaddr).is_null()) as c_int
}

pub fn get_page_size(vaddr: VirtAddr) -> u64 {
    let pml4_phys = kernel_pml4_phys();
    if pml4_phys.is_null() {
        return 0;
    }
    match walk_phys(pml4_phys, vaddr) {
        Ok(result) => result.page_size,
        Err(_) => 0,
    }
}
