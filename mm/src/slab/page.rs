//! Slab- and large-alloc-page primitives. Each slab page and each large-alloc
//! region carries a header at offset 0 (HHDM-addressed) whose leading `u32`
//! magic names the owning tier, so `kfree` discriminates without a side table.
//!
//! Those pages are owned by raw `PhysAddr`s rather than typed
//! `Frame<KernelMeta>` handles: the slab tier's bookkeeping must stay
//! heap-free, and wrapping a page as a `Frame` needs a `KVec` whose allocation
//! re-enters the slab during init.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_ostd::align_down_u64;
use slopos_ostd::sync::{ByteChain, RawLink};
use slopos_ostd::util::ptr_buf;

use slopos_abi::quota::KernelMetaAxis;
use slopos_ostd::process::quota::{ChargeSlot, root, try_charge};

use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::PAGE_SIZE_4KB;

pub(crate) const SLAB_MAGIC: u32 = 0x534C_4142;
pub(crate) const LARGE_MAGIC: u32 = 0x4C_4152_47;
pub(crate) const LARGE_FREE_MAGIC: u32 = 0x4C_4652_45;

/// In-page slab header. Accessed via OSTD `ptr_buf` helpers — never as a raw
/// `&mut SlabHeader` from outside them.
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
    // Not an ABI constraint — a guardrail against header bloat that would
    // shrink the per-slab object count.
    assert!(core::mem::size_of::<SlabHeader>() <= 64);
};

impl SlabHeader {
    #[inline]
    pub(crate) fn object_start_offset() -> usize {
        let raw = core::mem::size_of::<SlabHeader>();
        (raw + 15) & !15
    }

    /// Pointer to object `idx` in the slab page headed at `slab_base`
    /// (HHDM-addressed), or `None` if the object would extend past the page.
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
        Some(ptr_buf::nonnull_byte_offset_in(
            slab_base,
            off,
            PAGE_SIZE_4KB as usize,
        ))
    }

    /// Run `f` over object `obj`'s body (the bytes after the inline link slot).
    /// Caller owns the slab page exclusively.
    ///
    /// Scoped rather than returning the slice: the caller has only an address,
    /// so a returned `&mut [u8]` would carry a lifetime it picks — and two
    /// picks is two mutable views of one object.
    #[inline]
    pub(crate) fn with_body_slice_mut<R>(
        obj: NonNull<u8>,
        object_size: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> Option<R> {
        let link_bytes = core::mem::size_of::<*mut u8>();
        if object_size <= link_bytes {
            return None;
        }
        let body_len = object_size - link_bytes;
        Some(ptr_buf::with_at_mut::<u8, _>(obj, link_bytes, body_len, f))
    }
}

/// In-page header for allocations past 2048 bytes; `SlabHeader`'s counterpart.
#[repr(C)]
pub(crate) struct LargeAllocHeader {
    pub magic: u32,
    pub pages: u32,
    pub size: u32,
    pub _reserved: u32,
    pub next: RawLink<LargeAllocHeader>,
}

impl LargeAllocHeader {
    #[inline]
    pub(crate) fn body_offset() -> usize {
        let raw = core::mem::size_of::<LargeAllocHeader>();
        (raw + 15) & !15
    }

    /// The bound is one page rather than the region's real `pages * 4 KiB`: the
    /// large tier only ever hands out whole pages, and reading `header.pages`
    /// to tighten it would dereference the very pointer being validated. What
    /// the assertion catches is a `LargeAllocHeader` grown past a page.
    #[inline]
    pub(crate) fn body_ptr(header: NonNull<LargeAllocHeader>) -> NonNull<u8> {
        let base = header.cast::<u8>();
        ptr_buf::nonnull_byte_offset_in(base, Self::body_offset(), PAGE_SIZE_4KB as usize)
    }

    #[inline]
    pub(crate) fn with_body_view_mut<R>(
        header: NonNull<LargeAllocHeader>,
        len: usize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        let body = Self::body_ptr(header);
        ptr_buf::with_nonnull_mut(body, len, f)
    }
}

/// Which tier owns the page containing a `kfree`-supplied pointer.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PageKind {
    Slab { class_idx: u8 },
    Large,
}

