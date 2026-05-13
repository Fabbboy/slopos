//! In-place initialisation surface.
//!
//! SlopOS's in-house equivalent of `pinned-init`'s `Init<T, E>` +
//! `Zeroable` surface. We own it, it has no `Pin` machinery, and it
//! carries no macro-generated closure scratch. Consumed exclusively
//! through [`super::heap::KBox::try_init`] /
//! [`super::heap::PinBox::try_init`] and
//! [`super::heap::KBox::zeroed`] /
//! [`super::heap::PinBox::zeroed`].
//!
//! Design rationale:
//!
//! - SlopOS has no self-referential kernel types and no in-kernel
//!   async, so the `Pin` machinery that motivates `pinned-init` /
//!   Rust-for-Linux's `pin-init` is unneeded complexity for us. We
//!   expose only [`Init<T, E>`] — construct a `T` into a
//!   caller-provided slot.
//! - The two primitive constructors are [`init_from_closure`]
//!   (hand-rolled, per-field `addr_of_mut!` writes — the one used
//!   by large structs like `DataState` where the three-layer
//!   stack-safety gate demands ≤ 1 KiB frames) and [`init_zeroed`]
//!   (for `T: Zeroable`).
//! - We deliberately do not provide `try_init!` / `pin_init!`
//!   macros. Their per-field closure capture materialises every
//!   field's source expression on the closure's stack before
//!   writing to the heap slot, which inflates frame size for
//!   large structs (`DataState`'s init closure was ~2.3 KiB under
//!   that shape). Hand-rolled `init_from_closure` callers control
//!   their own capture set and write each field directly via
//!   `addr_of_mut!` — the outer function's frame stays small.

use core::convert::Infallible;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::num::{
    NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI128, NonZeroIsize, NonZeroU8,
    NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize,
};
use core::ptr::NonNull;

// ---------------------------------------------------------------------------
// Init<T, E>
// ---------------------------------------------------------------------------

/// Recipe for constructing a valid `T` into a caller-provided heap slot
/// without ever materialising a full `T` on the caller's stack.
///
/// The only consumers of this trait are
/// [`super::heap::KBox::try_init`] and
/// [`super::heap::PinBox::try_init`], both of which allocate the slot
/// first and then delegate to [`Init::__init`].
///
/// # Safety
///
/// Implementations of [`Init::__init`] must write a valid `T` into
/// `slot` before returning `Ok(())`. On `Err(_)`, `slot`'s bytes may
/// be partially written but the caller must not read them; the
/// allocator is responsible for freeing the uninitialised memory.
pub unsafe trait Init<T, E = Infallible> {
    /// Write a valid `T` into `slot`.
    ///
    /// # Safety
    ///
    /// `slot` must point to writable, properly aligned memory sized
    /// for `T`. The caller must not read `*slot` until this function
    /// returns `Ok(())`.
    unsafe fn __init(self, slot: *mut T) -> Result<(), E>;
}

/// Concrete [`Init`] implementation backing [`init_from_closure`].
///
/// Carrying a named type (rather than an `impl Trait` return) keeps
/// the init surface usable in positions that require a nameable
/// type, e.g. stored in a struct field or a function return whose
/// lifetime bound depends on the closure capture set.
pub struct InitClosure<T, E, F>(F, PhantomData<fn() -> (*mut T, E)>);

// SAFETY: forwards the caller's `Init::__init` safety contract to the
// wrapped closure, which is constructed via the `unsafe fn
// init_from_closure` entry point — the caller has already asserted
// the closure upholds the contract.
unsafe impl<T, E, F> Init<T, E> for InitClosure<T, E, F>
where
    F: FnOnce(*mut T) -> Result<(), E>,
{
    unsafe fn __init(self, slot: *mut T) -> Result<(), E> {
        (self.0)(slot)
    }
}

/// Build an [`Init<T, E>`] from a closure. The closure is the
/// primitive for hand-rolled constructors that write each field of
/// `T` via `core::ptr::addr_of_mut!` + `.write(_)` directly into the
/// heap slot — no intermediate `T` rvalue on the caller's stack.
///
/// # Safety
///
/// The closure must uphold [`Init::__init`]'s contract: on `Ok(())`,
/// every field of `*slot` that matters for `T`'s representation
/// invariants must be initialised to a valid value. The closure
/// must not read from `*slot` (the slot is uninitialised on entry).
pub unsafe fn init_from_closure<T, E, F>(f: F) -> InitClosure<T, E, F>
where
    F: FnOnce(*mut T) -> Result<(), E>,
{
    InitClosure(f, PhantomData)
}

