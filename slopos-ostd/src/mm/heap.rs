//! Kernel-wide allocation surface: the blessed wrappers (`KBox`, `KVec`,
//! `KArc`, `KVecDeque`, `KBTreeMap`, `PinBox`) plus the global allocator shim
//! (`KernelHeap`).
//!
//! The in-place [`Init<T, E>`] / [`Zeroable`] constructors exist so a large `T`
//! never materialises on a caller's stack; the by-value ones are for small `T`,
//! with `scripts/check_stack_sizes.sh` enforcing the bound. That surface is
//! in-house (see [`super::init`]) because SlopOS has no self-referential kernel
//! types and no in-kernel async.

use core::cell::SyncUnsafeCell;
use core::hint;
use core::mem::MaybeUninit;
use core::num::NonZeroUsize;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering, fence};

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};

pub use alloc::alloc::AllocError;

use super::init::{Init, Zeroable};
use crate::sync::BspToken;

const BUMP_HEAP_SIZE: usize = 2 * 1024 * 1024;

#[repr(C, align(16))]
struct AlignedHeap([u8; BUMP_HEAP_SIZE]);

#[unsafe(link_section = ".bss.heap")]
static BUMP_HEAP: SyncUnsafeCell<AlignedHeap> =
    SyncUnsafeCell::new(AlignedHeap([0; BUMP_HEAP_SIZE]));
static BUMP_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Set to `true` exactly once by [`register_kernel_slab_handle`]; gates
/// `KernelHeap::alloc/dealloc` dispatch.
static BACKEND_LIVE: AtomicBool = AtomicBool::new(false);

/// Registered backend handle: the address of a `'static` reference. Null until
/// [`register_kernel_slab_handle`] publishes it.
static BACKEND_SLOT: AtomicPtr<&'static dyn KernelHeapBackend> =
    AtomicPtr::new(core::ptr::null_mut());

/// Variable-size heap backend consumed by the [`KernelHeap`] global
/// allocator. Exactly one impl is registered per kernel via
/// [`register_kernel_slab_handle`].
///
/// Deliberately distinct from [`super::slab::Slab`], whose impls each know
/// their element size at compile time: a `size` parameter there would degrade
/// that per-class type-state guarantee.
pub trait KernelHeapBackend: Send + Sync {
    /// Allocate `size` bytes aligned to at least 16, or `None` when the pool is
    /// exhausted or `size` exceeds the implementation's upper bound. Contents
    /// are implementation-defined (the SlopOS in-tree impl zeroes them).
    fn alloc(&self, size: usize) -> Option<NonNull<u8>>;

    /// Return a pointer to the backend. `ptr` must be the exact value a prior
    /// [`KernelHeapBackend::alloc`] returned; the size is recovered from the
    /// backend's own bookkeeping.
    fn dealloc(&self, ptr: NonNull<u8>);
}

/// Register the kernel slab allocator as the [`KernelHeap`] backend. The
/// `&BspToken<'brand>` witnesses BSP-only init; `slot` points at a `'static`
/// reference held in the registering crate's BSS, so registration is a
/// lock-free, one-shot publish of a stable pointer.
///
/// Must be called exactly once after `slopos-mm`'s slab tier finishes
/// self-initialisation; subsequent calls panic. Layouts with `align > 16` get a
/// one-`usize` cookie written ahead of the user-visible pointer by this module,
/// so the backend itself stays layout-naive.
pub fn register_kernel_slab_handle<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn KernelHeapBackend,
) {
    let slot_ptr =
        slot as *const &'static dyn KernelHeapBackend as *mut &'static dyn KernelHeapBackend;
    let prev = BACKEND_SLOT.swap(slot_ptr, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "register_kernel_slab_handle: backend already registered"
    );
    BACKEND_LIVE.store(true, Ordering::Release);
}

#[inline]
fn align_up_usize(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

#[inline]
fn current_backend() -> Option<&'static dyn KernelHeapBackend> {
    if !BACKEND_LIVE.load(Ordering::Acquire) {
        return None;
    }
    let ptr = BACKEND_SLOT.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `register_kernel_slab_handle` published `ptr` as the address of a
    // `&'static &'static dyn KernelHeapBackend` before the Release store to
    // `BACKEND_LIVE`, which the Acquire load above orders against; the pointee
    // outlives the kernel.
    Some(*unsafe { &*ptr })
}

fn bump_alloc(layout: core::alloc::Layout) -> *mut u8 {
    let align = layout.align().max(8);
    let size = layout.size();
    let mut offset = BUMP_NEXT.load(Ordering::Relaxed);
    offset = align_up_usize(offset, align);
    if offset.checked_add(size).is_none_or(|n| n > BUMP_HEAP_SIZE) {
        return core::ptr::null_mut();
    }
    BUMP_NEXT.store(offset + size, Ordering::Relaxed);
    let base = BUMP_HEAP.get() as *mut u8;
    // SAFETY: `offset + size <= BUMP_HEAP_SIZE` per the bounds check above,
    // so the resulting pointer lies within the static `BUMP_HEAP` allocation.
    unsafe { base.add(offset) }
}

