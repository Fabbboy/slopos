//! Slab- and large-alloc-page primitives.
//!
//! Each slab page and each large-alloc region carries a small header
//! at offset 0 (HHDM-addressed). The header's leading `u32` is a magic
//! constant that identifies which tier owns the page — `SLAB_MAGIC` for
//! a slab page, `LARGE_MAGIC` for an active large alloc, `LARGE_FREE_MAGIC`
//! for a free large region awaiting reuse. The `kfree` path peeks at
//! that magic to discriminate without consulting a side table.
//!
//! Slab and large-alloc pages are owned by raw `PhysAddr`s (via
//! `alloc_kernel_page` / `free_page_frame`) rather than typed
//! `Frame<KernelMeta>` handles: the slab tier's own bookkeeping must
//! stay heap-free, and wrapping pages as `Frame` would require a
//! `KVec` (heap allocation) that re-enters the slab during init.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::align_down_u64;
use slopos_ostd::sync::{ByteChain, RawLink};
use slopos_ostd::util::ptr_buf;

use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::PAGE_SIZE_4KB;

pub(crate) const SLAB_MAGIC: u32 = 0x534C_4142;
pub(crate) const LARGE_MAGIC: u32 = 0x4C_4152_47;
pub(crate) const LARGE_FREE_MAGIC: u32 = 0x4C_4652_45;

/// In-page slab header. Lives at offset 0 of a slab page (HHDM
/// address). Accessed via OSTD `ptr_buf` helpers — never via raw
/// `&mut SlabHeader` from outside the helpers.
#[repr(C)]
pub(crate) struct SlabHeader {
    pub magic: u32,
    pub object_size: u32,
    pub total_count: u16,
    pub free_count: u16,
    /// Class index (0..=7); makes `page_kind_for` O(1).
    pub class_idx: u8,
    pub _pad: [u8; 3],
    pub next: RawLink<SlabHeader>,
    pub free_list: ByteChain,
}

const _: () = {
    // SlabHeader must fit comfortably in the first part of a 4 KiB
    // page; SIZE_CLASSES is bounded so the body still holds plenty of
    // objects. This isn't a hard ABI assertion — just a guardrail
    // against bloat that would shrink the per-slab object count.
    assert!(core::mem::size_of::<SlabHeader>() <= 64);
};

impl SlabHeader {
    /// Byte offset where the object array starts inside a slab page.
    #[inline]
    pub(crate) fn object_start_offset() -> usize {
        let raw = core::mem::size_of::<SlabHeader>();
        (raw + 15) & !15
    }

    /// Pointer to object `idx` inside a slab page whose header lives
    /// at `slab_base` (HHDM-addressed). Returns `None` if the object
    /// would extend past the page.
    #[inline]
    pub(crate) fn object_at(
        slab_base: NonNull<u8>,
        idx: usize,
        object_size: usize,
    ) -> Option<NonNull<u8>> {
        let start = Self::object_start_offset();
        let off = start.checked_add(idx.checked_mul(object_size)?)?;
        if off.checked_add(object_size)? > PAGE_SIZE_4KB as usize {
            return None;
        }
        Some(slopos_ostd::util::ptr_buf::nonnull_byte_offset(
            slab_base, off,
        ))
    }

    /// Mutable byte view of object `obj`'s body region (the bytes
    /// after the inline link slot). Caller owns the slab page
    /// exclusively.
    #[inline]
    pub(crate) fn body_slice_mut<'a>(obj: NonNull<u8>, object_size: usize) -> Option<&'a mut [u8]> {
        let link_bytes = core::mem::size_of::<*mut u8>();
        if object_size <= link_bytes {
            return None;
        }
        let body_len = object_size - link_bytes;
        Some(ptr_buf::borrow_at_mut::<u8>(obj, link_bytes, body_len))
    }
}

/// In-page large-allocation header. Same role as `SlabHeader` for
/// allocations > 2048 bytes.
#[repr(C)]
pub(crate) struct LargeAllocHeader {
    pub magic: u32,
    pub pages: u32,
    pub size: u32,
    pub _reserved: u32,
    pub next: RawLink<LargeAllocHeader>,
}

impl LargeAllocHeader {
    /// Byte offset of the user-visible body within a large-alloc
    /// region.
    #[inline]
    pub(crate) fn body_offset() -> usize {
        let raw = core::mem::size_of::<LargeAllocHeader>();
        (raw + 15) & !15
    }

    /// Body pointer for a header `header`.
    #[inline]
    pub(crate) fn body_ptr(header: NonNull<LargeAllocHeader>) -> NonNull<u8> {
        let base = header.cast::<u8>();
        slopos_ostd::util::ptr_buf::nonnull_byte_offset(base, Self::body_offset())
    }

