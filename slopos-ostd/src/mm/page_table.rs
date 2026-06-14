//! Page-table walk primitives. **All `unsafe` for PTE writes lives
//! in this module.**
//!
//! The on-disk PTE bit layout, the four-level walker, and the
//! intermediate-frame allocator glue all sit here so [`super::vm_space`]
//! can stay free of raw pointers — `VmSpace`, `Cursor`, and `CursorMut`
//! talk to PTEs only through the safe helpers below.
//!
//! # Soundness
//!
//! Page-table frames are sensitive: they are always typed
//! `Frame<PageTableMeta>` and never reachable as `UFrame`. Volatile
//! reads/writes go through the `Pte` wrapper so the compiler cannot
//! reorder PTE accesses against surrounding atomics. The
//! intermediate-allocator hand-off via [`Frame::into_raw`] /
//! [`Frame::from_raw`] keeps the slot's ref count exact (one ref
//! "owned by" the parent PTE), so no double-free or leak is possible
//! across map / unmap.

use core::sync::atomic::Ordering;

use bitflags::bitflags;
use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::mm::frame::{Frame, FrameAllocOptions, MetaSlot, Paddr, PageTableMeta, meta_slot_for};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::page_property::PageProperty;
use crate::mm::phys;

pub const PAGE_SIZE_4KB: u64 = 0x1000;
pub const PAGE_SIZE_2MB: u64 = 0x20_0000;
pub const PAGE_SIZE_1GB: u64 = 0x4000_0000;
pub const PAGE_TABLE_ENTRIES: usize = 512;

// ---------------------------------------------------------------------------
// PageTableLevel
// ---------------------------------------------------------------------------

/// Level of an x86_64 page-table entry. `Four` = top (PML4),
/// `One` = leaf 4 KiB PT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PageTableLevel {
    Four = 4,
    Three = 3,
    Two = 2,
    One = 1,
}

impl PageTableLevel {
    #[inline]
    pub const fn next_lower(self) -> Option<Self> {
        match self {
            Self::Four => Some(Self::Three),
            Self::Three => Some(Self::Two),
            Self::Two => Some(Self::One),
            Self::One => None,
        }
    }

    #[inline]
    pub const fn page_size(self) -> Option<u64> {
        match self {
            Self::Three => Some(PAGE_SIZE_1GB),
            Self::Two => Some(PAGE_SIZE_2MB),
            Self::One => Some(PAGE_SIZE_4KB),
            Self::Four => None,
        }
    }

    #[inline]
    pub const fn supports_huge_pages(self) -> bool {
        matches!(self, Self::Three | Self::Two)
    }

    #[inline]
    pub const fn index_of(self, vaddr: VirtAddr) -> usize {
        let shift = 12 + ((self as u8 - 1) * 9);
        ((vaddr.as_u64() >> shift) & 0x1FF) as usize
    }

    #[inline]
    pub const fn entry_size(self) -> u64 {
        1u64 << (12 + ((self as u8 - 1) * 9))
    }

    #[inline]
    pub const fn align_mask(self) -> u64 {
        !(self.entry_size() - 1)
    }

    #[inline]
    pub const fn offset_mask(self) -> u64 {
        self.entry_size() - 1
    }
}

// ---------------------------------------------------------------------------
// PteFlags — leaf + intermediate PTE bit layout.
// ---------------------------------------------------------------------------

bitflags! {
    /// On-disk x86_64 PTE bit pattern.
    ///
    /// `AVL_*` cover bits 9..=11 — the architectural "available to OS"
    /// field. OSTD declares them so [`Pte::flags`]'s `from_bits_truncate`
    /// preserves them on round-trip, but assigns no semantics; consumers
    /// (slopos-mm, etc.) attach meaning via
    /// [`super::page_property::PageProperty::software`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PteFlags: u64 {
        const PRESENT       = 1 << 0;
        const WRITABLE      = 1 << 1;
        const USER          = 1 << 2;
        const WRITE_THROUGH = 1 << 3;
        const CACHE_DISABLE = 1 << 4;
        const ACCESSED      = 1 << 5;
        const DIRTY         = 1 << 6;
        const HUGE          = 1 << 7;
        const GLOBAL        = 1 << 8;
        const AVL_9         = 1 << 9;
        const AVL_10        = 1 << 10;
        const AVL_11        = 1 << 11;
        const NO_EXECUTE    = 1 << 63;
    }
}

