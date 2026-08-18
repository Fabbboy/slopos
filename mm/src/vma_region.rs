//! Type-safe Virtual Memory Area subsystem.
//!
//! Each region's backing is an enum variant (not a flags bitfield), so the
//! compiler enforces exhaustive handling.
//!
//! Overlaps are prevented structurally: the gap finder returns addresses from
//! gaps between existing entries, and `insert` merges compatible adjacent
//! regions automatically.

use slopos_abi::quota::PagesAxis;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{ChargeSlot, Reservation, TryChargeError, try_charge};
use slopos_ostd::{KBTreeMap, KVec};

use crate::memfd::MemfdHandle;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// What backs a memory region's physical pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionBacking {
    /// Anonymous zero-fill on demand (heap, stack, mmap MAP_ANONYMOUS).
    Anonymous,
    /// Shared memfd — pages belong to the MemfdObject, not the process.
    /// Must not be freed on munmap; only mapcount decrement.
    SharedMemfd { handle: MemfdHandle },
    /// SlopRing shared region (SLOPRING § 5.1). The kernel-side ring object
    /// owns the frames as `Frame<RingMeta>`s and the user PTE holds an
    /// independent `from_in_use` ref, so a mapping outliving the fd cannot
    /// UAF; this VMA only reserves the virtual range. Not inherited across
    /// fork (the ring fd is close-on-fork — SLOPRING § 14).
    Ring,
}

/// Page protection bits. Separate from backing/state to prevent conflation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protection {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Protection {
    pub const RW: Self = Self {
        read: true,
        write: true,
        exec: false,
    };
    pub const RO: Self = Self {
        read: true,
        write: false,
        exec: false,
    };
    pub const RX: Self = Self {
        read: true,
        write: false,
        exec: true,
    };
    pub const NONE: Self = Self {
        read: false,
        write: false,
        exec: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionPurpose {
    /// Generic mmap'd region.
    General,
    /// brk-managed heap.
    Heap,
    /// Process stack.
    Stack,
    /// ELF .text (read+exec).
    Code,
    /// ELF .data/.bss (read+write).
    Data,
}

/// A virtual memory region with typed backing, protection, and purpose.
#[derive(Clone, Debug)]
pub struct VmaRegion {
    pub protection: Protection,
    pub backing: RegionBacking,
    /// Demand-paged: physical pages not yet allocated (fault-on-access).
    pub lazy: bool,
    /// Copy-on-write: shared read-only until written (fork).
    pub cow: bool,
    /// User-mode accessible (Ring 3).
    pub user: bool,
    pub purpose: RegionPurpose,
}

impl VmaRegion {
    pub fn can_merge_with(&self, other: &VmaRegion) -> bool {
        self.protection == other.protection
            && self.backing == other.backing
            && self.lazy == other.lazy
            && self.cow == other.cow
            && self.user == other.user
            && self.purpose == other.purpose
    }

    pub fn is_demand_paged(&self) -> bool {
        self.lazy
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self.backing, RegionBacking::Anonymous)
    }

    pub fn is_shared(&self) -> bool {
        matches!(self.backing, RegionBacking::SharedMemfd { .. })
    }

    /// `true` iff this region is a SlopRing shared mapping; like `is_shared()`,
    /// its PTEs must be unmapped *without* the anonymous-frame free path.
    pub fn is_ring(&self) -> bool {
        matches!(self.backing, RegionBacking::Ring)
    }

    pub fn memfd_handle(&self) -> Option<MemfdHandle> {
        match &self.backing {
            RegionBacking::SharedMemfd { handle } => Some(*handle),
            _ => None,
        }
    }

    pub fn to_page_flags(&self) -> PageFlags {
        let mut pf = PageFlags::PRESENT;
        if self.user {
            pf = pf.union(PageFlags::USER);
        }
        if self.cow {
            pf = pf.union(PageFlags::COW);
        } else if self.protection.write {
            pf = pf.union(PageFlags::WRITABLE);
        }
        if !self.protection.exec {
            pf = pf.union(PageFlags::NO_EXECUTE);
        }
        pf
    }
}

/// Pages spanned by the half-open range `[start, end)`.
#[inline]
fn range_pages(start: u64, end: u64) -> u32 {
    let bytes = end.saturating_sub(start);
    u32::try_from(bytes.div_ceil(PAGE_SIZE_4KB)).unwrap_or(u32::MAX)
}

/// A sorted map of non-overlapping virtual memory regions.
///
/// Key = start address, value = (end address, region).
/// All intervals are half-open: [start, end).
/// Invariant: no two entries overlap; maintained by construction.
///
/// One [`ChargeSlot<PagesAxis>`] covers the whole map rather than one per
/// [`VmaRegion`]: a scalar charge on a region cannot survive being split,
/// whereas the map itself is the carved set. [`link`](Self::link) and
/// [`unlink`](Self::unlink) are the only writers of both the tree and
/// `mapped_pages`, so the charge cannot drift; [`audit`](Self::audit) checks
/// that at runtime anyway.
pub struct VmaMap {
    map: KBTreeMap<u64, (u64, VmaRegion)>,
    /// Pages the tree currently spans. Maintained incrementally by
    /// `link`/`unlink` rather than recomputed, so a mutation stays O(log n).
    mapped_pages: u32,
    /// The account [`mapped_pages`](Self::mapped_pages) is charged to, kept
    /// separately because an empty slot names no account.
    account: AccountId,
    charge: ChargeSlot<PagesAxis>,
}

