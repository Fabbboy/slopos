//! Per-address-space typed page-table handle.
//!
//! `VmSpace` is the only handle to a process address space exposed
//! by OSTD. The PML4 frame is private; consumers cannot reach the
//! raw page-table pointer. The sole mutation path is through a
//! [`CursorMut`] held against `&mut VmSpace`.
//!
//! # Lifecycle
//!
//! - [`VmSpace::new`] allocates a fresh PML4 via the registered
//!   [`FrameAlloc`] and copies kernel-half mappings (indices 256..512)
//!   from the registered [`register_kernel_master_pml4`] master.
//! - Mutation happens through [`CursorMut`] held against
//!   `&mut VmSpace`. One mutator at a time per `VmSpace`.
//! - [`VmSpace::activate`] is the only sanctioned way to switch
//!   address spaces (CR3 write through
//!   [`crate::arch::x86_64::cr3::write_cr3_pcid`]).
//!
//! # Generation counter
//!
//! `VmSpace::generation()` reflects an `AtomicU64` that the
//! `CursorMut` bumps once per session (on `Drop`, if any commit
//! happened). Stale-handle detection compares against this value to
//! tell whether the address space has been mutated since a captured
//! snapshot.
//!
//! [`FrameAlloc`]: crate::mm::frame::FrameAlloc

use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::arch::x86_64::cr3::{Pcid, write_cr3_pcid};
use crate::mm::frame::{Frame, FrameAllocOptions, Paddr, PageTableMeta};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::page_property::PageProperty;
use crate::mm::page_table::{
    PAGE_SIZE_4KB, PageTableLevel, PteFlags, WalkMode, WalkOutcome, entry_in_table, read_leaf,
    reclaim_table_frame, walk_to_leaf,
};
use crate::mm::tlb;
use crate::mm::uframe::{AnyUFrameMeta, UFrame};

const KERNEL_HALF_START_INDEX: usize = 256;
const KERNEL_HALF_END_INDEX: usize = 512;

/// Per-address-space page-table handle.
///
/// `pml4` is intentionally private. The only mutation path is
/// through [`Self::cursor_mut`].
pub struct VmSpace {
    pml4: Frame<PageTableMeta>,
    pcid: Pcid,
    generation: AtomicU64,
}

// SAFETY: `Frame<PageTableMeta>` is `Send + Sync` (the underlying
// `META_SLOTS` machinery synchronises ref-count manipulation).
// Mutation through `&mut VmSpace` serialises page-table writes via
// the cursor.
unsafe impl Send for VmSpace {}
unsafe impl Sync for VmSpace {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Tried to map an already-present 4 KiB slot. Use `unmap` first.
    Overlap,
    /// Cursor moved past `range.end`.
    OutOfBounds,
    /// FrameAlloc returned `None` for an intermediate page-table frame.
    IntermediateAllocFailed,
    /// FrameAlloc not registered, or kernel-master PML4 not registered.
    Uninitialised,
    /// `range` is not page-aligned.
    UnalignedRange,
    /// Internal: a slot lookup failed for a paddr that should have a
    /// META_SLOTS entry. Indicates a kernel mis-configuration.
    PathCorrupt,
}

/// Read-only walking handle over `[range.start, range.end)`.
pub struct Cursor<'a> {
    space: &'a VmSpace,
    range: Range<VirtAddr>,
    cur: VirtAddr,
}

/// Mutating walking handle. Bumps `space.generation` once on `Drop`
/// if any commit happened.
pub struct CursorMut<'a> {
    space: &'a mut VmSpace,
    range: Range<VirtAddr>,
    cur: VirtAddr,
    dirty: bool,
}

/// Snapshot of a cursor's current entry.
#[derive(Debug, Clone, Copy)]
pub struct CursorEntry {
    pub vaddr: VirtAddr,
    pub paddr: Option<Paddr>,
    pub property: PageProperty,
    pub level: PageTableLevel,
}

// ---------------------------------------------------------------------------
// Kernel-master PML4 registration.
// ---------------------------------------------------------------------------

/// Storage for the kernel-master PML4 paddr. One-shot init via
/// [`register_kernel_master_pml4`].
static KERNEL_MASTER_PML4: AtomicU64 = AtomicU64::new(KERNEL_MASTER_UNINIT);
const KERNEL_MASTER_UNINIT: u64 = u64::MAX;

