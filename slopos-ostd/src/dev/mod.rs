//! Device-handle primitives.
//!
//! This module hosts type-agnostic helpers consumers need when
//! borrowing device state published as raw pointers — the canonical
//! example being a kernel-static `AtomicPtr<DeviceHandle>` whose
//! load is reborrowed as `&DeviceHandle` for the data plane.
//!
//! Alongside the [`FromRawPtr`] extension trait (a null-safe `&Self`
//! reborrow parallel to
//! [`crate::irq::interrupt_frame::InterruptFrame::from_ptr`]), this module
//! hosts [`Devres`] — the LIFO managed-resource bag a driver's probe uses to
//! acquire MMIO/IRQ/DMA resources that auto-release on failure or unbind.

pub mod devres;

pub use devres::{Devres, DevresError, ResourceObject};

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

    /// Reborrow `ptr` as `&Self` without a null check.
    ///
    /// Caller invariant: `ptr` is non-null, aligned, dereferenceable
    /// for `size_of::<Self>()` bytes, and no aliasing `&mut Self`
    /// exists for the lifetime `'a`. Matches the standard
    /// `&*ptr` precondition.
    ///
    /// Used by device handles whose backing `*const Self` is published
    /// once at registration and outlives every consumer (e.g. the net
    /// `DeviceHandle::dev` pointer is valid for the device's
    /// registered lifetime).
    fn from_ptr_unchecked<'a>(ptr: *const Self) -> &'a Self;
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

    #[inline]
    fn from_ptr_unchecked<'a>(ptr: *const Self) -> &'a Self {
        // SAFETY: caller asserts the contract documented on
        // `from_ptr_unchecked` — non-null, aligned, dereferenceable,
        // no aliasing mutable borrow.
        unsafe { &*ptr }
    }
}

/// Reborrow a `*const T` (including fat trait-object pointers) as
/// `&T` without a null check.
///
/// `?Sized` companion of [`FromRawPtr::from_ptr_unchecked`] — accepts
/// `*const dyn Trait` for device handles whose backing pointer is
/// published once and outlives every consumer.
///
/// # Safety
///
/// Caller invariant: `ptr` is non-null, dereferenceable, and no
/// aliasing `&mut T` exists for the lifetime `'a`. Matches the
/// standard `&*ptr` precondition.
#[inline]
pub fn borrow_dyn<'a, T: ?Sized>(ptr: *const T) -> &'a T {
    // SAFETY: caller upholds the contract documented above.
    unsafe { &*ptr }
}
