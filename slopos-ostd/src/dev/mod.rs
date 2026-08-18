//! Device-handle primitives.
//!
//! [`FromRawPtr`] is a null-safe `&Self` reborrow for device state published as
//! a raw pointer (e.g. a kernel-static `AtomicPtr<DeviceHandle>` reborrowed for
//! the data plane). [`Devres`] is the LIFO managed-resource bag a driver's probe
//! uses to acquire MMIO/IRQ/DMA resources that auto-release on failure or
//! unbind.

pub mod devres;

pub use devres::{Devres, DevresError, ResourceObject};

/// Null-safe `&Self` reborrow over a raw pointer. A blanket `impl<T>` makes the
/// method available on any type once the trait is in scope:
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
/// `'static` is what the reference actually is: every user is a device handle
/// published once at registration. A caller-chosen `'a` would let two calls
/// yield references the compiler believes unrelated, for every sized type in
/// the kernel. There is no `_mut` form because two `&'static mut` to one place
/// is aliasing UB; a caller needing one wants a scoped closure.
pub trait FromRawPtr: Sized + 'static {
    fn from_ptr(ptr: *const Self) -> Option<&'static Self>;

    /// Reborrow `ptr` as `&Self` without a null check.
    ///
    /// Caller invariant: `ptr` is non-null, plus the trait-level contract —
    /// the standard `&*ptr` precondition.
    fn from_ptr_unchecked(ptr: *const Self) -> &'static Self;
}

impl<T: 'static> FromRawPtr for T {
    #[inline]
    fn from_ptr(ptr: *const Self) -> Option<&'static Self> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: caller-asserted contract per trait docs; null just
            // checked.
            Some(unsafe { &*ptr })
        }
    }

    #[inline]
    fn from_ptr_unchecked(ptr: *const Self) -> &'static Self {
        // SAFETY: caller asserts the contract documented on
        // `from_ptr_unchecked`.
        unsafe { &*ptr }
    }
}
