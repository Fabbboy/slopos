//! `fn()`-pointer round-trip helpers.
//!
//! Subsystems that register a `fn()` callback into an `AtomicPtr<()>` transmute
//! the stored `*mut ()` back on invocation; wrapping that round trip here keeps
//! consumers `unsafe`-free.

#[inline]
pub fn encode(f: fn()) -> *mut () {
    // SAFETY: function pointers are the same size and alignment as
    // data pointers on every supported target; `transmute` from
    // `fn()` to `*mut ()` is documented in the reference.
    unsafe { core::mem::transmute::<fn(), *mut ()>(f) }
}

/// Returns `None` if `ptr` is null.
///
/// Caller invariant: the pointer was published by a prior `encode`
/// for the same `fn()` ABI (i.e. `fn()`, no arguments, no return).
#[inline]
pub fn decode(ptr: *mut ()) -> Option<fn()> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller asserts the pointer was produced by `encode`
    // for the matching ABI.
    Some(unsafe { core::mem::transmute::<*mut (), fn()>(ptr) })
}

/// Private, and reachable only through [`fn_ptr_decode_opt`]: a null input
/// would yield a zero-bit-pattern `F`, a null fn pointer handed to safe code.
#[inline]
fn transmute_fn_ptr<F: Copy + 'static>(ptr: *mut ()) -> F {
    // A compile error rather than a debug assertion: in a release build
    // `transmute_copy` on an over-wide `F` reads past the eight bytes `ptr`
    // actually occupies.
    const {
        assert!(
            core::mem::size_of::<F>() == core::mem::size_of::<*mut ()>(),
            "fn-pointer type must be pointer-sized"
        );
    }
    // SAFETY: `*mut ()` and any `fn(...) -> R` pointer share the same
    // size and alignment on supported targets; the caller asserts the
    // pointer was produced by a matching encode.
    unsafe { core::mem::transmute_copy::<*mut (), F>(&ptr) }
}

/// Recover a typed `fn`-pointer of arbitrary ABI from a `*mut ()` slot. `F`
/// must be pointer-sized, which is checked at monomorphisation. Returns `None`
/// on null, so a slot that was never published cannot be mistaken for a
/// callable function.
///
/// Caller invariant: a non-null `ptr` was produced by a prior transmute
/// of an `F`-typed function pointer.
#[inline]
pub fn fn_ptr_decode_opt<F: Copy + 'static>(ptr: *mut ()) -> Option<F> {
    if ptr.is_null() {
        None
    } else {
        Some(transmute_fn_ptr::<F>(ptr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};

    static FIRED: AtomicBool = AtomicBool::new(false);

    fn marker() {
        FIRED.store(true, Ordering::Release);
    }

    #[test]
    fn round_trip_invokes_original() {
        FIRED.store(false, Ordering::Release);
        let ptr = encode(marker);
        let recovered = decode(ptr).expect("non-null after encode");
        recovered();
        assert!(FIRED.load(Ordering::Acquire));
    }

    #[test]
    fn decode_null_returns_none() {
        assert!(decode(core::ptr::null_mut()).is_none());
    }
}
