//! `Frame<M>`: typed handle to a single physical 4 KiB frame.
//!
//! Per-page metadata `M` is carried inline in a `MetaSlot` that lives
//! in the static `META_SLOTS` array (one slot per physical frame,
//! indexed by `paddr / PAGE_SIZE`). The metadata is type-erased into
//! `MetaSlot::storage` (a fixed byte buffer sized for `MAX_META_SIZE`)
//! and dispatched through a per-`M` `MetaVtable` carrying the
//! `drop_in_place` and `on_drop` callbacks.
//!
//! Synchronisation goes through the slot's atomic fields. The `state`
//! field gates exclusive access to `storage`; `ref_count` pairs
//! Release-on-decrement with an Acquire fence on the last-ref path
//! so the final dropper sees every prior write to the slot.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use slopos_abi::addr::PhysAddr;

/// Physical address. Aliased to keep call sites lining up with the
/// `Paddr` vocabulary used throughout the typed-frame API.
pub type Paddr = PhysAddr;

/// Maximum inline byte budget for an [`AnyFrameMeta`] payload.
/// Consumers add a `const _: () = assert_meta_fits::<M>();` line per
/// impl to catch oversize meta types at compile time.
pub const MAX_META_SIZE: usize = 16;

/// Maximum alignment for an [`AnyFrameMeta`] payload. Equal to
/// [`MetaSlot`]'s alignment so the inline storage is always at least
/// this aligned in practice.
pub const MAX_META_ALIGN: usize = 8;

const PAGE_SIZE: usize = 4096;

/// Slot is unoccupied. `vtable` is null; `storage` is uninitialised.
pub(crate) const META_STATE_UNUSED: u8 = 0;
/// Slot holds a fully-initialised `M`. `vtable` and `storage` are valid.
pub(crate) const META_STATE_TYPED: u8 = 1;

// ---------------------------------------------------------------------------
// MetaSlot: per-physical-frame typed-metadata cell.
// ---------------------------------------------------------------------------

/// Aligned inline storage cell for the metadata payload. Wrapping the
/// raw byte buffer in a `repr(C, align(8))` newtype guarantees that
/// the storage offset within [`MetaSlot`] and the storage's native
/// alignment both meet [`MAX_META_ALIGN`].
#[repr(C, align(8))]
pub(crate) struct MetaStorage(pub(crate) [u8; MAX_META_SIZE]);

/// Fixed-layout per-frame slot. `ref_count` is at offset 0; the
/// const-assert below pins this layout so external verification can
/// rely on the field address.
#[repr(C, align(8))]
pub struct MetaSlot {
    /// Reference count. 0 ⇒ slot is `UNUSED`.
    pub(crate) ref_count: AtomicU32,
    /// `META_STATE_*`. Tracks slot lifecycle.
    pub(crate) state: AtomicU8,
    _pad0: [u8; 3],
    /// Pointer to the static `MetaVtable` for the inhabiting `M`.
    /// Null when `state == UNUSED`.
    pub(crate) vtable: AtomicPtr<MetaVtable>,
    /// Type-erased storage for `M`. Only valid when `state == TYPED`.
    pub(crate) storage: UnsafeCell<MaybeUninit<MetaStorage>>,
}

const _: () = assert!(
    core::mem::offset_of!(MetaSlot, ref_count) == 0,
    "MetaSlot::ref_count must be at offset 0"
);

const _: () = assert!(
    align_of::<MetaSlot>() >= MAX_META_ALIGN,
    "MetaSlot alignment must be at least MAX_META_ALIGN"
);

// SAFETY: every access path uses the atomic fields for
// synchronisation. `storage` is mutated only by code that has
// exclusive access via `state`-driven hand-off; readers see a fully
// initialised `M` via `borrow()`.
unsafe impl Sync for MetaSlot {}