/// The kernel's `#[global_allocator]` type. Owns the dispatch from
/// `core::alloc::GlobalAlloc` to either the OSTD bump pool (early boot)
/// or the `mm` crate's registered `KernelHeapBackend` (post-init).
///
/// `kernel/src/main.rs` is the only consumer:
///
/// ```ignore
/// #[global_allocator]
/// static GLOBAL_ALLOCATOR: KernelHeap = KernelHeap;
/// ```
pub struct KernelHeap;

unsafe impl core::alloc::GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let Some(backend) = current_backend() else {
            return bump_alloc(layout);
        };

        let align = layout.align().max(16);
        let size = layout.size();
        if align <= 16 {
            return backend
                .alloc(size)
                .map_or(core::ptr::null_mut(), |p| p.as_ptr());
        }

        // `extra` is the back-pointer cookie's footprint rounded up to the
        // slab-class minimum alignment, so stashing it ahead of the user-visible
        // pointer cannot cost that pointer its alignment guarantee.
        let extra = align_up_usize(core::mem::size_of::<usize>(), 16);
        let total = size.saturating_add(align).saturating_add(extra);
        let raw = match backend.alloc(total) {
            Some(p) => p.as_ptr(),
            None => return core::ptr::null_mut(),
        };

        let base = raw as usize;
        let aligned = align_up_usize(base + extra, align);
        let cookie = (aligned - core::mem::size_of::<usize>()) as *mut usize;
        // SAFETY: `aligned >= base + extra`, so `cookie` lives strictly
        // inside the just-allocated `[base, base + total)` region.
        //
        // This branch is OSTD's enforcement of **Inv. 10**: the pointer returned
        // below is aligned to `align` and backed by `size` usable bytes.
        unsafe {
            *cookie = base;
        }
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        if ptr.is_null() {
            return;
        }
        let Some(backend) = current_backend() else {
            // The bump pool never frees.
            return;
        };

        let align = layout.align().max(16);
        if align <= 16 {
            if let Some(nn) = NonNull::new(ptr) {
                backend.dealloc(nn);
            }
            return;
        }

        let cookie = ((ptr as usize) - core::mem::size_of::<usize>()) as *const usize;
        // SAFETY: this branch only runs for `ptr`s previously produced by
        // the `align > 16` branch of `alloc`, so the back-cookie was
        // written one `usize`-sized slot before `ptr`.
        let raw = unsafe { *cookie } as *mut u8;
        if let Some(nn) = NonNull::new(raw) {
            backend.dealloc(nn);
        }
    }
}

/// Kernel-wide pinned heap cell. The in-place constructor takes an
/// [`Init<T, E>`] recipe, so a `T` value never materialises on a caller's
/// stack.
pub struct PinBox<T: ?Sized> {
    inner: Pin<Box<T>>,
}

impl<T> PinBox<T> {
    /// Heap-allocate and initialise a `T` in place from an [`Init<T, E>`]
    /// recipe, then pin it.
    pub fn try_init<E>(init: impl Init<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit().map_err(E::from)?;
        // SAFETY: `boxed` is a freshly-allocated, properly aligned, writable slot
        // for a `T`. `init.__init` writes a valid `T` into it on `Ok(())`,
        // satisfying `assume_init`. On `Err(e)` the slot is freed as a
        // `Box<MaybeUninit<T>>`, running no drop glue over uninitialised memory.
        unsafe {
            let raw = Box::into_raw(boxed);
            let slot: *mut T = (*raw).as_mut_ptr();
            if let Err(e) = init.__init(slot) {
                let _ = Box::from_raw(raw);
                return Err(e);
            }
            let initialised: Box<T> = Box::from_raw(raw as *mut T);
            Ok(Self {
                inner: Box::into_pin(initialised),
            })
        }
    }
}

impl<T: Zeroable> PinBox<T> {
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
    /// [`PinBox::try_init`] or [`PinBox::zeroed`] instead so the `T`
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
///
/// `#[repr(transparent)]` over the single `Box<T>` field makes the
/// niche-optimisation layout spec-guaranteed: `Option<KBox<T>>` is
/// exactly one pointer wide, with `None` encoded as null. Lock-free
/// readers rely on this so a word-sized atomic load observes either
/// null or a valid box without tearing.
#[repr(transparent)]
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