impl PteFlags {
    /// Address mask. Bits 12..=51 hold the 4 KiB-aligned physical
    /// frame address; the rest are flag bits.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    /// Mask for PTE bits 9..=11 — the AVL ("available to OS") field
    /// that x86_64 reserves for software use. OSTD does not assign
    /// semantics to these bits; consumers (slopos-mm) define their
    /// own meanings (e.g. a copy-on-write marker) and route them
    /// through [`super::page_property::PageProperty::software`].
    pub const SOFTWARE_BITS_MASK: u64 = 0x0E00;
    /// Right-shift amount that moves the AVL bits down to the low
    /// three bits of a `u8`.
    pub const SOFTWARE_BITS_SHIFT: u32 = 9;
}

// ---------------------------------------------------------------------------
// Pte — a single 8-byte entry. Reads via `read_volatile`, writes via
// `write_volatile` so the compiler cannot reorder the access against
// surrounding atomics.
// ---------------------------------------------------------------------------

/// Wrapper over a single PTE slot. Holds a raw `*mut u64` and goes
/// through volatile reads/writes. Crate-private to keep pointers out
/// of the safe API.
#[derive(Clone, Copy)]
pub(crate) struct Pte {
    raw: *mut u64,
}

// SAFETY: a `Pte` is a thin pointer to a single 8-byte slot inside a
// page-table frame this OSTD invocation owns. Sharing/sending across
// threads is sound because reads/writes go through volatile ops; the
// surrounding `CursorMut` discipline (one `&mut VmSpace` at a time)
// serialises mutation.
unsafe impl Send for Pte {}
unsafe impl Sync for Pte {}

impl Pte {
    #[inline]
    pub(crate) fn read(self) -> u64 {
        // SAFETY: `self.raw` was constructed from a live page-table
        // frame's HHDM mapping (see `entry_in_table`); the slot is
        // inside the page and properly aligned. Volatile read so the
        // compiler does not assume the stored value is invariant.
        unsafe { core::ptr::read_volatile(self.raw) }
    }

    #[inline]
    pub(crate) fn write(self, value: u64) {
        // SAFETY: as `read`. Volatile write preserves ordering against
        // surrounding atomic ops in the cursor.
        unsafe { core::ptr::write_volatile(self.raw, value) }
    }

    #[inline]
    pub(crate) fn is_present(self) -> bool {
        self.read() & PteFlags::PRESENT.bits() != 0
    }

    #[inline]
    pub(crate) fn is_huge(self) -> bool {
        self.read() & PteFlags::HUGE.bits() != 0
    }

    #[inline]
    pub(crate) fn address(self) -> PhysAddr {
        PhysAddr::new(self.read() & PteFlags::ADDRESS_MASK)
    }

    #[inline]
    pub(crate) fn flags(self) -> PteFlags {
        PteFlags::from_bits_truncate(self.read())
    }

    #[inline]
    pub(crate) fn set(self, addr: PhysAddr, flags: PteFlags) {
        self.write((addr.as_u64() & PteFlags::ADDRESS_MASK) | flags.bits());
    }

    #[inline]
    pub(crate) fn set_flags_only(self, flags: PteFlags) {
        let cur = self.read();
        self.write((cur & PteFlags::ADDRESS_MASK) | flags.bits());
    }

    #[inline]
    pub(crate) fn clear(self) {
        self.write(0);
    }
}

/// Yield the PTE at `index` inside the page-table frame at `table_phys`.
/// Internal: callers must guarantee that `table_phys` is a live
/// `Frame<PageTableMeta>` for the duration of the returned `Pte`.
#[inline]
pub(crate) fn entry_in_table(table_phys: Paddr, index: usize) -> Pte {
    debug_assert!(index < PAGE_TABLE_ENTRIES);
    // SAFETY: `phys::phys_to_virt` returns the kernel HHDM mapping of
    // `table_phys`; that mapping is read+write for any frame the
    // kernel owns. The byte offset stays inside the 4 KiB frame
    // (`index < 512` ⇒ offset ≤ 4088). The pointer's alignment is
    // 8-byte because it's `8 * index` from a 4 KiB-aligned base.
    unsafe {
        let base = phys::phys_to_virt(table_phys) as *mut u64;
        Pte {
            raw: base.add(index),
        }
    }
}