impl MetaSlot {
    /// Construct a fresh, unused metadata slot. Behind a feature
    /// gate because production callers obtain slots from the
    /// boot-allocated `META_SLOTS` array — only host integration
    /// tests need to materialise scratch slots ad-hoc.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn new_unused() -> Self {
        Self {
            ref_count: AtomicU32::new(0),
            state: AtomicU8::new(META_STATE_UNUSED),
            _pad0: [0; 3],
            vtable: AtomicPtr::new(core::ptr::null_mut()),
            storage: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

// ---------------------------------------------------------------------------
// MetaVtable: type-erased dispatch for Drop.
// ---------------------------------------------------------------------------

/// Per-`M` dispatch table installed by [`Frame::from_unused`]. The
/// vtable lives in static memory (one per concrete `M` via the
/// associated-const pattern in [`HasVtable`]).
pub struct MetaVtable {
    /// Run `core::ptr::drop_in_place::<M>` on the storage payload.
    pub(crate) drop_in_place: unsafe fn(*mut u8),
    /// Run [`AnyFrameMeta::on_drop`] on the storage payload.
    pub(crate) on_drop: unsafe fn(*mut u8, Paddr),
}

// SAFETY: the dispatched function pointers `drop_in_place` and
// `on_drop` only act on storage owned by the surrounding `MetaSlot`,
// and synchronisation is through the slot's atomics.
unsafe impl Sync for MetaVtable {}

unsafe fn drop_in_place_for<M: AnyFrameMeta>(payload: *mut u8) {
    // SAFETY: caller (`Drop for Frame<M>`) holds the only remaining
    // ref and has just transitioned the slot out of TYPED, so we
    // have exclusive access to the M payload at `payload`.
    unsafe {
        core::ptr::drop_in_place(payload as *mut M);
    }
}

unsafe fn on_drop_for<M: AnyFrameMeta>(payload: *mut u8, paddr: Paddr) {
    // SAFETY: same as `drop_in_place_for` — exclusive access to a
    // valid M payload at `payload`.
    unsafe {
        (*(payload as *mut M)).on_drop(paddr);
    }
}

trait HasVtable {
    const VTABLE: &'static MetaVtable;
}

impl<M: AnyFrameMeta> HasVtable for M {
    const VTABLE: &'static MetaVtable = &MetaVtable {
        drop_in_place: drop_in_place_for::<M>,
        on_drop: on_drop_for::<M>,
    };
}

#[inline]
fn vtable_for<M: AnyFrameMeta>() -> &'static MetaVtable {
    <M as HasVtable>::VTABLE
}

// ---------------------------------------------------------------------------
// AnyFrameMeta + builtin meta types.
// ---------------------------------------------------------------------------

/// # Safety
///
/// Implementor's [`SIZE`](Self::SIZE) and [`ALIGN`](Self::ALIGN)
/// associated constants must match `Self`. [`on_drop`](Self::on_drop)
/// runs after the last `Frame<Self>` reference is released and
/// before the underlying physical frame is returned to the allocator.
pub unsafe trait AnyFrameMeta: Send + Sync + Sized + 'static {
    const SIZE: usize = size_of::<Self>();
    const ALIGN: usize = align_of::<Self>();

    fn on_drop(&mut self, paddr: Paddr);
}

/// Compile-time check that an `M` fits in a [`MetaSlot`]'s inline
/// storage. Every `AnyFrameMeta` impl in this crate calls this via a
/// `const _` block; downstream impls should do the same.
#[inline]
pub(crate) const fn assert_meta_fits<M: AnyFrameMeta>() {
    assert!(
        M::SIZE <= MAX_META_SIZE,
        "AnyFrameMeta::SIZE must be <= MAX_META_SIZE"
    );
    assert!(
        M::ALIGN <= MAX_META_ALIGN,
        "AnyFrameMeta::ALIGN must be <= MAX_META_ALIGN"
    );
}

/// Generic kernel-owned page (default for code that does not care
/// about per-page metadata). Returns its physical frame to the
/// registered [`FrameAlloc`] on `Drop`.
#[derive(Default)]
pub struct KernelMeta;

