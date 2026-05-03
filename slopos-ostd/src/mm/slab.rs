//! Slab-allocator trait surface.
//!
//! OSTD ships only the trait; concrete implementations live outside
//! the trusted core. The trait is consumed by code that needs a
//! per-type free-list (per-CPU caches, fixed-size object pools)
//! without paying for the general-purpose heap path on hot paths.

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