// ---------------------------------------------------------------------------
// Walk + huge-page split.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// `current_frame_allocator()` is `None` (FrameAlloc not registered).
    AllocUninitialised,
    /// FrameAlloc returned `None` for an intermediate page-table frame.
    AllocFailed,
    /// A PML4 entry on the path is marked HUGE — architecturally
    /// invalid (top-level entries are never leaves on x86_64).
    PathCorrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkMode {
    /// Read-only walk. Stop on the first missing intermediate and
    /// return [`WalkOutcome::NotPresent`]. Never allocates, never
    /// splits huge pages.
    Query,
    /// Mutating walk that creates missing intermediates and splits
    /// huge pages so the leaf is a real 4 KiB PT entry.
    Create,
    /// Mutating walk that does **not** create intermediates. Used by
    /// `unmap` / `protect`: if the path is incomplete there's nothing
    /// to do, so return [`WalkOutcome::NotPresent`].
    Mutate,
}

#[derive(Debug)]
pub(crate) enum WalkOutcome {
    LeafTable {
        leaf_table_phys: Paddr,
        leaf_index: usize,
        leaf_level: PageTableLevel,
    },
    /// Walk halted because an intermediate entry was not present.
    /// `stopped_at` is the level whose entry was missing — e.g.
    /// `Four` ⇒ the PML4 entry covering this vaddr is empty (the
    /// next 512 GiB of address space is unmapped). Consumers (in
    /// particular [`super::vm_space::Cursor::query`]) use this to
    /// skip empty subtrees in O(1) instead of advancing one 4 KiB
    /// page at a time.
    NotPresent { stopped_at: PageTableLevel },
}

/// Walk down from `pml4_phys` toward the entry containing `vaddr`,
/// honouring `mode`. The walk stops at the table whose entries cover
/// `target_level`-sized regions (`Three` for 1 GiB, `Two` for 2 MiB,
/// `One` for 4 KiB). In `Create` mode, intermediates are allocated
/// down to the table holding `target_level` entries; existing huge
/// entries that block reaching a deeper target are split. In `Query`
/// / `Mutate` mode the walk returns whichever leaf is found first
/// (huge or 4 KiB) without splitting — callers compare
/// `outcome.leaf_level` against their `S::LEVEL` razor.
///
/// See [`WalkMode`] / [`WalkOutcome`].
pub(crate) fn walk_to_leaf(
    pml4_phys: Paddr,
    vaddr: VirtAddr,
    user_mapping: bool,
    mode: WalkMode,
    target_level: PageTableLevel,
) -> Result<WalkOutcome, WalkError> {
    debug_assert!(
        target_level != PageTableLevel::Four,
        "walk_to_leaf target_level cannot be Four — PML4 entries are never leaves"
    );

    let pml4_idx = PageTableLevel::Four.index_of(vaddr);
    let pdpt_idx = PageTableLevel::Three.index_of(vaddr);
    let pd_idx = PageTableLevel::Two.index_of(vaddr);

    let pml4_e = entry_in_table(pml4_phys, pml4_idx);
    let pdpt_phys = match step_down(pml4_e, PageTableLevel::Four, user_mapping, mode)? {
        StepOutcome::Phys(p) => p,
        StepOutcome::NotPresent => {
            return Ok(WalkOutcome::NotPresent {
                stopped_at: PageTableLevel::Four,
            });
        }
    };

    // Target is 1 GiB (Level::Three): caller wants the PDPT entry as
    // the leaf. Stop here — no descent into PD.
    if target_level == PageTableLevel::Three {
        return Ok(WalkOutcome::LeafTable {
            leaf_table_phys: pdpt_phys,
            leaf_index: pdpt_idx,
            leaf_level: PageTableLevel::Three,
        });
    }

    let pdpt_e = entry_in_table(pdpt_phys, pdpt_idx);
    if pdpt_e.is_present() && pdpt_e.is_huge() {
        match mode {
            WalkMode::Query | WalkMode::Mutate => {
                return Ok(WalkOutcome::LeafTable {
                    leaf_table_phys: pdpt_phys,
                    leaf_index: pdpt_idx,
                    leaf_level: PageTableLevel::Three,
                });
            }
            WalkMode::Create => {
                split_pdpt_huge(pdpt_e)?;
            }
        }
    }
    let pd_phys = match step_down(pdpt_e, PageTableLevel::Three, user_mapping, mode)? {
        StepOutcome::Phys(p) => p,
        StepOutcome::NotPresent => {
            return Ok(WalkOutcome::NotPresent {
                stopped_at: PageTableLevel::Three,
            });
        }
    };

    // Target is 2 MiB (Level::Two): caller wants the PD entry as the
    // leaf. Stop here — no descent into PT.
    if target_level == PageTableLevel::Two {
        return Ok(WalkOutcome::LeafTable {
            leaf_table_phys: pd_phys,
            leaf_index: pd_idx,
            leaf_level: PageTableLevel::Two,
        });
    }

    let pd_e = entry_in_table(pd_phys, pd_idx);
    if pd_e.is_present() && pd_e.is_huge() {
        match mode {
            WalkMode::Query | WalkMode::Mutate => {
                return Ok(WalkOutcome::LeafTable {
                    leaf_table_phys: pd_phys,
                    leaf_index: pd_idx,
                    leaf_level: PageTableLevel::Two,
                });
            }
            WalkMode::Create => {
                split_pd_huge(pd_e)?;
            }
        }
    }
    let pt_phys = match step_down(pd_e, PageTableLevel::Two, user_mapping, mode)? {
        StepOutcome::Phys(p) => p,
        StepOutcome::NotPresent => {
            return Ok(WalkOutcome::NotPresent {
                stopped_at: PageTableLevel::Two,
            });
        }
    };

    // Target is 4 KiB (Level::One): default deepest path.
    Ok(WalkOutcome::LeafTable {
        leaf_table_phys: pt_phys,
        leaf_index: PageTableLevel::One.index_of(vaddr),
        leaf_level: PageTableLevel::One,
    })
}