// SAFETY: ZST has no representation invariants. `on_drop` returns
// the underlying physical frame to the allocator — required so
// `Frame<KernelMeta>` does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for KernelMeta {
    fn on_drop(&mut self, paddr: Paddr) {
        return_frame_to_allocator(paddr);
    }
}
const _: () = assert_meta_fits::<KernelMeta>();

/// Page-table frame metadata. `level` is the architectural level
/// (`4` = PML4, `1` = PT). `static_borrowed` is `true` only for the
/// wrapped boot kernel-master PML4 (constructed via
/// [`super::vm_space::VmSpace::wrap_existing`]) — that frame's
/// storage is statically owned by the bootloader, so it must NOT be
/// returned to the buddy allocator on Drop.
pub struct PageTableMeta {
    pub level: u8,
    pub static_borrowed: bool,
}

// SAFETY: fields are plain data. `on_drop` returns the page-table
// frame to the allocator unless the meta declares `static_borrowed`.
unsafe impl AnyFrameMeta for PageTableMeta {
    fn on_drop(&mut self, paddr: Paddr) {
        if !self.static_borrowed {
            return_frame_to_allocator(paddr);
        }
    }
}
const _: () = assert_meta_fits::<PageTableMeta>();

/// Untyped anonymous frame metadata. Returns its physical frame to
/// the registered [`FrameAlloc`] on `Drop`.
#[derive(Default)]
pub struct AnonymousMeta;

// SAFETY: ZST has no representation invariants. `on_drop` returns
// the underlying physical frame to the allocator — required so
// `UFrame<AnonymousMeta>` does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for AnonymousMeta {
    fn on_drop(&mut self, paddr: Paddr) {
        return_frame_to_allocator(paddr);
    }
}
const _: () = assert_meta_fits::<AnonymousMeta>();

/// Helper: dealloc `paddr` (one page) via the registered allocator.
/// No-op when no allocator is registered (test scaffolding can drop
/// frames before `register_frame_allocator` runs without panicking).
#[inline]
fn return_frame_to_allocator(paddr: Paddr) {
    if let Some(alloc) = crate::mm::frame_alloc::current_frame_allocator() {
        alloc.dealloc(paddr, 1);
    }
}

// ---------------------------------------------------------------------------
// META_SLOTS array.
// ---------------------------------------------------------------------------

struct MetaSlotsRegion {
    base: AtomicPtr<MetaSlot>,
    len: AtomicUsize,
}

static META_SLOTS: MetaSlotsRegion = MetaSlotsRegion {
    base: AtomicPtr::new(core::ptr::null_mut()),
    len: AtomicUsize::new(0),
};

/// One-shot boot wiring point.
///
/// # Safety
///
/// `slots` must point to `len` zero-initialised, non-aliased
/// [`MetaSlot`]s, valid for the static lifetime of the kernel. The
/// caller must not retain any other references to that storage.
pub unsafe fn init_meta_slots(slots: *mut MetaSlot, len: usize) {
    let prev = META_SLOTS.base.swap(slots, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::frame::init_meta_slots called twice"
    );
    META_SLOTS.len.store(len, Ordering::Release);
}

/// Test-only reset hook. Allows host integration-test binaries to
/// discard a previous `init_meta_slots` registration so a fresh
/// scratch array can be installed. Not exposed in production.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_meta_slots_for_test() {
    META_SLOTS
        .base
        .store(core::ptr::null_mut(), Ordering::Release);
    META_SLOTS.len.store(0, Ordering::Release);
}

#[inline]
pub(crate) fn meta_slot_for(paddr: Paddr) -> Option<&'static MetaSlot> {
    let base = META_SLOTS.base.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    let len = META_SLOTS.len.load(Ordering::Acquire);
    let idx = (paddr.as_u64() as usize) / PAGE_SIZE;
    if idx >= len {
        return None;
    }
    // SAFETY: `init_meta_slots`'s caller certified that
    // `[base, base + len)` is a valid `&'static [MetaSlot]`; we have
    // bounds-checked the index against `len`.
    Some(unsafe { &*base.add(idx) })
}

// ---------------------------------------------------------------------------
// Frame<M>
// ---------------------------------------------------------------------------

