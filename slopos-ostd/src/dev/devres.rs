//! Managed device resources — a LIFO-ordered, type-erased owned-resource bag.
//!
//! A driver attaches each resource it acquires during bring-up; the registry
//! drops the bag on probe failure or unbind, so there are no hand-rolled
//! per-error-site teardown paths.

use crate::dev::FromRawPtr;
use crate::{KBox, KVec};

/// Anything a [`Devres`] bag can own: a thread-safe, owned value.
pub trait ResourceObject: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> ResourceObject for T {}

/// Failure to attach a resource to a [`Devres`] bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevresError {
    /// The heap could not box the resource or grow the bag.
    OutOfMemory,
}

/// A bag of type-erased owned resources; drops them in reverse attach order,
/// so teardown unwinds bring-up exactly.
pub struct Devres {
    cells: KVec<KBox<dyn ResourceObject>>,
}

impl Devres {
    /// `const` so it can seed a registry slot's default variant without a heap
    /// touch.
    pub const fn new() -> Self {
        Self { cells: KVec::new() }
    }

    /// Take ownership of `res` and return a borrowed view valid for as long
    /// as the bag is borrowed.
    ///
    /// The returned `&T` points into the resource's own allocation, so it
    /// survives any internal regrowth of the bag. On a heap failure the
    /// resource's own `Drop` runs immediately.
    pub fn attach<T: Send + Sync + 'static>(&mut self, res: T) -> Result<&T, DevresError> {
        let boxed = KBox::try_new(res).map_err(|_| DevresError::OutOfMemory)?;
        let ptr: *const T = &*boxed;
        let erased: KBox<dyn ResourceObject> = boxed;
        self.cells
            .push(erased)
            .map_err(|_| DevresError::OutOfMemory)?;
        // The payload is owned by `self` and outlives this borrow.
        Ok(<T as FromRawPtr>::from_ptr_unchecked(ptr))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

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
        // A bare `KVec` drop runs front-to-back; the reverse drain is what
        // makes release LIFO, e.g. an IRQ binding masks delivery before the
        // DMA buffer a late interrupt could touch is unmapped.
        while let Some(cell) = self.cells.pop() {
            drop(cell);
        }
    }
}
