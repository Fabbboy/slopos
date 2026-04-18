//! Kernel-wide allocation surface.
//!
//! This crate is the only kernel crate that depends on `alloc` directly
//! (via `extern crate alloc;`). Every other kernel crate must route heap
//! allocation through the primitives re-exported here. The wrappers exist
//! so that large structs cannot materialise on a caller's stack: the only
//! public constructor for `PinBox<T>` takes a `PinInit<T>`, and the only
//! constructors for `KBox<T>` / `KVec<T>` that allocate-and-fill in place
//! require `T: Zeroable`. By-value constructors (`KBox::try_new`,
//! `KVec::push`, `KArc::try_new`, etc.) exist for small `T`; the ELF
//! post-link `.stack_sizes` gate (`scripts/check_stack_sizes.sh`) enforces
//! the upper bound on what counts as "small".
//!
//! Two `unsafe` blocks live in this module: `boxed_zeroed` and
//! `KVec::zeroed`. Both are guarded by a `T: Zeroable` bound that
//! certifies an all-zero bit pattern is a valid `T`.

#![no_std]
#![feature(allocator_api, coerce_unsized, unsize)]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::ops::{Deref, DerefMut};
use core::pin::Pin;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

pub use alloc::alloc::AllocError;
pub use pinned_init::{
    InPlaceInit, Init, MaybeZeroable, PinInit, Zeroable, init, init_from_closure, init_zeroed,
    pin_data, pin_init, pin_init_from_closure, pinned_drop, try_init, try_pin_init,
};

/// Re-export of the underlying `pinned_init` crate. Consumers needing the
/// `pin_init!` / `try_pin_init!` macros should reach them through this
/// path (`slopos_alloc::pinned_init::pin_init!`) so that the macros'
/// internal `$crate::__init_internal!` reference resolves correctly. The
/// re-exports above are item-level shortcuts that work for trait imports
/// but break macro hygiene across crate boundaries.
pub use pinned_init;

/// Kernel-wide pinned heap cell. The sole public constructor that runs
/// initialisation in-place takes a `PinInit<T>`, so a `T` value never
/// materialises on a caller's stack for the heap-direct path.
pub struct PinBox<T: ?Sized> {
    inner: Pin<Box<T>>,
}

impl<T> PinBox<T> {
    /// Heap-allocate and pin-initialise a `T` in place.
    pub fn pin_init<E>(init: impl PinInit<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        Box::try_pin_init(init).map(|inner| Self { inner })
    }
}

impl<T: Zeroable> PinBox<T> {
    /// Heap-allocate a zero-initialised `T`. Safe because `T: Zeroable`
    /// certifies an all-zero bit pattern is a valid `T`.
    pub fn zeroed() -> Result<Self, AllocError> {
        boxed_zeroed()
    }
}

impl<T> PinBox<T> {
    /// Wrap an existing rvalue `T` in a fresh heap allocation.
    ///
    /// Intended for **small** types where the brief stack
    /// materialisation of `value` is not a stack-safety concern.
    /// Large types (anything close to or above 1 KiB) should use
    /// [`PinBox::pin_init`] or [`PinBox::zeroed`] instead so the `T`
    /// never touches a caller's stack — this is the whole reason
    /// `PinBox` exists.
    ///
    /// The ELF post-link stack-sizes gate (`scripts/check_stack_sizes.sh`)
    /// enforces that rule from the other direction: a frame growing
    /// beyond the threshold will fail the build regardless of which
    /// constructor produced it.
    pub fn try_new(value: T) -> Result<Self, AllocError> {
        let boxed = Box::try_new(value)?;
        Ok(Self {
            inner: Box::into_pin(boxed),
        })
    }
}

impl<T: ?Sized> Deref for PinBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized + Unpin> DerefMut for PinBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        Pin::get_mut(self.inner.as_mut())
    }
}

impl<T: ?Sized> PinBox<T> {
    /// Borrow the wrapped `Pin<&mut T>` without unpinning.
    pub fn as_pin_mut(&mut self) -> Pin<&mut T> {
        self.inner.as_mut()
    }
}