    /// Heap-allocate and initialise a `T` in place from an
    /// [`Init<T, E>`] recipe. The `T` rvalue never materialises on
    /// the caller's stack — the recipe writes fields directly into
    /// the freshly-allocated heap slot.
    pub fn try_init<E>(init: impl Init<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        let boxed: Box<core::mem::MaybeUninit<T>> = Box::try_new_uninit().map_err(E::from)?;
        // SAFETY: see `PinBox::try_init` — identical invariants. The
        // `Box::try_new_uninit::<T>()` slot is sized and aligned to
        // `T`'s `Layout` by construction, upholding **Inv. 10**: the
        // `T` value the `init` recipe writes into `slot` lands in
        // storage that meets `T`'s size and alignment requirements.
        unsafe {
            let raw = Box::into_raw(boxed);
            let slot: *mut T = (*raw).as_mut_ptr();
            if let Err(e) = init.__init(slot) {
                let _ = Box::from_raw(raw);
                return Err(e);
            }
            Ok(Self {
                inner: Box::from_raw(raw as *mut T),
            })
        }
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

    /// Reclaim and drop a raw pointer that was previously produced by
    /// [`KBox::into_raw`]. Convenience wrapper for the common
    /// "atomically swap an `AtomicPtr<T>`, then free the previous
    /// pointer" pattern.
    ///
    /// # Safety
    /// `ptr` must originate from a matching `into_raw` call and must
    /// not be aliased. No other reference may exist to the value at
    /// `ptr` at the time of the reclaim.
    pub unsafe fn reclaim_raw(ptr: *mut T) {
        if !ptr.is_null() {
            // SAFETY: caller upholds Box::from_raw invariants; we drop
            // the reconstructed Box immediately, freeing the allocation.
            unsafe {
                drop(Self::from_raw(ptr));
            }
        }
    }

    /// Leak the boxed value into a static-lifetime reference.
    ///
    /// `&'static mut T` rather than a caller-chosen `&'a mut T`: the allocation
    /// is deliberately never freed, so `'static` is what it actually is, and a
    /// lifetime the caller picks would be one it could pick twice. The box is
    /// consumed, so a second call against the same allocation cannot be
    /// written either — but saying `'static` means the signature no longer
    /// relies on that to be sound.
    ///
    /// See [`KBox::leak_unsized`] for the `?Sized` case.
    pub fn leak(b: Self) -> &'static mut T
    where
        T: 'static,
    {
        Box::leak(b.inner)
    }

    /// Move the boxed value out, freeing the allocation. Equivalent to
    /// `Box::into_inner`. Used by the hermetic-state framework to take
    /// ownership of a type-erased snapshot before invoking `restore`.
    pub fn into_inner(b: Self) -> T {
        // `Box<T>` supports move-out via `*` in stable Rust.
        *b.inner
    }
}

impl<T: ?Sized> KBox<T> {
    /// Leak the boxed value into a static-lifetime reference. Works
    /// for both sized and unsized `T` (e.g. `dyn Trait` for trait
    /// objects whose registry holds them for the kernel's lifetime).
    pub fn leak_unsized(b: Self) -> &'static mut T {
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

/// Largest live reference count. Reaching this value makes that counter
/// permanently saturated: subsequent clones and drops leave it unchanged.
/// Leaking an allocation is the only sound response to a count that can no
/// longer represent all live handles; wrapping would permit use-after-free.
const KARC_MAX_REFCOUNT: usize = isize::MAX as usize;

/// Sentinel used to exclude concurrent downgrades while `get_mut` verifies
/// unique ownership. It is deliberately outside the saturating count range.
const KARC_WEAK_LOCKED: usize = usize::MAX;

#[repr(C, align(2))]
struct KArcInner<T: ?Sized> {
    strong: AtomicUsize,
    /// Includes one implicit weak reference while `strong` is non-zero.
    weak: AtomicUsize,
    /// The tail position is load-bearing: it lets `KArc<T>` unsize to
    /// `KArc<dyn Trait>` without changing the allocation or pointer metadata.
    data: T,
}

enum RefcountRelease {
    Remaining,
    Last,
    Saturated,
}

/// Increment a live reference count without allowing integer wraparound.
/// Once saturated, the counter stays saturated for the allocation's lifetime.
#[inline]
fn refcount_increment_saturating(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current >= KARC_MAX_REFCOUNT {
            return;
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Release one reference. A CAS loop is required because a racing increment
/// may saturate the counter; a plain `fetch_sub` could then undo saturation
/// and make uncounted handles reachable by a later deallocation.
#[inline]
fn refcount_release(counter: &AtomicUsize) -> RefcountRelease {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == KARC_MAX_REFCOUNT {
            return RefcountRelease::Saturated;
        }
        debug_assert!(current > 0, "KArc reference count underflow");
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) if current == 1 => return RefcountRelease::Last,
            Ok(_) => return RefcountRelease::Remaining,
            Err(observed) => current = observed,
        }
    }
}

/// Kernel-owned atomically reference-counted pointer. Allocation is fallible;
/// cloning is allocation-free and saturates rather than wrapping its count.
///
/// As with [`KBox::try_new`] the rvalue passed to [`KArc::try_new`] does
/// briefly land on the caller's stack; large `T` should be constructed
/// via [`KArc::try_init`], which writes the `T` directly into the Arc's
/// heap allocation without a stack materialisation step.
pub struct KArc<T: ?Sized> {
    ptr: NonNull<KArcInner<T>>,
}

impl<T> KArc<T> {
    pub fn try_new(value: T) -> Result<Self, AllocError> {
        let inner = Box::try_new(KArcInner {
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            data: value,
        })?;
        // SAFETY: `Box::into_raw` never returns null.
        let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(inner)) };
        Ok(Self { ptr })
    }

    /// Heap-allocate and initialise a `T` in place from an
    /// [`Init<T, E>`] recipe. The `T` rvalue never materialises on the
    /// caller's stack — the recipe writes fields directly into the
    /// freshly-allocated `Arc` slot. Mirrors [`KBox::try_init`]; use it
    /// for large `T` that would otherwise blow the stack-frame gate.
    pub fn try_init<E>(init: impl Init<T, E>) -> Result<Self, E>
    where
        E: From<AllocError>,
    {
        let uninit: Box<MaybeUninit<KArcInner<T>>> = Box::try_new_uninit().map_err(E::from)?;
        let allocation = Box::into_raw(uninit);
        let inner = allocation.cast::<KArcInner<T>>();
        // SAFETY: `allocation` is a fresh, correctly sized and aligned slot.
        // The two header fields are initialized before the recipe receives
        // the data slot. On recipe failure the original `MaybeUninit` box is
        // reconstructed, so no partially initialized field is dropped.
        unsafe {
            core::ptr::addr_of_mut!((*inner).strong).write(AtomicUsize::new(1));
            core::ptr::addr_of_mut!((*inner).weak).write(AtomicUsize::new(1));
            let slot = core::ptr::addr_of_mut!((*inner).data);
            if let Err(error) = init.__init(slot) {
                drop(Box::from_raw(allocation));
                return Err(error);
            }
            Ok(Self {
                ptr: NonNull::new_unchecked(inner),
            })
        }
    }

    /// Heap-allocate a `T` whose initialiser receives a [`KWeak<T>`]
    /// pointing back at the allocation being constructed, enabling a
    /// self-referential weak link (e.g. a parent/child pair where the
    /// child holds a weak back-pointer to the parent).
    ///
    /// `data_fn` can clone the supplied weak handle, but upgrading it returns
    /// `None` until the returned value has been written completely. Allocation
    /// failure is reported before `data_fn` runs.
    pub fn try_new_cyclic<F>(data_fn: F) -> Result<Self, AllocError>
    where
        F: FnOnce(&KWeak<T>) -> T,
    {
        let uninit: Box<MaybeUninit<KArcInner<T>>> = Box::try_new_uninit()?;
        let inner = Box::into_raw(uninit).cast::<KArcInner<T>>();
        // SAFETY: `inner` points to a fresh `KArcInner<T>` allocation. The
        // temporary `KWeak` owns the implicit weak count. Forgetting it after
        // publication transfers that count to the strong-reference set. If
        // `data_fn` unwinds in a host test, its Drop instead deallocates the
        // still-uninitialized allocation without reading `data`.
        unsafe {
            core::ptr::addr_of_mut!((*inner).strong).write(AtomicUsize::new(0));
            core::ptr::addr_of_mut!((*inner).weak).write(AtomicUsize::new(1));
            let weak = KWeak {
                ptr: NonNull::new_unchecked(inner),
            };
            let value = data_fn(&weak);
            core::ptr::addr_of_mut!((*inner).data).write(value);
            (*inner).strong.store(1, Ordering::Release);
            core::mem::forget(weak);
            Ok(Self {
                ptr: NonNull::new_unchecked(inner),
            })
        }
    }
}

