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

// ---------------------------------------------------------------------------
// SlotPtr<T> + safe field-writer helpers for in-place init closures.
// ---------------------------------------------------------------------------

/// Safe wrapper around `*mut T` for use inside [`Init::__init`]
/// closures.
///
/// Wraps the per-field `addr_of_mut!((*slot).field).write(value)`
/// pattern behind a single safe method [`SlotPtr::write_field`], which
/// takes a `FieldAccessor` closure returning `*mut U` and a `value: U`.
/// The single `unsafe` interior to OSTD covers the `*mut U`
/// dereference plus the `.write(value)` call; consumer crates stay
/// `unsafe`-free.
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

    /// Write `value` into the field reachable from `getter(slot.raw())`.
    ///
    /// `getter` is a `fn(*mut T) -> *mut U` that computes a field's
    /// address using `addr_of_mut!` — the macro itself does not
    /// dereference, only the syntactic `(*slot).field` expression
    /// inside it does. We accept that dereference inside the helper.
    ///
    /// Typical use:
    /// ```ignore
    /// slot.write_field(
    ///     |p| core::ptr::addr_of_mut!((*p).field) as *mut u32,
    ///     0xdeadbeef,
    /// );
    /// ```
    /// Or with the [`write_field!`] macro that fills in the closure:
    /// ```ignore
    /// write_field!(slot.field, 0xdeadbeef);
    /// ```
    #[inline]
    pub fn write_field<U>(&self, getter: unsafe fn(*mut T) -> *mut U, value: U) {
        // SAFETY: caller of `SlotPtr::from_raw` asserts the slot is
        // writable for `size_of::<T>()` bytes; the `getter` returns a
        // pointer into that slot, so the `.write(value)` lands inside
        // the caller's slot.
        unsafe {
            let p = getter(self.raw);
            p.write(value);
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

    /// Take a typed `*mut U` to the result of `getter(slot)` — for
    /// the rare per-element-array-write loops that cannot fit the
    /// `write_field` shape.
    ///
    /// # Safety
    ///
    /// Caller must use the returned pointer only to write `U` values
    /// inside the slot, and must finish every required write before
    /// the enclosing closure returns `Ok(())`.
    #[inline]
    pub unsafe fn field_ptr<U>(&self, getter: unsafe fn(*mut T) -> *mut U) -> *mut U {
        // SAFETY: caller forwards the safety contract.
        unsafe { getter(self.raw) }
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

/// Sugar over [`SlotPtr::write_field`]: given a `slot: SlotPtr<T>`
/// expression and `field` access path, write `value` into the field
/// via `addr_of_mut!`.
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
        // SAFETY: the surrounding `init_struct_with` provides a
        // `SlotPtr<T>` whose underlying slot is writable for
        // `size_of::<T>()` bytes. `addr_of_mut!` computes a
        // sub-pointer without reading, and `.write(value)` lands
        // inside the slot.
        unsafe {
            let __slot_ptr = ($slot).raw();
            core::ptr::addr_of_mut!((*__slot_ptr).$field).write($value);
        }
    }};
}

/// Sugar for writing every element of an array-typed field. Iterates
/// `0..count` and writes `f(i)` to element `i`.
///
/// ```ignore
/// use slopos_ostd::write_array_field;
/// write_array_field!(slot, slots, NUM_SLOTS, KVec::<TimerEntry>::new);
/// ```
#[macro_export]
macro_rules! write_array_field {
    ($slot:expr, $field:tt, $count:expr, $f:expr) => {{
        // SAFETY: the surrounding `init_struct_with` closure has a
        // valid `SlotPtr<T>` whose underlying slot is writable for
        // `size_of::<T>()` bytes (covering the array field). The
        // `addr_of_mut!` macro computes the array's base address
        // without reading; we then write each element through the
        // typed pointer.
        unsafe {
            let __array_ptr = core::ptr::addr_of_mut!((*($slot).raw()).$field);
            let __closure = $f;
            for __i in 0..$count {
                // `addr_of_mut!((*array_ptr)[i])` yields the
                // correctly-typed element pointer; `.write()` then
                // takes the closure's return type without further
                // inference hops.
                core::ptr::addr_of_mut!((*__array_ptr)[__i]).write(__closure(__i));
            }
        }
    }};
}

/// Write `*mut T`-style raw fields when the value is an `Init` that
/// must populate a nested slot (e.g. a `SpinLock<Inner>` whose
/// `init_with(...)` recipe needs to be `__init`'d into the field).
///
/// ```ignore
/// use slopos_ostd::write_init_field;
/// write_init_field!(slot, inner, SpinLock::<Inner>::init_with(...))?;
/// ```
#[macro_export]
macro_rules! write_init_field {
    ($slot:expr, $field:tt, $init:expr) => {{
        let __init = $init;
        // SAFETY: `init_struct_with` guarantees the slot is valid
        // for `T`; the `addr_of_mut!` computes the field address
        // and `Init::__init` writes a valid value into it.
        let __res: Result<(), _> = unsafe {
            let __field_ptr = core::ptr::addr_of_mut!((*($slot).raw()).$field);
            $crate::mm::init::Init::__init(__init, __field_ptr)
        };
        __res
    }};
}

/// Zero-fill a single field of a `SlotPtr<T>`. The caller asserts the
/// resulting all-zero bytes form a representationally valid value for
/// the field's type.
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
        // SAFETY: caller asserts the all-zero pattern is a valid
        // value for the field's type. `addr_of_mut!` computes the
        // field address without reading. `write_bytes` writes
        // exactly N bytes where N is the field's size derived from
        // the pointed-to type via the helper below.
        unsafe {
            let __field_ptr = core::ptr::addr_of_mut!((*($slot).raw()).$field);
            $crate::mm::init::__zero_at_typed_ptr(__field_ptr);
        }
    }};
}

/// Internal helper for `zero_field!`: zero-fills exactly
/// `size_of::<T>()` bytes starting at `ptr`.
///
/// # Safety
///
/// `ptr` must point to writable memory aligned for `T` and sized for
/// `T`. The caller must additionally assert that the all-zero bit
/// pattern is representationally valid for `T`.
#[inline]
#[doc(hidden)]
pub unsafe fn __zero_at_typed_ptr<T>(ptr: *mut T) {
    // SAFETY: caller of the surrounding macro upholds the contract.
    unsafe {
        core::ptr::write_bytes(ptr as *mut u8, 0, core::mem::size_of::<T>());
    }
}
