//! Send-safe owning slot for a typed raw pointer with a safe-by-convention
//! reborrow surface.
//!
//! **Caller's contract.** Any thread that calls `with_mut` is responsible for
//! ensuring exclusive access to `*ptr` for the duration of the borrow —
//! typically by holding the surrounding container's lock or by being the sole
//! owner of the `RawLink` slot.
//!
//! This is **not** a synchronisation primitive — it provides only the
//! lifetime and Send/Sync paperwork.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Owning slot for a typed raw pointer, replacing `*mut T` fields in
/// lock-protected structures.
pub struct RawLink<T> {
    ptr: AtomicPtr<T>,
}

// SAFETY: `RawLink` is just a typed pointer slot. Cross-thread transfer
// is sound because the caller's lock (not this primitive) governs when
// `with_mut` reborrows are valid; an unowned slot has no aliasing concern.
unsafe impl<T: Send> Send for RawLink<T> {}
unsafe impl<T: Send> Sync for RawLink<T> {}

impl<T> RawLink<T> {
    #[inline]
    pub const fn null() -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    #[inline]
    pub const fn new(target: *mut T) -> Self {
        Self {
            ptr: AtomicPtr::new(target),
        }
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        self.ptr.load(Ordering::Relaxed).is_null()
    }

    #[inline]
    pub fn load(&self) -> Option<NonNull<T>> {
        NonNull::new(self.ptr.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store(&self, target: Option<NonNull<T>>) {
        let raw = target.map_or(core::ptr::null_mut(), |p| p.as_ptr());
        self.ptr.store(raw, Ordering::Relaxed);
    }

    #[inline]
    pub fn clear(&self) {
        self.ptr.store(core::ptr::null_mut(), Ordering::Relaxed);
    }

    /// Run `f` over `&mut T` if the slot is non-null.
    ///
    /// **Caller's contract:** the pointed-to `T` must be valid, properly
    /// aligned, and not concurrently accessed for the duration of the
    /// closure.
    #[inline]
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let ptr = self.ptr.load(Ordering::Relaxed);
        if ptr.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `*ptr` is live and exclusively borrowed
        // for the duration of the closure (see caller's contract above).
        let target: &mut T = unsafe { &mut *ptr };
        Some(f(target))
    }

    /// Run `f` over `&mut T` for an explicit `target` pointer, for a list-walk
    /// holding the next pointer in a local variable rather than in a slot.
    ///
    /// **Caller's contract:** identical to [`with_mut`].
    #[inline]
    pub fn with_mut_at<R>(target: Option<NonNull<T>>, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let p = target?;
        // SAFETY: caller's contract.
        let target: &mut T = unsafe { &mut *p.as_ptr() };
        Some(f(target))
    }
}

impl<T> Default for RawLink<T> {
    fn default() -> Self {
        Self::null()
    }
}

/// Chained linked-list of free objects within a slab — each free object's
/// first `size_of::<*mut u8>()` bytes hold a pointer to the next free object,
/// so the link slot lives **inside** the user-data area and is reused as
/// object body once allocated.
///
/// **Caller's contract:** every `obj` passed to `push_front` must be a
/// pointer to a region of `>= size_of::<*mut u8>()` bytes that the caller
/// owns exclusively. The region must remain valid for the lifetime of this
/// chain. Double-free is a soundness hole the caller must avoid.
pub struct ByteChain {
    head: RawLink<u8>,
}

impl ByteChain {
    pub const fn new() -> Self {
        Self {
            head: RawLink::null(),
        }
    }

    pub const fn from_head(head: *mut u8) -> Self {
        Self {
            head: RawLink::new(head),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    #[inline]
    pub fn head(&self) -> Option<NonNull<u8>> {
        self.head.load()
    }

    #[inline]
    pub fn set_head(&self, target: Option<NonNull<u8>>) {
        self.head.store(target);
    }

    /// Pop the head off the chain. The next-pointer embedded in the popped
    /// object is **not** cleared — the caller may reuse the bytes immediately.
    #[inline]
    pub fn pop_front(&self) -> Option<NonNull<u8>> {
        let head = self.head.load()?;
        // SAFETY: caller's contract — `head` points at a valid >= 8 byte
        // region we own; the first 8 bytes hold the next-pointer.
        let next = unsafe { *(head.as_ptr() as *const *mut u8) };
        self.head.store(NonNull::new(next));
        Some(head)
    }

    /// `obj` must satisfy the contract documented on the type.
    #[inline]
    pub fn push_front(&self, obj: NonNull<u8>) {
        let old = self.head.load();
        let old_raw = old.map_or(core::ptr::null_mut(), |p| p.as_ptr());
        // SAFETY: caller's contract — `obj` is a valid >= 8 byte region we
        // are repurposing as a chain link.
        unsafe {
            *(obj.as_ptr() as *mut *mut u8) = old_raw;
        }
        self.head.store(Some(obj));
    }

    /// Read the next pointer embedded in `obj`.
    ///
    /// **Caller's contract:** identical to [`Self::push_front`].
    #[inline]
    pub fn read_next(obj: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: caller's contract.
        let next = unsafe { *(obj.as_ptr() as *const *mut u8) };
        NonNull::new(next)
    }

    /// Write `next` into the link slot embedded at `obj`.
    ///
    /// **Caller's contract:** identical to [`Self::push_front`].
    #[inline]
    pub fn write_next(obj: NonNull<u8>, next: Option<NonNull<u8>>) {
        let raw = next.map_or(core::ptr::null_mut(), |p| p.as_ptr());
        // SAFETY: caller's contract.
        unsafe {
            *(obj.as_ptr() as *mut *mut u8) = raw;
        }
    }
}

impl Default for ByteChain {
    fn default() -> Self {
        Self::new()
    }
}
