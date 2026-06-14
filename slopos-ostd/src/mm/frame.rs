//! `Frame<M>`: typed handle to a single physical 4 KiB frame.
//!
//! Per-page metadata `M` is carried inline in a `MetaSlot` that lives
//! in the static `META_SLOTS` array (one slot per physical frame,
//! indexed by `paddr / PAGE_SIZE`). The metadata is type-erased into
//! `MetaSlot::storage` (a fixed byte buffer sized for `MAX_META_SIZE`)
//! and dispatched through a per-`M` `MetaVtable` carrying the
//! `drop_in_place` / `returns_frame` callbacks and the type's canonical
//! [`core::any::TypeId`] (the cross-crate-stable type-identity key).
//!
//! Synchronisation goes through the slot's atomic fields. The `state`
//! field gates exclusive access to `storage`; `ref_count` pairs
//! Release-on-decrement with an Acquire fence on the last-ref path
//! so the final dropper sees every prior write to the slot.
//!
//! # Verification
//!
//! The reference-count state machine in this module ([`MetaSlot`],
//! [`Frame::from_unused`], [`Frame::from_in_use`], and [`Drop for Frame`])
//! is machine-checked under Verus. `verification/proofs/frame_refcount.rs`
//! mirrors the atomic-bounded transitions as an abstract state machine and
//! proves the three reference-count invariants hold across every concurrent
//! interleaving:
//!
//!   * (I1) `ref_count > 0` ⇒ the frame is allocated and off the
//!     allocator free list;
//!   * (I2) the last `Drop` releases the frame to the allocator exactly
//!     once (no double-free);
//!   * (I3) concurrent [`Frame::from_in_use`] (clone) and `Drop` cannot
//!     use-after-free.
//!
//! The proof also encodes the *broken* unconditional `fetch_add(1)` clone
//! — the Asterinas paper Fig. 9 UB — and shows it violates (I1), proving
//! the conditional `fetch_update` in [`Frame::from_in_use`] is load-bearing.
//! Verified against the pinned Verus toolchain recorded in
//! `verification/verus.toml`; run `just verify` to re-check. Any change to
//! the atomic protocol below must keep `frame_refcount.rs` in sync (see
//! `verification/STATUS.md`).

use core::any::TypeId;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use slopos_abi::addr::PhysAddr;

use crate::sync::BspToken;

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

/// `ref_count` sentinel: the slot is free and claimable by
/// [`Frame::from_unused`]. NOT zero — [`init_meta_slots`] seeds every
/// slot to this, so a freshly-zeroed slot (`0`) is `BUSY`, never `UNUSED`.
pub(crate) const REF_COUNT_UNUSED: u32 = u32::MAX;
/// `ref_count` transient: a slot is being constructed (`from_unused`)
/// or destructed (`Drop`/`into_phys_release`). The owning operation holds
/// the slot exclusively — `from_unused` retries and `from_in_use` refuses
/// while a slot reads `BUSY`. Chosen as `0` so `Drop`'s `fetch_sub(1)`
/// from the last live ref (`1`) lands here automatically.
pub(crate) const REF_COUNT_BUSY: u32 = 0;
/// Largest live reference count. Values above this are reserved for the
/// sentinels; `from_in_use` refuses to bump past it (overflow guard).
pub(crate) const REF_COUNT_MAX: u32 = i32::MAX as u32;

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
    /// The slot's whole lifecycle state machine, folded into one atomic:
    /// [`REF_COUNT_UNUSED`] = free/claimable, [`REF_COUNT_BUSY`] =
    /// transient construct/destruct (exclusively owned), `1..=`
    /// [`REF_COUNT_MAX`] = that many live `Frame` handles.
    pub(crate) ref_count: AtomicU32,
    /// Pointer to the `MetaVtable` for the inhabiting `M`. The vtable carries
    /// both the `Drop`-dispatch callbacks and the type's canonical
    /// [`core::any::TypeId`]. Its *pointer* is never compared for identity
    /// (a `const`-promoted vtable static has no unique address across crates
    /// / codegen units — in release `-Zshare-generics` is off, so each crate
    /// inlines its own copy; comparing those addresses falsely reported a
    /// live `RingMeta` slot as a type mismatch and wedged desktop bring-up).
    /// Type identity reads the `TypeId` *value* it carries — see
    /// [`Frame::from_in_use`]. Null while the slot is `UNUSED`.
    pub(crate) vtable: AtomicPtr<MetaVtable>,
    /// Type-erased storage for `M`. Only valid while the slot is live
    /// (`ref_count` in `1..=REF_COUNT_MAX`).
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
            ref_count: AtomicU32::new(REF_COUNT_UNUSED),
            vtable: AtomicPtr::new(core::ptr::null_mut()),
            storage: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

// ---------------------------------------------------------------------------
// MetaVtable: type-erased dispatch for Drop.
// ---------------------------------------------------------------------------

/// Per-`M` dispatch table installed by [`Frame::from_unused`]. Carries the
/// type-erased `Drop`-dispatch callbacks (`drop_in_place` / `returns_frame`)
/// and the type's canonical [`TypeId`].
///
/// Built via the associated-const pattern in [`HasVtable`]. Because that is a
/// `const` (not a `static`), its `&`-promoted referent has internal linkage
/// and **no guaranteed unique address across crates / codegen units** — so a
/// `MetaVtable` *pointer* must NEVER be compared for identity to decide a
/// slot's type. Instead, type identity reads the [`TypeId`] *value* this
/// table carries: `TypeId::of::<M>()` is identical in every crate by language
/// guarantee, regardless of which crate's vtable copy a slot happens to hold.
/// This mirrors Asterinas's `ostd`, which likewise stores its `DynMetadata`
/// vtable for dispatch only and decides type identity via `core::any::TypeId`
/// (`Any::is`), never by comparing the vtable pointer.
pub struct MetaVtable {
    /// Run `core::ptr::drop_in_place::<M>` on the storage payload.
    pub(crate) drop_in_place: unsafe fn(*mut u8),
    /// Query [`AnyFrameMeta::returns_frame_on_last_drop`] on the live
    /// payload. A side-effect-free read: the `Frame` lifecycle uses it to
    /// decide whether to hand the backing page back to the allocator, and
    /// runs that free itself *after* the slot is reset to UNUSED. The
    /// query MUST NOT free the page (see the `Drop` impl's ordering).
    pub(crate) returns_frame: unsafe fn(*const u8) -> bool,
    /// The canonical [`TypeId`] of `M`. A value, not a pointer — identical
    /// across all crates/codegen units, so it is the sound cross-crate
    /// type-identity key (`from_in_use` compares this against
    /// `TypeId::of::<M>()`). Returned through a function pointer rather than
    /// stored inline so it needs no const-eval of `TypeId::of` and matches
    /// the other dispatch entries.
    pub(crate) type_id: fn() -> TypeId,
}

// SAFETY: the dispatched function pointers `drop_in_place`,
// `returns_frame`, and `type_id` only act on storage owned by the
// surrounding `MetaSlot` (or return a compile-time constant), and
// synchronisation is through the slot's atomics.
unsafe impl Sync for MetaVtable {}

unsafe fn drop_in_place_for<M: AnyFrameMeta>(payload: *mut u8) {
    // SAFETY: caller (`Drop for Frame<M>`) holds the only remaining
    // ref and has just transitioned the slot to BUSY, so we
    // have exclusive access to the M payload at `payload`.
    unsafe {
        core::ptr::drop_in_place(payload as *mut M);
    }
}