impl<T: ?Sized> KArc<T> {
    #[inline]
    fn inner(&self) -> &KArcInner<T> {
        // SAFETY: every live `KArc` owns one strong reference, so the header
        // and initialized data remain allocated for this borrow.
        unsafe { self.ptr.as_ref() }
    }

    /// Returns a mutable reference to the inner value iff no other strong or
    /// weak handle can access this allocation.
    ///
    /// Returns `None` when the strong or weak ref-count exceeds one,
    /// because handing out `&mut T` while another clone exists would
    /// alias the inner value.
    #[inline]
    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        let inner = this.inner();
        if inner
            .weak
            .compare_exchange(1, KARC_WEAK_LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        let unique = inner.strong.load(Ordering::Acquire) == 1;
        inner.weak.store(1, Ordering::Release);
        if !unique {
            return None;
        }
        // SAFETY: the locked weak count proved that no explicit `KWeak`
        // exists, the strong count is one, and `&mut KArc` prevents access
        // through this sole handle for the returned borrow.
        Some(unsafe { &mut (*this.ptr.as_ptr()).data })
    }

    /// Strong reference count. Useful for invariant assertions in
    /// callers that rely on sole ownership for `get_mut` to succeed.
    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.load(Ordering::Relaxed)
    }

    /// Weak reference count. The count is 0 when no [`KWeak`] points at
    /// the allocation (a lone strong `KArc` reports 0, not 1 — the
    /// implicit weak that backs the strong count is not exposed). Useful
    /// for invariant assertions about outstanding weak handles.
    #[inline]
    pub fn weak_count(this: &Self) -> usize {
        let inner = this.inner();
        let weak = inner.weak.load(Ordering::Acquire);
        let strong = inner.strong.load(Ordering::Relaxed);
        if strong == 0 {
            0
        } else if weak == KARC_MAX_REFCOUNT {
            KARC_MAX_REFCOUNT
        } else {
            weak - 1
        }
    }

    /// Create a [`KWeak`] handle that does *not* keep the allocation
    /// alive. The weak handle [`upgrade`](KWeak::upgrade)s back to a
    /// strong [`KArc`] only while at least one strong reference survives;
    /// once the last strong `KArc` drops, every `KWeak` upgrade yields
    /// `None`. Mirrors [`alloc::sync::Arc::downgrade`].
    #[inline]
    pub fn downgrade(this: &Self) -> KWeak<T> {
        let weak = &this.inner().weak;
        let mut current = weak.load(Ordering::Relaxed);
        loop {
            if current == KARC_WEAK_LOCKED {
                hint::spin_loop();
                current = weak.load(Ordering::Relaxed);
                continue;
            }
            if current == KARC_MAX_REFCOUNT {
                break;
            }
            match weak.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        KWeak { ptr: this.ptr }
    }

    /// Returns `true` if both handles point at the same allocation.
    /// Mirrors [`alloc::sync::Arc::ptr_eq`]. This compares allocation
    /// identity, never the pointee — safe for zero-sized and unsized `T`.
    #[inline]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        core::ptr::addr_eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }

    /// Return a stable raw pointer to the shared value without transferring
    /// ownership. The pointer remains valid only while at least one strong
    /// [`KArc`] is alive; callers that retain it must retain a strong handle
    /// for the same duration.
    ///
    /// Derived from the handle's raw pointer rather than through a
    /// `&KArcInner`, so it carries provenance over the whole allocation and not
    /// just the `data` field. That is what makes it interchangeable with
    /// [`KArc::into_raw`] — the placement primitives hand these pointers to
    /// [`KArc::from_raw`], which walks *backwards* out of `data` to reach the
    /// reference counts in the header.
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        // SAFETY: `this` owns a live strong reference, so the allocation and its
        // initialized `data` field are valid for the duration of this call.
        unsafe { core::ptr::addr_of!((*this.ptr.as_ptr()).data) }
    }

    /// The allocation's stable node pointer, as a [`NonNull`].
    ///
    /// The same address [`KArc::as_ptr`] and [`KArc::into_raw`] yield, which is
    /// also the node pointer the placement primitives and the intrusive links
    /// are keyed on — so a container can derive its key from a handle without a
    /// null check, and without the pointer ever being the *source* of a
    /// reference. Borrowing, not owning: parking the result requires a matching
    /// retain or leak.
    #[inline]
    pub fn node(this: &Self) -> NonNull<T> {
        // SAFETY: `as_ptr` derives from a live allocation's initialized `data`
        // field, whose address is never null.
        unsafe { NonNull::new_unchecked(Self::as_ptr(this).cast_mut()) }
    }

    /// Move one strong reference into an OSTD-owned raw slot.
    pub(crate) fn into_raw(this: Self) -> *const T
    where
        T: Sized,
    {
        // SAFETY: `this` owns a live strong reference, so its initialized data
        // field has a stable address for the duration of this operation.
        let data = unsafe { core::ptr::addr_of!((*this.ptr.as_ptr()).data) };
        core::mem::forget(this);
        data
    }

    /// Reconstruct a strong reference previously moved with `into_raw`.
    ///
    /// # Safety
    /// `data` must be the still-live result of exactly one matching
    /// `KArc::into_raw` call for the same `T`, and that raw ownership must not
    /// already have been reconstructed.
    pub(crate) unsafe fn from_raw(data: *const T) -> Self
    where
        T: Sized,
    {
        let offset = core::mem::offset_of!(KArcInner<T>, data);
        let inner = data.cast::<u8>().wrapping_sub(offset).cast_mut().cast();
        // SAFETY: the caller guarantees this is the matching live allocation.
        Self {
            ptr: unsafe { NonNull::new_unchecked(inner) },
        }
    }

    /// Release one strong reference **without** running the destructor.
    ///
    /// Returns `Some(data)` exactly when this call took the strong count from
    /// one to zero. In that case the caller **uniquely** owns the allocation:
    /// `T` is still initialized, the implicit weak reference is still held, and
    /// no other thread can reach the value. That uniqueness is what makes it
    /// sound to park the allocation in an intrusive list — the link slot has no
    /// contention — and destroy it later from a context where the destructor
    /// may safely run. The returned pointer must reach
    /// [`KArc::destroy_deferred`] exactly once.
    ///
    /// Deciding finality here rather than by reading the count beforehand is
    /// load-bearing: a `strong_count == 1` pre-check is racy, because two
    /// holders can both observe two and both then drop.
    ///
    /// A saturated count never destroys, matching [`Drop`].
    ///
    /// The returned address is the same one [`KArc::as_ptr`] and
    /// [`KArc::into_raw`] yield, so it interchanges with the placement
    /// primitives' node pointers.
    pub(crate) fn release_deferrable(this: Self) -> Option<NonNull<T>>
    where
        T: Sized,
    {
        let ptr = this.ptr;
        let release = refcount_release(&this.inner().strong);
        // This handle's release is already accounted for above; `Drop` must
        // not repeat it.
        core::mem::forget(this);
        match release {
            RefcountRelease::Remaining | RefcountRelease::Saturated => None,
            RefcountRelease::Last => {
                // SAFETY: the allocation is still mapped (the implicit weak is
                // untouched) and `data` is still initialized, so its address is
                // valid and non-null.
                let data = unsafe { core::ptr::addr_of_mut!((*ptr.as_ptr()).data) };
                // SAFETY: a field address inside a live allocation is non-null.
                Some(unsafe { NonNull::new_unchecked(data) })
            }
        }
    }

    /// Run the destruction sequence that [`KArc::release_deferrable`] deferred.
    ///
    /// # Safety
    /// `data` must be the result of exactly one [`KArc::release_deferrable`]
    /// call that returned `Some`, and must not already have been destroyed.
    pub(crate) unsafe fn destroy_deferred(data: NonNull<T>)
    where
        T: Sized,
    {
        let offset = core::mem::offset_of!(KArcInner<T>, data);
        let inner = data
            .as_ptr()
            .cast::<u8>()
            .wrapping_sub(offset)
            .cast::<KArcInner<T>>();
        // SAFETY: per the caller contract this is the matching live allocation
        // whose strong count this thread took to zero, with the implicit weak
        // still held.
        unsafe { Self::finish_last_strong(NonNull::new_unchecked(inner)) };
    }

    /// Test-only view of [`KArc::release_deferrable`]. The production surface
    /// stays `pub(crate)` (raw strong ownership belongs inside OSTD), but the
    /// racing-releaser property is a property of the generic refcount, not of
    /// any one payload — and `TaskInner` is `!Send`, so it cannot be raced in a
    /// host test.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn release_deferrable_for_test(this: Self) -> Option<NonNull<T>>
    where
        T: Sized,
    {
        Self::release_deferrable(this)
    }

    /// Test-only view of [`KArc::destroy_deferred`].
    ///
    /// # Safety
    /// Same contract as [`KArc::destroy_deferred`].
    #[cfg(any(test, feature = "test-helpers"))]
    pub unsafe fn destroy_deferred_for_test(data: NonNull<T>)
    where
        T: Sized,
    {
        // SAFETY: forwarded verbatim to the caller.
        unsafe { Self::destroy_deferred(data) }
    }

    /// Test hook that places the strong count one step below saturation.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn prepare_strong_saturation_for_test(this: &Self) {
        debug_assert_eq!(this.inner().strong.load(Ordering::Relaxed), 1);
        this.inner()
            .strong
            .store(KARC_MAX_REFCOUNT - 1, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "test-helpers"))]
    pub const fn max_refcount_for_test() -> usize {
        KARC_MAX_REFCOUNT
    }
}

