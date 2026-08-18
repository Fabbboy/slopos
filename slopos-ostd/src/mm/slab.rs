//! Slab-allocator trait surface. OSTD ships only the trait; the
//! implementations live outside the trusted core.
//!
//! # Verification
//!
//! `verification/proofs/slab_lifetime.rs` machine-checks the lifecycle behind
//! this trait:
//!
//!   * (Inv. 9) a slot cannot outlive its parent slab — an outstanding cell
//!     pins its page against buddy reclaim;
//!   * (Inv. 10) a slot is only used for an object it meets the size and
//!     alignment of.

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
