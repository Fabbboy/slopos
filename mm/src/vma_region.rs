//! Type-safe Virtual Memory Area subsystem.
//!
//! Replaces the hand-rolled RB tree (`vma_tree.rs`) with a `BTreeMap`-backed
//! design inspired by Redox OS. Each region's backing is an enum variant
//! (not a flags bitfield), so the compiler enforces exhaustive handling.
//!
//! Overlaps are prevented structurally: the gap finder returns addresses from
//! gaps between existing entries, and `insert` merges compatible adjacent
//! regions automatically.

use slopos_ostd::{KBTreeMap, KVec};

use crate::memfd::MemfdHandle;
use crate::paging_defs::PageFlags;

// ---------------------------------------------------------------------------
// RegionBacking — what provides pages for this region
// ---------------------------------------------------------------------------

/// What backs a memory region's physical pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionBacking {
    /// Anonymous zero-fill on demand (heap, stack, mmap MAP_ANONYMOUS).
    Anonymous,
    /// Shared memfd — pages belong to the MemfdObject, not the process.
    /// Must not be freed on munmap; only mapcount decrement.
    SharedMemfd { handle: MemfdHandle },
}

// ---------------------------------------------------------------------------
// Protection — hardware page protection (orthogonal to backing type)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// RegionPurpose — semantic tag for special regions (brk, stack)
// ---------------------------------------------------------------------------

/// Semantic purpose of a memory region. Used by brk/stack special handling.
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

// ---------------------------------------------------------------------------
// VmaRegion — typed metadata for a virtual memory region
// ---------------------------------------------------------------------------

/// A virtual memory region with typed backing, protection, and purpose.
///
/// Replaces the old `VmaNode` + `VmaFlags` bitfield. The `RegionBacking`
/// enum makes it impossible to accidentally conflate anonymous and shared
/// mappings — the compiler forces exhaustive match.
#[derive(Clone, Debug)]
pub struct VmaRegion {
    pub protection: Protection,
    pub backing: RegionBacking,
    /// Demand-paged: physical pages not yet allocated (fault-on-access).
    pub lazy: bool,
    /// Copy-on-write: shared read-only until written (fork).
    pub cow: bool,
    /// User-mode accessible (Ring 3). Kernel-only mappings set this false.
    pub user: bool,
    pub purpose: RegionPurpose,
}

impl VmaRegion {
    /// Can two adjacent regions be merged into one?
    /// Requires identical protection, backing variant, and state.
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

    pub fn memfd_handle(&self) -> MemfdHandle {
        match &self.backing {
            RegionBacking::SharedMemfd { handle } => *handle,
            _ => MemfdHandle::NONE,
        }
    }

    /// Convert to x86-64 page table entry flags.
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

// ---------------------------------------------------------------------------
// VmaMap — sorted, non-overlapping map of virtual memory regions
// ---------------------------------------------------------------------------

/// A sorted map of non-overlapping virtual memory regions.
///
/// Key = start address, value = (end address, region).
/// All intervals are half-open: [start, end).
/// Invariant: no two entries overlap; maintained by construction.
pub struct VmaMap {
    map: KBTreeMap<u64, (u64, VmaRegion)>,
}

impl VmaMap {
    pub const fn new() -> Self {
        Self {
            map: KBTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a region, merging with compatible adjacent regions.
    ///
    /// In debug builds, asserts that no incompatible overlap exists (gap
    /// finder guarantees non-overlap by construction).
    #[allow(unused_mut)]
    pub fn insert(&mut self, mut start: u64, mut end: u64, region: VmaRegion) {
        // Try merge with predecessor (entry whose end == start).
        let merge_pred = self
            .map
            .range(..start)
            .next_back()
            .filter(|entry| entry.1.0 == start && entry.1.1.can_merge_with(&region))
            .map(|entry| *entry.0);
        if let Some(pred_start) = merge_pred {
            start = pred_start;
            self.map.remove(&pred_start);
        }

        // Try merge with successor (entry whose start == end).
        let merge_succ = self
            .map
            .range(end..)
            .next()
            .filter(|entry| *entry.0 == end && entry.1.1.can_merge_with(&region))
            .map(|entry| (*entry.0, entry.1.0));
        if let Some((succ_start, succ_end)) = merge_succ {
            end = succ_end;
            self.map.remove(&succ_start);
        }

        // Debug: assert no actual overlap.
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

        self.map.insert(start, (end, region));
    }

    /// Remove the exact region starting at `start`. Returns the removed region.
    pub fn remove_exact(&mut self, start: u64) -> Option<(u64, VmaRegion)> {
        self.map.remove(&start)
    }

    /// Find the region containing address `addr` (point query).
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

        // Check if a predecessor straddles `from`.
        if let Some(entry) = self.map.range(..from).next_back() {
            let pred_end = entry.1.0;
            if pred_end > candidate {
                candidate = pred_end;
            }
        }

        // Walk entries from `from` onward.
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
        // Collect affected keys to avoid borrow conflict.
        let affected: KVec<u64> = KVec::from_iter_fallible(
            self.map
                .range(..end)
                .filter(|entry| entry.1.0 > start && *entry.0 < end)
                .map(|entry| *entry.0),
        )
        .expect("remove_range: affected alloc");

        // Also check predecessor that might straddle `start`.
        let pred = self
            .map
            .range(..start)
            .next_back()
            .map(|entry| (*entry.0, entry.1.0));
        if let Some((pred_start, pred_end)) = pred {
            if pred_end > start && !affected.iter().any(|k| *k == pred_start) {
                // This predecessor overlaps but wasn't caught by the range query.
                let (pred_end_val, region) = self.map.remove(&pred_start).unwrap();
                let overlap_start = start;
                let overlap_end = pred_end_val.min(end);
                on_removed(overlap_start, overlap_end, &region);
                // Re-insert left remnant.
                if pred_start < start {
                    self.map.insert(pred_start, (start, region.clone()));
                }
                // Re-insert right remnant.
                if pred_end_val > end {
                    self.map.insert(end, (pred_end_val, region));
                }
            }
        }

        for key in affected {
            if let Some((vma_end, region)) = self.map.remove(&key) {
                let vma_start = key;
                let overlap_start = vma_start.max(start);
                let overlap_end = vma_end.min(end);
                on_removed(overlap_start, overlap_end, &region);

                // Re-insert left remnant [vma_start, start).
                if vma_start < start {
                    self.map.insert(vma_start, (start, region.clone()));
                }
                // Re-insert right remnant [end, vma_end).
                if vma_end > end {
                    self.map.insert(end, (vma_end, region));
                }
            }
        }
    }

    /// Shrink a region's start (for partial munmap from the left).
    pub fn set_start(&mut self, old_start: u64, new_start: u64) {
        if let Some((end, region)) = self.map.remove(&old_start) {
            self.map.insert(new_start, (end, region));
        }
    }

    /// Change a region's end (for brk, partial munmap from the right).
    pub fn set_end(&mut self, start: u64, new_end: u64) {
        if let Some(val) = self.map.get_mut(&start) {
            val.0 = new_end;
        }
    }

    /// Drain all regions, calling `on_each(start, end, &region)` before removal.
    pub fn drain(&mut self, mut on_each: impl FnMut(u64, u64, &VmaRegion)) {
        // Collect all keys first (can't mutate during iteration).
        let keys: KVec<u64> =
            KVec::from_iter_fallible(self.map.keys().copied()).expect("vma drain: alloc");
        for key in keys {
            if let Some((end, region)) = self.map.remove(&key) {
                on_each(key, end, &region);
            }
        }
    }

    /// Clear all regions without callbacks.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}