/// Owned typed handle to a single physical 4 KiB frame.
///
/// Cloning is via [`Frame::from_in_use`] (ref-count bump, returns a
/// new `Frame<M>` for the same physical page). Dropping the last
/// `Frame<M>` runs `M::on_drop`, drops the inline `M`, and returns
/// the underlying physical frame to the registered allocator.
pub struct Frame<M: AnyFrameMeta> {
    ptr: *const MetaSlot,
    _marker: PhantomData<M>,
}

// SAFETY: `Frame<M>` is a thin wrapper over a pointer into the static
// `META_SLOTS` array; sharing/sending across threads is sound because
// `M: Send + Sync` (transitively required by `AnyFrameMeta`) and
// ref-count manipulation is atomic.
unsafe impl<M: AnyFrameMeta> Send for Frame<M> {}
unsafe impl<M: AnyFrameMeta> Sync for Frame<M> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// `paddr` falls outside the `META_SLOTS` array.
    OutOfRange,
    /// `META_SLOTS` not initialised.
    NotInitialised,
    /// Slot was not in the expected state (e.g. `from_unused` on a
    /// slot already TYPED, or `from_in_use` on an UNUSED slot).
    StateMismatch,
}

impl<M: AnyFrameMeta> Frame<M> {
    /// Wrap a freshly-allocated, currently-unused physical frame and
    /// install `meta` into its slot.
    ///
    /// Returns [`FrameError::NotInitialised`] when [`init_meta_slots`]
    /// has not yet been called, [`FrameError::OutOfRange`] when
    /// `paddr` does not have a slot, and [`FrameError::StateMismatch`]
    /// when the slot is already TYPED.
    pub fn from_unused(paddr: Paddr, meta: M) -> Result<Self, FrameError> {
        const {
            assert_meta_fits::<M>();
        }
        let slot = match meta_slot_for(paddr) {
            Some(s) => s,
            None => {
                return Err(if META_SLOTS.base.load(Ordering::Acquire).is_null() {
                    FrameError::NotInitialised
                } else {
                    FrameError::OutOfRange
                });
            }
        };
        slot.state
            .compare_exchange(
                META_STATE_UNUSED,
                META_STATE_TYPED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| FrameError::StateMismatch)?;
        // SAFETY: the CAS above transitioned the slot from UNUSED to
        // TYPED, so we hold exclusive access to `storage` until we
        // publish the slot via the ref_count store below.
        unsafe {
            let storage = slot.storage.get() as *mut M;
            core::ptr::write(storage, meta);
        }
        slot.vtable.store(
            vtable_for::<M>() as *const _ as *mut MetaVtable,
            Ordering::Release,
        );
        slot.ref_count.store(1, Ordering::Release);
        Ok(Self {
            ptr: slot,
            _marker: PhantomData,
        })
    }

    /// Borrow an already-live frame, bumping its ref-count by one.
    /// The caller's `paddr` must point to a slot currently in the
    /// `TYPED` state with the same `M`; mismatches return
    /// [`FrameError::StateMismatch`].
    pub fn from_in_use(paddr: Paddr) -> Result<Self, FrameError> {
        let slot = meta_slot_for(paddr).ok_or(FrameError::OutOfRange)?;
        if slot.state.load(Ordering::Acquire) != META_STATE_TYPED {
            return Err(FrameError::StateMismatch);
        }
        // Acquire-fetch — pairs with the Release on the last-ref
        // decrement in `Drop`.
        let prev = slot.ref_count.fetch_add(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "from_in_use on a slot with ref_count == 0");
        Ok(Self {
            ptr: slot,
            _marker: PhantomData,
        })
    }

    /// Physical address of the underlying frame, derived from the
    /// slot's index in `META_SLOTS`.
    pub fn paddr(&self) -> Paddr {
        let base = META_SLOTS.base.load(Ordering::Acquire);
        let idx = (self.ptr as usize - base as usize) / size_of::<MetaSlot>();
        PhysAddr::new((idx * PAGE_SIZE) as u64)
    }

