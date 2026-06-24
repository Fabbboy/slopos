//! Managed device resources — a LIFO-ordered, type-erased owned-resource bag.
//!
//! A driver acquires resources (DMA buffers, IRQ bindings, MMIO windows)
//! during bring-up and must release them in reverse order if bring-up fails
//! partway, or when the device later unbinds. [`Devres`] makes that automatic:
//! each acquired resource is `attach`ed and the bag drops everything it holds
//! in reverse attach order when it is dropped. The registry hands an empty bag
//! to a driver's probe and, on failure, simply drops it — every resource's own
//! `Drop` runs, so there are no hand-rolled per-error-site teardown paths.
//!
//! The bag is type-erased ([`KBox<dyn ResourceObject>`]) so it can hold a
//! heterogeneous mix of resource types. Erasure is a safe unsizing coercion;
//! the only `unsafe` is the `&T` reborrow of a freshly-boxed resource, absorbed
//! by ostd's [`crate::dev::FromRawPtr`].

use crate::dev::FromRawPtr;
use crate::{KBox, KVec};

/// Anything a [`Devres`] bag can own: a thread-safe, owned value.
///
/// The blanket impl makes every qualifying type a resource with no
/// per-type boilerplate; the bound is exactly what type-erasing into a
/// `'static` `Send + Sync` trait object requires.
pub trait ResourceObject: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> ResourceObject for T {}

/// Failure to attach a resource to a [`Devres`] bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevresError {
    /// The heap could not box the resource or grow the bag.
    OutOfMemory,
}

/// A LIFO-ordered bag of type-erased owned resources.
///
/// Resources drop in reverse attach order: the last thing acquired is the
/// first thing released, so teardown unwinds bring-up exactly.
pub struct Devres {
    cells: KVec<KBox<dyn ResourceObject>>,
}

impl Devres {
    /// An empty bag. `const` so it can seed a registry slot's default
    /// variant without a heap touch.
    pub const fn new() -> Self {
        Self { cells: KVec::new() }
    }

    /// Take ownership of `res` and return a borrowed view valid for as long
    /// as the bag is borrowed.
    ///
    /// The resource is boxed on the heap (its rvalue never lands in the
    /// caller's stack frame), unsize-coerced into the bag, and pushed. On a
    /// heap failure the resource's own `Drop` runs immediately, so a failed
    /// attach never leaks. The returned `&T` aliases the boxed value, which
    /// lives in its own allocation and is therefore stable across any
    /// internal regrowth of the bag.
    pub fn attach<T: Send + Sync + 'static>(&mut self, res: T) -> Result<&T, DevresError> {
        let boxed = KBox::try_new(res).map_err(|_| DevresError::OutOfMemory)?;
        // Stable heap address of the payload: pushing the box below moves
        // only the fat pointer, never the `T` itself.
        let ptr: *const T = &*boxed;
        let erased: KBox<dyn ResourceObject> = boxed;
        self.cells
            .push(erased)
            .map_err(|_| DevresError::OutOfMemory)?;
        // The payload is owned by `self` and outlives this borrow; the
        // reborrow's `unsafe` is encapsulated in ostd's `FromRawPtr`.
        Ok(<T as FromRawPtr>::from_ptr_unchecked(ptr))
    }

    /// Number of resources currently held.
    #[inline]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether the bag holds no resources.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cells.len() == 0
    }
}

impl Default for Devres {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Devres {
    fn drop(&mut self) {
        // Pop to drop in reverse attach order. A bare `KVec`/`Vec` drop runs
        // front-to-back, so the explicit reverse drain is what makes release
        // LIFO (e.g. an IRQ binding masks delivery before a DMA buffer that a
        // late interrupt could have touched is unmapped).
        while let Some(cell) = self.cells.pop() {
            drop(cell);
        }
    }
}