/// An [`Init<T, Infallible>`] that fills `T`'s slot with all-zero
/// bytes. Safe because `T: Zeroable` certifies that pattern is a
/// valid `T`.
pub fn init_zeroed<T: Zeroable>()
-> InitClosure<T, Infallible, impl FnOnce(*mut T) -> Result<(), Infallible>> {
    // SAFETY: `T: Zeroable` ⇒ the all-zero bit pattern is a valid
    // `T`. `write_bytes` zeroes exactly `size_of::<T>()` bytes
    // starting at `slot`, satisfying `Init::__init`'s post-condition.
    // The closure's inner `unsafe` is ASSERTION of that safety
    // contract to `init_from_closure`.
    unsafe {
        init_from_closure(|slot: *mut T| {
            // SAFETY: `slot` is writable for `size_of::<T>()` bytes
            // per `Init::__init`'s precondition; `T: Zeroable` means
            // the zero pattern is representationally valid.
            core::ptr::write_bytes(slot as *mut u8, 0, core::mem::size_of::<T>());
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Zeroable
// ---------------------------------------------------------------------------

/// Marker: every byte pattern of all-zeros is a valid value of `T`.
///
/// This is the primitive that lets [`super::heap::KBox::zeroed`] and
/// [`super::heap::KVec::zeroed`] use the allocator's zeroed memory
/// directly without further initialisation.
///
/// # Safety
///
/// Implementors must ensure that a `T` constructed from all-zero
/// bytes upholds every invariant `T` depends on for soundness:
///
/// - No niche-constrained field (bool, char, `NonZero*`, `NonNull`,
///   enum discriminant) has a zero that would form an invalid
///   representation.
/// - No reference (`&T`, `&mut T`) appears in a position requiring
///   a valid pointee — all-zero references are null, which is UB
///   to dereference. (`Option<&T>` is safe because `None` is the
///   zero niche.)
/// - No `unsafe` invariant beyond the types composes to "zero bytes
///   is valid" — e.g. an `InvariantLifetime` phantom requiring
///   exclusivity must be upheld separately.
pub unsafe trait Zeroable {}

// ---------- primitive impls ----------

macro_rules! impl_zeroable_primitive {
    ($($t:ty),* $(,)?) => {
        $( unsafe impl Zeroable for $t {} )*
    };
}

impl_zeroable_primitive!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
);

// `bool` is Zeroable: `false` is represented as `0x00`, so the
// all-zero bit pattern is a well-formed `bool`. (The trait does not
// require every bit pattern to be valid — that stronger property is
// `Pod`'s job. `bool` is NOT `Pod` because `0x02..=0xFF` are invalid
// representations, but it IS `Zeroable` because the zero byte alone is
// a valid representation.) `char` remains excluded: while `U+0000` is
// also a valid `char`, treating `char` as zero-byte-valid invites
// surprises around niche layout in container types, and no current
// kernel call site needs it.
unsafe impl Zeroable for bool {}

// ---------- composite impls ----------

unsafe impl Zeroable for () {}
unsafe impl<T: ?Sized> Zeroable for PhantomData<T> {}
unsafe impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}
unsafe impl<T> Zeroable for MaybeUninit<T> {}

// Raw pointers: all-zero is null, which is a valid raw pointer
// representation (dereferencing it is UB, but the pointer *value*
// is fine).
unsafe impl<T: ?Sized> Zeroable for *const T where *const T: Sized {}
unsafe impl<T: ?Sized> Zeroable for *mut T where *mut T: Sized {}

// Niche-optimised Options: the zero pattern is the `None` variant.
unsafe impl<T: ?Sized> Zeroable for Option<NonNull<T>> where Option<NonNull<T>>: Sized {}
unsafe impl<'a, T: ?Sized + 'a> Zeroable for Option<&'a T> where Option<&'a T>: Sized {}
unsafe impl<'a, T: ?Sized + 'a> Zeroable for Option<&'a mut T> where Option<&'a mut T>: Sized {}

macro_rules! impl_zeroable_option_nonzero {
    ($($t:ty),* $(,)?) => {
        $( unsafe impl Zeroable for Option<$t> {} )*
    };
}

impl_zeroable_option_nonzero!(
    NonZeroU8,
    NonZeroU16,
    NonZeroU32,
    NonZeroU64,
    NonZeroU128,
    NonZeroUsize,
    NonZeroI8,
    NonZeroI16,
    NonZeroI32,
    NonZeroI64,
    NonZeroI128,
    NonZeroIsize,
);

// ---------- tuple impls ----------

macro_rules! impl_zeroable_tuple {
    ($($t:ident),+ $(,)?) => {
        unsafe impl<$($t: Zeroable),+> Zeroable for ($($t,)+) {}
    };
}

impl_zeroable_tuple!(A);
impl_zeroable_tuple!(A, B);
impl_zeroable_tuple!(A, B, C);
impl_zeroable_tuple!(A, B, C, D);
impl_zeroable_tuple!(A, B, C, D, E);
impl_zeroable_tuple!(A, B, C, D, E, F);
impl_zeroable_tuple!(A, B, C, D, E, F, G);
impl_zeroable_tuple!(A, B, C, D, E, F, G, H);
impl_zeroable_tuple!(A, B, C, D, E, F, G, H, I);
impl_zeroable_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_zeroable_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_zeroable_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);