/// Test-only / boot-only: install the kernel-master PML4 paddr.
///
/// # Safety
///
/// `paddr` must point to a 4 KiB-aligned, valid PML4 reachable
/// through the kernel HHDM. Indices 256..512 of that PML4 must
/// describe the canonical kernel mappings; `VmSpace::new` byte-copies
/// those entries into every fresh address space. The mapping must
/// persist for the static lifetime of the kernel.
pub unsafe fn register_kernel_master_pml4(paddr: PhysAddr) {
    let prev = KERNEL_MASTER_PML4.swap(paddr.as_u64(), Ordering::AcqRel);
    assert_eq!(
        prev, KERNEL_MASTER_UNINIT,
        "register_kernel_master_pml4 called twice"
    );
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_kernel_master_for_test() {
    KERNEL_MASTER_PML4.store(KERNEL_MASTER_UNINIT, Ordering::Release);
}

// ---------------------------------------------------------------------------
// PCID assignment. Monotonic counter, never reused, masked to the
// architectural 12 bits. Suitable until a real ASID slot scheduler
// supersedes it.
// ---------------------------------------------------------------------------

static NEXT_PCID: AtomicU64 = AtomicU64::new(1);

fn alloc_pcid() -> Pcid {
    let raw = NEXT_PCID.fetch_add(1, Ordering::Relaxed);
    Pcid::new((raw & 0x0FFF) as u16)
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_pcid_counter_for_test() {
    NEXT_PCID.store(1, Ordering::Release);
}

// ---------------------------------------------------------------------------
// VmSpace
// ---------------------------------------------------------------------------

impl VmSpace {
    /// Allocate a fresh address space: a zeroed PML4 with kernel-half
    /// mappings copied from the registered kernel master.
    pub fn new() -> Result<Self, MapError> {
        let alloc = current_frame_allocator().ok_or(MapError::Uninitialised)?;
        let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
        if master == KERNEL_MASTER_UNINIT {
            return Err(MapError::Uninitialised);
        }
        let master_phys = PhysAddr::new(master);

        let pml4_phys = alloc
            .alloc(FrameAllocOptions::single().zeroed())
            .ok_or(MapError::IntermediateAllocFailed)?;
        let pml4 = Frame::<PageTableMeta>::from_unused(
            pml4_phys,
            PageTableMeta {
                level: PageTableLevel::Four as u8,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;

        copy_kernel_half(master_phys, pml4_phys);

        Ok(Self {
            pml4,
            pcid: alloc_pcid(),
            generation: AtomicU64::new(0),
        })
    }

    /// Physical address of the PML4 frame.
    pub fn pml4_paddr(&self) -> PhysAddr {
        self.pml4.paddr()
    }

    pub fn pcid(&self) -> Pcid {
        self.pcid
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Read-only cursor over `range`. `range` must be 4 KiB-aligned.
    pub fn cursor(&self, range: Range<VirtAddr>) -> Result<Cursor<'_>, MapError> {
        check_range_alignment(&range)?;
        Ok(Cursor {
            space: self,
            cur: range.start,
            range,
        })
    }

    /// Mutating cursor over `range`. `range` must be 4 KiB-aligned.
    pub fn cursor_mut(&mut self, range: Range<VirtAddr>) -> Result<CursorMut<'_>, MapError> {
        check_range_alignment(&range)?;
        Ok(CursorMut {
            cur: range.start,
            range,
            space: self,
            dirty: false,
        })
    }

    /// Switch the current CPU to this address space. Only sanctioned
    /// CR3 write path.
    ///
    /// # Safety
    ///
    /// See [`crate::arch::x86_64::cr3::write_cr3_pcid`] — kernel-half
    /// invariant.
    pub unsafe fn activate(&self) {
        // SAFETY: `self.pml4` is a live page-table frame whose
        // kernel-half indices 256..512 were copied from the master
        // at construction; the caller upholds the rest of
        // `write_cr3_pcid`'s contract.
        unsafe { write_cr3_pcid(self.pml4_paddr(), self.pcid, true) };
    }
}

fn check_range_alignment(range: &Range<VirtAddr>) -> Result<(), MapError> {
    if range.start.as_u64() & 0xFFF != 0 || range.end.as_u64() & 0xFFF != 0 {
        return Err(MapError::UnalignedRange);
    }
    if range.start.as_u64() > range.end.as_u64() {
        return Err(MapError::UnalignedRange);
    }
    Ok(())
}

fn copy_kernel_half(master_phys: PhysAddr, dest_phys: PhysAddr) {
    for i in KERNEL_HALF_START_INDEX..KERNEL_HALF_END_INDEX {
        let src = entry_in_table(master_phys, i);
        let dst = entry_in_table(dest_phys, i);
        dst.write(src.read());
    }
}

// ---------------------------------------------------------------------------
// Cursor (read-only)
// ---------------------------------------------------------------------------

impl Cursor<'_> {
    /// Snapshot of the current entry. `paddr == None` ⇒ not present.
    pub fn query(&self) -> Result<CursorEntry, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        match walk_to_leaf(self.space.pml4_paddr(), self.cur, false, WalkMode::Query)
            .map_err(map_walk_err)?
        {
            outcome @ WalkOutcome::LeafTable { .. } => match read_leaf(&outcome) {
                Some((paddr, property, level)) => Ok(CursorEntry {
                    vaddr: self.cur,
                    paddr: Some(paddr),
                    property,
                    level,
                }),
                None => Ok(CursorEntry {
                    vaddr: self.cur,
                    paddr: None,
                    property: PageProperty::default(),
                    level: PageTableLevel::One,
                }),
            },
            WalkOutcome::NotPresent => Ok(CursorEntry {
                vaddr: self.cur,
                paddr: None,
                property: PageProperty::default(),
                level: PageTableLevel::One,
            }),
        }
    }

    pub fn vaddr(&self) -> VirtAddr {
        self.cur
    }

    pub fn next(&mut self) -> Result<(), MapError> {
        let next = self
            .cur
            .as_u64()
            .checked_add(PAGE_SIZE_4KB)
            .ok_or(MapError::OutOfBounds)?;
        if next > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = VirtAddr::new(next);
        Ok(())
    }

    pub fn seek(&mut self, vaddr: VirtAddr) -> Result<(), MapError> {
        if vaddr.as_u64() & 0xFFF != 0 {
            return Err(MapError::UnalignedRange);
        }
        if vaddr.as_u64() < self.range.start.as_u64() || vaddr.as_u64() > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = vaddr;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CursorMut
// ---------------------------------------------------------------------------

impl<'a> CursorMut<'a> {
    pub fn vaddr(&self) -> VirtAddr {
        self.cur
    }

    pub fn next(&mut self) -> Result<(), MapError> {
        let next = self
            .cur
            .as_u64()
            .checked_add(PAGE_SIZE_4KB)
            .ok_or(MapError::OutOfBounds)?;
        if next > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = VirtAddr::new(next);
        Ok(())
    }

    pub fn seek(&mut self, vaddr: VirtAddr) -> Result<(), MapError> {
        if vaddr.as_u64() & 0xFFF != 0 {
            return Err(MapError::UnalignedRange);
        }
        if vaddr.as_u64() < self.range.start.as_u64() || vaddr.as_u64() > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = vaddr;
        Ok(())
    }

    /// Snapshot of the current entry — same data as the read-only
    /// cursor's [`Cursor::query`].
    pub fn query(&self) -> Result<CursorEntry, MapError> {
        // Reuse the read-only path by constructing a temporary
        // `Cursor` over the same range.
        let probe = Cursor {
            space: self.space,
            range: self.range.clone(),
            cur: self.cur,
        };
        probe.query()
    }

    /// Map `frame` at the cursor's current vaddr with `prop`. Consumes
    /// `frame`; on success the returned `Ok(())` means the underlying
    /// `Frame<M>` has been leaked into the leaf PTE (its single ref
    /// is now held by the page table). Reverse via [`Self::unmap`].
    pub fn map<M: AnyUFrameMeta>(
        &mut self,
        frame: UFrame<M>,
        prop: PageProperty,
    ) -> Result<(), MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            prop.user,
            WalkMode::Create,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            // Create mode never returns NotPresent — it would have
            // allocated. Treat as corruption.
            return Err(MapError::PathCorrupt);
        };
        if leaf_level != PageTableLevel::One {
            // Hit a huge leaf that wasn't split. With Create mode
            // walk_to_leaf splits on the way down, so reaching here
            // means corruption.
            return Err(MapError::PathCorrupt);
        }

        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if pte.is_present() {
            return Err(MapError::Overlap);
        }

        // Leak the UFrame's ref into the PTE: the frame's ref count
        // stays at 1, conceptually owned by the leaf entry.
        let inner = frame.into_frame();
        let paddr = inner.paddr();
        let _slot = inner.into_raw();

        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        pte.set(paddr, flags);
        self.dirty = true;
        Ok(())
    }

    /// Unmap the current entry. Returns the freed `UFrame` (with
    /// ref count == 1) when one was present.
    pub fn unmap<M: AnyUFrameMeta>(&mut self) -> Result<Option<UFrame<M>>, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        let outcome = walk_to_leaf(self.space.pml4_paddr(), self.cur, false, WalkMode::Mutate)
            .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            return Ok(None);
        };
        if leaf_level != PageTableLevel::One {
            // Don't unmap a huge entry — caller asked for a 4 KiB
            // unmap. Treat as not-present from the caller's view.
            return Ok(None);
        }

        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(None);
        }

        let paddr = pte.address();
        pte.clear();
        tlb::flush_local(self.cur);
        self.dirty = true;

        // Reclaim the leaked UFrame ref.
        // SAFETY: at `map` time we leaked exactly one ref to this
        // slot via `Frame::into_raw`; clearing the PTE here removes
        // the only path that held that ref. `from_raw_at` re-wraps
        // without bumping the count, so accounting is exact.
        let frame: Frame<M> =
            unsafe { Frame::<M>::from_raw_at(paddr).map_err(|_| MapError::PathCorrupt)? };
        Ok(Some(UFrame::<M>::from_frame(frame)))
    }

    /// Update the access/cache properties of the current entry
    /// without remapping. No-op when no mapping is present.
    pub fn protect(&mut self, prop: PageProperty) -> Result<(), MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            prop.user,
            WalkMode::Mutate,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            return Ok(());
        };
        if leaf_level != PageTableLevel::One {
            return Ok(());
        }
        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(());
        }
        // Carry forward the address; refresh access flags.
        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        pte.set_flags_only(flags);
        tlb::flush_local(self.cur);
        self.dirty = true;
        Ok(())
    }
}