enum StepOutcome {
    Phys(Paddr),
    NotPresent,
}

fn step_down(
    parent: Pte,
    parent_level: PageTableLevel,
    user_mapping: bool,
    mode: WalkMode,
) -> Result<StepOutcome, WalkError> {
    if parent.is_present() {
        if parent_level == PageTableLevel::Four && parent.is_huge() {
            crate::klog_warn!(
                "page_table: PML4 entry marked HUGE (architecturally impossible) \
                 addr=0x{:x} raw=0x{:x} -> PathCorrupt",
                parent.address().as_u64(),
                parent.read(),
            );
            return Err(WalkError::PathCorrupt);
        }
        if mode == WalkMode::Create && user_mapping && !parent.flags().contains(PteFlags::USER) {
            // Promote the intermediate to USER so the leaf is
            // reachable from ring 3. Mirrors
            // `mm/src/paging/tables.rs`'s `add_flags(USER)` discipline.
            let bits = parent.read() | PteFlags::USER.bits();
            parent.write(bits);
        }
        return Ok(StepOutcome::Phys(parent.address()));
    }

    match mode {
        WalkMode::Query | WalkMode::Mutate => return Ok(StepOutcome::NotPresent),
        WalkMode::Create => {}
    }

    // Create: allocate a fresh, zeroed intermediate.
    let alloc = current_frame_allocator().ok_or(WalkError::AllocUninitialised)?;
    let new_phys = match alloc.alloc(FrameAllocOptions::single().zeroed()) {
        Some(p) => p,
        None => {
            crate::klog_warn!(
                "page_table: intermediate alloc returned NULL (parent_level={:?}) -> AllocFailed",
                parent_level,
            );
            return Err(WalkError::AllocFailed);
        }
    };

    let level = parent_level
        .next_lower()
        .expect("step_down called with leaf parent level");
    let frame = match Frame::<PageTableMeta>::from_unused(
        new_phys,
        PageTableMeta {
            level: level as u8,
            static_borrowed: false,
        },
    ) {
        Ok(f) => f,
        Err(e) => {
            // from_unused fails when `new_phys`'s MetaSlot is not claimable —
            // a page on the buddy free list whose slot is still typed.
            // `slot_kind` names the subsystem that still owns it.
            let snap = crate::mm::frame::slot_snapshot(new_phys);
            crate::klog_warn!(
                "page_table: intermediate from_unused FAILED phys=0x{:x} parent_level={:?} \
                 frame_err={:?} slot_kind={:?} raw_rc=0x{:x} vtable=0x{:x} -> PathCorrupt \
                 (buddy handed a paddr whose MetaSlot is not UNUSED)",
                new_phys.as_u64(),
                parent_level,
                e,
                snap.kind,
                snap.raw_ref_count,
                snap.vtable_addr,
            );
            // Leak `new_phys` rather than return it: if its slot is still
            // owned, that owner's Drop frees the page, so deallocating here
            // would double-free.
            return Err(WalkError::PathCorrupt);
        }
    };
    // Leak the typed handle into the parent PTE — the page-table
    // frame is now "owned by" the parent slot. Reclaimed on unmap
    // via `reclaim_table_frame`.
    let _slot_ptr = frame.into_raw();

    let mut intermediate = PteFlags::PRESENT | PteFlags::WRITABLE;
    if user_mapping {
        intermediate |= PteFlags::USER;
    }
    parent.set(new_phys, intermediate);
    Ok(StepOutcome::Phys(new_phys))
}