/// Raw byte allocation through the global kernel allocator. Mirrors
/// [`alloc::alloc::alloc`]; intended for low-level pools (RCU node
/// freelist, slab pre-allocation) that manage their own typed
/// constructors. Returns null on failure — callers must check.
///
/// # Safety
/// `layout.size()` must be non-zero. The returned pointer is
/// uninitialised; reading it before writing is UB. Use [`raw_dealloc`]
/// with the same `layout` to free.
pub unsafe fn raw_alloc(layout: core::alloc::Layout) -> *mut u8 {
    // SAFETY: caller upholds layout.size() != 0 invariant.
    unsafe { alloc::alloc::alloc(layout) }
}

/// Raw byte deallocation matching [`raw_alloc`].
///
/// # Safety
/// `ptr` must have been obtained from [`raw_alloc`] with the same
/// `layout`. Double-free is UB.
pub unsafe fn raw_dealloc(ptr: *mut u8, layout: core::alloc::Layout) {
    // SAFETY: caller upholds the prior raw_alloc invariant.
    unsafe { alloc::alloc::dealloc(ptr, layout) }
}

/// Heap-direct zeroed allocation of any `T: Zeroable`.
pub fn boxed_zeroed<T: Zeroable>() -> Result<PinBox<T>, AllocError> {
    let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit()?;
    // SAFETY: `T: Zeroable` ⇒ an all-zero bit pattern is a valid `T`.
    // `write_bytes` zeroes the whole allocation; `assume_init` is then
    // sound. `Box::into_pin` pins the result.
    let init = unsafe {
        let mut boxed = boxed;
        core::ptr::write_bytes(boxed.as_mut_ptr(), 0, 1);
        boxed.assume_init()
    };
    Ok(PinBox {
        inner: Box::into_pin(init),
    })
}

/// Kernel-blessed boxed slot. Fallible. The `zeroed` constructor allocates
/// in place (no stack temp) and requires `T: Zeroable`; `try_new` is the
/// small-`T` escape hatch and shares its caveat with [`PinBox::try_new`].
pub struct KBox<T: ?Sized> {
    inner: Box<T>,
}

impl<T: Zeroable> KBox<T> {
    /// Heap-allocate and zero-initialise. `T: Zeroable` required.
    pub fn zeroed() -> Result<Self, AllocError> {
        let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit()?;
        // SAFETY: see `boxed_zeroed` above; same invariant.
        let inner = unsafe {
            let mut boxed = boxed;
            core::ptr::write_bytes(boxed.as_mut_ptr(), 0, 1);
            boxed.assume_init()
        };
        Ok(Self { inner })
    }
}

impl<T> KBox<T> {
    /// Heap-allocate a value moved from the caller. Same caveat as
    /// [`PinBox::try_new`]: prefer [`KBox::zeroed`] for large `T`.
    pub fn try_new(value: T) -> Result<Self, AllocError> {
        Box::try_new(value).map(|inner| Self { inner })
    }

    /// Heap-allocate and initialise a `T` in place from an `impl Init<T, E>`
    /// recipe. The `T` rvalue never materialises on the caller's stack —
    /// the `init!` macro writes the fields directly into the freshly
    /// allocated heap slot.
    pub fn try_init<E>(init: impl Init<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        Box::try_init(init).map(|inner| Self { inner })
    }

    /// Convert to a raw pointer; caller becomes responsible for freeing
    /// via [`KBox::from_raw`] (or otherwise managing the allocation).
    pub fn into_raw(b: Self) -> *mut T {
        Box::into_raw(b.inner)
    }

    /// Reconstruct a `KBox` from a raw pointer previously obtained via
    /// [`KBox::into_raw`].
    ///
    /// # Safety
    /// `ptr` must originate from a matching `into_raw` call (or another
    /// allocation that `Box::from_raw` would accept) and must not be
    /// aliased.
    pub unsafe fn from_raw(ptr: *mut T) -> Self {
        // SAFETY: caller upholds Box::from_raw invariants.
        Self {
            inner: unsafe { Box::from_raw(ptr) },
        }
    }

    /// Leak the boxed value into a `'static` reference.
    pub fn leak<'a>(b: Self) -> &'a mut T
    where
        T: 'a,
    {
        Box::leak(b.inner)
    }
}

