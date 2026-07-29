//! Large-allocation tier (> 2048 bytes → direct frame allocation).
//!
//! Allocations are taken from the buddy as contiguous multi-page
//! regions whose first page carries a `LargeAllocHeader` (magic +
//! page-count + size). The active list is intrusively threaded
//! through the header's `next: RawLink<LargeAllocHeader>` pointer; a
//! free list (also intrusive) holds returned regions for first-fit
//! reuse.
//!
//! No external tracking table (`KBTreeMap` etc.) is used: the
//! discriminator on `kfree` is the page magic at the 4 KiB-aligned
//! base of the freed pointer (see `super::page::page_kind_for`),
//! which is set/cleared by the allocator and never collides with
//! [`super::page::SLAB_MAGIC`] because both magics are written by the
//! allocator itself at allocation/free time.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::sync::{LOCK_LEVEL_ALLOCATOR, RawLink, SpinLock};

use super::page::{
    LARGE_FREE_MAGIC, LARGE_MAGIC, LargeAllocHeader, alloc_large_pages, large_alloc_count_inc,
};
use super::poison::{POISON_FREED, poison_object_body};
use crate::paging_defs::PAGE_SIZE_4KB;

const SLAB_DEBUG: bool = false;

pub(crate) struct LargeInner {
    free_list: RawLink<LargeAllocHeader>,
}

impl LargeInner {
    pub(crate) const fn new() -> Self {
        Self {
            free_list: RawLink::null(),
        }
    }
}

pub struct LargeAlloc {
    inner: SpinLock<LargeInner>,
    pub(crate) total_bytes_allocated: AtomicU64,
    pub(crate) total_bytes_freed: AtomicU64,
}

impl LargeAlloc {
    pub(crate) const fn new() -> Self {
        Self {
            inner: SpinLock::new(LargeInner::new(), LOCK_LEVEL_ALLOCATOR),
            total_bytes_allocated: AtomicU64::new(0),
            total_bytes_freed: AtomicU64::new(0),
        }
    }

    /// Allocate `size` bytes (size > 2048 path).
    pub fn alloc(&self, size: usize) -> Option<NonNull<u8>> {
        let header_size = LargeAllocHeader::body_offset();
        let total = size.checked_add(header_size)?;
        let pages = total.div_ceil(PAGE_SIZE_4KB as usize) as u32;
        if pages == 0 {
            return None;
        }

        // Free-list first-fit walk.
        {
            let state = self.inner.lock();
            let mut prev: Option<NonNull<LargeAllocHeader>> = None;
            let mut current = state.free_list.load();
            while let Some(curr) = current {
                let snap = RawLink::<LargeAllocHeader>::with_mut_at(Some(curr), |h| {
                    (h.pages, h.next.load())
                });
                let Some((slab_pages, next)) = snap else {
                    break;
                };
                if slab_pages >= pages {
                    // Detach `curr`.
                    match prev {
                        None => state.free_list.store(next),
                        Some(p) => {
                            RawLink::<LargeAllocHeader>::with_mut_at(Some(p), |h| {
                                h.next.store(next)
                            });
                        }
                    }
                    RawLink::<LargeAllocHeader>::with_mut_at(Some(curr), |h| {
                        h.magic = LARGE_MAGIC;
                        h.size = size as u32;
                        h.next = RawLink::null();
                    });
                    self.total_bytes_allocated
                        .fetch_add(size as u64, Ordering::Relaxed);
                    return Some(LargeAllocHeader::body_ptr(curr));
                }
                prev = Some(curr);
                current = next;
            }
        }

        // Fresh allocation from the buddy.
        let (base, _paddr) = alloc_large_pages(pages)?;
        let Some(header_nn) = NonNull::new(base.as_ptr() as *mut LargeAllocHeader) else {
            return None;
        };
        RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
            h.magic = LARGE_MAGIC;
            h.pages = pages;
            h.size = size as u32;
            h._reserved = 0;
            h.next = RawLink::null();
        });
        large_alloc_count_inc();
        self.total_bytes_allocated
            .fetch_add(size as u64, Ordering::Relaxed);
        Some(LargeAllocHeader::body_ptr(header_nn))
    }

    /// Return a previously [`Self::alloc`]-ed pointer. The caller has
    /// already established (via the page magic at the 4 KiB-aligned
    /// base) that this pointer belongs to a large region.
    pub fn dealloc(&self, ptr: NonNull<u8>) {
        let base_addr = (ptr.as_ptr() as u64) & !(PAGE_SIZE_4KB - 1);
        let Some(header_nn) = NonNull::new(base_addr as *mut LargeAllocHeader) else {
            return;
        };

        let state = self.inner.lock();
        let prev_head = state.free_list.load();

        let snap = RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
            if h.magic != LARGE_MAGIC {
                return None;
            }
            let pages = h.pages;
            let size = h.size as u64;
            h.magic = LARGE_FREE_MAGIC;
            h.next.store(prev_head);
            Some((pages, size))
        })
        .flatten();
        let Some((pages, size)) = snap else {
            return;
        };
        state.free_list.store(Some(header_nn));
        self.total_bytes_freed.fetch_add(size, Ordering::Relaxed);

        if SLAB_DEBUG {
            let hdr_sz = LargeAllocHeader::body_offset();
            let total_bytes = (pages as usize) * PAGE_SIZE_4KB as usize;
            if total_bytes > hdr_sz {
                let body_len = total_bytes - hdr_sz;
                LargeAllocHeader::with_body_view_mut(header_nn, body_len, |body| {
                    poison_object_body(body, POISON_FREED)
                });
            }
        }
        let _ = pages;
        // Free regions stay on the first-fit free list; the buddy keeps
        // ownership of the physical pages until the slab itself is torn
        // down.
    }
}