fn split_pdpt_huge(pdpt_entry: Pte) -> Result<(), WalkError> {
    debug_assert!(pdpt_entry.is_present() && pdpt_entry.is_huge());
    let alloc = current_frame_allocator().ok_or(WalkError::AllocUninitialised)?;
    let pd_phys = alloc
        .alloc(FrameAllocOptions::single().zeroed())
        .ok_or(WalkError::AllocFailed)?;

    let huge_phys = pdpt_entry.address();
    let huge_flags = pdpt_entry.flags();

    for i in 0..PAGE_TABLE_ENTRIES {
        let child_phys = PhysAddr::new(huge_phys.as_u64() + (i as u64) * PAGE_SIZE_2MB);
        let child = entry_in_table(pd_phys, i);
        child.set(child_phys, huge_flags | PteFlags::HUGE);
    }

    let frame = Frame::<PageTableMeta>::from_unused(
        pd_phys,
        PageTableMeta {
            level: PageTableLevel::Two as u8,
            static_borrowed: false,
        },
    )
    .map_err(|e| {
        let snap = crate::mm::frame::slot_snapshot(pd_phys);
        crate::klog_warn!(
            "page_table: split_pdpt_huge from_unused FAILED phys=0x{:x} frame_err={:?} \
             slot_kind={:?} raw_rc=0x{:x} -> PathCorrupt",
            pd_phys.as_u64(),
            e,
            snap.kind,
            snap.raw_ref_count,
        );
        WalkError::PathCorrupt
    })?;
    let _ = frame.into_raw();

    pdpt_entry.set(pd_phys, table_flags_from_leaf(huge_flags));
    Ok(())
}

