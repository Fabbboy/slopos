//! In-place initialisation surface: SlopOS's in-house [`Init<T, E>`] +
//! [`Zeroable`], consumed through [`super::heap::KBox::try_init`] /
//! [`super::heap::PinBox::try_init`] and their `zeroed` counterparts.
//!
//! No `Pin` machinery: SlopOS has no self-referential kernel types and no
//! in-kernel async. No `try_init!` / `pin_init!` macros either — their
//! per-field closure capture materialises every field's source expression on
//! the closure's stack before writing the heap slot, inflating frame size for
//! large structs, whereas hand-rolled [`init_struct_with`] callers control
//! their own capture set and write straight into the slot.

#[cfg(debug_assertions)]
use core::cell::Cell;
use core::convert::Infallible;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::num::{
    NonZeroI8, NonZeroI16, NonZeroI32, NonZeroI64, NonZeroI128, NonZeroIsize, NonZeroU8,
    NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize,
};
use core::ptr::NonNull;

/// Recipe for constructing a valid `T` into a caller-provided heap slot
/// without ever materialising a full `T` on the caller's stack.
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

/// Concrete [`Init`] implementation backing [`init_from_closure`]. A named
/// type rather than an `impl Trait` return, so it stays usable in positions
/// that require a nameable type — a struct field, or a return whose lifetime
/// bound depends on the closure capture set.
pub struct InitClosure<T, E, F>(F, PhantomData<fn() -> (*mut T, E)>);

// SAFETY: forwards `Init::__init`'s contract to the wrapped closure; the
// `unsafe fn init_from_closure` entry point is where the caller asserted the
// closure upholds it.
unsafe impl<T, E, F> Init<T, E> for InitClosure<T, E, F>
where
    F: FnOnce(*mut T) -> Result<(), E>,
{
    unsafe fn __init(self, slot: *mut T) -> Result<(), E> {
        (self.0)(slot)
    }
}

/// Build an [`Init<T, E>`] from a closure that writes each field of `T`
/// directly into the heap slot — no intermediate `T` rvalue on the caller's
/// stack.
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

/// An [`Init`] that moves an already-owned `value` into the slot. Placement,
/// not construction: a large `T` the caller already holds reaches
/// `KArc::try_init` / `KBox::try_init` without the extra whole-`T` temporary
/// `try_new` would materialise on the stack.
pub fn init_from_owned<T, E>(value: T) -> InitClosure<T, E, impl FnOnce(*mut T) -> Result<(), E>> {
    // SAFETY: the closure writes the slot exactly once, from an owned
    // value it consumes, so no `T` is duplicated or dropped twice.
    unsafe {
        init_from_closure(move |slot: *mut T| -> Result<(), E> {
            slot.write(value);
            Ok(())
        })
    }
}

/// An [`Init<T, Infallible>`] that fills `T`'s slot with all-zero
/// bytes. Safe because `T: Zeroable` certifies that pattern is a
/// valid `T`.
pub fn init_zeroed<T: Zeroable>()
-> InitClosure<T, Infallible, impl FnOnce(*mut T) -> Result<(), Infallible>> {
    // SAFETY: `T: Zeroable` ⇒ the all-zero bit pattern is a valid `T`, so
    // zeroing exactly `size_of::<T>()` bytes satisfies `Init::__init`'s
    // post-condition.
    unsafe {
        init_from_closure(|slot: *mut T| {
            // SAFETY: `slot` is writable for `size_of::<T>()` bytes per
            // `Init::__init`'s precondition.
            core::ptr::write_bytes(slot as *mut u8, 0, core::mem::size_of::<T>());
            Ok(())
        })
    }
}

/// Marker: every byte pattern of all-zeros is a valid value of `T`.
///
/// This is what lets [`super::heap::KBox::zeroed`] and
/// [`super::heap::KVec::zeroed`] use the allocator's zeroed memory directly
/// without further initialisation.
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

macro_rules! impl_zeroable_primitive {
    ($($t:ty),* $(,)?) => {
        $( unsafe impl Zeroable for $t {} )*
    };
}

impl_zeroable_primitive!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
);

// `false` is `0x00`, so the all-zero pattern is a well-formed `bool`. (`bool`
// is not `Pod` — `0x02..=0xFF` are invalid — but `Zeroable` asks only about the
// zero byte.) `char` stays excluded: no kernel call site needs it, and
// zero-byte-valid `char` invites surprises around niche layout in containers.
unsafe impl Zeroable for bool {}

unsafe impl Zeroable for () {}
unsafe impl<T: ?Sized> Zeroable for PhantomData<T> {}
unsafe impl<T: Zeroable, const N: usize> Zeroable for [T; N] {}
unsafe impl<T> Zeroable for MaybeUninit<T> {}

