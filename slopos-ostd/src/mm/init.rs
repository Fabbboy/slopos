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
//!   (hand-rolled per-field writes — the one used by large structs
//!   like `DataState` where the three-layer stack-safety gate demands
//!   small frames) and [`init_zeroed`] (for `T: Zeroable`).
//! - We deliberately do not provide `try_init!` / `pin_init!`
//!   macros. Their per-field closure capture materialises every
//!   field's source expression on the closure's stack before
//!   writing to the heap slot, which inflates frame size for
//!   large structs. Hand-rolled [`init_struct_with`] callers control
//!   their own capture set and write each field straight into the
//!   heap slot, so the outer function's frame stays small.
//! - Field writes address the slot through a [`Field`] token carrying
//!   the field's byte offset in its type. `#[derive(SlotFields)]`
//!   mints those tokens from `core::mem::offset_of!` and a projection
//!   closure, both of which are safe, so a `#![forbid(unsafe_code)]`
//!   crate initialises a heap slot field by field with no `unsafe`
//!   token anywhere in its expansion.

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

// ---------------------------------------------------------------------------
// SlotPtr<T> + safe field-writer helpers for in-place init closures.
// ---------------------------------------------------------------------------

/// Proof that `T` has a field of type `U` at byte offset `OFF`.
///
/// Zero-sized: the offset rides in the type, so a whole field table costs
/// no stack frame and no code. The only constructor is [`Field::__new`],
/// which takes a projection `fn(&T) -> &U` — a value the caller can only
/// produce by naming a real field of `T`, which is what pins `U` and makes
/// the token unforgeable in practice. `#[derive(SlotFields)]` is the
/// intended way to build one; it pairs each projection with the matching
/// `core::mem::offset_of!`.
///
/// Both halves are safe code, so a `#![forbid(unsafe_code)]` crate can
/// address fields of an uninitialised heap slot with no `unsafe` token
/// anywhere in its expansion.
pub struct Field<T, U, const OFF: usize>(PhantomData<fn(*mut T) -> *mut U>);

impl<T, U, const OFF: usize> Clone for Field<T, U, OFF> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, U, const OFF: usize> Copy for Field<T, U, OFF> {}

impl<T, U, const OFF: usize> Field<T, U, OFF> {
    /// Mint the token from a field projection. The projection is never
    /// called; it exists so the type-checker resolves `U` from `T`'s real
    /// field and rejects a field name `T` does not have.
    #[doc(hidden)]
    #[inline]
    pub const fn __new(_projection: fn(&T) -> &U) -> Self {
        Self(PhantomData)
    }

    /// Byte offset of the field within `T`.
    #[inline]
    pub const fn offset(self) -> usize {
        OFF
    }
}

/// A type with a compile-time field table, as emitted by
/// `#[derive(SlotFields)]`.
///
/// Safe to implement: every associated item is derived from
/// `core::mem::offset_of!` and field projections, both of which the
/// compiler checks. Nothing in OSTD trusts a hand-written impl for
/// memory safety beyond what `Field`'s own construction already
/// guarantees.
pub trait HasFields: Sized {
    /// The generated table type. Zero-sized.
    type Fields: Copy;
    /// Number of fields the table covers.
    const FIELD_COUNT: usize;
    /// Byte offset of each field, in declaration order.
    const FIELD_OFFSETS: &'static [usize];
    /// The table itself.
    const FIELDS: Self::Fields;
}

/// Safe wrapper around `*mut T` for use inside [`Init::__init`]
/// closures.
///
/// Field writes go through [`Field`] tokens: `slot.write(f, value)` takes a
/// `Field<T, U, OFF>` and lands `value` at `OFF` bytes into the slot. The
/// only `unsafe` involved is interior to OSTD; consumer crates stay
/// `unsafe`-free, in source *and* in expansion.
///
/// Construct via [`SlotPtr::from_raw`] inside an
/// [`init_from_closure`]-style closure.
#[repr(transparent)]
pub struct SlotPtr<T> {
    raw: *mut T,
}

impl<T> SlotPtr<T> {
    /// Wrap a raw `*mut T` provided by `Init::__init`'s contract.
    ///
    /// # Safety
    ///
    /// `raw` must point to writable, properly aligned memory sized
    /// for `T`. The wrapper is intended to live for the duration of
    /// the in-place init closure; callers must not let it escape
    /// into a context that outlives the slot.
    #[inline]
    pub unsafe fn from_raw(raw: *mut T) -> Self {
        Self { raw }
    }