impl<T, U> core::ops::CoerceUnsized<KArc<U>> for KArc<T>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{
}

/// Kernel-blessed `Weak<T>`. A non-owning handle to a [`KArc`]
/// allocation: it never keeps the inner value alive, and
/// [`upgrade`](KWeak::upgrade) returns `None` once the last strong
/// [`KArc`] has dropped. Used to break ownership cycles and to hold a
/// reference that must not extend the referent's lifetime (e.g. a poll
/// registration that observes — but never resurrects — a closed file).
pub struct KWeak<T: ?Sized> {
    ptr: NonNull<KArcInner<T>>,
}

impl<T> KWeak<T> {
    /// An empty weak handle that never upgrades to a value. Mirrors
    /// [`alloc::sync::Weak::new`]; useful as a sentinel / default, and
    /// `const` so empty handles can seed static registries.
    #[inline]
    pub const fn new() -> Self {
        // Matches the standard library's empty-weak representation. A real
        // `KArcInner` has alignment >= 2, so this address cannot identify an
        // allocation. It is never dereferenced.
        Self {
            ptr: NonNull::without_provenance(NonZeroUsize::MAX),
        }
    }

    /// Identity address of the referent, without upgrading and without
    /// touching any reference count. Null for an empty [`KWeak::new`] handle.
    ///
    /// The result is a **comparison token only and must never be
    /// dereferenced**: the strong count may already be zero, in which case `T`
    /// has been dropped. Computing it is nonetheless always well-defined — a
    /// live weak count keeps the allocation itself mapped, and this reads no
    /// field — and it equals [`KArc::as_ptr`] for any strong handle onto the
    /// same allocation.
    ///
    /// Exists so a registry can answer "is this pointer one of mine?" without
    /// upgrading, because an upgraded handle dropped under a lock could be the
    /// final reference and run an allocator-heavy destructor there.
    #[inline]
    pub fn as_ptr(&self) -> *const T {
        if self.is_empty() {
            return core::ptr::null();
        }
        let offset = core::mem::offset_of!(KArcInner<T>, data);
        self.ptr.as_ptr().cast::<u8>().wrapping_add(offset).cast()
    }
}