// Raw pointers: all-zero is null, a valid pointer *value* (only
// dereferencing it is UB).
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

/// Proof that `T` has a field of type `U` at byte offset `OFF`.
///
/// Zero-sized: the offset rides in the type, so a whole field table costs no
/// stack frame and no code. The only constructor is [`Field::__new`], whose
/// projection `fn(&T) -> &U` the caller can only produce by naming a real field
/// of `T` — that is what pins `U` and makes the token unforgeable in practice.
/// `#[derive(SlotFields)]` pairs each projection with the matching
/// `core::mem::offset_of!`. Both halves are safe code, so a
/// `#![forbid(unsafe_code)]` crate can address fields of an uninitialised heap
/// slot with no `unsafe` token anywhere in its expansion.
pub struct Field<T, U, const OFF: usize>(PhantomData<fn(*mut T) -> *mut U>);

impl<T, U, const OFF: usize> Clone for Field<T, U, OFF> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, U, const OFF: usize> Copy for Field<T, U, OFF> {}

impl<T, U, const OFF: usize> Field<T, U, OFF> {
    /// Mint the token from a field projection. The projection is never called;
    /// it exists so the type-checker resolves `U` from `T`'s real field and
    /// rejects a field name `T` does not have.
    #[doc(hidden)]
    #[inline]
    pub const fn __new(_projection: fn(&T) -> &U) -> Self {
        Self(PhantomData)
    }

    #[inline]
    pub const fn offset(self) -> usize {
        OFF
    }
}

/// A type with a compile-time field table, as emitted by
/// `#[derive(SlotFields)]`.
///
/// Safe to implement: nothing in OSTD trusts a hand-written impl for memory
/// safety beyond what [`Field`]'s own construction already guarantees.
pub trait HasFields: Sized {
    /// The generated table type. Zero-sized.
    type Fields: Copy;
    const FIELD_COUNT: usize;
    /// Byte offset of each field, in declaration order.
    const FIELD_OFFSETS: &'static [usize];
    const FIELDS: Self::Fields;
}

/// Safe wrapper around `*mut T` for use inside [`Init::__init`] closures.
///
/// Field writes go through [`Field`] tokens, so the only `unsafe` involved is
/// interior to OSTD and consumer crates stay `unsafe`-free in source *and* in
/// expansion. Construct via [`SlotPtr::from_raw`] inside an
/// [`init_from_closure`]-style closure.
pub struct SlotPtr<T> {
    raw: *mut T,
    /// Which fields have been written, by index into
    /// [`HasFields::FIELD_OFFSETS`]; the top bit is the `zero_all` flag.
    #[cfg(debug_assertions)]
    covered: Cell<u64>,
}

/// Proof that a slot holds a fully initialised `T`.
///
/// [`init_struct_with`]'s closure must return one, and [`SlotPtr::finish`] is
/// the only thing that mints one. Without that, a `#![forbid(unsafe_code)]`
/// crate could write `KBox::try_init(init_struct_with(|_slot| Ok(())))` and get
/// a `T` whose bytes were never written — a safe path to uninitialised memory.
pub struct Initialised<T>(PhantomData<fn() -> T>);