impl<T: ?Sized> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized> AsRef<T> for KBox<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> AsMut<T> for KBox<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized + core::fmt::Debug> core::fmt::Debug for KBox<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&*self.inner, f)
    }
}

impl<T, U> core::ops::CoerceUnsized<KBox<U>> for KBox<T>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{
}

impl<T: ?Sized + core::fmt::Debug> core::fmt::Debug for PinBox<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&*self.inner, f)
    }
}

/// Kernel-blessed fallible `Vec<T>`.
///
/// `KVec::zeroed(len)` is the heap-direct, in-place initialised path
/// (requires `T: Zeroable`). `KVec::with_capacity` / `KVec::push` cover
/// the dynamic-growth case for any `T`; for large `T` callers should
/// rely on the `.stack_sizes` gate to enforce the upper bound on
/// individual `push` rvalues.
pub struct KVec<T> {
    inner: alloc::vec::Vec<T>,
}

impl<T> KVec<T> {
    /// An empty vector, no allocation.
    pub const fn new() -> Self {
        Self {
            inner: alloc::vec::Vec::new(),
        }
    }

    /// Reserve `cap` slots up-front. Fails on allocation error.
    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut v = alloc::vec::Vec::new();
        if cap > 0 {
            v.try_reserve_exact(cap).map_err(|_| AllocError)?;
        }
        Ok(Self { inner: v })
    }

    /// Push a value, growing the backing buffer if needed.
    pub fn push(&mut self, value: T) -> Result<(), AllocError> {
        self.inner.try_reserve(1).map_err(|_| AllocError)?;
        self.inner.push(value);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.inner.pop()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    pub fn truncate(&mut self, len: usize) {
        self.inner.truncate(len);
    }

    /// Drop excess capacity (best-effort; allocator may not honour).
    pub fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }

    /// Move every element from `other` into `self`, leaving `other` empty.
    pub fn append(&mut self, other: &mut Self) {
        self.inner
            .try_reserve(other.inner.len())
            .expect("KVec::append: alloc");
        self.inner.append(&mut other.inner);
    }

    /// Reserve capacity for `additional` more elements. Fallible.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        self.inner.try_reserve(additional).map_err(|_| AllocError)
    }

    /// Reserve capacity for exactly `additional` more elements. Fallible.
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<(), AllocError> {
        self.inner
            .try_reserve_exact(additional)
            .map_err(|_| AllocError)
    }

    /// Append every element of `slice` by `Copy`. Reserves up-front.
    pub fn extend_from_slice(&mut self, slice: &[T]) -> Result<(), AllocError>
    where
        T: Copy,
    {
        self.inner
            .try_reserve(slice.len())
            .map_err(|_| AllocError)?;
        self.inner.extend_from_slice(slice);
        Ok(())
    }

    /// Resize to `new_len`, padding with clones of `value`.
    pub fn resize(&mut self, new_len: usize, value: T) -> Result<(), AllocError>
    where
        T: Clone,
    {
        if new_len > self.inner.len() {
            self.inner
                .try_reserve(new_len - self.inner.len())
                .map_err(|_| AllocError)?;
        }
        self.inner.resize(new_len, value);
        Ok(())
    }

    pub fn drain<R>(&mut self, range: R) -> alloc::vec::Drain<'_, T>
    where
        R: core::ops::RangeBounds<usize>,
    {
        self.inner.drain(range)
    }

    pub fn swap_remove(&mut self, index: usize) -> T {
        self.inner.swap_remove(index)
    }

    pub fn remove(&mut self, index: usize) -> T {
        self.inner.remove(index)
    }

    pub fn insert(&mut self, index: usize, value: T) -> Result<(), AllocError> {
        self.inner.try_reserve(1).map_err(|_| AllocError)?;
        self.inner.insert(index, value);
        Ok(())
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.inner.retain(f);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.inner
    }

    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.inner.iter()
    }

    pub fn iter_mut(&mut self) -> core::slice::IterMut<'_, T> {
        self.inner.iter_mut()
    }

    /// Build a `KVec` from an iterator, surfacing allocation failure.
    /// Honours `size_hint`'s lower bound to amortise reservations.
    pub fn from_iter_fallible<I>(iter: I) -> Result<Self, AllocError>
    where
        I: IntoIterator<Item = T>,
    {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let mut out = Self::with_capacity(lower)?;
        for v in iter {
            out.push(v)?;
        }
        Ok(out)
    }

    /// Forwards [`alloc::vec::Vec::set_len`].
    ///
    /// # Safety
    /// `new_len` must be `<= self.capacity()`, and the elements at
    /// `[old_len, new_len)` must already be initialised (i.e., the
    /// caller has copied valid `T` values into the backing memory).
    pub unsafe fn set_len(&mut self, new_len: usize) {
        // SAFETY: caller upholds Vec::set_len contract.
        unsafe { self.inner.set_len(new_len) }
    }

    /// Splits the vector at `at`, keeping `[0, at)` and returning a new
    /// `KVec` holding `[at, len)`.
    pub fn split_off(&mut self, at: usize) -> Self {
        Self {
            inner: self.inner.split_off(at),
        }
    }
}