impl VmaMap {
    pub const fn new() -> Self {
        Self {
            map: KBTreeMap::new(),
            mapped_pages: 0,
            account: AccountId::NONE,
            charge: ChargeSlot::empty(),
        }
    }

    /// Name the principal this address space's pages are charged to.
    ///
    /// Anything already mapped is re-charged against the new account, so the
    /// binding order is not load-bearing.
    pub fn bind_account(&mut self, account: AccountId) {
        if self.account == account {
            return;
        }
        self.charge.take();
        self.account = account;
        if self.mapped_pages != 0
            && let Ok(reservation) = try_charge::<PagesAxis>(account, self.mapped_pages)
        {
            self.charge.put(reservation);
        }
    }

    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    #[inline]
    pub fn mapped_pages(&self) -> u32 {
        self.mapped_pages
    }

    /// Pages the charge token currently holds.
    #[inline]
    pub fn charged_pages(&self) -> u32 {
        self.charge.amount()
    }

    /// Recompute the tree's span and report it beside `mapped_pages` and the
    /// charge — the runtime form of "the charge equals the map".
    pub fn audit(&self) -> (u32, u32, u32) {
        let walked = self.map.iter().fold(0u32, |acc, entry| {
            acc.saturating_add(range_pages(*entry.0, entry.1.0))
        });
        (walked, self.mapped_pages, self.charge.amount())
    }

    /// Add one entry to the tree. Tracks the span; never touches the charge,
    /// which only [`settle`](Self::settle) and an `insert`'s reservation move.
    fn link(&mut self, start: u64, end: u64, region: VmaRegion) {
        self.mapped_pages = self.mapped_pages.saturating_add(range_pages(start, end));
        self.map.insert(start, (end, region));
    }

    /// Remove one entry from the tree. Tracks the span; never touches the charge.
    fn unlink(&mut self, start: u64) -> Option<(u64, VmaRegion)> {
        let (end, region) = self.map.remove(&start)?;
        self.mapped_pages = self.mapped_pages.saturating_sub(range_pages(start, end));
        Some((end, region))
    }

