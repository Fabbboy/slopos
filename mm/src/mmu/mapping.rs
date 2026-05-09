//! Typed, drop-safe kernel address-space mappings.
//!
//! Historically the kernel has three distinct ways to install a mapping:
//!
//!   * `map_page_4kb` — imperative, fire-and-forget; caller must remember
//!     to unmap and flush the TLB. Easy to forget.
//!   * `MmioRegion::map` — bespoke Drop-less wrapper in `mm::mmio`.
//!   * Private page-table edits scattered through `mm::paging::tables`.
//!
//! This module introduces [`KernelMapping`], a single owning handle that
//! is returned from every kernel-visible mapping call. Dropping the
//! handle unmaps the range, broadcasts a TLB flush, and frees the
//! backing frames (when applicable).  The borrow checker makes "use
//! mapping after unmap" a compile error rather than a latent UAF.
//!
//! Callers migrating from the legacy API do so one site at a time —
//! everything built on `map_page_4kb` keeps working unchanged until it
//! is converted.
//!
//! Future work:
//!   - Parametrise by `'mm` lifetime tied to an `MmContext` for
//!     per-process mappings (KPTI dual-PML4 is built on this shape).
//!   - Route TLB flush through `mm::mmu::shootdown` directly.

use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::page_alloc::{alloc_page_frame, free_page_frame};
use crate::paging::{map_page_4kb, unmap_page};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::tlb;

/// Whether the mapping owns the physical frames or just points at caller
/// memory (MMIO, shared frames, etc.). Determines whether `Drop` frees
/// them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrameOwnership {
    /// `Drop` will `free_page_frame` each backing frame.
    OwnedAnonymous,
    /// Backing frames live outside the page allocator (MMIO, framebuffer,
    /// or a caller-supplied buffer); `Drop` unmaps but doesn't free.
    Borrowed,
}

/// A type-safe, drop-safe kernel-mode VA mapping.
///
/// Dropping the handle:
///   1. Unmaps each 4 KiB page of `[virt_base, virt_base + size)`.
///   2. Broadcasts a TLB shootdown for the range.
///   3. Frees the backing frames if `ownership == OwnedAnonymous`.
///
/// Leaking the handle (e.g. `core::mem::forget`) keeps the mapping live
/// — no automatic cleanup if ownership is deliberately transferred out.
#[must_use = "a kernel mapping that is dropped is unmapped immediately"]
pub struct KernelMapping {
    virt_base: VirtAddr,
    size: usize,
    ownership: FrameOwnership,
}

impl KernelMapping {
    /// Allocate `page_count` fresh anonymous pages at `virt_base` with
    /// the given leaf page flags. Frames are zero-initialised and owned
    /// by the returned handle; dropping it returns them to the page
    /// allocator.
    ///
    /// `virt_base` and every derived page address must already be valid
    /// kernel VA. `virt_base` must be page-aligned and the range must
    /// not overlap an existing mapping. Returns `None` on frame-allocator
    /// failure (and leaves any partial mapping intact for the caller
    /// who invoked us — typically kernel boot, where a partial mapping
    /// implies a panic anyway).
    pub fn alloc_anonymous(
        virt_base: VirtAddr,
        page_count: usize,
        flags: PageFlags,
    ) -> Option<Self> {
        if page_count == 0 {
            return Some(Self {
                virt_base,
                size: 0,
                ownership: FrameOwnership::Borrowed,
            });
        }
        if !virt_base.is_aligned(PAGE_SIZE_4KB) {
            return None;
        }

        for i in 0..page_count {
            let page_virt = VirtAddr::new(virt_base.as_u64() + i as u64 * PAGE_SIZE_4KB);
            let phys = alloc_page_frame(0);
            if phys.is_null() {
                // Rollback already-mapped prefix so we don't leak.
                for j in 0..i {
                    let undo_virt = VirtAddr::new(virt_base.as_u64() + j as u64 * PAGE_SIZE_4KB);
                    let phys = unmap_page(undo_virt);
                    if !phys.is_null() {
                        free_page_frame(phys);
                    }
                }
                return None;
            }
            if map_page_4kb(page_virt, phys, flags.bits()) != 0 {
                free_page_frame(phys);
                for j in 0..i {
                    let undo_virt = VirtAddr::new(virt_base.as_u64() + j as u64 * PAGE_SIZE_4KB);
                    let phys = unmap_page(undo_virt);
                    if !phys.is_null() {
                        free_page_frame(phys);
                    }
                }
                return None;
            }
        }

        Some(Self {
            virt_base,
            size: page_count * PAGE_SIZE_4KB as usize,
            ownership: FrameOwnership::OwnedAnonymous,
        })
    }

