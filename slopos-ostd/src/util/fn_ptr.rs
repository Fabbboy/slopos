//! `fn()`-pointer round-trip helpers.
//!
//! Several kernel subsystems register a `fn()` callback into an
//! `AtomicPtr<()>` (e.g. NAPI's per-device kick), then later transmute
//! the stored `*mut ()` back to a `fn()` for invocation. Each round
//! trip needs one `unsafe { core::mem::transmute(...) }`; that pattern
//! is wrapped here so consumers stay `unsafe`-free.
//!
//! The encode/decode pair is reflexive — `decode(encode(f)) == f` for
//! every `fn()`-typed value `f` — and the decode side panics
//! semantically (returns `None`) on the null sentinel.

/// Encode a `fn()` as a `*mut ()` for storage in an `AtomicPtr`.
#[inline]
pub fn encode(f: fn()) -> *mut () {
    // SAFETY: function pointers are the same size and alignment as
    // data pointers on every supported target; `transmute` from
    // `fn()` to `*mut ()` is documented in the reference.
    unsafe { core::mem::transmute::<fn(), *mut ()>(f) }
}

/// Decode a `*mut ()` produced by [`encode`] back into the original
/// `fn()`. Returns `None` if `ptr` is null.
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

/// Recover a typed `fn`-pointer of arbitrary ABI from a `*mut ()` slot.
/// Generic over the function-pointer type `F` so callers can recover
/// `fn(u8)`, `fn() -> i32`, etc. — panics if `F` is not exactly the
/// size of a `*mut ()` (function pointers and data pointers must have
/// the same layout on every supported target).
///
/// # Safety contract on the caller
///
/// `ptr` must have been produced by a prior transmute of an `F`-typed
/// function pointer into a `*mut ()` (typically via
/// `core::mem::transmute` or a registered service-table cast). If
/// `ptr.is_null()`, the function returns a zero-bit-pattern `F` which
/// the caller should null-check before invoking; the safer entry
/// points are crate-specific `Option<F>` wrappers (see
/// [`fn_ptr_decode_opt`]).
#[inline]
pub fn fn_ptr_from_raw<F: Copy + 'static>(ptr: *mut ()) -> F {
    debug_assert_eq!(
        core::mem::size_of::<F>(),
        core::mem::size_of::<*mut ()>(),
        "fn_ptr_from_raw: F must be pointer-sized"
    );
    // SAFETY: `*mut ()` and any `fn(...) -> R` pointer share the same
    // size and alignment on supported targets; the caller asserts the
    // pointer was produced by a matching encode.
    unsafe { core::mem::transmute_copy::<*mut (), F>(&ptr) }
}

/// `Option<F>` sibling of [`fn_ptr_from_raw`]: returns `None` on null,
/// `Some(F)` otherwise. Use when the caller wants a null-check at the
/// recover site.
#[inline]
pub fn fn_ptr_decode_opt<F: Copy + 'static>(ptr: *mut ()) -> Option<F> {
    if ptr.is_null() {
        None
    } else {
        Some(fn_ptr_from_raw::<F>(ptr))
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
