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
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::arch::x86_64::cr3::{Pcid, write_cr3_pcid};
use crate::mm::frame::{Frame, FrameAllocOptions, Paddr, PageTableMeta};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::page_property::PageProperty;
use crate::mm::page_size::PageSize;
use crate::mm::page_table::{
    PAGE_SIZE_4KB, PageTableLevel, PteFlags, WalkMode, WalkOutcome, entry_in_table, read_leaf,
    reclaim_leaked_frame, walk_to_leaf,
};
use crate::mm::tlb;
use crate::mm::uframe::{AnyUFrameMeta, UFrame};

const KERNEL_HALF_START_INDEX: usize = 256;
const KERNEL_HALF_END_INDEX: usize = 512;

/// Per-address-space page-table handle.
///
/// `pml4` is intentionally private. The only mutation path is
/// through [`Self::cursor_mut`].
///
/// `kernel_gen` is the [`KERNEL_MASTER_GEN`] value this VmSpace's
/// kernel-half (PML4 indices 256..512) was last synced against.
/// `activate` calls [`resync_kernel_half_if_stale`] before installing
/// CR3, so any post-construction kernel-master mutation propagates
/// to every running address space at next context switch.
pub struct VmSpace {
    pml4: Frame<PageTableMeta>,
    pcid: Pcid,
    generation: AtomicU64,
    kernel_gen: AtomicU64,
    /// Opaque handle the consumer (slopos-mm) attaches via
    /// [`Self::set_mm_ctx_handle`]; threaded through
    /// [`CursorUnmapHook`] callbacks so the consumer can route TLB /
    /// LUF state per-process. `0` ⇒ unset.
    mm_ctx_handle: AtomicU64,
}

