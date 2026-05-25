//! Generation-counter handles for slot-based tables.
//!
//! [`Handle<T>`] is a small `Copy` token pairing a 32-bit slot index with
//! a 64-bit generation. [`HandleTable<T>`] stores values in recyclable
//! slots; every removal bumps the slot's generation, so a handle minted
//! before a slot was reused resolves to [`HandleError::Stale`] rather than
//! silently aliasing the slot's new occupant. This is the canonical
//! defence against use-after-reuse on kernel object tables (file
//! descriptors, pipes, process address spaces, tasks): a stale reference
//! becomes a typed error, never undefined behaviour.
//!
//! # Growth modes
//!
//! - [`HandleTable::new`] / [`HandleTable::with_capacity`] build a
//!   *growable* table: [`insert`](HandleTable::insert) appends a fresh
//!   slot (reallocating the spine) when no recycled slot is free.
//! - [`HandleTable::with_fixed_capacity`] pre-fills every slot up front
//!   and forbids growth, so the backing spine's pointer and length are
//!   stable for the table's whole life. This is the contract that lets a
//!   caller scan the table lock-free (see
//!   [`SpinLock::read_atomic_field`](crate::sync::SpinLock::read_atomic_field))
//!   without observing a reallocated spine — the table never moves a slot
//!   body once `insert` has placed it.
//!
//! # Generation
//!
//! The generation lives in the slot, not the value, and is bumped on
//! [`remove`](HandleTable::remove). A handle carries the generation that
//! was current when it was minted; resolution compares the two. Slot
//! reuse therefore never produces an aliasing read — the old handle's
//! generation no longer matches.

use core::marker::PhantomData;

use crate::mm::heap::{AllocError, KVec};

/// A `Copy` token identifying one slot of a [`HandleTable`].
///
/// The `PhantomData<fn() -> T>` marker makes `Handle<T>` unconditionally
/// `Copy`/`Send`/`Sync` regardless of `T`, so a handle can live inside a
/// `#[derive(Copy)]` struct and reference a table whose value type is
/// itself non-`Copy`.
pub struct Handle<T> {
    slot: u32,
    generation: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// Reconstruct a handle from its raw parts.
    ///
    /// Used by callers that must store a handle in a narrower encoding
    /// (e.g. packed into a `usize`) and rebuild it before resolution.
    /// Forging a handle is harmless: [`HandleTable`] validates the slot
    /// and generation on every access, so a bogus handle resolves to a
    /// typed [`HandleError`], never an aliasing read.
    #[inline]
    pub const fn from_parts(slot: u32, generation: u64) -> Self {
        Self {
            slot,
            generation,
            _marker: PhantomData,
        }
    }

    /// The slot index this handle refers to.
    #[inline]
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    /// The generation this handle was minted with.
    #[inline]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

// Hand-written trait impls (rather than `#[derive]`) so the bounds stay
// free of `T`: a `Handle<T>` is plain data regardless of `T`.
impl<T> Clone for Handle<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Handle<T> {}
impl<T> PartialEq for Handle<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot && self.generation == other.generation
    }
}
impl<T> Eq for Handle<T> {}
impl<T> core::hash::Hash for Handle<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.slot.hash(state);
        self.generation.hash(state);
    }
}
impl<T> core::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handle")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Why a [`Handle`] failed to resolve against its [`HandleTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleError {
    /// The slot's generation no longer matches the handle — the slot was
    /// removed and (possibly) reused since the handle was minted.
    Stale,
    /// The slot index lies outside the table's spine.
    OutOfBounds,
    /// The slot exists but currently holds no value.
    NoEntry,
    /// A fixed-capacity table has no free slot left for an insert.
    Full,
}

struct HandleSlot<T> {
    value: Option<T>,
    generation: u64,
}

/// A table of generation-checked slots.
///
/// See the [module documentation](self) for the growth-mode and
/// generation contracts.
pub struct HandleTable<T> {
    slots: KVec<HandleSlot<T>>,
    /// Recycled slot indices, ready for the next `insert`.
    free_list: KVec<u32>,
    /// Number of currently-occupied slots.
    live: usize,
    /// One past the highest slot index ever occupied. Bounds iteration so
    /// scans skip slots that have never held a value.
    high_water: u32,
    /// When `true`, `insert` never grows the spine (returns
    /// [`HandleError::Full`] instead) — the lock-free-scan contract.
    fixed_capacity: bool,
}

impl<T> HandleTable<T> {
    /// An empty, growable table. No allocation.
    pub const fn new() -> Self {
        Self {
            slots: KVec::new(),
            free_list: KVec::new(),
            live: 0,
            high_water: 0,
            fixed_capacity: false,
        }
    }

