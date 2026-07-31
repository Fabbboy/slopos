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
/// - the allocation is published once and never freed,
/// - no aliased `&mut Self` ever exists.
///
/// # Why `&'static`, and why there is no `_mut` form
///
/// Every user of this trait is a device handle whose backing pointer is
/// published once at registration and outlives the machine, so `'static` is
/// what the reference actually is. A caller-chosen `'a` would have been
/// strictly worse than useless here: the caller picks it, so two calls yield
/// two references the compiler believes are unrelated, and the blanket impl
/// makes that available for *every* sized type in the kernel.
///
/// The mutable form is gone rather than made `'static`, because `'static`
/// would not have fixed it — two `&'static mut` to one place is still instant
/// aliasing UB. A future caller that needs one wants a scoped closure, not
/// this trait.
pub trait FromRawPtr: Sized + 'static {
    fn from_ptr(ptr: *const Self) -> Option<&'static Self>;

    /// Reborrow `ptr` as `&Self` without a null check.
    ///
    /// Caller invariant: `ptr` is non-null, aligned, dereferenceable
    /// for `size_of::<Self>()` bytes, and no aliasing `&mut Self`
    /// exists. Matches the standard `&*ptr` precondition.
    ///
    /// Used by device handles whose backing `*const Self` is published
    /// once at registration and outlives every consumer (e.g. the net
    /// `DeviceHandle::dev` pointer is valid for the device's
    /// registered lifetime).
    fn from_ptr_unchecked(ptr: *const Self) -> &'static Self;
}

impl<T: 'static> FromRawPtr for T {
    #[inline]
    fn from_ptr(ptr: *const Self) -> Option<&'static Self> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: caller-asserted contract per trait docs;
            // null was just checked.
            Some(unsafe { &*ptr })
        }
    }

    #[inline]
    fn from_ptr_unchecked(ptr: *const Self) -> &'static Self {
        // SAFETY: caller asserts the contract documented on
        // `from_ptr_unchecked` — non-null, aligned, dereferenceable,
        // no aliasing mutable borrow.
        unsafe { &*ptr }
    }
}
