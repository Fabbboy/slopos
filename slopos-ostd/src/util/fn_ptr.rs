//! Safe round-trip between an `AtomicPtr<()>` slot and a typed `fn`
//! pointer.
//!
//! Kernel-half subsystems publish callbacks (TLB shootdown IPI sender,
//! LAPIC ID reader, etc.) via an `AtomicPtr<()>` lazy-init slot so the
//! caller side does not depend on the producer crate. Recovering the
//! typed `fn` pointer requires a `core::mem::transmute` between
//! `*mut ()` and the function pointer type; folding that transmute
//! into OSTD keeps the consumer side in safe Rust.
//!
//! Every helper here is **safe to call** but the interior `unsafe` is
//! sound only when the caller has confirmed that:
//!
//! - The `*mut ()` slot was originally populated via [`fn_ptr_to_raw`]
//!   from a `fn` pointer of the **same signature** as the one
//!   recovered through [`fn_ptr_from_raw`],
//! - The slot value is non-null at the moment of recovery (callers
//!   typically guard with `if ptr.is_null() { return; }` first).

/// Convert a typed `fn` pointer to a `*mut ()` for storage in an
/// `AtomicPtr<()>` slot. Mirror of [`fn_ptr_from_raw`].
#[inline]
pub fn fn_ptr_to_raw<F: Copy>(f: F) -> *mut () {
    // SAFETY: `F` is constrained to `Copy` so the bitwise reinterpret
    // is sound — function pointers are `Copy` in Rust. The `*mut ()`
    // is the universal slot type for an `AtomicPtr<()>`.
    debug_assert_eq!(
        core::mem::size_of::<F>(),
        core::mem::size_of::<*mut ()>(),
        "fn_ptr_to_raw: F must be a function pointer of pointer size"
    );
    unsafe { core::mem::transmute_copy::<F, *mut ()>(&f) }
}

/// Recover a typed `fn` pointer from an `AtomicPtr<()>` slot value.
/// The caller must guarantee the slot was populated by a corresponding
/// [`fn_ptr_to_raw`] call of the **same** signature, and that the
/// slot is non-null at this point.
#[inline]
pub fn fn_ptr_from_raw<F: Copy>(raw: *mut ()) -> F {
    debug_assert!(!raw.is_null(), "fn_ptr_from_raw: raw slot was null");
    debug_assert_eq!(
        core::mem::size_of::<F>(),
        core::mem::size_of::<*mut ()>(),
        "fn_ptr_from_raw: F must be a function pointer of pointer size"
    );
    // SAFETY: caller's contract — `raw` originated from
    // [`fn_ptr_to_raw`] with the same `F`; same-sized bit-for-bit
    // reinterpret is sound for function pointers.
    unsafe { core::mem::transmute_copy::<*mut (), F>(&raw) }
}