#[cfg(debug_assertions)]
const ZEROED_BIT: u32 = 63;

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
        Self {
            raw,
            #[cfg(debug_assertions)]
            covered: Cell::new(0),
        }
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn mark(&self, offset: usize)
    where
        T: HasFields,
    {
        if let Some(index) = T::FIELD_OFFSETS.iter().position(|&o| o == offset)
            && index < ZEROED_BIT as usize
        {
            self.covered.set(self.covered.get() | (1u64 << index));
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn mark(&self, _offset: usize)
    where
        T: HasFields,
    {
    }

    /// Raw `*mut T`, for forwarding to nested [`Init::__init`] calls.
    #[inline]
    pub fn raw(&self) -> *mut T {
        self.raw
    }

    /// Zero every byte of the slot, as a first step before patching select
    /// fields. Safe even when `T` is not `Zeroable` — the caller commits to
    /// overwriting any non-zero-valid field before the closure returns.
    #[inline]
    pub fn zero_all(&self) {
        // SAFETY: per `from_raw`'s contract the slot is writable for
        // `size_of::<T>()` bytes; zero-fill never reads.
        unsafe {
            core::ptr::write_bytes(self.raw as *mut u8, 0, core::mem::size_of::<T>());
        }
        #[cfg(debug_assertions)]
        self.covered.set(self.covered.get() | (1u64 << ZEROED_BIT));
    }
}

impl<T: HasFields> SlotPtr<T> {
    /// The slot type's compile-time field table. Zero-sized, so returning it
    /// by value costs nothing.
    #[inline]
    pub fn fields(&self) -> T::Fields {
        T::FIELDS
    }

    /// Write `value` into the field `f` names. Prefer the [`write_field!`]
    /// macro, which resolves the [`Field`] token out of the generated table.
    ///
    /// Deliberately `#[inline]` and not `#[inline(always)]`. Under the debug
    /// profile the stack-size gate measures, force-inlining gives each call its
    /// own non-reused slots in the caller's frame: a 28-field initialiser
    /// measures 248 bytes as written and 2 576 bytes with `inline(always)`,
    /// which is over the 2 KiB ceiling. Release folds both to nothing.
    #[inline]
    pub fn write<U, const OFF: usize>(&self, _f: Field<T, U, OFF>, value: U) {
        // SAFETY: `from_raw`'s contract makes the slot writable for
        // `size_of::<T>()` bytes. `Field<T, U, OFF>` exists only if `T`
        // really has a `U`-typed field at `OFF`, so `OFF + size_of::<U>()`
        // is within the slot and the pointer is aligned for `U`.
        unsafe {
            self.raw.cast::<u8>().add(OFF).cast::<U>().write(value);
        }
        self.mark(OFF);
    }

    /// Zero-fill the field `f` names. `U: Zeroable` is the type-system
    /// discharge of "the all-zero pattern is a valid `U`".
    #[inline]
    pub fn zero<U: Zeroable, const OFF: usize>(&self, _f: Field<T, U, OFF>) {
        // SAFETY: as `write`, plus `U: Zeroable` certifies the resulting
        // bytes form a valid `U`.
        unsafe {
            core::ptr::write_bytes(self.raw.cast::<u8>().add(OFF), 0, core::mem::size_of::<U>());
        }
        self.mark(OFF);
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
        let result = unsafe { Init::__init(init, self.raw.cast::<u8>().add(OFF).cast::<U>()) };
        if result.is_ok() {
            self.mark(OFF);
        }
        result
    }

    /// Write one element of an array-typed field, without materialising the
    /// array.
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
        if index + 1 == N {
            self.mark(OFF);
        }
    }

    /// Close the slot out and hand back the proof that `T` is initialised.
    ///
    /// Under `debug_assertions` this asserts that the closure either zeroed the
    /// whole slot or wrote every field. Release builds skip it: the barrier
    /// that actually closes the hole is [`Initialised`] being unforgeable.
    ///
    /// Types with more than 63 fields are not tracked; the coverage word is
    /// one `u64` and the zero-flag takes the top bit.
    #[inline]
    pub fn finish(self) -> Initialised<T> {
        #[cfg(debug_assertions)]
        {
            let covered = self.covered.get();
            let zeroed = covered & (1u64 << ZEROED_BIT) != 0;
            let count = T::FIELD_COUNT;
            if !zeroed && count <= ZEROED_BIT as usize {
                let expected = if count == 64 {
                    u64::MAX
                } else {
                    (1u64 << count) - 1
                };
                assert!(
                    covered & expected == expected,
                    "SlotPtr::finish: init closure left fields unwritten",
                );
            }
        }
        Initialised(PhantomData)
    }
}

/// Build an [`Init<T, E>`] from a closure that operates on a [`SlotPtr<T>`] —
/// the entry point for the "zero + patch a few fields" and "write every field"
/// idioms, preferred over the lower-level [`init_from_closure`].
///
/// The closure must return [`Initialised<T>`], which only [`SlotPtr::finish`]
/// mints, so it cannot claim success without having gone through the slot.
pub fn init_struct_with<T, E, F>(f: F) -> InitClosure<T, E, impl FnOnce(*mut T) -> Result<(), E>>
where
    F: FnOnce(SlotPtr<T>) -> Result<Initialised<T>, E>,
{
    // SAFETY: `Init::__init`'s post-condition — a valid `T` on `Ok(())` — is
    // discharged by the `Initialised<T>` the closure has to produce, which only
    // `SlotPtr::finish` can mint.
    unsafe {
        init_from_closure(move |slot: *mut T| -> Result<(), E> {
            // SAFETY: `slot` is writable for `size_of::<T>()` bytes by
            // `Init::__init`'s precondition.
            let wrapper = SlotPtr::from_raw(slot);
            f(wrapper).map(|_proof| ())
        })
    }
}

/// Sugar over [`SlotPtr::write`]: write `value` into the named field of `slot`.
/// `T` must carry `#[derive(SlotFields)]`; the name resolves against the
/// generated table, so a field `T` does not have is a compile error.
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

/// Sugar for writing every element of an array-typed field, so no array rvalue
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

/// Zero-fill a single field of a `SlotPtr<T>`; the field's type must be
/// [`Zeroable`], which certifies the all-zero bytes form a valid value. Used
/// for fields whose `new()` value is all-zero, to avoid materialising a
/// by-value rvalue on the caller's stack.
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