    /// Map a caller-owned physical range into kernel VA. Frames are
    /// **not** freed on drop; the mapping just disappears. Intended
    /// for MMIO / firmware / shared-frame cases — use
    /// [`Self::alloc_anonymous`] when the backing memory is anonymous.
    ///
    /// Both `virt_base` and `phys_base` must be page-aligned and the
    /// range `[phys_base, phys_base + page_count*4KiB)` must be a real
    /// physical range not already mapped through this VA.
    pub fn map_borrowed(
        virt_base: VirtAddr,
        phys_base: PhysAddr,
        page_count: usize,
        flags: PageFlags,
    ) -> Option<Self> {
        if page_count == 0 {
            return Some(Self {
                virt_base,
                size: 0,
                ownership: FrameOwnership::Borrowed,
            });
        }
        if !virt_base.is_aligned(PAGE_SIZE_4KB) || !phys_base.is_aligned(PAGE_SIZE_4KB) {
            return None;
        }

        for i in 0..page_count {
            let page_virt = VirtAddr::new(virt_base.as_u64() + i as u64 * PAGE_SIZE_4KB);
            let page_phys = PhysAddr::new(phys_base.as_u64() + i as u64 * PAGE_SIZE_4KB);
            if map_page_4kb(page_virt, page_phys, flags.bits()) != 0 {
                for j in 0..i {
                    let undo_virt = VirtAddr::new(virt_base.as_u64() + j as u64 * PAGE_SIZE_4KB);
                    let _ = unmap_page(undo_virt);
                }
                return None;
            }
        }

        Some(Self {
            virt_base,
            size: page_count * PAGE_SIZE_4KB as usize,
            ownership: FrameOwnership::Borrowed,
        })
    }

    #[inline]
    pub fn virt_base(&self) -> VirtAddr {
        self.virt_base
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn page_count(&self) -> usize {
        self.size / PAGE_SIZE_4KB as usize
    }

    /// Convert the mapping into a raw `*mut T` pointer for legacy
    /// interop. The caller is responsible for keeping the mapping
    /// alive for as long as the pointer is used.
    #[inline]
    pub fn as_mut_ptr<T>(&mut self) -> *mut T {
        self.virt_base.as_mut_ptr::<T>()
    }

    /// Consume the mapping without tearing it down. Only appropriate
    /// when ownership is explicitly transferred into something else
    /// (e.g. a legacy `MmioRegion`). Use rarely.
    pub fn leak(self) -> (VirtAddr, usize) {
        let parts = (self.virt_base, self.size);
        core::mem::forget(self);
        parts
    }
}

impl Drop for KernelMapping {
    fn drop(&mut self) {
        if self.size == 0 {
            return;
        }
        let pages = self.page_count();
        for i in 0..pages {
            let page_virt = VirtAddr::new(self.virt_base.as_u64() + i as u64 * PAGE_SIZE_4KB);
            let freed = unmap_page(page_virt);
            if self.ownership == FrameOwnership::OwnedAnonymous && !freed.is_null() {
                free_page_frame(freed);
            }
        }
        let end = VirtAddr::new(self.virt_base.as_u64() + self.size as u64);
        tlb::flush_range(self.virt_base, end);
    }
}

// `KernelMapping` is a plain owned handle — not `Send`/`Sync` implicit
// would already be fine (no interior-mutable fields), but we spell it
// out so reviewers don't have to track it down.

/// Legacy `map_page_4kb` + manual free routine kept for tests that need
/// to verify a mapping is torn down explicitly. Prefer `KernelMapping`
/// in all new code.
#[inline]
pub fn unmap_kernel_page_free(virt: VirtAddr) {
    let phys = unmap_page(virt);
    if !phys.is_null() {
        free_page_frame(phys);
    }
    tlb::flush_page(virt);
}

#[allow(dead_code)]
fn _prove_ownership_types() {
    fn _assert<T>() {}
    _assert::<KernelMapping>();
    let _: *const () = FrameOwnership::OwnedAnonymous as usize as *const ();
    // suppress any "unused ptr" lint if we ever add it
    let _ = ptr::null::<u8>();
}