unsafe fn returns_frame_for<M: AnyFrameMeta>(payload: *const u8) -> bool {
    // SAFETY: same as `drop_in_place_for` — exclusive access to a
    // valid M payload at `payload`; this only reads it.
    unsafe { (*(payload as *const M)).returns_frame_on_last_drop() }
}

/// Returns the canonical [`TypeId`] of `M`. Stored as a fn pointer in each
/// `MetaVtable` so the *value* (which is identical across crates) is the
/// type-identity key, never the vtable's address.
fn type_id_of<M: AnyFrameMeta>() -> TypeId {
    TypeId::of::<M>()
}

trait HasVtable {
    const VTABLE: &'static MetaVtable;
}

impl<M: AnyFrameMeta> HasVtable for M {
    const VTABLE: &'static MetaVtable = &MetaVtable {
        drop_in_place: drop_in_place_for::<M>,
        returns_frame: returns_frame_for::<M>,
        type_id: type_id_of::<M>,
    };
}

/// The dispatch vtable for `M`. The returned pointer is safe to **call
/// through** from any crate, but its address is NOT stable across crates (it
/// is a `const`-promoted internal static — see [`MetaVtable`]). For type
/// identity compare the `TypeId` *value* it carries (`(*vt).type_id)()`),
/// never this pointer.
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
/// associated constants must match `Self`.
pub unsafe trait AnyFrameMeta: Send + Sync + Sized + 'static {
    const SIZE: usize = size_of::<Self>();
    const ALIGN: usize = align_of::<Self>();

    /// Whether dropping the last `Frame<Self>` for a slot should return
    /// its backing physical page to the registered [`FrameAlloc`].
    ///
    /// This is a **pure query** on the live payload, evaluated by the
    /// `Frame` lifecycle *before* the payload is dropped. It MUST NOT free
    /// the page itself: the lifecycle owns the free and runs it only after
    /// the slot has been reset to UNUSED, so a free-listed page is never
    /// observable with a still-TYPED slot (the ordering `from_unused`
    /// relies on — see `Frame`'s `Drop` impl).
    ///
    /// Most metas own their page and return it (the `true` default).
    /// Override to `false` for frames whose page is owned elsewhere — the
    /// statically-borrowed kernel-master page table
    /// ([`PageTableMeta::static_borrowed`]) and externally-managed DMA
    /// segments.
    fn returns_frame_on_last_drop(&self) -> bool {
        true
    }
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

// SAFETY: ZST has no representation invariants. Owns its page, so it
// takes the default `returns_frame_on_last_drop == true` — required so
// `Frame<KernelMeta>` does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for KernelMeta {}
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

// SAFETY: fields are plain data. The page-table frame returns to the
// allocator on last Drop unless the meta declares `static_borrowed` (the
// bootloader-owned kernel-master PML4), which must never be freed.
unsafe impl AnyFrameMeta for PageTableMeta {
    fn returns_frame_on_last_drop(&self) -> bool {
        !self.static_borrowed
    }
}
const _: () = assert_meta_fits::<PageTableMeta>();

/// Untyped anonymous frame metadata. Returns its physical frame to
/// the registered [`FrameAlloc`] on `Drop`.
#[derive(Default)]
pub struct AnonymousMeta;

// SAFETY: ZST has no representation invariants. Owns its page, so it
// takes the default `returns_frame_on_last_drop == true` — required so
// `UFrame<AnonymousMeta>` does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for AnonymousMeta {}
const _: () = assert_meta_fits::<AnonymousMeta>();

/// Page-cache frame metadata. One slot per cached page of file or
/// block-device contents; the metadata carries the dirty bit and an
/// opaque owner-backref key chosen by the consumer (e.g. the ext2
/// `BlockCache` encodes the on-disk block number; a future
/// file-page-cache layer could pack `(inode, page_index)`). Atomics
/// so the dirty bit can be sampled / flipped from shared references
/// without taking the consumer's outer lock for the read.
///
/// Sized exactly at the [`MAX_META_SIZE`] cap: `AtomicU8 (1) +
/// 7 padding + AtomicU64 (8) = 16` bytes. `assert_meta_fits` below
/// is the load-bearing compile-time gate.
#[derive(Default)]
pub struct PageCacheMeta {
    /// 0 = clean, 1 = dirty. `AtomicU8` so consumers can read the
    /// dirty bit through a shared `Frame` borrow without exclusive
    /// access. Producers (the writer that mutates the page bytes)
    /// must already hold an exclusive view of the page contents, so
    /// the dirty store and the byte writes are linearised by the
    /// consumer's slot lock.
    pub dirty: AtomicU8,
    /// Opaque owner-backref key. Encoding is the consumer's choice;
    /// the ext2 `BlockCache` stores the on-disk block number. Atomic
    /// so a future inode-keyed variant can update it without holding
    /// the slot lock (e.g. during background writeback).
    pub owner_key: AtomicU64,
}

// SAFETY: payload is two atomics with no cross-field invariants
// beyond `Atomic*`'s own contract. Owns its page, so it takes the
// default `returns_frame_on_last_drop == true` — required so the
// `Frame<PageCacheMeta>` does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for PageCacheMeta {}
const _: () = assert_meta_fits::<PageCacheMeta>();

/// Network packet-buffer frame metadata. One slot per pre-allocated
/// packet buffer; the buffer's bytes live in the frame and are reached
/// through [`Frame::<PacketMeta>::as_bytes`] / [`as_bytes_mut`].
///
/// The single `reserved` field is currently unused by the network
/// stack — it exists so the typed-metadata shape parallels
/// [`PageCacheMeta`] and a future RX-offload path can stamp a cached
/// length or flags without taking the pool lock. Atomic so that future
/// use needs no metadata re-versioning; sized well within the
/// [`MAX_META_SIZE`] cap (`AtomicU64` = 8 bytes).
#[derive(Default)]
pub struct PacketMeta {
    /// Reserved for network-layer use; left zero today.
    pub reserved: AtomicU64,
}

// SAFETY: payload is a single atomic with no cross-field invariant
// beyond `AtomicU64`'s own contract. Owns its page, so it takes the
// default `returns_frame_on_last_drop == true` so a `Frame<PacketMeta>`
// does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for PacketMeta {}
const _: () = assert_meta_fits::<PacketMeta>();

/// Frame metadata for a SlopRing shared-memory region page (SLOPRING
/// § 5.2). A ring's SQ/CQ live in `Frame<RingMeta>`s mapped read+write
/// into both the kernel (HHDM) and the owning process (`cursor_mut`,
/// exactly the `process_vm_mmap_shared` path). `RingMeta` is *dual*:
/// `AnyFrameMeta` (so it owns a real frame, freed on last `Drop`) **and**
/// `AnyUFrameMeta` (so the kernel may only reach the bytes through the
/// `UFrame` byte-copy / volatile interface, never a `&Sqe` / `&mut Cqe`
/// — AD-3 / Inv. 4/5). Modelled on [`AnonymousMeta`], the existing
/// `AnyUFrameMeta` exemplar — *not* on `PacketMeta`/`PageCacheMeta`,
/// which are `AnyFrameMeta`-only kernel-owned types.
///
/// The payload carries the owning ring's generation-handle bits so a
/// stray mapping can be traced back to its ring; it is one `AtomicU64`
/// (8 B), well within [`MAX_META_SIZE`].
#[derive(Default)]
pub struct RingMeta {
    /// Generation-handle bits of the owning ring object. Atomic so the
    /// owner can stamp/clear it without holding the frame's slot lock.
    pub ring_handle_bits: AtomicU64,
}