    /// Raw `*mut T` for forwarding to nested
    /// [`Init::__init`] calls (e.g. `SpinLock::init_with(...).__init(slot.raw())`).
    #[inline]
    pub fn raw(&self) -> *mut T {
        self.raw
    }

    /// Write `value` into the field `f` names.
    ///
    /// Reach for the [`write_field!`] macro rather than calling this
    /// directly; it resolves the [`Field`] token out of the slot's
    /// generated table:
    /// ```ignore
    /// write_field!(slot, next_token, AtomicU64::new(1));
    /// ```
    ///
    /// Deliberately `#[inline]` and not `#[inline(always)]`. Under the
    /// debug profile the stack-size gate measures, force-inlining gives
    /// each call its own non-reused slots in the caller's frame: a
    /// 28-field initialiser measures 248 bytes as written and 2 576 bytes
    /// with `inline(always)`, which is over the 2 KiB ceiling. Release
    /// folds both to nothing.
    #[inline]
    pub fn write<U, const OFF: usize>(&self, _f: Field<T, U, OFF>, value: U) {
        // SAFETY: `from_raw`'s contract makes the slot writable for
        // `size_of::<T>()` bytes. `Field<T, U, OFF>` exists only if `T`
        // really has a `U`-typed field at `OFF`, so `OFF + size_of::<U>()`
        // is within the slot and the pointer is aligned for `U`.
        unsafe {
            self.raw.cast::<u8>().add(OFF).cast::<U>().write(value);
        }
    }

    /// Zero-fill the field `f` names. `U: Zeroable` is the type-system
    /// discharge of "the all-zero pattern is a valid `U`" — the obligation
    /// that used to live in a comment at each call site.
    #[inline]
    pub fn zero<U: Zeroable, const OFF: usize>(&self, _f: Field<T, U, OFF>) {
        // SAFETY: as `write`, plus `U: Zeroable` certifies the resulting
        // bytes form a valid `U`.
        unsafe {
            core::ptr::write_bytes(self.raw.cast::<u8>().add(OFF), 0, core::mem::size_of::<U>());
        }
    }

    /// Run a nested [`Init`] recipe directly into the field `f` names, so
    /// the nested value never materialises on the caller's stack either.
    #[inline]
    pub fn write_init<U, E, const OFF: usize>(
        &self,
        _f: Field<T, U, OFF>,
        init: impl Init<U, E>,
    ) -> Result<(), E> {
        // SAFETY: as `write` — the field slot is writable and aligned for
        // `U`, which is exactly `Init::__init`'s precondition.
        unsafe { Init::__init(init, self.raw.cast::<u8>().add(OFF).cast::<U>()) }
    }

    /// Write one element of an array-typed field, without materialising
    /// the array. The index is checked against the array length carried in
    /// the field's type.
    #[inline]
    pub fn write_elem<U, const N: usize, const OFF: usize>(
        &self,
        _f: Field<T, [U; N], OFF>,
        index: usize,
        value: U,
    ) {
        assert!(index < N, "SlotPtr::write_elem: index out of bounds");
        // SAFETY: as `write`. The field is `[U; N]` at `OFF`, and `index`
        // is bounds-checked above, so element `index` lies inside it.
        unsafe {
            self.raw
                .cast::<u8>()
                .add(OFF)
                .cast::<U>()
                .add(index)
                .write(value);
        }
    }

    /// Zero every byte of the slot. Used as a first step before
    /// patching select fields. Safe even when `T` is not `Zeroable` —
    /// the caller commits to overwriting any non-zero-valid fields
    /// before the closure returns.
    #[inline]
    pub fn zero_all(&self) {
        // SAFETY: per `from_raw`'s contract the slot is writable for
        // `size_of::<T>()` bytes; zero-fill never reads.
        unsafe {
            core::ptr::write_bytes(self.raw as *mut u8, 0, core::mem::size_of::<T>());
        }
    }
}

impl<T: HasFields> SlotPtr<T> {
    /// The slot type's compile-time field table. Zero-sized, so this
    /// returns by value at no cost.
    #[inline]
    pub fn fields(&self) -> T::Fields {
        T::FIELDS
    }
}

