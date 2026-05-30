//! Slab-allocator trait surface.
//!
//! OSTD ships only the trait; concrete implementations live outside
//! the trusted core. The trait is consumed by code that needs a
//! per-type free-list (per-CPU caches, fixed-size object pools)
//! without paying for the general-purpose heap path on hot paths.
//!
//! # Verification
//!
//! The slab object lifecycle backing this trait (the kernel-side
//! `mm::slab::allocator::SlabAllocator<SIZE>` grow/alloc/dealloc path and
//! `mm::slab::KernelSlab`'s size-class dispatch) is machine-checked under
//! Verus. `verification/proofs/slab_lifetime.rs` mirrors the per-class slab
//! page as an abstract state machine and proves the two slab soundness
//! invariants:
//!
//!   * (Inv. 9) a slot — or any object derived from it — cannot outlive its
//!     parent slab: an outstanding cell pins its page, so a page is never
//!     reclaimed to the buddy while a cell is in flight (no dangling slot);
//!   * (Inv. 10) an object is built from a slot only when the slot meets the
//!     object's size and alignment: the size-class chooser always returns a
//!     cell at least as large (and at least 16-byte aligned) as the request.
//!
//! The proof also encodes the *broken* unconditional reclaim (free a page
//! with live cells) and the *broken* always-smallest size class, and shows
//! each violates its invariant — proving the `outstanding == 0` reclaim
//! guard and the size-class scan are load-bearing. Verified against the
//! pinned Verus toolchain in `verification/verus.toml`; run `just verify`
//! to re-check. See `verification/STATUS.md`.

/// Per-type slab allocator.
///
/// Producers hand out `Slot` values that the consumer later returns
/// via `dealloc`. A `Slot` is anything that uniquely identifies a
/// slab cell — typically a `KBox<T>` for owning slabs, or a typed
/// index for indexed slabs.
pub trait Slab: Send + Sync {
    type Slot;

    fn alloc(&self) -> Option<Self::Slot>;
    fn dealloc(&self, slot: Self::Slot);
}