    /// A growable table with `cap` slots reserved up front (so the first
    /// `cap` inserts do not reallocate the spine).
    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            slots: KVec::with_capacity(cap)?,
            free_list: KVec::new(),
            live: 0,
            high_water: 0,
            fixed_capacity: false,
        })
    }

    /// A fixed-capacity table: every slot is pre-created empty, growth is
    /// forbidden, and the backing spine never reallocates. Required for
    /// tables scanned lock-free.
    pub fn with_fixed_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut slots = KVec::with_capacity(cap)?;
        let mut free_list = KVec::with_capacity(cap)?;
        for _ in 0..cap {
            slots.push(HandleSlot {
                value: None,
                generation: 0,
            })?;
        }
        // Reverse order so the first `insert` consumes slot 0, keeping the
        // high-water mark tight for early allocations.
        for idx in (0..cap as u32).rev() {
            free_list.push(idx)?;
        }
        Ok(Self {
            slots,
            free_list,
            live: 0,
            high_water: 0,
            fixed_capacity: true,
        })
    }

    /// Insert `value`, returning a fresh handle.
    ///
    /// Reuses a recycled slot when one is free; otherwise appends a new
    /// slot (growable mode) or fails with [`HandleError::Full`]
    /// (fixed-capacity mode).
    pub fn insert(&mut self, value: T) -> Result<Handle<T>, HandleError> {
        if let Some(idx) = self.free_list.pop() {
            let slot = &mut self.slots[idx as usize];
            debug_assert!(slot.value.is_none());
            slot.value = Some(value);
            let generation = slot.generation;
            self.live += 1;
            if idx + 1 > self.high_water {
                self.high_water = idx + 1;
            }
            return Ok(Handle::from_parts(idx, generation));
        }
        if self.fixed_capacity {
            return Err(HandleError::Full);
        }
        let idx = self.slots.len() as u32;
        self.slots
            .push(HandleSlot {
                value: Some(value),
                generation: 0,
            })
            .map_err(|_| HandleError::Full)?;
        self.live += 1;
        self.high_water = idx + 1;
        Ok(Handle::from_parts(idx, 0))
    }

    fn resolve(&self, h: Handle<T>) -> Result<usize, HandleError> {
        let idx = h.slot as usize;
        let slot = self.slots.get(idx).ok_or(HandleError::OutOfBounds)?;
        if slot.value.is_none() {
            return Err(HandleError::NoEntry);
        }
        if slot.generation != h.generation {
            return Err(HandleError::Stale);
        }
        Ok(idx)
    }

    /// Borrow the value `h` refers to, or report why it is unreachable.
    pub fn get(&self, h: Handle<T>) -> Result<&T, HandleError> {
        let idx = self.resolve(h)?;
        Ok(self.slots[idx].value.as_ref().unwrap())
    }

    /// Mutably borrow the value `h` refers to.
    pub fn get_mut(&mut self, h: Handle<T>) -> Result<&mut T, HandleError> {
        let idx = self.resolve(h)?;
        Ok(self.slots[idx].value.as_mut().unwrap())
    }

    /// Remove and return the value `h` refers to, bumping the slot's
    /// generation so any other handle to that slot becomes
    /// [`HandleError::Stale`].
    pub fn remove(&mut self, h: Handle<T>) -> Result<T, HandleError> {
        let idx = self.resolve(h)?;
        let slot = &mut self.slots[idx];
        let value = slot.value.take().expect("resolved slot is occupied");
        slot.generation = slot.generation.wrapping_add(1);
        // In fixed-capacity mode `free_list` was reserved to `cap`, so this
        // push never reallocates; in growable mode a realloc of the
        // free-list (not the value spine) is harmless.
        let _ = self.free_list.push(idx as u32);
        self.live -= 1;
        Ok(value)
    }

    /// Whether `h` currently resolves to a live value.
    pub fn contains(&self, h: Handle<T>) -> bool {
        self.resolve(h).is_ok()
    }

    /// Number of occupied slots.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether no slot is occupied.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Capacity of the spine (total slots, occupied or not).
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// One past the highest slot index ever occupied. Iteration and
    /// lock-free scans only need to look this far.
    pub fn high_water(&self) -> usize {
        self.high_water as usize
    }

    /// Iterate live `(handle, &value)` pairs, bounded by the high-water
    /// mark.
    ///
    /// This is a plain shared-reference scan, so it is also the lock-free
    /// read path: a caller holding the table's `SpinLock` through
    /// [`read_atomic_field`](crate::sync::SpinLock::read_atomic_field) may
    /// call it **only** when the table was built with
    /// [`with_fixed_capacity`](Self::with_fixed_capacity) (spine never
    /// reallocates) and every field the visitor reads is a tear-free
    /// naturally-aligned load.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> + '_ {
        let bound = self.high_water as usize;
        self.slots[..bound].iter().enumerate().filter_map(|(i, s)| {
            s.value
                .as_ref()
                .map(|v| (Handle::from_parts(i as u32, s.generation), v))
        })
    }

    /// Mutable counterpart to [`iter`](Self::iter).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Handle<T>, &mut T)> + '_ {
        let bound = self.high_water as usize;
        self.slots[..bound]
            .iter_mut()
            .enumerate()
            .filter_map(|(i, s)| {
                let generation = s.generation;
                s.value
                    .as_mut()
                    .map(|v| (Handle::from_parts(i as u32, generation), v))
            })
    }

    /// Iterate the handles of every live slot.
    pub fn handles(&self) -> impl Iterator<Item = Handle<T>> + '_ {
        self.iter().map(|(h, _)| h)
    }
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_roundtrip() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(10).unwrap();
        let b = t.insert(20).unwrap();
        assert_eq!(*t.get(a).unwrap(), 10);
        assert_eq!(*t.get(b).unwrap(), 20);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn get_mut_mutates() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(1).unwrap();
        *t.get_mut(a).unwrap() += 41;
        assert_eq!(*t.get(a).unwrap(), 42);
    }

    #[test]
    fn remove_returns_value_and_empties() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(7).unwrap();
        assert_eq!(t.remove(a).unwrap(), 7);
        assert!(t.is_empty());
    }

    #[test]
    fn out_of_bounds_handle() {
        let t: HandleTable<u32> = HandleTable::new();
        let bogus = Handle::<u32>::from_parts(5, 0);
        assert_eq!(t.get(bogus), Err(HandleError::OutOfBounds));
    }

    #[test]
    fn removed_slot_is_no_entry() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(1).unwrap();
        t.remove(a).unwrap();
        // Same slot, but now empty and the handle's generation is stale.
        // Empty wins: NoEntry.
        assert_eq!(t.get(a), Err(HandleError::NoEntry));
    }

    #[test]
    fn stale_handle_after_reuse() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(100).unwrap();
        t.remove(a).unwrap();
        // Reuse slot 0 for a new value; it gets a bumped generation.
        let b = t.insert(200).unwrap();
        assert_eq!(
            a.slot(),
            b.slot(),
            "slot must be recycled to prove the point"
        );
        assert_eq!(t.get(b).unwrap(), &200);
        // The stale handle must NOT alias the new occupant.
        assert_eq!(t.get(a), Err(HandleError::Stale));
        assert_eq!(t.remove(a), Err(HandleError::Stale));
    }

    #[test]
    fn fixed_capacity_insert_full() {
        let mut t: HandleTable<u32> = HandleTable::with_fixed_capacity(2).unwrap();
        assert_eq!(t.capacity(), 2);
        let _a = t.insert(1).unwrap();
        let _b = t.insert(2).unwrap();
        assert_eq!(t.insert(3), Err(HandleError::Full));
    }

    #[test]
    fn fixed_capacity_recycles_without_growth() {
        let mut t: HandleTable<u32> = HandleTable::with_fixed_capacity(1).unwrap();
        let a = t.insert(1).unwrap();
        t.remove(a).unwrap();
        let b = t.insert(2).unwrap(); // reuses the only slot
        assert_eq!(a.slot(), b.slot());
        assert_eq!(t.get(a), Err(HandleError::Stale));
        assert_eq!(*t.get(b).unwrap(), 2);
        assert_eq!(t.capacity(), 1);
    }

    #[test]
    fn iter_yields_live_only() {
        let mut t: HandleTable<u32> = HandleTable::new();
        let a = t.insert(1).unwrap();
        let b = t.insert(2).unwrap();
        let _c = t.insert(3).unwrap();
        t.remove(b).unwrap();
        let mut vals: alloc::vec::Vec<u32> = t.iter().map(|(_, v)| *v).collect();
        vals.sort_unstable();
        assert_eq!(vals, alloc::vec![1, 3]);
        // The surviving handle still resolves.
        assert_eq!(*t.get(a).unwrap(), 1);
    }

    #[test]
    fn high_water_bounds_iteration() {
        let mut t: HandleTable<u32> = HandleTable::with_fixed_capacity(8).unwrap();
        let a = t.insert(1).unwrap();
        let b = t.insert(2).unwrap();
        assert_eq!(t.high_water(), 2);
        t.remove(a).unwrap();
        t.remove(b).unwrap();
        // High-water stays at its peak; iteration finds nothing live.
        assert_eq!(t.high_water(), 2);
        assert_eq!(t.iter().count(), 0);
    }
}
