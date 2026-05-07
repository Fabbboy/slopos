//! Device-handle primitives.
//!
//! This module hosts type-agnostic helpers consumers need when
//! borrowing device state published as raw pointers — the canonical
//! example being a kernel-static `AtomicPtr<DeviceHandle>` whose
//! load is reborrowed as `&DeviceHandle` for the data plane.
//!
//! The single primitive currently exposed is the [`FromRawPtr`]
//! extension trait, which provides a null-safe `&Self` reborrow
//! parallel to [`crate::irq::interrupt_frame::InterruptFrame::from_ptr`].

/// Null-safe `&Self` reborrow over a raw pointer.
///
/// A blanket `impl<T>` makes the trait method available on any type
/// once the trait is in scope:
///
/// ```ignore
/// use slopos_ostd::dev::FromRawPtr;
/// let h: Option<&MyHandle> = MyHandle::from_ptr(ptr);
/// ```
///
/// # Safety contract on the caller
///
/// The trait's methods are safe to call, but the soundness of the
/// returned reference depends on the caller upholding, for any
/// non-null `ptr`:
///
/// - `ptr` is aligned for `Self`,
/// - `ptr` is dereferenceable for `size_of::<Self>()` bytes,
/// - the underlying allocation outlives `'a`,
/// - for [`from_ptr`](FromRawPtr::from_ptr): no aliased `&mut Self`
///   exists for the duration of `'a`,
/// - for [`from_ptr_mut`](FromRawPtr::from_ptr_mut): no other live
///   borrow (`&Self` or `&mut Self`) exists for the duration of `'a`.
///
/// Mirrors the contract documented on
/// [`crate::irq::interrupt_frame::InterruptFrame::from_ptr`]; the
/// blanket impl absorbs the `unsafe { &*ptr }` deref so consumer
/// crates stay in safe Rust.
pub trait FromRawPtr: Sized {
    fn from_ptr<'a>(ptr: *const Self) -> Option<&'a Self>;
    fn from_ptr_mut<'a>(ptr: *mut Self) -> Option<&'a mut Self>;
}

impl<T> FromRawPtr for T {
    #[inline]
    fn from_ptr<'a>(ptr: *const Self) -> Option<&'a Self> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: caller-asserted contract per trait docs;
            // null was just checked.
            Some(unsafe { &*ptr })
        }
    }

    #[inline]
    fn from_ptr_mut<'a>(ptr: *mut Self) -> Option<&'a mut Self> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: as `from_ptr`, plus exclusive-access asserted.
            Some(unsafe { &mut *ptr })
        }
    }
}