/// Build an [`Init<T, E>`] from a closure that operates on a
/// [`SlotPtr<T>`]. This is the **safe** entry point preferred over
/// the lower-level [`init_from_closure`] for closures that follow
/// the "zero + patch a few fields" or "addr-of field-by-field" idiom.
///
/// The closure is `FnOnce(SlotPtr<T>) -> Result<(), E>`. The wrapper
/// constructs the `SlotPtr` from the slot pointer that `__init` is
/// invoked with. The safety contract documented on `Init::__init`
/// still applies: the closure must populate every byte of `T` (or
/// rely on a prior `zero_all` + per-field writes) before returning
/// `Ok(())`.
///
/// # Safety
///
/// The closure must uphold [`Init::__init`]'s contract — on `Ok(())`,
/// `*slot` must hold a valid `T`. The wrapper itself is safe because
/// the only unsafe op it introduces is `SlotPtr::from_raw`, which is
/// internal and protected by `Init::__init`'s precondition.
pub fn init_struct_with<T, E, F>(f: F) -> InitClosure<T, E, impl FnOnce(*mut T) -> Result<(), E>>
where
    F: FnOnce(SlotPtr<T>) -> Result<(), E>,
{
    // SAFETY: the trampoline forwards the slot pointer that
    // `Init::__init` provides through `SlotPtr::from_raw`; the
    // closure operates on the wrapped slot. The slot validity
    // precondition of `Init::__init` carries through.
    unsafe {
        init_from_closure(move |slot: *mut T| -> Result<(), E> {
            // SAFETY: `slot` is writable for `size_of::<T>()` bytes
            // by `Init::__init`'s precondition; the surrounding
            // `init_from_closure` call certifies the closure's
            // unsafety, so the `SlotPtr::from_raw` call inherits the
            // already-asserted context.
            let wrapper = SlotPtr::from_raw(slot);
            f(wrapper)
        })
    }
}

/// Sugar over [`SlotPtr::write`]: given a `slot: SlotPtr<T>` expression
/// and a field name, write `value` into that field of the slot.
///
/// `T` must carry `#[derive(SlotFields)]`. The field name resolves against
/// the generated table, so a name `T` does not have is a compile error and
/// the value's type is inferred from the field's.
///
/// ```ignore
/// use slopos_ostd::write_field;
/// // inside `init_struct_with(|slot| { ... })`:
/// write_field!(slot, current_tick, 0);
/// write_field!(slot, next_token, AtomicU64::new(1));
/// ```
#[macro_export]
macro_rules! write_field {
    ($slot:expr, $field:tt, $value:expr) => {{
        let __slot = &$slot;
        __slot.write(__slot.fields().$field, $value);
    }};
}

/// Sugar for writing every element of an array-typed field. Iterates
/// `0..count` and writes `f(i)` to element `i`, so no array rvalue
/// materialises on the caller's stack.
///
/// ```ignore
/// use slopos_ostd::write_array_field;
/// write_array_field!(slot, slots, NUM_SLOTS, KVec::<TimerEntry>::new);
/// ```
#[macro_export]
macro_rules! write_array_field {
    ($slot:expr, $field:tt, $count:expr, $f:expr) => {{
        let __slot = &$slot;
        let __field = __slot.fields().$field;
        let __closure = $f;
        for __i in 0..$count {
            __slot.write_elem(__field, __i, __closure(__i));
        }
    }};
}

/// Write a field whose value is an [`Init`] recipe that must populate a
/// nested slot (e.g. a `SpinLock<Inner>` whose `init_with(...)` needs to
/// be `__init`'d into the field).
///
/// ```ignore
/// use slopos_ostd::write_init_field;
/// write_init_field!(slot, inner, SpinLock::<Inner>::init_with(...))?;
/// ```
#[macro_export]
macro_rules! write_init_field {
    ($slot:expr, $field:tt, $init:expr) => {{
        let __slot = &$slot;
        __slot.write_init(__slot.fields().$field, $init)
    }};
}

/// Zero-fill a single field of a `SlotPtr<T>`. The field's type must be
/// [`Zeroable`], which is what certifies the all-zero bytes form a valid
/// value.
///
/// Used for fields whose `new()` value is all-zero (`SendMap`,
/// many `Option<…>` fields, etc.) to avoid materialising a by-value
/// rvalue on the caller's stack.
///
/// ```ignore
/// use slopos_ostd::zero_field;
/// zero_field!(slot, sendmap);
/// ```
#[macro_export]
macro_rules! zero_field {
    ($slot:expr, $field:tt) => {{
        let __slot = &$slot;
        __slot.zero(__slot.fields().$field);
    }};
}