impl<T: Zeroable> KVec<T> {
    /// Allocate `len` zeroed elements. Fails with `AllocError` if the
    /// allocation cannot be satisfied.
    pub fn zeroed(len: usize) -> Result<Self, AllocError> {
        let mut v: alloc::vec::Vec<T> = alloc::vec::Vec::new();
        v.try_reserve_exact(len).map_err(|_| AllocError)?;
        // SAFETY: capacity ≥ len (just reserved). `T: Zeroable` ⇒ the
        // zeroed backing memory is a valid sequence of `T`s; we commit
        // that fact via `set_len` after zeroing.
        unsafe {
            core::ptr::write_bytes(v.as_mut_ptr(), 0, len);
            v.set_len(len);
        }
        Ok(Self { inner: v })
    }
}

impl<T: Clone> KVec<T> {
    /// Allocate `len` copies of `value`. Fallible counterpart to the
    /// `vec![value; len]` literal.
    pub fn filled(value: T, len: usize) -> Result<Self, AllocError> {
        let mut out = Self::with_capacity(len)?;
        for _ in 0..len {
            out.inner.push(value.clone());
        }
        Ok(out)
    }
}

impl<T> Default for KVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Clone for KVec<T> {
    /// Panics on allocation failure. Kernel call sites that need
    /// fallible clone should iterate manually with `KVec::push`.
    fn clone(&self) -> Self {
        let mut out = Self::with_capacity(self.inner.len()).expect("KVec::clone: alloc");
        for v in self.inner.iter() {
            out.inner.push(v.clone());
        }
        out
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for KVec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.inner, f)
    }
}

impl<T> FromIterator<T> for KVec<T> {
    /// Panics on allocation failure. Use `KVec::from_iter_fallible` for
    /// `Result`-returning variants.
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_iter_fallible(iter).expect("KVec::from_iter: alloc")
    }
}

impl<T> Extend<T> for KVec<T> {
    /// Panics on allocation failure.
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for v in iter {
            self.push(v).expect("KVec::extend: alloc");
        }
    }
}

impl<T: PartialEq> PartialEq for KVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<T: Eq> Eq for KVec<T> {}

impl<T> Deref for KVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.inner
    }
}

impl<T> DerefMut for KVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.inner
    }
}

impl<T> AsRef<[T]> for KVec<T> {
    fn as_ref(&self) -> &[T] {
        &self.inner
    }
}

impl<T> AsMut<[T]> for KVec<T> {
    fn as_mut(&mut self) -> &mut [T] {
        &mut self.inner
    }
}

impl<T> IntoIterator for KVec<T> {
    type Item = T;
    type IntoIter = alloc::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a KVec<T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut KVec<T> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter_mut()
    }
}

/// Kernel-blessed `Arc<T>`. Fallible constructor; ref-counted clone is
/// infallible (just bumps the refcount).
///
/// As with [`KBox::try_new`] the rvalue passed to [`KArc::try_new`] does
/// briefly land on the caller's stack; large `T` should be constructed
/// via a future `KArc::pin_init` (not yet wired — no kernel call site
/// requires it) or via `pinned_init`'s `Arc::pin_init` directly.
pub struct KArc<T: ?Sized> {
    inner: Arc<T>,
}

impl<T> KArc<T> {
    pub fn try_new(value: T) -> Result<Self, AllocError> {
        Arc::try_new(value).map(|inner| Self { inner })
    }
}