impl Drop for CursorMut<'_> {
    fn drop(&mut self) {
        if self.dirty {
            self.space.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn map_walk_err(e: crate::mm::page_table::WalkError) -> MapError {
    use crate::mm::page_table::WalkError;
    match e {
        WalkError::AllocUninitialised => MapError::Uninitialised,
        WalkError::AllocFailed => MapError::IntermediateAllocFailed,
        WalkError::PathCorrupt => MapError::PathCorrupt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_error_eq() {
        assert_eq!(MapError::Overlap, MapError::Overlap);
        assert_ne!(MapError::Overlap, MapError::OutOfBounds);
    }

    #[test]
    fn vm_space_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<VmSpace>();
        assert_sync::<VmSpace>();
    }

    #[test]
    fn check_range_alignment_rejects_unaligned() {
        let r = VirtAddr::new(0x1000)..VirtAddr::new(0x2001);
        assert_eq!(check_range_alignment(&r), Err(MapError::UnalignedRange));
    }

    #[test]
    fn check_range_alignment_rejects_inverted() {
        let r = VirtAddr::new(0x2000)..VirtAddr::new(0x1000);
        assert_eq!(check_range_alignment(&r), Err(MapError::UnalignedRange));
    }

    #[test]
    fn check_range_alignment_accepts_empty_aligned() {
        let r = VirtAddr::new(0x1000)..VirtAddr::new(0x1000);
        assert!(check_range_alignment(&r).is_ok());
    }
}

// `reclaim_table_frame` is plumbed through here so a future
// garbage-collect-empty-intermediate-tables pass can call it. Not
// exercised by the current cursor — empty intermediates linger until
// the address space is dropped.
#[allow(dead_code)]
fn _force_link_reclaim(p: Paddr) {
    // SAFETY: never called.
    unsafe { reclaim_table_frame(p) };
}