fn split_pd_huge(pd_entry: Pte) -> Result<(), WalkError> {
    debug_assert!(pd_entry.is_present() && pd_entry.is_huge());
    let alloc = current_frame_allocator().ok_or(WalkError::AllocUninitialised)?;
    let pt_phys = alloc
        .alloc(FrameAllocOptions::single().zeroed())
        .ok_or(WalkError::AllocFailed)?;

    let huge_phys = pd_entry.address();
    let mut huge_flags = pd_entry.flags();
    huge_flags.remove(PteFlags::HUGE);

    for i in 0..PAGE_TABLE_ENTRIES {
        let child_phys = PhysAddr::new(huge_phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
        let child = entry_in_table(pt_phys, i);
        child.set(child_phys, huge_flags);
    }

    let frame = Frame::<PageTableMeta>::from_unused(
        pt_phys,
        PageTableMeta {
            level: PageTableLevel::One as u8,
            static_borrowed: false,
        },
    )
    .map_err(|e| {
        let snap = crate::mm::frame::slot_snapshot(pt_phys);
        crate::klog_warn!(
            "page_table: split_pd_huge from_unused FAILED phys=0x{:x} frame_err={:?} \
             slot_kind={:?} raw_rc=0x{:x} -> PathCorrupt",
            pt_phys.as_u64(),
            e,
            snap.kind,
            snap.raw_ref_count,
        );
        WalkError::PathCorrupt
    })?;
    let _ = frame.into_raw();

    pd_entry.set(pt_phys, table_flags_from_leaf(huge_flags));
    Ok(())
}

fn table_flags_from_leaf(leaf_flags: PteFlags) -> PteFlags {
    let mut flags = PteFlags::PRESENT;
    if leaf_flags.contains(PteFlags::WRITABLE) {
        flags |= PteFlags::WRITABLE;
    }
    if leaf_flags.contains(PteFlags::USER) {
        flags |= PteFlags::USER;
    }
    flags
}

// ---------------------------------------------------------------------------
// Reclaim + leaf-property helpers.
// ---------------------------------------------------------------------------

/// Reclaim a leaked `Frame<_>` referenced by `phys`, regardless of
/// the meta type that was originally installed. The slot's stored
/// vtable carries the type-correct Drop dispatch — the `M` parameter
/// of `Frame::from_raw` is just `PhantomData` and is never read at
/// Drop time. Used both:
///
/// 1. During [`VmSpace::Drop`]'s user-half tree walk, where every
///    intermediate page-table frame and every leaf user frame must
///    be returned to the allocator.
/// 2. By a future garbage-collect-empty-intermediate-tables pass on
///    a live cursor.
///
/// # Safety
///
/// Caller asserts that exactly one ref to the slot was previously
/// leaked into a PTE via `Frame::into_raw`, that PTE has been (or
/// will be, atomically with this call) cleared, and no other
/// reference to the slot is outstanding.
pub(crate) unsafe fn reclaim_leaked_frame(phys: Paddr) {
    let Some(slot_ptr) = meta_slot_for_paddr_ptr(phys) else {
        return;
    };
    // SAFETY: caller's contract — the slot has one outstanding ref
    // owned by the (now-cleared) parent PTE. The `PageTableMeta`
    // type parameter on `from_raw` is `PhantomData` only — at Drop
    // time the slot's stored vtable performs the correct
    // `drop_in_place` and `returns_frame` dispatch for whatever `M`
    // was originally installed.
    let frame: Frame<PageTableMeta> = unsafe { Frame::from_raw(slot_ptr) };
    drop(frame);
}

/// Decode a leaf [`WalkOutcome`] into `(paddr, property, level)`.
/// Returns `None` if the walk did not reach a present leaf.
pub(crate) fn read_leaf(outcome: &WalkOutcome) -> Option<(Paddr, PageProperty, PageTableLevel)> {
    let WalkOutcome::LeafTable {
        leaf_table_phys,
        leaf_index,
        leaf_level,
    } = *outcome
    else {
        return None;
    };
    let pte = entry_in_table(leaf_table_phys, leaf_index);
    if !pte.is_present() {
        return None;
    }
    let prop = PageProperty::from_leaf_flags(pte.flags());
    Some((pte.address(), prop, leaf_level))
}

// ---------------------------------------------------------------------------
// META_SLOTS lookup helper. We need the raw `*const MetaSlot` so we
// can call `Frame::from_raw` (which takes a slot pointer, not a
// paddr). The crate-private `meta_slot_for` returns `&'static MetaSlot`;
// we cast to a `*const MetaSlot` here.
// ---------------------------------------------------------------------------

#[inline]
fn meta_slot_for_paddr_ptr(paddr: Paddr) -> Option<*const MetaSlot> {
    meta_slot_for(paddr).map(|s| s as *const MetaSlot)
}

// Reference imports the linter would otherwise flag.
const _: Ordering = Ordering::Acquire;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_table_level_index() {
        let v = VirtAddr::new(0x0000_5566_7788_9000);
        assert_eq!(PageTableLevel::One.index_of(v), 0x089);
        assert_eq!(PageTableLevel::Two.index_of(v), 0x1BC);
        assert_eq!(PageTableLevel::Three.index_of(v), 0x199);
        assert_eq!(PageTableLevel::Four.index_of(v), 0x0AA);
    }

    #[test]
    fn page_table_level_index_aligned() {
        // 0x4000_0000 — single bit at level-3 boundary. Tests shift
        // arithmetic without cross-digit interactions.
        let v = VirtAddr::new(0x4000_0000);
        assert_eq!(PageTableLevel::One.index_of(v), 0);
        assert_eq!(PageTableLevel::Two.index_of(v), 0);
        assert_eq!(PageTableLevel::Three.index_of(v), 1);
    }

    #[test]
    fn page_table_level_entry_size() {
        assert_eq!(PageTableLevel::One.entry_size(), 0x1000);
        assert_eq!(PageTableLevel::Two.entry_size(), 0x20_0000);
        assert_eq!(PageTableLevel::Three.entry_size(), 0x4000_0000);
    }

    #[test]
    fn page_table_level_align_mask() {
        assert_eq!(PageTableLevel::One.align_mask(), !0xfff_u64);
        assert_eq!(PageTableLevel::One.offset_mask(), 0xfff);
    }

    #[test]
    fn pte_flags_bits() {
        assert_eq!(PteFlags::PRESENT.bits(), 0x001);
        assert_eq!(PteFlags::WRITABLE.bits(), 0x002);
        assert_eq!(PteFlags::USER.bits(), 0x004);
        assert_eq!(PteFlags::HUGE.bits(), 0x080);
        assert_eq!(PteFlags::GLOBAL.bits(), 0x100);
        assert_eq!(PteFlags::NO_EXECUTE.bits(), 1u64 << 63);
    }

    #[test]
    fn pte_flags_address_mask() {
        let raw = 0x0000_1234_5678_9007u64;
        assert_eq!(raw & PteFlags::ADDRESS_MASK, 0x0000_1234_5678_9000);
    }

    /// Compile-time pin of every architectural PTE bit value. This
    /// catches accidental drift in OSTD's bitflags definition — for
    /// example, a refactor that swaps the `WRITABLE` and `USER` bits
    /// would break boot but the kernel would already be running on
    /// the wrong values by then. Pinning to hex literals here makes
    /// the bit-layout commitment explicit at the OSTD level.
    #[test]
    fn pte_flags_pinned_to_x86_64_arch() {
        assert_eq!(PteFlags::PRESENT.bits(), 1u64 << 0);
        assert_eq!(PteFlags::WRITABLE.bits(), 1u64 << 1);
        assert_eq!(PteFlags::USER.bits(), 1u64 << 2);
        assert_eq!(PteFlags::WRITE_THROUGH.bits(), 1u64 << 3);
        assert_eq!(PteFlags::CACHE_DISABLE.bits(), 1u64 << 4);
        assert_eq!(PteFlags::ACCESSED.bits(), 1u64 << 5);
        assert_eq!(PteFlags::DIRTY.bits(), 1u64 << 6);
        assert_eq!(PteFlags::HUGE.bits(), 1u64 << 7);
        assert_eq!(PteFlags::GLOBAL.bits(), 1u64 << 8);
        assert_eq!(PteFlags::AVL_9.bits(), 1u64 << 9);
        assert_eq!(PteFlags::AVL_10.bits(), 1u64 << 10);
        assert_eq!(PteFlags::AVL_11.bits(), 1u64 << 11);
        assert_eq!(PteFlags::NO_EXECUTE.bits(), 1u64 << 63);
        assert_eq!(PteFlags::ADDRESS_MASK, 0x000F_FFFF_FFFF_F000);
        assert_eq!(PteFlags::SOFTWARE_BITS_MASK, 0x0E00);
        assert_eq!(PteFlags::SOFTWARE_BITS_SHIFT, 9);
    }
}