impl<T: ?Sized> Clone for KArc<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: ?Sized> Deref for KArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> AsRef<T> for KArc<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

/// Kernel-blessed `VecDeque<T>`. Same allocation discipline as
/// [`KVec`]: explicit fallible `with_capacity` / `push_back`.
pub struct KVecDeque<T> {
    inner: VecDeque<T>,
}

impl<T> KVecDeque<T> {
    pub const fn new() -> Self {
        Self {
            inner: VecDeque::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut v = VecDeque::new();
        if cap > 0 {
            v.try_reserve_exact(cap).map_err(|_| AllocError)?;
        }
        Ok(Self { inner: v })
    }

    pub fn push_back(&mut self, value: T) -> Result<(), AllocError> {
        self.inner.try_reserve(1).map_err(|_| AllocError)?;
        self.inner.push_back(value);
        Ok(())
    }

    pub fn push_front(&mut self, value: T) -> Result<(), AllocError> {
        self.inner.try_reserve(1).map_err(|_| AllocError)?;
        self.inner.push_front(value);
        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.inner.pop_front()
    }

    pub fn pop_back(&mut self) -> Option<T> {
        self.inner.pop_back()
    }

    pub fn front(&self) -> Option<&T> {
        self.inner.front()
    }

    pub fn front_mut(&mut self) -> Option<&mut T> {
        self.inner.front_mut()
    }

    pub fn back(&self) -> Option<&T> {
        self.inner.back()
    }

    pub fn back_mut(&mut self) -> Option<&mut T> {
        self.inner.back_mut()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    pub fn iter(&self) -> alloc::collections::vec_deque::Iter<'_, T> {
        self.inner.iter()
    }

    pub fn iter_mut(&mut self) -> alloc::collections::vec_deque::IterMut<'_, T> {
        self.inner.iter_mut()
    }

    pub fn drain<R>(&mut self, range: R) -> alloc::collections::vec_deque::Drain<'_, T>
    where
        R: core::ops::RangeBounds<usize>,
    {
        self.inner.drain(range)
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.inner.retain(f);
    }
}

impl<T> Default for KVecDeque<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Kernel-blessed `BTreeMap<K, V>`. `BTreeMap::new` itself does not
/// allocate; insertions reach a node-allocation path that, in the
/// stable allocator API, panics on OOM. The fallible `try_insert`
/// surface is intentionally not exposed because the upstream type
/// does not provide one. Consumers that absolutely need fallible
/// insert should switch to a `KVec`-of-pairs.
pub struct KBTreeMap<K, V> {
    inner: BTreeMap<K, V>,
}

impl<K, V> KBTreeMap<K, V> {
    pub const fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl<K: Ord, V> KBTreeMap<K, V> {
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.inner.insert(key, value)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.inner.remove(key)
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.inner.get(key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.inner.get_mut(key)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.inner.contains_key(key)
    }

    pub fn iter(&self) -> alloc::collections::btree_map::Iter<'_, K, V> {
        self.inner.iter()
    }

    pub fn iter_mut(&mut self) -> alloc::collections::btree_map::IterMut<'_, K, V> {
        self.inner.iter_mut()
    }

    pub fn keys(&self) -> alloc::collections::btree_map::Keys<'_, K, V> {
        self.inner.keys()
    }

    pub fn values(&self) -> alloc::collections::btree_map::Values<'_, K, V> {
        self.inner.values()
    }

    pub fn values_mut(&mut self) -> alloc::collections::btree_map::ValuesMut<'_, K, V> {
        self.inner.values_mut()
    }

    pub fn entry(&mut self, key: K) -> alloc::collections::btree_map::Entry<'_, K, V> {
        self.inner.entry(key)
    }

    pub fn range<R>(&self, range: R) -> alloc::collections::btree_map::Range<'_, K, V>
    where
        R: core::ops::RangeBounds<K>,
    {
        self.inner.range(range)
    }

    pub fn range_mut<R>(&mut self, range: R) -> alloc::collections::btree_map::RangeMut<'_, K, V>
    where
        R: core::ops::RangeBounds<K>,
    {
        self.inner.range_mut(range)
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

impl<K, V> Default for KBTreeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