impl<T> Default for KWeak<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ?Sized> KWeak<T> {
    #[inline]
    fn is_empty(&self) -> bool {
        self.ptr.as_ptr().cast::<()>().addr() == usize::MAX
    }

    #[inline]
    fn counters(&self) -> Option<(&AtomicUsize, &AtomicUsize)> {
        if self.is_empty() {
            return None;
        }
        let ptr = self.ptr.as_ptr();
        // SAFETY: a live weak count keeps the allocation and its two header
        // atomics valid even after `data` has been dropped. We deliberately do
        // not form a reference to the whole, potentially dead, `KArcInner<T>`.
        Some(unsafe { (&(*ptr).strong, &(*ptr).weak) })
    }

    /// Attempt to promote this weak handle to a strong [`KArc`],
    /// bumping the strong count. Returns `None` if the inner value has
    /// already been dropped (the last strong `KArc` is gone) or if this
    /// is an empty [`KWeak::new`] handle. Mirrors
    /// [`alloc::sync::Weak::upgrade`].
    #[inline]
    pub fn upgrade(&self) -> Option<KArc<T>> {
        let (strong, _) = self.counters()?;
        let mut current = strong.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return None;
            }
            if current == KARC_MAX_REFCOUNT {
                return Some(KArc { ptr: self.ptr });
            }
            match strong.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(KArc { ptr: self.ptr }),
                Err(observed) => current = observed,
            }
        }
    }

    /// Strong reference count of the referent, or 0 if it has been
    /// dropped / this is an empty handle. Mirrors
    /// [`alloc::sync::Weak::strong_count`].
    #[inline]
    pub fn strong_count(&self) -> usize {
        self.counters()
            .map_or(0, |(strong, _)| strong.load(Ordering::Relaxed))
    }
}