// SAFETY: `Frame<PageTableMeta>` is `Send + Sync` (the underlying
// `META_SLOTS` machinery synchronises ref-count manipulation).
// Mutation through `&mut VmSpace` serialises page-table writes via
// the cursor.
unsafe impl Send for VmSpace {}
unsafe impl Sync for VmSpace {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Tried to map an already-present slot. Use `unmap` first.
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
    /// Cursor's current vaddr is not aligned to `S::BYTES` for the
    /// requested huge-page operation.
    UnalignedCursor,
    /// Frame's physical address is not aligned to `S::BYTES` for the
    /// requested huge-page operation.
    UnalignedFrame,
    /// `unmap::<S>` / `protect::<S>` invoked on a leaf whose actual
    /// page size differs from `S` (e.g. `unmap::<Size4Kb>` on a
    /// 2 MiB leaf, or `unmap::<Size2Mb>` on a 4 KiB leaf).
    SizeMismatch,
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

// ---------------------------------------------------------------------------
// Kernel-master generation counter.
// ---------------------------------------------------------------------------

/// Monotonic counter bumped whenever the kernel master PML4 has its
/// kernel-half mappings (indices 256..512) mutated. Per-VmSpace
/// `kernel_gen` is compared against this value at activate time, and
/// the kernel half is resynced lazily when it lags.
///
/// Starts at 0; the first user-VmSpace's `kernel_gen` is initialised
/// to whatever value is current at construction. A boot-time bump
/// before any VmSpace exists is therefore a no-op (intended).
static KERNEL_MASTER_GEN: AtomicU64 = AtomicU64::new(0);

/// Bump the kernel-master generation. Call from slopos-mm whenever
/// the kernel master PML4's indices 256..512 are mutated (e.g. a new
/// permanent kernel mapping is installed). Each running `VmSpace`
/// observes the bump on its next [`VmSpace::activate`] call and
/// re-copies its own kernel half from the master.
pub fn bump_kernel_master_gen() {
    KERNEL_MASTER_GEN.fetch_add(1, Ordering::AcqRel);
}

/// Current kernel-master generation. Mostly diagnostic; the lazy
/// resync inside `activate` is the production consumer.
pub fn kernel_master_gen() -> u64 {
    KERNEL_MASTER_GEN.load(Ordering::Acquire)
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_kernel_master_gen_for_test() {
    KERNEL_MASTER_GEN.store(0, Ordering::Release);
}

// ---------------------------------------------------------------------------
// CursorUnmapHook — slopos-mm bridges TLB-shootdown / LUF policy here.
// ---------------------------------------------------------------------------

/// Trait the consumer-side TLB / Lazy-User-Flush coordinator implements.
/// `mm_ctx_handle` is an opaque `u64` whose meaning is defined entirely
/// by the consumer — OSTD only stashes it on each `VmSpace` (via
/// [`VmSpace::set_mm_ctx_handle`]) and threads it through these
/// callbacks.
pub trait CursorUnmapHook: Send + Sync {
    /// Fired at the end of [`CursorMut::unmap`] for entries that had
    /// the `USER` bit set. Consumer typically queues a deferred TLB
    /// shootdown (e.g. via slopos-mm's LUF queue) here.
    fn after_unmap(&self, vaddr: VirtAddr, paddr: PhysAddr, mm_ctx_handle: u64);

    /// Fired at the start of [`VmSpace::activate`]. Consumer typically
    /// stores the new active context for per-CPU LUF state.
    fn on_activate(&self, mm_ctx_handle: u64);
}

static CURSOR_UNMAP_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// One-shot registration of the consumer-side cursor-unmap hook.
/// Same double-reference pattern as
/// [`crate::mm::frame_alloc::register_frame_allocator`].
///
/// # Safety
///
/// `slot` must outlive the kernel; the underlying `dyn CursorUnmapHook`
/// must be sound for concurrent calls from any CPU.
pub unsafe fn register_cursor_unmap_hook(slot: &'static &'static dyn CursorUnmapHook) {
    let raw = slot as *const &'static dyn CursorUnmapHook as *mut ();
    let prev = CURSOR_UNMAP_HOOK.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::vm_space::register_cursor_unmap_hook called twice"
    );
}

#[inline]
fn current_cursor_unmap_hook() -> Option<&'static dyn CursorUnmapHook> {
    let raw = CURSOR_UNMAP_HOOK.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_cursor_unmap_hook` from
    // a `&'static &'static dyn CursorUnmapHook`; that storage is
    // `'static` by contract.
    let slot = unsafe { &*(raw as *const &'static dyn CursorUnmapHook) };
    Some(*slot)
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_cursor_unmap_hook_for_test() {
    CURSOR_UNMAP_HOOK.store(core::ptr::null_mut(), Ordering::Release);
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
                static_borrowed: false,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;

        copy_kernel_half(master_phys, pml4_phys);

        Ok(Self {
            pml4,
            pcid: alloc_pcid(),
            generation: AtomicU64::new(0),
            kernel_gen: AtomicU64::new(KERNEL_MASTER_GEN.load(Ordering::Acquire)),
            mm_ctx_handle: AtomicU64::new(0),
        })
    }

    /// Wrap an already-installed PML4 frame (e.g. the live kernel
    /// master left behind by Limine) as a `VmSpace`. The frame's
    /// contents are preserved — only the META_SLOTS entry transitions
    /// from UNUSED to TYPED to make the frame ref-counted.
    ///
    /// Use this for `KERNEL_VM_SPACE` (the singleton wrapping the
    /// boot kernel master). Fresh user address spaces use
    /// [`Self::new`] instead.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    ///
    /// 1. `pml4_phys` is 4 KiB-aligned and reachable via the kernel
    ///    HHDM mapping.
    /// 2. The frame's META_SLOTS entry is currently UNUSED (no other
    ///    `Frame<_>` handle exists for this paddr); the call may
    ///    return [`MapError::PathCorrupt`] otherwise.
    /// 3. PML4 indices 256..512 already contain the canonical
    ///    kernel-half mappings — `wrap_existing` does **not** copy
    ///    from `KERNEL_MASTER_PML4` (the wrapped frame typically
    ///    *is* the kernel master).
    /// 4. `pcid` is appropriate for this address space ([`Pcid::KERNEL`]
    ///    for the kernel master).
    pub unsafe fn wrap_existing(pml4_phys: PhysAddr, pcid: Pcid) -> Result<Self, MapError> {
        let pml4 = Frame::<PageTableMeta>::from_unused(
            pml4_phys,
            PageTableMeta {
                level: PageTableLevel::Four as u8,
                static_borrowed: true,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;
        Ok(Self {
            pml4,
            pcid,
            generation: AtomicU64::new(0),
            kernel_gen: AtomicU64::new(KERNEL_MASTER_GEN.load(Ordering::Acquire)),
            mm_ctx_handle: AtomicU64::new(0),
        })
    }

    /// Resync this VmSpace's kernel-half (PML4 indices 256..512) from
    /// the registered master if [`KERNEL_MASTER_GEN`] has advanced
    /// since the last sync. Cheap when up-to-date — single Acquire
    /// load + compare. Returns whether a resync ran.
    ///
    /// Called automatically from [`Self::activate`]; safe to call
    /// from any context (no `&mut self` needed because the kernel
    /// half is the kernel's exclusive write domain — slopos-mm
    /// updates the master before bumping the generation, and cursor
    /// mutations only touch indices 0..256).
    pub fn resync_kernel_half_if_stale(&self) -> bool {
        let master_gen = KERNEL_MASTER_GEN.load(Ordering::Acquire);
        let local_gen = self.kernel_gen.load(Ordering::Acquire);
        if local_gen == master_gen {
            return false;
        }
        let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
        if master == KERNEL_MASTER_UNINIT {
            // Race window during boot: master not yet registered.
            // Leave the kernel half as-is; the next activate retry
            // will pick up the resync once boot completes.
            return false;
        }
        let master_phys = PhysAddr::new(master);
        copy_kernel_half(master_phys, self.pml4.paddr());
        // CAS the local gen up to the value we read; if a concurrent
        // resync raced ahead and stored a higher value, leave it.
        let _ = self.kernel_gen.compare_exchange(
            local_gen,
            master_gen,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        true
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
    /// CR3 write path. Resyncs the kernel half from the registered
    /// master if [`KERNEL_MASTER_GEN`] has advanced since the last
    /// sync — cheap when up-to-date (single atomic load). Fires the
    /// registered [`CursorUnmapHook::on_activate`] callback (if any)
    /// so the consumer's per-CPU LUF state tracks the current
    /// process.
    ///
    /// # Safety
    ///
    /// See [`crate::arch::x86_64::cr3::write_cr3_pcid`] — kernel-half
    /// invariant.
    pub unsafe fn activate(&self) {
        // Pick up any kernel-master mutation that happened since last
        // activate. This is the framekernel-correct sync point: by
        // the time CR3 holds this VmSpace's PML4, every kernel-half
        // mapping the rest of the kernel has installed is visible.
        let _ = self.resync_kernel_half_if_stale();
        if let Some(hook) = current_cursor_unmap_hook() {
            hook.on_activate(self.mm_ctx_handle.load(Ordering::Acquire));
        }
        // NOFLUSH is only architecturally legal when `CR4.PCIDE` is
        // enabled; on platforms / hypervisors that don't surface PCID
        // (slopos's `mm/src/mmu/asid::init_bsp` skipped CR4.PCIDE),
        // setting it here yields a #GP. Probe the live CR4 once per
        // activate — cheap, single MOV.
        let pcide_enabled = (crate::cpu::x86_64::control_regs::read_cr4()
            & crate::cpu::x86_64::control_regs::Cr4Flags::PCIDE.bits())
            != 0;
        // SAFETY: `self.pml4` is a live page-table frame whose
        // kernel-half indices 256..512 were copied from the master
        // at construction (and resynced just above); the caller
        // upholds the rest of `write_cr3_pcid`'s contract.
        unsafe { write_cr3_pcid(self.pml4_paddr(), self.pcid, pcide_enabled) };
    }

    /// Stash an opaque consumer-defined identifier for this address
    /// space. Threaded through [`CursorUnmapHook::after_unmap`] and
    /// [`CursorUnmapHook::on_activate`]. Idempotent for repeated calls
    /// with the same value; panics on conflicting reassignment so a
    /// process is never silently re-bound to a different identity.
    pub fn set_mm_ctx_handle(&self, handle: u64) {
        let prev =
            self.mm_ctx_handle
                .compare_exchange(0, handle, Ordering::AcqRel, Ordering::Acquire);
        match prev {
            Ok(_) => {}
            Err(existing) => {
                assert_eq!(
                    existing, handle,
                    "VmSpace::set_mm_ctx_handle: conflicting reassignment \
                     (existing={existing:#x}, new={handle:#x})"
                );
            }
        }
    }

    /// Read the consumer-set context handle (`0` ⇒ unset).
    pub fn mm_ctx_handle(&self) -> u64 {
        self.mm_ctx_handle.load(Ordering::Acquire)
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
    /// Walks the actual leaf (4 KiB / 2 MiB / 1 GiB); the returned
    /// `level` reflects the leaf's real size.
    pub fn query(&self) -> Result<CursorEntry, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        match walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            false,
            WalkMode::Query,
            PageTableLevel::One,
        )
        .map_err(map_walk_err)?
        {
            outcome @ WalkOutcome::LeafTable { .. } => match read_leaf(&outcome) {
                Some((paddr, property, level)) => Ok(CursorEntry {
                    vaddr: self.cur,
                    paddr: Some(paddr),
                    property,
                    level,
                }),
                None => {
                    // Walk reached the leaf level but the entry is
                    // empty. The next mapping starts at most one
                    // leaf-size away — surface that level so
                    // `entry.level.entry_size()` skips correctly.
                    let leaf_level = match outcome {
                        WalkOutcome::LeafTable { leaf_level, .. } => leaf_level,
                        WalkOutcome::NotPresent { .. } => PageTableLevel::One,
                    };
                    Ok(CursorEntry {
                        vaddr: self.cur,
                        paddr: None,
                        property: PageProperty::default(),
                        level: leaf_level,
                    })
                }
            },
            WalkOutcome::NotPresent { stopped_at } => Ok(CursorEntry {
                vaddr: self.cur,
                paddr: None,
                property: PageProperty::default(),
                // The level whose entry was missing — caller advances
                // by `level.entry_size()` to skip the empty subtree.
                // E.g. PML4 entry missing ⇒ skip 512 GiB.
                level: stopped_at,
            }),
        }
    }

    pub fn vaddr(&self) -> VirtAddr {
        self.cur
    }

    /// Advance the cursor by one 4 KiB page.
    pub fn next(&mut self) -> Result<(), MapError> {
        self.advance(PAGE_SIZE_4KB)
    }

    /// Advance the cursor by `bytes` (must be a positive multiple of
    /// 4 KiB). Stops at `range.end`; one past the end is allowed,
    /// further advance returns [`MapError::OutOfBounds`].
    pub fn advance(&mut self, bytes: u64) -> Result<(), MapError> {
        if bytes == 0 || bytes & (PAGE_SIZE_4KB - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let next = self
            .cur
            .as_u64()
            .checked_add(bytes)
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

    /// Advance the cursor by one 4 KiB page.
    pub fn next(&mut self) -> Result<(), MapError> {
        self.advance(PAGE_SIZE_4KB)
    }

    /// Advance the cursor by `bytes` (must be a positive multiple of
    /// 4 KiB). Stops at `range.end`; one past the end is allowed,
    /// further advance returns [`MapError::OutOfBounds`].
    pub fn advance(&mut self, bytes: u64) -> Result<(), MapError> {
        if bytes == 0 || bytes & (PAGE_SIZE_4KB - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let next = self
            .cur
            .as_u64()
            .checked_add(bytes)
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

    /// Map `frame` at the cursor's current vaddr with `prop`, at leaf
    /// size `S`. Consumes `frame`; on success the returned `Ok(())`
    /// means the underlying `Frame<M>` has been leaked into the leaf
    /// PTE (its single ref is now held by the page table). Reverse
    /// via [`Self::unmap::<S>`].
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::UnalignedFrame`] — `frame.paddr() % S::BYTES != 0`.
    /// * [`MapError::Overlap`] — leaf already present.
    /// * [`MapError::IntermediateAllocFailed`] — page-table frame alloc failed.
    pub fn map<S: PageSize, M: AnyUFrameMeta>(
        &mut self,
        frame: UFrame<M>,
        prop: PageProperty,
    ) -> Result<(), MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }
        // The mapped region must fit inside `range`. Otherwise a
        // 2 MiB map at the last 4 KiB of `range` would silently extend
        // past `range.end`.
        let map_end = self
            .cur
            .as_u64()
            .checked_add(S::BYTES)
            .ok_or(MapError::OutOfBounds)?;
        if map_end > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if frame.paddr().as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedFrame);
        }

        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            prop.user,
            WalkMode::Create,
            S::LEVEL,
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
        if leaf_level != S::LEVEL {
            // walk_to_leaf in Create mode splits any blocking huge
            // page on the way down; reaching here with a wrong level
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
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set(paddr, flags);
        self.dirty = true;
        Ok(())
    }

    /// Unmap the current entry at leaf size `S`. Returns the freed
    /// `UFrame` (ref count == 1) when one was present.
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::SizeMismatch`] — leaf is present at a different
    ///   size than `S` (e.g. `unmap::<Size4Kb>` on a 2 MiB leaf).
    pub fn unmap<S: PageSize, M: AnyUFrameMeta>(&mut self) -> Result<Option<UFrame<M>>, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }

        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            false,
            WalkMode::Mutate,
            S::LEVEL,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            return Ok(None);
        };
        if leaf_level != S::LEVEL {
            // The entry is at a different page size than the caller
            // expected — flag it explicitly so the caller can recover
            // (e.g. by retrying with the right size) instead of
            // silently dropping the unmap on the floor.
            return Err(MapError::SizeMismatch);
        }

        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(None);
        }
        // Sanity razor: a huge-leaf-sized cursor unmap on a 4 KiB
        // entry, or vice versa, would have failed `S::LEVEL` above.
        debug_assert_eq!(pte.is_huge(), S::HUGE_BIT);

        let paddr = pte.address();
        let was_user = pte.flags().contains(PteFlags::USER);
        pte.clear();
        // Local TLB invalidation for the freed leaf. Cross-CPU
        // shootdown is the consumer's responsibility (slopos-mm wraps
        // these calls with `tlb_shootdown`).
        flush_leaf_local::<S>(self.cur);
        self.dirty = true;

        // Fire the cursor-unmap hook for user-space leaves. Slopos-mm
        // dispatches this into its LUF queue; tests can observe it
        // for shootdown-coverage assertions.
        if was_user {
            if let Some(hook) = current_cursor_unmap_hook() {
                hook.after_unmap(
                    self.cur,
                    paddr,
                    self.space.mm_ctx_handle.load(Ordering::Acquire),
                );
            }
        }

        // Reclaim the leaked UFrame ref.
        // SAFETY: at `map` time we leaked exactly one ref to this
        // slot via `Frame::into_raw`; clearing the PTE here removes
        // the only path that held that ref. `from_raw_at` re-wraps
        // without bumping the count, so accounting is exact.
        let frame: Frame<M> =
            unsafe { Frame::<M>::from_raw_at(paddr).map_err(|_| MapError::PathCorrupt)? };
        Ok(Some(UFrame::<M>::from_frame(frame)))
    }

    /// Update the access/cache properties of the current entry at
    /// leaf size `S` without remapping. No-op when no mapping is
    /// present (returns `Ok(())`).
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::SizeMismatch`] — present leaf is a different size than `S`.
    pub fn protect<S: PageSize>(&mut self, prop: PageProperty) -> Result<(), MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }
        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            prop.user,
            WalkMode::Mutate,
            S::LEVEL,
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
        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(());
        }
        if leaf_level != S::LEVEL {
            return Err(MapError::SizeMismatch);
        }
        // Carry forward the address; refresh access flags.
        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set_flags_only(flags);
        flush_leaf_local::<S>(self.cur);
        self.dirty = true;
        Ok(())
    }
}

/// Local-CPU invalidation for a leaf at size `S`. For 4 KiB entries
/// emits a single INVLPG; for huge leaves the consumer's TLB driver
/// must invalidate every 4 KiB page in the range (issued via repeated
/// INVLPG — typed coordinator wraps this).
#[inline]
fn flush_leaf_local<S: PageSize>(start: VirtAddr) {
    let mut offset: u64 = 0;
    while offset < S::BYTES {
        tlb::flush_local(VirtAddr::new(start.as_u64() + offset));
        offset += PAGE_SIZE_4KB;
    }
}

// ---------------------------------------------------------------------------
// CursorMut range helpers.
// ---------------------------------------------------------------------------

impl CursorMut<'_> {
    /// Map a sequence of consecutive `S`-sized leaves starting at the
    /// cursor's current vaddr, advancing the cursor by `S::BYTES` after
    /// each successful map. Stops on the first error and returns the
    /// number of mappings that were installed (the cursor is left at
    /// the position immediately after the last installed leaf).
    ///
    /// On success the returned `usize` equals the number of frames the
    /// iterator produced. Trailing `Ok(())` from `map` requires the
    /// cursor's `range` to extend at least `frames.len() * S::BYTES`
    /// bytes from the starting `cur`.
    pub fn map_range<S, M, I>(&mut self, frames: I, prop: PageProperty) -> Result<usize, MapError>
    where
        S: PageSize,
        M: AnyUFrameMeta,
        I: IntoIterator<Item = UFrame<M>>,
    {
        let mut count = 0usize;
        for frame in frames {
            self.map::<S, M>(frame, prop)?;
            self.advance(S::BYTES)?;
            count += 1;
        }
        Ok(count)
    }

    /// Apply [`Self::protect::<S>`] across `len_bytes` of leaves
    /// starting at the cursor's current vaddr. `len_bytes` must be a
    /// positive multiple of `S::BYTES`. Skips not-present entries
    /// (consistent with the per-entry `protect`); returns the first
    /// error encountered.
    pub fn protect_range<S>(&mut self, len_bytes: u64, prop: PageProperty) -> Result<(), MapError>
    where
        S: PageSize,
    {
        if len_bytes == 0 || len_bytes & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let mut remaining = len_bytes;
        while remaining > 0 {
            self.protect::<S>(prop)?;
            self.advance(S::BYTES)?;
            remaining -= S::BYTES;
        }
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

// ---------------------------------------------------------------------------
// VmSpace::Drop — flush-free user-half tree teardown.
// ---------------------------------------------------------------------------
//
// A VmSpace is dropped only when its `KArc` refcount hits zero — by
// construction no CPU is using it (otherwise a `KArc` reference would
// be live). The user-half walker therefore needs no per-page TLB
// flush; consumers issue a single up-front `tlb_shootdown(All)` in
// their teardown path before letting the VmSpace's last `KArc` drop.
//
// The walker recurses into every present non-huge intermediate
// (PDPT/PD/PT) under PML4 indices 0..256, reclaims every leaf frame
// (4 KiB user pages and any huge leaves), then reclaims the
// intermediate tables themselves via `reclaim_leaked_frame`. The
// kernel half (256..512) is intentionally skipped — those entries
// point into the shared kernel master, which `KERNEL_VM_SPACE` owns
// (and never drops, since it lives behind a `OnceLock`).
//
// Recursion depth is fixed at 3 (PML4 → PDPT → PD → PT), and each
// frame's stack budget is bounded by the loop counter + a couple of
// scratch variables — well under the 2 KiB threshold enforced by
// `scripts/check_stack_sizes.sh`.

impl Drop for VmSpace {
    fn drop(&mut self) {
        drop_user_half_tree(self.pml4.paddr());
        // `self.pml4: Frame<PageTableMeta>` drops next, decrementing
        // the PML4 frame's ref count to zero and returning it to the
        // allocator (or, in the wrapped-kernel-master case, leaving
        // the OnceLock storage's static lifetime to keep the slot
        // pinned — KERNEL_VM_SPACE never drops in production).
    }
}

fn drop_user_half_tree(pml4_phys: Paddr) {
    for i in 0..KERNEL_HALF_START_INDEX {
        let pte = entry_in_table(pml4_phys, i);
        if !pte.is_present() {
            continue;
        }
        debug_assert!(
            !pte.is_huge(),
            "PML4 huge entry at index {i} — architecturally invalid",
        );
        let child = pte.address();
        // SAFETY: every present, non-huge PML4 entry was created by
        // `step_down` (in WalkMode::Create) which leaked exactly one
        // `Frame<PageTableMeta>` ref into this PTE. The VmSpace's
        // refcount is zero (we are inside its Drop), so no CPU is
        // walking this tree concurrently — recursion is safe.
        recursively_reclaim_subtree(child, PageTableLevel::Three);
    }
}

fn recursively_reclaim_subtree(table_phys: Paddr, level: PageTableLevel) {
    use crate::mm::page_table::PAGE_TABLE_ENTRIES;
    for i in 0..PAGE_TABLE_ENTRIES {
        let pte = entry_in_table(table_phys, i);
        if !pte.is_present() {
            continue;
        }
        let child = pte.address();
        if pte.is_huge() {
            // Huge user-half leaf (2 MiB at level Two, 1 GiB at
            // level Three). Reclaim the leaked META_SLOTS ref —
            // `Frame::on_drop` then dispatches to the registered
            // `FrameAlloc::dealloc`. NOTE: today's `Frame<M>` carries
            // no per-page size, so `dealloc` is invoked with
            // `size_pages = 1`; the trailing pages of the huge region
            // are returned to the buddy allocator only when the
            // consumer's `FrameAlloc` impl tracks the allocation
            // size in its own bookkeeping (the production allocator
            // shim does, since allocs go through `alloc_pages_at`).
            // SAFETY: see drop_user_half_tree's SAFETY comment.
            unsafe { reclaim_leaked_frame(child) };
        } else if level == PageTableLevel::One {
            // 4 KiB leaf — the common case. Reclaim the leaked
            // user-frame ref.
            // SAFETY: see drop_user_half_tree's SAFETY comment.
            unsafe { reclaim_leaked_frame(child) };
        } else {
            let next_level = level
                .next_lower()
                .expect("recursively_reclaim_subtree called with leaf level");
            recursively_reclaim_subtree(child, next_level);
        }
    }
    // Reclaim this intermediate page-table frame itself.
    // SAFETY: every intermediate page-table frame was leaked into
    // its parent PTE by `step_down`. The parent PTE's pre-cleared /
    // not-cleared state is irrelevant — no CPU is walking the tree
    // (VmSpace refcount is zero), so the slot can be returned to
    // the allocator without further coordination.
    unsafe { reclaim_leaked_frame(table_phys) };
}