/// Determine which tier owns `ptr`'s allocation from the magic at its
/// 4 KiB-aligned base. `None` means neither magic matched, so the pointer is a
/// wild free or a double free.
#[inline]
pub(crate) fn page_kind_for(ptr: NonNull<u8>) -> Option<PageKind> {
    let base_addr = align_down_u64(ptr.as_ptr() as u64, PAGE_SIZE_4KB);
    let base = NonNull::new(base_addr as *mut u8)?;
    let magic = read_u32_at(base, 0);
    if magic == SLAB_MAGIC {
        // `object_size` follows the magic at offset 4.
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
    // The page is held exclusively by the owning class's lock holder during
    // writes, and `kfree` only reads after the writer published the header via
    // its `Release` magic store.
    ptr_buf::with_at_mut::<u32, _>(base, off, 1, |slice| slice[0])
}

/// The kernel heap's own page charge, held against the root account.
///
/// These are the pages backing *every* kernel allocation, charged so the root's
/// `KernelMeta` row reconciles against the buddy's own allocated count. They go
/// to the root and **not** to whoever allocated, because the slab is shared;
/// per-object attribution is what the tier-1 object charges are for.
///
/// A `.bss` slot rather than a `Charge` field on the page header: that header
/// is `#[repr(C)]`, written through raw pointers, and sits at a fixed offset
/// the free path reads back — the placement the design prohibits for a token.
static HEAP_PAGES: ChargeSlot<KernelMetaAxis> = ChargeSlot::empty();

/// Charge `pages` of kernel-heap backing to the root, or refuse. Refusing is
/// what makes the ceiling real rather than advisory: the caller propagates it
/// as an allocation failure, which every slab and large-alloc path handles.
fn charge_heap_pages(pages: u32) -> bool {
    match try_charge::<KernelMetaAxis>(root(), pages) {
        Ok(reservation) => {
            HEAP_PAGES.grow(reservation);
            true
        }
        Err(_) => false,
    }
}

/// A fresh zero-initialised 4 KiB kernel page, as an HHDM-addressed base
/// pointer and its paddr.
#[inline]
pub(crate) fn alloc_slab_page() -> Option<(NonNull<u8>, PhysAddr)> {
    let paddr = alloc_kernel_page();
    if paddr.is_null() {
        return None;
    }
    let virt = paddr.to_virt();
    let Some(base) = NonNull::new(virt.as_u64() as *mut u8) else {
        free_page_frame(paddr);
        return None;
    };
    if !charge_heap_pages(1) {
        free_page_frame(paddr);
        return None;
    }
    Some((base, paddr))
}

/// `pages` contiguous zeroed kernel pages, as an HHDM-addressed base pointer
/// and its paddr.
///
/// Large-tier allocations are charged here and never again at the call site: a
/// second charge would make the root's row mean two different quantities and
/// stop it reconciling against the buddy.
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
    let Some(base) = NonNull::new(virt.as_u64() as *mut u8) else {
        free_page_frame(paddr);
        return None;
    };
    if !charge_heap_pages(pages) {
        free_page_frame(paddr);
        return None;
    }
    Some((base, paddr))
}

pub fn charged_heap_pages() -> u32 {
    HEAP_PAGES.amount()
}

/// Recover the paddr backing an HHDM-addressed page.
#[inline]
pub(crate) fn paddr_for_page(base: NonNull<u8>) -> PhysAddr {
    use crate::hhdm::VirtAddrHhdm;
    use slopos_abi::addr::VirtAddr;
    let virt = VirtAddr::new(base.as_ptr() as u64);
    virt.to_phys_hhdm()
}

/// Free a slab page by HHDM-addressed base, returning its paddr to the buddy
/// and its charge to the root.
#[inline]
pub(crate) fn free_kernel_page(base: NonNull<u8>) {
    let paddr = paddr_for_page(base);
    free_page_frame(paddr);
    HEAP_PAGES.shrink(1);
}

pub(crate) static SLAB_PAGE_COUNT: AtomicU32 = AtomicU32::new(0);

pub(crate) static LARGE_ALLOC_COUNT: AtomicU32 = AtomicU32::new(0);

#[inline]
pub(crate) fn slab_page_count_inc() {
    SLAB_PAGE_COUNT.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub(crate) fn large_alloc_count_inc() {
    LARGE_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
}