// Compile-time razors. These survive even if every external consumer
// is deleted — they protect OSTD's PTE-bit invariants forever.
const _: () = {
    assert!(PteFlags::PRESENT.bits() == 1 << 0);
    assert!(PteFlags::WRITABLE.bits() == 1 << 1);
    assert!(PteFlags::USER.bits() == 1 << 2);
    assert!(PteFlags::WRITE_THROUGH.bits() == 1 << 3);
    assert!(PteFlags::CACHE_DISABLE.bits() == 1 << 4);
    assert!(PteFlags::ACCESSED.bits() == 1 << 5);
    assert!(PteFlags::DIRTY.bits() == 1 << 6);
    assert!(PteFlags::HUGE.bits() == 1 << 7);
    assert!(PteFlags::GLOBAL.bits() == 1 << 8);
    assert!(PteFlags::AVL_9.bits() == 1 << 9);
    assert!(PteFlags::AVL_10.bits() == 1 << 10);
    assert!(PteFlags::AVL_11.bits() == 1 << 11);
    assert!(PteFlags::NO_EXECUTE.bits() == 1u64 << 63);
    assert!(PteFlags::ADDRESS_MASK == 0x000F_FFFF_FFFF_F000);
    assert!(PteFlags::SOFTWARE_BITS_MASK == 0x0E00);
};