impl<T: ?Sized> Clone for KWeak<T> {
    fn clone(&self) -> Self {
        if let Some((_, weak)) = self.counters() {
            refcount_increment_saturating(weak);
        }
        Self { ptr: self.ptr }
    }
}

impl<T, U> core::ops::CoerceUnsized<KWeak<U>> for KWeak<T>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{
}

impl<T: ?Sized> Clone for KArc<T> {
    fn clone(&self) -> Self {
        refcount_increment_saturating(&self.inner().strong);
        Self { ptr: self.ptr }
    }
}

impl<T: ?Sized> KArc<T> {
    /// The sole strong-destruction sequence. Both [`Drop`] and
    /// [`KArc::destroy_deferred`] funnel here, so there is exactly one
    /// ordering and one deallocation path to audit.
    ///
    /// # Safety
    /// The caller must have taken this allocation's strong count from one to
    /// zero exactly once, and must not yet have released the implicit weak
    /// reference that the strong count owned.
    unsafe fn finish_last_strong(ptr: NonNull<KArcInner<T>>) {
        // Every prior reference release publishes accesses through Release;
        // this Acquire fence makes them visible before `T`'s destructor runs.
        fence(Ordering::Acquire);
        // Materialize the implicit weak as an RAII guard before calling user
        // drop glue. It releases the allocation on both normal return and a
        // host-test unwind; kernel panics abort, but the stronger invariant
        // keeps this primitive correct in every build mode.
        let implicit = KWeak { ptr };
        // SAFETY: the caller changed the strong count from one to zero, so no
        // strong handle can access data and weak upgrades can no longer win.
        unsafe {
            core::ptr::drop_in_place(core::ptr::addr_of_mut!((*ptr.as_ptr()).data));
        }
        drop(implicit);
    }
}