// SAFETY: payload is a single atomic with no cross-field invariant
// beyond `AtomicU64`'s own contract. Owns its page, so it takes the
// default `returns_frame_on_last_drop == true` so a `Frame<RingMeta>`
// does not leak the page on its last Drop.
unsafe impl AnyFrameMeta for RingMeta {}
const _: () = assert_meta_fits::<RingMeta>();

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

/// One-shot boot wiring point. The `&BspToken<'brand>` witnesses
/// BSP-only init; `slots` must point to `len` zero-initialised,
/// non-aliased [`MetaSlot`]s valid for the static lifetime of the
/// kernel — that's a raw-pointer soundness obligation the type
/// system cannot express, so the `# Safety` block survives as an
/// inline contract on the unsafe deref below.
pub fn init_meta_slots<'brand>(_token: &BspToken<'brand>, slots: *mut MetaSlot, len: usize) {
    // Seed every slot to UNUSED. The production caller hands us zeroed
    // pages, and zero is `REF_COUNT_BUSY`, not `UNUSED` — without this the
    // first `from_unused` on any frame would observe BUSY and spin forever.
    // The vtable needs no seeding: zeroed = null = "no type", correct for an
    // unused slot.
    // SAFETY: the caller certified `[slots, slots + len)` is a valid,
    // exclusively-owned `[MetaSlot]` for the kernel's static lifetime; this
    // runs BSP-only (witnessed by `_token`) before any other CPU or any
    // `Frame` operation can observe the array, so the relaxed stores are
    // published by the `Release` `base.swap` below.
    for i in 0..len {
        unsafe {
            (*slots.add(i))
                .ref_count
                .store(REF_COUNT_UNUSED, Ordering::Relaxed);
        }
    }
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
/// `Frame<M>` drops the inline `M`, resets the slot to UNUSED, and —
/// when `M::returns_frame_on_last_drop()` is `true` — returns the
/// underlying physical frame to the registered allocator (in that order;
/// see the `Drop` impl).
pub struct Frame<M: AnyFrameMeta, S: init_state::InitState = init_state::Zeroed> {
    ptr: *const MetaSlot,
    _marker: PhantomData<(M, S)>,
}

// SAFETY: `Frame<M, S>` is a thin wrapper over a pointer into the
// static `META_SLOTS` array; sharing/sending across threads is sound
// because `M: Send + Sync` (transitively required by `AnyFrameMeta`)
// and ref-count manipulation is atomic. `S` is a zero-size phantom
// state and contributes no runtime data.
unsafe impl<M: AnyFrameMeta, S: init_state::InitState> Send for Frame<M, S> {}
unsafe impl<M: AnyFrameMeta, S: init_state::InitState> Sync for Frame<M, S> {}

/// Typed initialisation-state markers for [`Frame<M, S>`].
///
/// The two states encode whether the kernel has proven the frame's
/// 4 KiB region currently reads as all-zero:
///
/// - [`Zeroed`] — every byte was either freshly allocated through a
///   zero-on-alloc path or scrubbed via [`Frame::scrub`]. Safe to use
///   anywhere. The default state for [`Frame::alloc`] etc.
/// - [`Uninit`] — the frame was acquired through an explicit
///   `unsafe { ... }` opt-out path and may still hold the previous
///   owner's bytes. Cannot be passed to APIs that require zeroed
///   memory (kernel stacks, page tables, task slots) until promoted
///   via [`Frame::scrub`] or [`Frame::assume_zeroed`].
///
/// Bug history motivating the typestate: a kernel test wrote
/// `(i & 0xFF) as u8` at offset `i` to a page, freed it without
/// scrubbing; the buddy returned the dirty page; a subsequent `ret`
/// decoded `0xd8..0xdf` at offsets `0xd8..0xdf` as a return address;
/// the CPU jumped to `0xdfdedddcdbdad9d8`. With the typestate,
/// constructing a `Frame<_, Zeroed>` from a non-zero region requires
/// `unsafe { Frame::assume_zeroed }` — an explicit audit point.
pub mod init_state {
    /// The frame's 4 KiB region currently reads as all-zero.
    #[derive(Debug)]
    pub enum Zeroed {}
    /// The frame may hold the previous owner's bytes.
    #[derive(Debug)]
    pub enum Uninit {}

    /// Sealed marker trait — only [`Zeroed`] and [`Uninit`] are
    /// allowed as the `S` type parameter on [`super::Frame`].
    pub trait InitState: sealed::Sealed + 'static {}
    impl InitState for Zeroed {}
    impl InitState for Uninit {}

    mod sealed {
        pub trait Sealed {}
        impl Sealed for super::Zeroed {}
        impl Sealed for super::Uninit {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// `paddr` falls outside the `META_SLOTS` array.
    OutOfRange,
    /// `META_SLOTS` not initialised.
    NotInitialised,
    /// Slot was not in the expected state (e.g. `from_unused` on a
    /// live slot, or `from_in_use` on an UNUSED/BUSY one).
    StateMismatch,
}

impl<M: AnyFrameMeta, S: init_state::InitState> Frame<M, S> {
    /// Wrap a freshly-allocated, currently-unused physical frame and
    /// install `meta` into its slot.
    ///
    /// Returns [`FrameError::NotInitialised`] when [`init_meta_slots`]
    /// has not yet been called, [`FrameError::OutOfRange`] when
    /// `paddr` does not have a slot, and [`FrameError::StateMismatch`]
    /// when the slot is already live.
    ///
    /// **Soundness invariant (Inv. 1, Asterinas paper §4.3).** This is
    /// the framekernel's single entry point for claiming a physical
    /// frame as a typed `Frame<M>`. The atomic UNUSED→BUSY→live `ref_count`
    /// transition below means at most one `Frame<M>` exists for any
    /// given `paddr` at any time; together with the contract on
    /// `FrameAlloc::alloc` (registered via
    /// [`crate::mm::frame_alloc::register_frame_allocator`]) — that
    /// the injected allocator only returns paddrs corresponding to
    /// currently unused physical memory — every successful return
    /// from `from_unused` originates from memory not aliased to any
    /// other live OSTD object.
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
        // Claim the slot UNUSED -> BUSY. BUSY is the transient state a
        // concurrent construct/destruct of the *same* paddr holds; spin
        // while we observe it and retry, then claim. (In practice a
        // dropping frame's BUSY window is unreachable here — the page is
        // not returned to the allocator until after the slot is UNUSED, so
        // nobody is handed this paddr mid-teardown — but the interlock makes
        // the claim sound regardless of the free-vs-reset ordering.) A live
        // count is a genuine StateMismatch. The Acquire pairs with the
        // final `Release` store of UNUSED in `Drop`, so on a successful
        // claim the prior occupant's `drop_in_place` has happened-before our
        // `storage` write below.
        loop {
            match slot.ref_count.compare_exchange_weak(
                REF_COUNT_UNUSED,
                REF_COUNT_BUSY,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(REF_COUNT_UNUSED) => {} // spurious weak failure — retry
                Err(REF_COUNT_BUSY) => core::hint::spin_loop(),
                Err(_) => return Err(FrameError::StateMismatch),
            }
        }
        // SAFETY: the CAS above transitioned the slot UNUSED -> BUSY, so we
        // hold it exclusively until we publish the live `ref_count` below.
        // This is the moment OSTD trusts Inv. 1: the caller has certified
        // that `paddr` came from currently-unused memory (via the registered
        // FrameAlloc), so no other live `Frame<M'>` aliases the bytes we are
        // about to write.
        unsafe {
            let storage = slot.storage.get() as *mut M;
            core::ptr::write(storage, meta);
        }
        // Publish the dispatch+identity vtable before the live ref_count. The
        // `Acquire` load of `vtable` in `from_in_use` pairs with this
        // `Release` store, so a matching type implies this `storage` write is
        // visible.
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
    /// The caller's `paddr` must point to a live slot (`ref_count` in
    /// `1..=REF_COUNT_MAX`) with the same `M`; mismatches return
    /// [`FrameError::StateMismatch`].
    pub fn from_in_use(paddr: Paddr) -> Result<Self, FrameError> {
        let slot = meta_slot_for(paddr).ok_or(FrameError::OutOfRange)?;
        // Type-identity gate: the slot must already hold an `M`. We compare
        // the `TypeId` *value* the slot's vtable carries against
        // `TypeId::of::<M>()` — NOT the vtable *pointer*. A `const`-promoted
        // vtable static has no unique cross-crate address (release builds
        // disable `-Zshare-generics`, so the `mm` crate's `vtable_for::<M>()`
        // and the slopos-ostd copy stored by `from_unused` differ); the old
        // pointer compare therefore spuriously rejected live `RingMeta` slots
        // and wedged desktop bring-up. `TypeId::of::<M>()` is identical in
        // every crate by language guarantee, so the value check is sound
        // across the `mm`/`slopos-ostd` boundary (this is how Asterinas's
        // `ostd` does it — vtable for dispatch, `TypeId` for identity).
        //
        // The `Acquire` load pairs with `from_unused`'s `Release` vtable
        // store. A null vtable means the slot is `UNUSED` (no `M`) → reject
        // before dereferencing. The bump below is the atomic "live AND
        // increment"; reading the type first is sound because the caller
        // holds an existing ref (the `from_in_use` contract), so the slot
        // cannot be torn down and re-typed between this load and the bump.
        let vt = slot.vtable.load(Ordering::Acquire);
        if vt.is_null() {
            return Err(FrameError::StateMismatch);
        }
        // SAFETY: `vt` is non-null, so it points at the `'static` `MetaVtable`
        // a `from_unused` published for this slot; reading its `type_id` fn
        // pointer and calling it returns a compile-time-constant `TypeId`.
        if unsafe { ((*vt).type_id)() } != TypeId::of::<M>() {
            return Err(FrameError::StateMismatch);
        }
        // Conditional increment — succeed only from a *live* count. `Drop`'s
        // `fetch_sub(1)` from the last ref lands the slot at `BUSY` (0)
        // before it tears the slot down; an unconditional `fetch_add(1)`
        // would bump it and revive a slot whose `drop_in_place` is already
        // running (the wrong vtable / a half-destroyed `M`). Refusing
        // `BUSY`/`UNUSED`/overflow forbids that.
        //
        // VERIFIED: `verification/proofs/frame_refcount.rs` proves this
        // conditional bump preserves the slot invariant on every reachable
        // state, and that the unconditional `fetch_add(1)` alternative
        // (the Asterinas Fig. 9 UB) violates it. This refusal-to-revive is
        // the single line the use-after-free proof (I3) leans on.
        slot.ref_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |prev| {
                if prev == REF_COUNT_BUSY || prev == REF_COUNT_UNUSED || prev >= REF_COUNT_MAX {
                    None
                } else {
                    Some(prev + 1)
                }
            })
            .map_err(|_| FrameError::StateMismatch)?;
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
        // `Frame` was constructed; the ref-count is in `1..=REF_COUNT_MAX`
        // for the lifetime of `self` (so never a sentinel here).
        let rc = unsafe { (*self.ptr).ref_count.load(Ordering::Acquire) };
        live_ref_count(rc)
    }
}

/// Map a raw `ref_count` to a caller-facing live count: the `UNUSED` and
/// `BUSY` sentinels both report `0` (no live owner).
#[inline]
fn live_ref_count(raw: u32) -> u32 {
    match raw {
        REF_COUNT_UNUSED | REF_COUNT_BUSY => 0,
        n => n,
    }
}

/// Peek at the META_SLOTS refcount for `paddr` without constructing
/// a `Frame<M>` (no inc / dec). Returns `0` for paddrs whose slot is
/// UNUSED, BUSY, or out of range. Used by COW resolution in slopos-mm to
/// decide single- vs multi-owner without disturbing the count.
pub fn reference_count_at(paddr: Paddr) -> u32 {
    let Some(slot) = meta_slot_for(paddr) else {
        return 0;
    };
    live_ref_count(slot.ref_count.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------
// Read-only slot diagnostics.
//
// Classify a frame's `MetaSlot` (decode the `TypeId` its vtable carries into
// a concrete meta kind) and report META_SLOTS coverage, for error-path
// logging where a caller otherwise sees only an opaque error. Read-only
// (Acquire loads, no mutation), so sound to call from any context.
// ---------------------------------------------------------------------------

/// Which built-in [`AnyFrameMeta`] a [`MetaSlot`] currently holds, decoded
/// from the [`TypeId`] its vtable carries (for live slots) or its `ref_count`
/// sentinel (for free / transient slots).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotMetaKind {
    /// `ref_count == REF_COUNT_UNUSED`: free and claimable by `from_unused`.
    Unused,
    /// `ref_count == REF_COUNT_BUSY`: a transient construct/destruct window
    /// (some other CPU owns the slot exclusively right now).
    Busy,
    Kernel,
    PageTable,
    Anonymous,
    PageCache,
    Packet,
    Ring,
    DmaCoherent,
    DmaStream,
    /// Live (non-sentinel `ref_count`) but the vtable's `TypeId` matches none
    /// of the known built-in metas — a sign of slot corruption.
    Unknown,
}

/// Read-only snapshot of a physical frame's [`MetaSlot`] for diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct SlotSnapshot {
    /// `false` when `paddr` lies outside the installed META_SLOTS array
    /// (the constructor would have returned [`FrameError::OutOfRange`]).
    pub in_range: bool,
    /// `false` when [`init_meta_slots`] has not run yet
    /// ([`FrameError::NotInitialised`]).
    pub initialised: bool,
    /// The raw `ref_count` word, sentinels included.
    pub raw_ref_count: u32,
    /// The stored vtable pointer as an integer (`0` == null).
    pub vtable_addr: usize,
    /// The decoded metadata kind.
    pub kind: SlotMetaKind,
}

/// Decode the [`TypeId`] a slot's vtable carries into a diagnostic
/// [`SlotMetaKind`]. Compares `TypeId` *values* (crate-stable), so it agrees
/// with `from_in_use`'s identity gate — unlike the old vtable-pointer
/// classifier, which compared addresses and could disagree across crates.
fn classify_vtable_typeid(vt: *const MetaVtable) -> SlotMetaKind {
    use crate::mm::dma::{DmaCoherentMeta, DmaStreamMeta};
    if vt.is_null() {
        return SlotMetaKind::Unknown;
    }
    // SAFETY: a non-null vtable points at the `'static` `MetaVtable` a
    // `from_unused` published; calling its `type_id` fn returns a
    // compile-time-constant `TypeId` value.
    let tid = unsafe { ((*vt).type_id)() };
    if tid == TypeId::of::<KernelMeta>() {
        SlotMetaKind::Kernel
    } else if tid == TypeId::of::<PageTableMeta>() {
        SlotMetaKind::PageTable
    } else if tid == TypeId::of::<AnonymousMeta>() {
        SlotMetaKind::Anonymous
    } else if tid == TypeId::of::<PageCacheMeta>() {
        SlotMetaKind::PageCache
    } else if tid == TypeId::of::<PacketMeta>() {
        SlotMetaKind::Packet
    } else if tid == TypeId::of::<RingMeta>() {
        SlotMetaKind::Ring
    } else if tid == TypeId::of::<DmaCoherentMeta>() {
        SlotMetaKind::DmaCoherent
    } else if tid == TypeId::of::<DmaStreamMeta>() {
        SlotMetaKind::DmaStream
    } else {
        SlotMetaKind::Unknown
    }
}

/// Capture a read-only [`SlotSnapshot`] for `paddr`. Cheap; intended for
/// cold diagnostic paths only.
pub fn slot_snapshot(paddr: Paddr) -> SlotSnapshot {
    if META_SLOTS.base.load(Ordering::Acquire).is_null() {
        return SlotSnapshot {
            in_range: false,
            initialised: false,
            raw_ref_count: 0,
            vtable_addr: 0,
            kind: SlotMetaKind::Unknown,
        };
    }
    let Some(slot) = meta_slot_for(paddr) else {
        return SlotSnapshot {
            in_range: false,
            initialised: true,
            raw_ref_count: 0,
            vtable_addr: 0,
            kind: SlotMetaKind::Unknown,
        };
    };
    let rc = slot.ref_count.load(Ordering::Acquire);
    let vt = slot.vtable.load(Ordering::Acquire);
    let kind = match rc {
        REF_COUNT_UNUSED => SlotMetaKind::Unused,
        REF_COUNT_BUSY => SlotMetaKind::Busy,
        _ => classify_vtable_typeid(vt),
    };
    SlotSnapshot {
        in_range: true,
        initialised: true,
        raw_ref_count: rc,
        vtable_addr: vt as usize,
        kind,
    }
}

/// META_SLOTS coverage: `(len_slots, max_paddr_exclusive, initialised)`. Any
/// `paddr >= max_paddr_exclusive` has no slot, so the frame constructors
/// return [`FrameError::OutOfRange`] for it.
pub fn meta_slots_coverage() -> (usize, u64, bool) {
    if META_SLOTS.base.load(Ordering::Acquire).is_null() {
        return (0, 0, false);
    }
    let len = META_SLOTS.len.load(Ordering::Acquire);
    (len, (len as u64) * (PAGE_SIZE as u64), true)
}

impl<M: AnyFrameMeta, S: init_state::InitState> Frame<M, S> {
    pub fn borrow(&self) -> &M {
        // SAFETY: the slot is live for the lifetime of `self`
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
        match slot.ref_count.load(Ordering::Acquire) {
            REF_COUNT_UNUSED | REF_COUNT_BUSY => return Err(FrameError::StateMismatch),
            _ => {}
        }
        // SAFETY: caller's contract above. The slot is live and exactly
        // one ref is outstanding (leaked); the new `Frame` takes ownership
        // of that ref without bumping the count.
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
impl Frame<KernelMeta, init_state::Zeroed> {
    /// Allocate a single kernel page, zeroed. The default — and the
    /// only safe path. The page allocator's `init_on_alloc=1`-style
    /// zero-by-default contract guarantees the returned region reads
    /// as all-zero, matching the [`init_state::Zeroed`] state tag.
    pub fn alloc(opts: FrameAllocOptions) -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        // Always request zeroing, regardless of `opts.zeroing`. The
        // type tag is the source of truth; runtime opts cannot weaken
        // the contract.
        let opts = opts.zeroed();
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
        Self::alloc(FrameAllocOptions::single())
    }

    /// Safe wrapper: allocate a fresh zeroed `Frame<KernelMeta>` and
    /// immediately release its `MetaSlot` to `UNUSED`, returning the
    /// raw `Paddr` for handoff to legacy raw-paddr free paths.
    ///
    /// Returns [`Paddr::null`] on allocation failure. The
    /// "sole owner" invariant required by [`Frame::into_phys_release`]
    /// is satisfied automatically because the `Frame` produced by
    /// [`Self::alloc`] is the only handle for the slot at the moment
    /// of this call.
    ///
    /// Caller takes ownership of the returned `Paddr` and is solely
    /// responsible for eventual deallocation via
    /// `slopos_mm::page_alloc::free_page_frame`.
    pub fn alloc_release_phys(opts: FrameAllocOptions) -> Paddr {
        match Self::alloc(opts) {
            Some(f) => {
                // SAFETY: `f` is the sole `Frame` handle for the slot
                // — `Self::alloc` returned it moments ago and we have
                // not exposed it to any other consumer. The
                // sole-owner invariant of `into_phys_release` is
                // therefore upheld.
                unsafe { f.into_phys_release() }
            }
            None => Paddr::NULL,
        }
    }
}

impl Frame<KernelMeta, init_state::Uninit> {
    /// Hot-path opt-out: allocate a page **without** scrubbing.
    ///
    /// Returns a [`Frame<KernelMeta, Uninit>`]; the caller cannot pass
    /// it anywhere that expects [`init_state::Zeroed`] until promoting
    /// it via [`Frame::scrub`] (writes 4 KiB of zeros) or
    /// [`Frame::assume_zeroed`] (caller-asserts the bytes are
    /// already zero, e.g. for fresh BSS-mapped pages).
    ///
    /// # Safety
    ///
    /// The caller must guarantee that they will overwrite the entire
    /// 4 KiB region before any reader observes it, *or* that the
    /// allocator gave them a page whose contents are sourced from a
    /// trusted producer. Wild-jump RIPs of the form
    /// `0xdfdedddcdbdad9d8` are the canonical symptom of getting this
    /// wrong — they are `(i & 0xFF) as u8`-pattern bytes from a
    /// previous owner that the kernel's `ret` decoded as a return
    /// address.
    pub unsafe fn alloc_uninit(opts: FrameAllocOptions) -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        // Explicitly disable zeroing — caller has accepted the
        // burden of post-conditioning the page contents.
        let opts = FrameAllocOptions {
            zeroing: false,
            ..opts
        };
        let paddr = alloc.alloc(opts)?;
        match Self::from_unused(paddr, KernelMeta) {
            Ok(frame) => Some(frame),
            Err(_) => {
                alloc.dealloc(paddr, opts.size_pages.max(1));
                None
            }
        }
    }
}

impl<M: AnyFrameMeta> Frame<M, init_state::Uninit> {
    /// Scrub the frame's 4 KiB region and promote the typestate
    /// to [`init_state::Zeroed`]. Always-safe; performs the actual
    /// memset.
    pub fn scrub(self) -> Frame<M, init_state::Zeroed> {
        // SAFETY: HHDM mapping covers every alloc-able physical
        // frame; the caller-visible `self` proves we have exclusive
        // access (Frame is move-only on the typestate axis).
        unsafe {
            let virt = crate::mm::phys::phys_to_virt(self.paddr());
            core::ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE);
        }
        // SAFETY: bytes are now all-zero, satisfying the
        // `Zeroed` invariant.
        unsafe { self.assume_zeroed() }
    }

    /// Assert that the frame's contents are already all-zero and
    /// promote the typestate without performing a memset.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that every byte of the 4 KiB region
    /// reads as zero. Misuse re-introduces the
    /// `0xdfdedddcdbdad9d8`-class bug the typestate exists to
    /// prevent.
    pub unsafe fn assume_zeroed(self) -> Frame<M, init_state::Zeroed> {
        let ptr = self.ptr;
        // Bypass Drop on `self` by leaking its handle into the new
        // typestate-tagged value; the underlying refcount is
        // unchanged.
        core::mem::forget(self);
        Frame {
            ptr,
            _marker: PhantomData,
        }
    }
}

// State-agnostic methods on `Frame<KernelMeta, S>`. The KernelMeta
// API surface (HHDM-backed reads/writes, raw conversions) doesn't
// care whether the frame's bytes are zero or not — only allocation
// and the [`Frame::scrub`]/[`Frame::assume_zeroed`] state-promotion
// helpers do.
impl<S: init_state::InitState> Frame<KernelMeta, S> {
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
    /// dropping the underlying ref. The slot stays live with one
    /// outstanding ref; the caller is responsible for either re-wrapping
    /// it via [`Frame::from_raw_at`] or releasing the page another way.
    #[inline]
    pub fn into_phys(self) -> Paddr {
        let paddr = self.paddr();
        let _slot = self.into_raw();
        paddr
    }

    /// Consume the handle, **release** the underlying [`MetaSlot`] to
    /// `UNUSED` (drops the inline metadata in place, resets refcount
    /// to zero), and return the raw physical address. The page is
    /// **not** returned to the buddy allocator — the caller takes
    /// ownership of the raw `Paddr` and is responsible for freeing
    /// it via `free_page_frame` or transferring ownership to a
    /// non-`Frame` consumer.
    ///
    /// Bridge for legacy raw-`PhysAddr` call sites that have not yet
    /// migrated to holding a `Frame<KernelMeta>` directly. The
    /// allocation goes through the typestate (zero-by-default,
    /// `unsafe { alloc_uninit }` audit point) but the resulting page
    /// is handed off as a raw paddr so existing free paths continue
    /// to work unchanged.
    ///
    /// # Safety
    ///
    /// **Caller's invariant**: no other `Frame` handle exists for the
    /// returned `Paddr` at the moment of this call. The typestate alone
    /// is *not* sufficient to guarantee this — [`Frame::from_in_use`]
    /// can produce additional handles aliasing the same slot via a
    /// conditional ref-count CAS. Callers must therefore preserve the
    /// "sole owner" property by other means (e.g. by holding the only
    /// reference returned from `Frame::alloc` and never publishing the
    /// `Paddr` to a `from_in_use` consumer).
    ///
    /// After this call, no [`MetaSlot`] reservation tracks the page.
    /// The caller becomes solely responsible for eventual deallocation
    /// (typically via `slopos_mm::page_alloc::free_page_frame`). A
    /// missed free leaks the page; a double-free corrupts the buddy
    /// allocator.
    pub unsafe fn into_phys_release(self) -> Paddr {
        let paddr = self.paddr();
        // SAFETY: caller has asserted (above invariant) that `self` is the
        // sole `Frame` handle for the slot, so its ref_count must equal 1.
        // The debug_assert traps test-time misuse — e.g. a `from_in_use`-
        // aliased handle reaching this path. Steps:
        //   1. Move the slot to `BUSY` so a concurrent `from_in_use`
        //      refuses and `from_unused` retries while we tear down.
        //   2. Drop the inline `M` payload via the vtable's `drop_in_place`.
        //   3. Do NOT return the page to the allocator (unlike `Drop`); the
        //      caller takes ownership of the raw paddr.
        //   4. Null the vtable and publish `ref_count = UNUSED` so a future
        //      `from_unused` for this paddr succeeds.
        unsafe {
            let slot = &*self.ptr;
            debug_assert_eq!(
                slot.ref_count.load(Ordering::Acquire),
                1,
                "into_phys_release: ref_count != 1; aliased handle exists \
                 (typestate alone does not guarantee uniqueness — see SAFETY note)"
            );
            slot.ref_count.store(REF_COUNT_BUSY, Ordering::Release);
            let vt = slot.vtable.load(Ordering::Acquire);
            let storage = slot.storage.get() as *mut u8;
            ((*vt).drop_in_place)(storage);
            slot.vtable.store(core::ptr::null_mut(), Ordering::Release);
            slot.ref_count.store(REF_COUNT_UNUSED, Ordering::Release);
        }
        // Skip Drop entirely — we just hand-released the slot above.
        core::mem::forget(self);
        paddr
    }

    /// Read a `T: Pod` at byte offset `offset` inside this frame.
    /// Returns `None` if `offset + size_of::<T>()` would exceed
    /// `PAGE_SIZE_4KB`.
    pub fn read_at<T: crate::mm::Pod>(&self, offset: usize) -> Option<T> {
        let needed = core::mem::size_of::<T>();
        if offset.checked_add(needed)? > crate::mm::page_table::PAGE_SIZE_4KB as usize {
            return None;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *const T;
        // SAFETY: HHDM mapping covers the frame; offset bounds-checked;
        // `T: Pod` makes any byte pattern a valid `T`. `read_unaligned`
        // lifts the alignment requirement.
        Some(unsafe { core::ptr::read_unaligned(p) })
    }

    /// Read a `T: Pod` via a *volatile* load — for device-visible ring
    /// buffer slots that the hardware updates concurrently.
    pub fn read_volatile_at<T: crate::mm::Pod>(&self, offset: usize) -> Option<T> {
        let needed = core::mem::size_of::<T>();
        if offset.checked_add(needed)? > crate::mm::page_table::PAGE_SIZE_4KB as usize {
            return None;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *const T;
        // SAFETY: see `read_at`. Volatile semantics are required when
        // the hardware can mutate the slot under us.
        Some(unsafe { core::ptr::read_volatile(p) })
    }

    /// Write `value` at byte offset `offset`. Returns `false` if the
    /// write would extend past the frame.
    pub fn write_at<T: crate::mm::Pod>(&self, offset: usize, value: &T) -> bool {
        let needed = core::mem::size_of::<T>();
        if offset
            .checked_add(needed)
            .map(|e| e > crate::mm::page_table::PAGE_SIZE_4KB as usize)
            .unwrap_or(true)
        {
            return false;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *mut T;
        // SAFETY: bounds-checked; `T: Pod` so the write of any byte
        // pattern is valid.
        unsafe {
            core::ptr::write_unaligned(p, *value);
        }
        true
    }

    /// Volatile sibling of [`Self::write_at`].
    pub fn write_volatile_at<T: crate::mm::Pod>(&self, offset: usize, value: T) -> bool {
        let needed = core::mem::size_of::<T>();
        if offset
            .checked_add(needed)
            .map(|e| e > crate::mm::page_table::PAGE_SIZE_4KB as usize)
            .unwrap_or(true)
        {
            return false;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *mut T;
        // SAFETY: see `write_at`. Volatile is required for ring slots
        // visible to a hardware consumer.
        unsafe {
            core::ptr::write_volatile(p, value);
        }
        true
    }

    /// Copy `src` into the frame starting at `offset`. Returns `false`
    /// if the slice would not fit.
    pub fn write_slice(&self, offset: usize, src: &[u8]) -> bool {
        if offset
            .checked_add(src.len())
            .map(|e| e > crate::mm::page_table::PAGE_SIZE_4KB as usize)
            .unwrap_or(true)
        {
            return false;
        }
        let dst = (self.virt_addr_u64() as usize + offset) as *mut u8;
        // SAFETY: bounds-checked above; `src` and the HHDM destination
        // do not alias because `src` lives in kernel-stack / heap and
        // the HHDM is a fresh mapping.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        }
        true
    }

    /// Copy `dst.len()` bytes starting at `offset` out of the frame.
    /// Returns `false` if the slice would not fit.
    pub fn read_slice(&self, offset: usize, dst: &mut [u8]) -> bool {
        if offset
            .checked_add(dst.len())
            .map(|e| e > crate::mm::page_table::PAGE_SIZE_4KB as usize)
            .unwrap_or(true)
        {
            return false;
        }
        let src = (self.virt_addr_u64() as usize + offset) as *const u8;
        // SAFETY: bounds-checked above; the byte slice destination
        // is unique for the call's duration.
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        }
        true
    }

    /// Borrow a byte view into the frame at `offset` for `len` bytes.
    /// The borrow's lifetime is the caller's borrow of `self`. Returns
    /// `None` if out-of-bounds.
    pub fn slice_at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset.checked_add(len)? > crate::mm::page_table::PAGE_SIZE_4KB as usize {
            return None;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *const u8;
        // SAFETY: bounds-checked; the HHDM mapping outlives `&self`
        // and no other path can mutate this byte range without going
        // through one of the safe `Frame` methods.
        Some(unsafe { core::slice::from_raw_parts(p, len) })
    }

    /// Mutable sibling of [`Self::slice_at`].
    pub fn slice_at_mut(&mut self, offset: usize, len: usize) -> Option<&mut [u8]> {
        if offset.checked_add(len)? > crate::mm::page_table::PAGE_SIZE_4KB as usize {
            return None;
        }
        let p = (self.virt_addr_u64() as usize + offset) as *mut u8;
        // SAFETY: `&mut self` makes this borrow unique.
        Some(unsafe { core::slice::from_raw_parts_mut(p, len) })
    }
}

// ---------------------------------------------------------------------------
// Frame<PageCacheMeta> convenience surface.
//
// Mirrors the `Frame<KernelMeta>` HHDM accessors but specialised to the
// page-cache meta. Consumers (the ext2 `BlockCache` is the only one
// today) hold one `Frame<PageCacheMeta>` per cached page and need:
//
//   * a safe `alloc()` that pairs `FrameAlloc::alloc` with `from_unused`,
//   * `&[u8]` / `&mut [u8]` views over the page's HHDM mapping for the
//     existing `data() -> &[u8]` / `data_mut() -> &mut [u8]` call sites,
//   * atomic getters / setters for the dirty bit and owner-key backref.
//
// The `&mut [u8]` view is sound because page-cache slots have a strict
// one-handle-per-paddr invariant: `BlockCache` constructs its frames
// via `Frame::alloc` and never publishes the paddr to a `from_in_use`
// consumer, so `&mut Frame<PageCacheMeta>` is the only mutable view of
// the page bytes. The SAFETY comments on the helpers document the
// invariant.
// ---------------------------------------------------------------------------

impl Frame<PageCacheMeta, init_state::Zeroed> {
    /// Allocate a single zeroed page and install a fresh
    /// [`PageCacheMeta`] (dirty=0, owner_key=0) into its slot. Returns
    /// `None` if no allocator is registered or the buddy returned no
    /// page. Mirrors [`Frame::<KernelMeta>::alloc`] for the
    /// page-cache flavour.
    pub fn alloc() -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        let opts = FrameAllocOptions::single().zeroed();
        let paddr = alloc.alloc(opts)?;
        match Self::from_unused(paddr, PageCacheMeta::default()) {
            Ok(frame) => Some(frame),
            Err(_) => {
                alloc.dealloc(paddr, 1);
                None
            }
        }
    }
}

impl<S: init_state::InitState> Frame<PageCacheMeta, S> {
    /// Kernel HHDM virtual address pointing at this frame's contents.
    #[inline]
    pub fn virt_addr_u64(&self) -> u64 {
        crate::mm::phys::phys_to_virt(self.paddr()) as u64
    }

    /// Read-only byte view of the full 4 KiB frame through the HHDM
    /// mapping. The borrow's lifetime is the caller's borrow of `self`.
    pub fn as_bytes(&self) -> &[u8] {
        let p = self.virt_addr_u64() as *const u8;
        // SAFETY: the slot is live (ref_count >= 1) for `&self`,
        // so the HHDM mapping is live for the returned borrow's
        // lifetime. The 4 KiB range fits a single physical frame.
        // Shared `&[u8]` from a shared `&Frame` is consistent with
        // OSTD's stance for kernel-typed pages (slab, page tables).
        unsafe { core::slice::from_raw_parts(p, crate::mm::page_table::PAGE_SIZE_4KB as usize) }
    }

    /// Mutable byte view of the full 4 KiB frame through the HHDM
    /// mapping. Requires `&mut self` to enforce exclusive access at
    /// the source level.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let p = self.virt_addr_u64() as *mut u8;
        // SAFETY: `&mut self` guarantees this is the only outstanding
        // borrow of the frame through *this* handle. The page-cache
        // contract (one `Frame<PageCacheMeta>` handle per paddr; no
        // `from_in_use` calls on `PageCacheMeta`) ensures no second
        // handle exists either. The HHDM mapping is live.
        unsafe { core::slice::from_raw_parts_mut(p, crate::mm::page_table::PAGE_SIZE_4KB as usize) }
    }

    /// `true` iff the page is currently marked dirty.
    #[inline]
    pub fn dirty(&self) -> bool {
        self.borrow().dirty.load(Ordering::Acquire) != 0
    }

    /// Set / clear the dirty bit.
    #[inline]
    pub fn set_dirty(&self, dirty: bool) {
        self.borrow()
            .dirty
            .store(u8::from(dirty), Ordering::Release);
    }

    /// Read the opaque owner-backref key.
    #[inline]
    pub fn owner_key(&self) -> u64 {
        self.borrow().owner_key.load(Ordering::Acquire)
    }

    /// Publish a new owner-backref key.
    #[inline]
    pub fn set_owner_key(&self, key: u64) {
        self.borrow().owner_key.store(key, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Frame<PacketMeta> convenience surface.
//
// Mirrors the Frame<PageCacheMeta> shape: `alloc` pairs the registered
// frame allocator with `from_unused`; `as_bytes`/`as_bytes_mut` expose
// the frame's bytes through the HHDM mapping. The `&mut self` on
// `as_bytes_mut` makes exclusive access compiler-enforced — the packet
// pool lends each frame to exactly one PacketBuf at a time, so the
// borrow checker (not an informal convention) guarantees there is no
// second mutable view of the page bytes.
// ---------------------------------------------------------------------------

impl Frame<PacketMeta, init_state::Zeroed> {
    /// Allocate a single zeroed page and install a fresh
    /// [`PacketMeta`] into its slot. Returns `None` if no allocator is
    /// registered or the buddy returned no page.
    pub fn alloc() -> Option<Self> {
        let alloc = crate::mm::frame_alloc::current_frame_allocator()?;
        let opts = FrameAllocOptions::single().zeroed();
        let paddr = alloc.alloc(opts)?;
        match Self::from_unused(paddr, PacketMeta::default()) {
            Ok(frame) => Some(frame),
            Err(_) => {
                alloc.dealloc(paddr, 1);
                None
            }
        }
    }
}

impl<S: init_state::InitState> Frame<PacketMeta, S> {
    /// Kernel HHDM virtual address pointing at this frame's contents.
    #[inline]
    pub fn virt_addr_u64(&self) -> u64 {
        crate::mm::phys::phys_to_virt(self.paddr()) as u64
    }

    /// Read-only byte view of the full 4 KiB frame through the HHDM
    /// mapping. The borrow's lifetime is the caller's borrow of `self`.
    pub fn as_bytes(&self) -> &[u8] {
        let p = self.virt_addr_u64() as *const u8;
        // SAFETY: the slot is live (ref_count >= 1) for `&self`,
        // so the HHDM mapping is live for the returned borrow's
        // lifetime. The 4 KiB range fits a single physical frame.
        unsafe { core::slice::from_raw_parts(p, crate::mm::page_table::PAGE_SIZE_4KB as usize) }
    }

    /// Mutable byte view of the full 4 KiB frame through the HHDM
    /// mapping. Requires `&mut self` to enforce exclusive access at the
    /// source level.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let p = self.virt_addr_u64() as *mut u8;
        // SAFETY: `&mut self` guarantees this is the only outstanding
        // borrow of the frame through *this* handle, and the packet
        // pool holds one `Frame<PacketMeta>` handle per paddr (no
        // `from_in_use` calls on `PacketMeta`), so no second handle
        // exists either. The HHDM mapping is live.
        unsafe { core::slice::from_raw_parts_mut(p, crate::mm::page_table::PAGE_SIZE_4KB as usize) }
    }
}

impl<M: AnyFrameMeta, S: init_state::InitState> Drop for Frame<M, S> {
    fn drop(&mut self) {
        // SAFETY: `ptr` points at a live `MetaSlot` for as long as
        // `ref_count > 0`, which is true while this `Frame` is alive.
        let slot = unsafe { &*self.ptr };
        // Release on the decrement; Acquire fence on the last-ref path.
        // `fetch_sub(1)` from the last live ref (1) lands the slot at `BUSY`
        // (0): we now own it exclusively for teardown — `from_unused`
        // retries and `from_in_use` refuses while it reads BUSY.
        if slot.ref_count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        core::sync::atomic::fence(Ordering::Acquire);
        let vt = slot.vtable.load(Ordering::Acquire);
        let storage = slot.storage.get() as *mut u8;
        let paddr = self.paddr();
        // Publish the slot as UNUSED before returning the page, so a
        // free-listed paddr always has a claimable slot. Order:
        //   1. Snapshot the return-the-page decision while the payload is
        //      live (`PageTableMeta` reads `self.static_borrowed`).
        //   2. Drop the inline `M`. The slot is BUSY, so this is the only
        //      handle: exclusive access.
        //   3. Publish the slot UNUSED (vtable null, then `ref_count`). The
        //      page is still ours, so no `from_unused` for this paddr races.
        //   4. Return the page. Later `from_unused` on it observes UNUSED.
        // SAFETY: sole remaining reference; `vtable` is the static
        // `MetaVtable` for the installed `M`; `storage` holds a valid `M`
        // until `drop_in_place`.
        let return_page = unsafe { ((*vt).returns_frame)(storage as *const u8) };
        unsafe {
            ((*vt).drop_in_place)(storage);
        }
        slot.vtable.store(core::ptr::null_mut(), Ordering::Release);
        slot.ref_count.store(REF_COUNT_UNUSED, Ordering::Release);
        if return_page {
            return_frame_to_allocator(paddr);
        }
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
    /// Bypass the per-CPU page cache (PCP). Equivalent to the legacy
    /// `ALLOC_FLAG_NO_PCP` flag — used by stress / OOM tests that
    /// want every alloc to hit the global buddy free-lists rather
    /// than draining the per-CPU stack.
    pub no_pcp: bool,
    /// Restrict to DMA-suitable physical memory (legacy
    /// `ALLOC_FLAG_DMA`). Wired through to the buddy's order-shift
    /// machinery; preserves the policy semantics for callers that
    /// program DMA descriptors with the returned physical address.
    pub dma: bool,
}

impl FrameAllocOptions {
    pub const fn single() -> Self {
        Self {
            size_pages: 1,
            zeroing: false,
            align_pages: 1,
            no_pcp: false,
            dma: false,
        }
    }

    pub const fn zeroed(self) -> Self {
        Self {
            zeroing: true,
            ..self
        }
    }

    pub const fn with_no_pcp(self) -> Self {
        Self {
            no_pcp: true,
            ..self
        }
    }

    pub const fn with_dma(self) -> Self {
        Self { dma: true, ..self }
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

    // MetaSlot layout (ref_count @ 0, alignment >= MAX_META_ALIGN) is
    // pinned by the `const _` asserts beside the struct definition; no
    // runtime duplicate here.

    #[test]
    fn meta_size_fits() {
        assert!(KernelMeta::SIZE <= MAX_META_SIZE);
        assert!(PageTableMeta::SIZE <= MAX_META_SIZE);
        assert!(AnonymousMeta::SIZE <= MAX_META_SIZE);
        assert!(PageCacheMeta::SIZE <= MAX_META_SIZE);
        assert!(PacketMeta::SIZE <= MAX_META_SIZE);
    }

    #[test]
    fn page_cache_meta_atomics_default_to_zero() {
        let m = PageCacheMeta::default();
        assert_eq!(m.dirty.load(Ordering::Acquire), 0);
        assert_eq!(m.owner_key.load(Ordering::Acquire), 0);
    }

    #[test]
    fn packet_meta_atomics_default_to_zero() {
        let m = PacketMeta::default();
        assert_eq!(m.reserved.load(Ordering::Acquire), 0);
    }

    #[test]
    fn vtables_carry_distinct_canonical_type_ids() {
        // The type-identity gate in `from_in_use` compares the `TypeId` value
        // each builtin's vtable carries. `TypeId::of::<M>()` is unique per
        // type by language guarantee, so two metas can never collide — this
        // test documents that and checks the vtable plumbing returns the
        // canonical value (not, say, the same fn for every M).
        let ids = [
            (vtable_for::<KernelMeta>().type_id)(),
            (vtable_for::<PageTableMeta>().type_id)(),
            (vtable_for::<AnonymousMeta>().type_id)(),
            (vtable_for::<PageCacheMeta>().type_id)(),
            (vtable_for::<PacketMeta>().type_id)(),
            (vtable_for::<RingMeta>().type_id)(),
        ];
        let expected = [
            TypeId::of::<KernelMeta>(),
            TypeId::of::<PageTableMeta>(),
            TypeId::of::<AnonymousMeta>(),
            TypeId::of::<PageCacheMeta>(),
            TypeId::of::<PacketMeta>(),
            TypeId::of::<RingMeta>(),
        ];
        for (i, (got, want)) in ids.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got, want, "vtable {i} carries the wrong TypeId");
            for other in &ids[i + 1..] {
                assert_ne!(got, other, "two builtins share a TypeId (impossible)");
            }
        }
    }
}