    pub fn reference_count(&self) -> u32 {
        // SAFETY: `ptr` was obtained from a live MetaSlot when this
        // `Frame` was constructed; the ref-count is at least 1 for
        // the lifetime of `self`.
        unsafe { (*self.ptr).ref_count.load(Ordering::Acquire) }
    }
}

/// Peek at the META_SLOTS refcount for `paddr` without constructing
/// a `Frame<M>` (no inc / dec). Returns `0` for paddrs whose slot is
/// UNUSED or out of range. Used by COW resolution in slopos-mm to
/// decide single- vs multi-owner without disturbing the count.
pub fn reference_count_at(paddr: Paddr) -> u32 {
    let Some(slot) = meta_slot_for(paddr) else {
        return 0;
    };
    slot.ref_count.load(Ordering::Acquire)
}

impl<M: AnyFrameMeta> Frame<M> {
    pub fn borrow(&self) -> &M {
        // SAFETY: the slot is `TYPED` for the lifetime of `self`
        // (ref_count ≥ 1 guarantees no Drop has fired); `storage`
        // contains a valid `M` placed by `from_unused`. The pointer
        // cast is sound because `M::ALIGN ≤ MAX_META_ALIGN` and
        // `MetaStorage` is `align(MAX_META_ALIGN)`.
        unsafe {
            let slot = &*self.ptr;
            &*(slot.storage.get() as *const M)
        }
    }

    pub fn into_raw(self) -> *const MetaSlot {
        let p = self.ptr;
        core::mem::forget(self);
        p
    }

    /// # Safety
    ///
    /// `ptr` must be the result of a prior [`Frame::<M>::into_raw`]
    /// call that has not been re-wrapped already, and `M` must match
    /// the type that produced `ptr`.
    pub unsafe fn from_raw(ptr: *const MetaSlot) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Reclaim a previously [`into_raw`]-leaked frame by physical
    /// address. Used by the [`super::vm_space::CursorMut`] unmap path
    /// where a single ref was leaked into a PTE; clearing the PTE
    /// hands ownership back through this function so the slot's ref
    /// count never goes negative or doubles up.
    ///
    /// Returns [`FrameError::OutOfRange`] / [`FrameError::NotInitialised`]
    /// for unknown paddrs and [`FrameError::StateMismatch`] when the
    /// slot is `UNUSED`.
    ///
    /// [`into_raw`]: Frame::into_raw
    ///
    /// # Safety
    ///
    /// Caller asserts:
    ///
    /// 1. exactly one ref to the slot was previously leaked via
    ///    `into_raw` and has not been reclaimed since,
    /// 2. `M` matches the metadata type that produced the leaked
    ///    `Frame`.
    pub unsafe fn from_raw_at(paddr: Paddr) -> Result<Self, FrameError> {
        let slot = meta_slot_for(paddr).ok_or_else(|| {
            if META_SLOTS.base.load(Ordering::Acquire).is_null() {
                FrameError::NotInitialised
            } else {
                FrameError::OutOfRange
            }
        })?;
        if slot.state.load(Ordering::Acquire) != META_STATE_TYPED {
            return Err(FrameError::StateMismatch);
        }
        // SAFETY: caller's contract above. The slot is `TYPED` and
        // exactly one ref is outstanding (leaked); the new `Frame`
        // takes ownership of that ref without bumping the count.
        Ok(Self {
            ptr: slot as *const MetaSlot,
            _marker: PhantomData,
        })
    }
}