    /// Give back whatever the charge holds above what the tree spans.
    ///
    /// Only ever a shrink, so it is infallible: growth is always pre-reserved
    /// by the caller that wanted it, and a `munmap` must not be refusable
    /// against a ceiling it is *reducing* the use of.
    fn settle(&mut self) {
        self.charge
            .shrink(self.charge.amount().saturating_sub(self.mapped_pages));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a region, merging with compatible adjacent regions.
    ///
    /// Charges `[start, end)` against this map's account before touching the
    /// tree, so a refusal leaves the address space exactly as it found it. A
    /// merge absorbs entries whose pages are already charged and widens the
    /// new entry by exactly as much, so the reservation taken here is the net
    /// growth however many neighbours merge.
    pub fn insert(
        &mut self,
        start: u64,
        end: u64,
        region: VmaRegion,
    ) -> Result<(), TryChargeError> {
        let reserved = self.reserve_pages(start, end)?;
        self.insert_reserved(start, end, region, reserved);
        Ok(())
    }

    /// Take the page charge for `[start, end)` without touching the tree.
    ///
    /// For a caller that must map before it can link: a refusal then happens
    /// before any page-table write. The reservation refunds itself if dropped.
    pub fn reserve_pages(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Reservation<PagesAxis>, TryChargeError> {
        try_charge::<PagesAxis>(self.account, range_pages(start, end))
    }

    /// [`insert`](Self::insert) with the charge already taken.
    #[allow(unused_mut)]
    pub fn insert_reserved(
        &mut self,
        mut start: u64,
        mut end: u64,
        region: VmaRegion,
        reservation: Reservation<PagesAxis>,
    ) {
        let merge_pred = self
            .map
            .range(..start)
            .next_back()
            .filter(|entry| entry.1.0 == start && entry.1.1.can_merge_with(&region))
            .map(|entry| *entry.0);
        // The absorbed neighbour's pages stay charged: they are re-linked below
        // as part of the widened entry, a move rather than a removal.
        if let Some(pred_start) = merge_pred {
            start = pred_start;
            self.unlink(pred_start);
        }

        let merge_succ = self
            .map
            .range(end..)
            .next()
            .filter(|entry| *entry.0 == end && entry.1.1.can_merge_with(&region))
            .map(|entry| (*entry.0, entry.1.0));
        if let Some((succ_start, succ_end)) = merge_succ {
            end = succ_end;
            self.unlink(succ_start);
        }

        #[cfg(debug_assertions)]
        {
            for entry in self.map.range(..end) {
                let s = *entry.0;
                let e = entry.1.0;
                debug_assert!(
                    e <= start,
                    "VmaMap::insert: overlap detected [{:#x},{:#x}) vs [{:#x},{:#x})",
                    s,
                    e,
                    start,
                    end
                );
            }
        }

        self.charge.grow(reservation);
        self.link(start, end, region);
        debug_assert_eq!(
            self.charge.amount(),
            self.mapped_pages,
            "VmaMap::insert left the page charge disagreeing with the tree"
        );
    }

    /// Find the region containing address `addr`.
    pub fn find_containing(&self, addr: u64) -> Option<(u64, u64, &VmaRegion)> {
        let entry = self.map.range(..=addr).next_back()?;
        let start = *entry.0;
        let end = entry.1.0;
        let region = &entry.1.1;
        if addr < end {
            Some((start, end, region))
        } else {
            None
        }
    }

    /// Find a region that fully covers [start, end).
    pub fn find_covering(&self, start: u64, end: u64) -> Option<(u64, u64, &VmaRegion)> {
        let entry = self.map.range(..=start).next_back()?;
        let vma_start = *entry.0;
        let vma_end = entry.1.0;
        let region = &entry.1.1;
        if vma_start <= start && vma_end >= end {
            Some((vma_start, vma_end, region))
        } else {
            None
        }
    }

    /// Find a region that fully covers [start, end), mutable.
    pub fn find_covering_mut(
        &mut self,
        start: u64,
        end: u64,
    ) -> Option<(u64, u64, &mut VmaRegion)> {
        let key = {
            let entry = self.map.range(..=start).next_back()?;
            let vma_start = *entry.0;
            let vma_end = entry.1.0;
            if vma_start <= start && vma_end >= end {
                Some(vma_start)
            } else {
                None
            }
        }?;
        let val = self.map.get_mut(&key)?;
        Some((key, val.0, &mut val.1))
    }

    /// Find the first gap >= `size` bytes in [from, limit).
    pub fn find_gap(&self, from: u64, limit: u64, size: u64) -> Option<u64> {
        if size == 0 {
            return None;
        }

        let mut candidate = from;

        if let Some(entry) = self.map.range(..from).next_back() {
            let pred_end = entry.1.0;
            if pred_end > candidate {
                candidate = pred_end;
            }
        }

        for entry in self.map.range(from..) {
            let vma_start = *entry.0;
            let vma_end = entry.1.0;
            if candidate + size <= vma_start {
                return Some(candidate);
            }
            if vma_end > candidate {
                candidate = vma_end;
            }
        }

        if candidate + size <= limit {
            Some(candidate)
        } else {
            None
        }
    }

    /// Iterate all regions in address order: (start, end, &region).
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64, &VmaRegion)> {
        self.map
            .iter()
            .map(|entry| (*entry.0, entry.1.0, &entry.1.1))
    }

    /// Remove all regions overlapping [start, end), splitting at boundaries.
    /// Calls `on_removed(overlap_start, overlap_end, &region)` for each
    /// removed portion.
    pub fn remove_range(
        &mut self,
        start: u64,
        end: u64,
        mut on_removed: impl FnMut(u64, u64, &VmaRegion),
    ) {
        let affected: KVec<u64> = KVec::from_iter_fallible(
            self.map
                .range(..end)
                .filter(|entry| entry.1.0 > start && *entry.0 < end)
                .map(|entry| *entry.0),
        )
        .expect("remove_range: affected alloc");

        let pred = self
            .map
            .range(..start)
            .next_back()
            .map(|entry| (*entry.0, entry.1.0));
        if let Some((pred_start, pred_end)) = pred {
            if pred_end > start && !affected.iter().any(|k| *k == pred_start) {
                let (pred_end_val, region) = self.unlink(pred_start).unwrap();
                let overlap_start = start;
                let overlap_end = pred_end_val.min(end);
                on_removed(overlap_start, overlap_end, &region);
                if pred_start < start {
                    self.link(pred_start, start, region.clone());
                }
                if pred_end_val > end {
                    self.link(end, pred_end_val, region);
                }
            }
        }

        for key in affected {
            if let Some((vma_end, region)) = self.unlink(key) {
                let vma_start = key;
                let overlap_start = vma_start.max(start);
                let overlap_end = vma_end.min(end);
                on_removed(overlap_start, overlap_end, &region);

                if vma_start < start {
                    self.link(vma_start, start, region.clone());
                }
                if vma_end > end {
                    self.link(end, vma_end, region);
                }
            }
        }
        // One settle for the whole range: the remnants are re-linked before the
        // charge is reconciled, so a split refunds exactly the carved hole.
        self.settle();
    }

    /// Drain all regions, calling `on_each(start, end, &region)` before removal.
    pub fn drain(&mut self, mut on_each: impl FnMut(u64, u64, &VmaRegion)) {
        let keys: KVec<u64> =
            KVec::from_iter_fallible(self.map.keys().copied()).expect("vma drain: alloc");
        for key in keys {
            if let Some((end, region)) = self.unlink(key) {
                on_each(key, end, &region);
            }
        }
        self.settle();
    }

    /// Clear all regions without callbacks, refunding every page.
    pub fn clear(&mut self) {
        self.map.clear();
        self.mapped_pages = 0;
        self.charge.take();
    }
}