impl<T: ?Sized> Drop for KArc<T> {
    fn drop(&mut self) {
        match refcount_release(&self.inner().strong) {
            RefcountRelease::Remaining | RefcountRelease::Saturated => return,
            RefcountRelease::Last => {}
        }
        // SAFETY: this thread won the one-to-zero transition, exactly once,
        // and has not touched the implicit weak.
        unsafe { Self::finish_last_strong(self.ptr) };
    }
}

impl<T: ?Sized> Drop for KWeak<T> {
    fn drop(&mut self) {
        self.drop_inner();
    }
}

impl<T: ?Sized> KWeak<T> {
    fn drop_inner(&mut self) {
        let Some((_, weak)) = self.counters() else {
            return;
        };
        match refcount_release(weak) {
            RefcountRelease::Remaining | RefcountRelease::Saturated => return,
            RefcountRelease::Last => {}
        }

        // Pair with every weak Release before returning the storage to the
        // allocator. `Layout::for_value_raw` uses pointer metadata only; it
        // does not read the already-dropped `data` tail.
        fence(Ordering::Acquire);
        // SAFETY: this was the final weak reference, the strong count is zero,
        // and the layout is that of the original (possibly unsized) allocation.
        unsafe {
            let layout = core::alloc::Layout::for_value_raw(self.ptr.as_ptr());
            alloc::alloc::dealloc(self.ptr.cast::<u8>().as_ptr(), layout);
        }
    }
}

// SAFETY: shared access through either handle is limited to `&T`; moving a
// handle between CPUs is sound exactly when `T` is both Send and Sync.
unsafe impl<T: ?Sized + Send + Sync> Send for KArc<T> {}
// SAFETY: see the `Send` implementation above.
unsafe impl<T: ?Sized + Send + Sync> Sync for KArc<T> {}
// SAFETY: weak handles access only atomic header fields until a successful
// upgrade produces a `KArc<T>` under the same Send + Sync bounds.
unsafe impl<T: ?Sized + Send + Sync> Send for KWeak<T> {}
// SAFETY: see the `KWeak` Send implementation above.
unsafe impl<T: ?Sized + Send + Sync> Sync for KWeak<T> {}

impl<T: ?Sized> Deref for KArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner().data
    }
}

impl<T: ?Sized> AsRef<T> for KArc<T> {
    fn as_ref(&self) -> &T {
        &self.inner().data
    }
}

#[cfg(test)]
mod karc_tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::{KArc, KWeak};

    static RAW_ROUND_TRIP_DROPS: AtomicUsize = AtomicUsize::new(0);

    struct DropProbe(u32);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            RAW_ROUND_TRIP_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn raw_strong_ownership_round_trip_drops_exactly_once() {
        RAW_ROUND_TRIP_DROPS.store(0, Ordering::Relaxed);
        let arc = KArc::try_new(DropProbe(0xCAFE)).expect("KArc allocation");
        let data = KArc::into_raw(arc);
        // SAFETY: `data` is the live result of the immediately preceding
        // `into_raw` and this call reconstructs that ownership exactly once.
        let arc = unsafe { KArc::from_raw(data) };
        assert_eq!(arc.0, 0xCAFE);
        assert_eq!(RAW_ROUND_TRIP_DROPS.load(Ordering::Relaxed), 0);
        drop(arc);
        assert_eq!(RAW_ROUND_TRIP_DROPS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn get_mut_requires_no_other_strong_or_weak_handles() {
        let mut arc = KArc::try_new(10_u32).expect("KArc allocation");
        *KArc::get_mut(&mut arc).expect("fresh KArc is unique") = 20;

        let weak = KArc::downgrade(&arc);
        assert!(KArc::get_mut(&mut arc).is_none());
        drop(weak);

        let clone = arc.clone();
        assert!(KArc::get_mut(&mut arc).is_none());
        drop(clone);
        assert_eq!(KArc::get_mut(&mut arc).map(|value| *value), Some(20));
    }

    #[test]
    fn empty_weak_is_cloneable_and_never_upgrades() {
        let weak = KWeak::<u64>::new();
        let clone = weak.clone();
        assert_eq!(weak.strong_count(), 0);
        assert!(weak.upgrade().is_none());
        assert!(clone.upgrade().is_none());
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