/// Convenience surface for `Frame<KernelMeta>` — the untyped kernel
/// page handle. Centralises HHDM translation and allocator round-trip
/// here so non-OSTD callers get a fully safe API and the residual
/// unsafe stays in OSTD.
impl Frame<KernelMeta> {
    /// Allocate a single kernel page through the registered
    /// [`FrameAlloc`] and wrap it. Returns `None` if no allocator is
    /// registered or the allocator returns nothing.
    pub fn alloc(opts: FrameAllocOptions) -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        let paddr = alloc.alloc(opts)?;
        match Self::from_unused(paddr, KernelMeta) {
            Ok(frame) => Some(frame),
            Err(_) => {
                alloc.dealloc(paddr, opts.size_pages.max(1));
                None
            }
        }
    }

    /// Convenience: zeroed single-page allocation.
    pub fn alloc_zeroed() -> Option<Self> {
        Self::alloc(FrameAllocOptions::single().zeroed())
    }

    /// Physical address as a raw `u64`.
    #[inline]
    pub fn phys_u64(&self) -> u64 {
        self.paddr().as_u64()
    }

    /// Kernel HHDM virtual address pointing at this frame's contents.
    /// Requires [`crate::mm::phys::init_phys_virt_offset`] to have run.
    #[inline]
    pub fn virt_addr_u64(&self) -> u64 {
        crate::mm::phys::phys_to_virt(self.paddr()) as u64
    }

    /// Typed mutable pointer into this frame via the kernel HHDM.
    #[inline]
    pub fn as_mut_ptr<T>(&self) -> *mut T {
        crate::mm::phys::phys_to_virt(self.paddr()) as *mut T
    }

    /// Typed const pointer into this frame via the kernel HHDM.
    #[inline]
    pub fn as_ptr<T>(&self) -> *const T {
        crate::mm::phys::phys_to_virt(self.paddr()) as *const T
    }

    /// Consume the handle and return the physical address without
    /// dropping the underlying ref. The slot stays `TYPED` with one
    /// outstanding ref; the caller is responsible for either re-wrapping
    /// it via [`Frame::from_raw_at`] or releasing the page another way.
    #[inline]
    pub fn into_phys(self) -> Paddr {
        let paddr = self.paddr();
        let _slot = self.into_raw();
        paddr
    }
}

impl<M: AnyFrameMeta> Drop for Frame<M> {
    fn drop(&mut self) {
        // SAFETY: `ptr` points at a live `MetaSlot` for as long as
        // `ref_count > 0`, which is true while this `Frame` is alive.
        let slot = unsafe { &*self.ptr };
        // Release on the decrement; Acquire fence on the last-ref
        // path; pairs with `from_in_use`'s AcqRel add.
        if slot.ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        core::sync::atomic::fence(Ordering::Acquire);
        let vt = slot.vtable.load(Ordering::Acquire);
        let storage = slot.storage.get() as *mut u8;
        let paddr = self.paddr();
        // SAFETY: we hold the only remaining reference; `vtable`
        // points to the static `MetaVtable` for the `M` that was
        // installed by `from_unused`; `storage` holds a valid `M`.
        unsafe {
            ((*vt).on_drop)(storage, paddr);
            ((*vt).drop_in_place)(storage);
        }
        slot.vtable.store(core::ptr::null_mut(), Ordering::Release);
        slot.state.store(META_STATE_UNUSED, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Allocation surface.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct FrameAllocOptions {
    pub size_pages: usize,
    pub zeroing: bool,
    pub align_pages: usize,
}

impl FrameAllocOptions {
    pub const fn single() -> Self {
        Self {
            size_pages: 1,
            zeroing: false,
            align_pages: 1,
        }
    }

    pub const fn zeroed(self) -> Self {
        Self {
            zeroing: true,
            ..self
        }
    }
}

pub trait FrameAlloc: Send + Sync + 'static {
    fn alloc(&self, opts: FrameAllocOptions) -> Option<Paddr>;
    fn dealloc(&self, paddr: Paddr, size_pages: usize);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_slot_layout() {
        assert_eq!(core::mem::offset_of!(MetaSlot, ref_count), 0);
        assert!(core::mem::align_of::<MetaSlot>() >= MAX_META_ALIGN);
    }

    #[test]
    fn meta_size_fits() {
        assert!(KernelMeta::SIZE <= MAX_META_SIZE);
        assert!(PageTableMeta::SIZE <= MAX_META_SIZE);
        assert!(AnonymousMeta::SIZE <= MAX_META_SIZE);
    }
}