    /// Mutable byte view spanning `len` bytes starting at the body of
    /// a large-alloc header.
    #[inline]
    pub(crate) fn body_view_mut<'a>(header: NonNull<LargeAllocHeader>, len: usize) -> &'a mut [u8] {
        let body = Self::body_ptr(header);
        ptr_buf::borrow_nonnull_mut(body, len)
    }
}

/// Outcome of [`page_kind_for`]: which tier owns the page containing a
/// `kfree`-supplied pointer.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PageKind {
    Slab { class_idx: u8 },
    Large,
}

/// Peek the leading bytes of the 4 KiB-aligned base of `ptr` and
/// determine which tier (slab or large) owns the allocation. Returns
/// `None` if the magic is neither `SLAB_MAGIC` nor `LARGE_MAGIC` (in
/// which case the pointer is a wild free or double free).
#[inline]
pub(crate) fn page_kind_for(ptr: NonNull<u8>) -> Option<PageKind> {
    let base_addr = align_down_u64(ptr.as_ptr() as u64, PAGE_SIZE_4KB);
    let base = NonNull::new(base_addr as *mut u8)?;
    let magic = read_u32_at(base, 0);
    if magic == SLAB_MAGIC {
        // `object_size` field follows magic; resolve to a class index
        // via the SIZE_CLASSES table.
        let object_size = read_u32_at(base, 4);
        let class_idx = class_idx_of_size(object_size as usize)?;
        Some(PageKind::Slab { class_idx })
    } else if magic == LARGE_MAGIC {
        Some(PageKind::Large)
    } else {
        None
    }
}

#[inline]
fn class_idx_of_size(size: usize) -> Option<u8> {
    let table = super::SIZE_CLASSES;
    let mut i = 0;
    while i < table.len() {
        if table[i] == size {
            return Some(i as u8);
        }
        i += 1;
    }
    None
}

#[inline]
fn read_u32_at(base: NonNull<u8>, off: usize) -> u32 {
    // Word-aligned read of an immutable header field. Goes through
    // OSTD's safe `borrow_at_mut::<u32>` over a single-element slice
    // so the residual `unsafe` stays inside OSTD; the page is held
    // exclusively by the owning class's lock holder during writes,
    // and `kfree` only reads after the writer published the header
    // via `Release` magic stores.
    let slice: &mut [u32] = ptr_buf::borrow_at_mut::<u32>(base, off, 1);
    slice[0]
}

/// Allocate a fresh, zero-initialised 4 KiB kernel page and return
/// its HHDM-addressed base pointer + paddr. Returns `None` on
/// allocation failure.
#[inline]
pub(crate) fn alloc_slab_page() -> Option<(NonNull<u8>, PhysAddr)> {
    let paddr = alloc_kernel_page();
    if paddr.is_null() {
        return None;
    }
    let virt = paddr.to_virt();
    let base = NonNull::new(virt.as_u64() as *mut u8)?;
    Some((base, paddr))
}

/// Allocate `pages` contiguous zeroed kernel pages and return the
/// HHDM-addressed base pointer + paddr. Returns `None` on failure.
#[inline]
pub(crate) fn alloc_large_pages(pages: u32) -> Option<(NonNull<u8>, PhysAddr)> {
    if pages == 0 {
        return None;
    }
    let paddr = crate::page_alloc::alloc_kernel_pages(pages);
    if paddr.is_null() {
        return None;
    }
    let virt = paddr.to_virt();
    let base = NonNull::new(virt.as_u64() as *mut u8)?;
    Some((base, paddr))
}

/// Recover the paddr backing an HHDM-addressed page.
#[inline]
pub(crate) fn paddr_for_page(base: NonNull<u8>) -> PhysAddr {
    use crate::hhdm::VirtAddrHhdm;
    use slopos_abi::addr::VirtAddr;
    let virt = VirtAddr::new(base.as_ptr() as u64);
    virt.to_phys_hhdm()
}

/// Free a slab page by HHDM-addressed base. Returns the underlying
/// paddr to the buddy.
#[inline]
pub(crate) fn free_kernel_page(base: NonNull<u8>) {
    let paddr = paddr_for_page(base);
    free_page_frame(paddr);
}

// ---------------------------------------------------------------------------
// Counters — read-only stats for the `stats` module's snapshot path.
// ---------------------------------------------------------------------------

/// Total slab pages currently held by the slab tier. Incremented when
/// `SlabAllocator::grow_one` claims a fresh page from the buddy.
pub(crate) static SLAB_PAGE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Total large-alloc regions currently in flight.
pub(crate) static LARGE_ALLOC_COUNT: AtomicU32 = AtomicU32::new(0);

#[inline]
pub(crate) fn slab_page_count_inc() {
    SLAB_PAGE_COUNT.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub(crate) fn large_alloc_count_inc() {
    LARGE_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
}
