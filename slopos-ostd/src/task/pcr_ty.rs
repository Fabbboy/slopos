//! Which `TaskInner` monomorphisation the PCR's current-task slot holds.
//!
//! The slot is a `*mut ()`. Nothing in its type stops a reader from casting it
//! to a `TaskInner<A, B>` the writer never published, which would reinterpret
//! the task body at whatever offsets `A` and `B` imply — and both the reader
//! and the writer are safe code in crates that forbid `unsafe`, so neither
//! site could be made to carry the obligation.
//!
//! This is where the obligation lives instead. [`CurrentTask::get`] and the
//! typed publisher both require [`PcrTaskType`], which holds exactly when both
//! stack parameters are declared [`PcrStackTy`] — so reader and writer are
//! bound to the same monomorphisation by the type system rather than by a
//! comment on each.
//!
//! # What the seal buys, and what it does not
//!
//! [`declare_pcr_stack_type!`] is exported, so any crate can declare any type.
//! This does not make a mismatched cast *impossible*; it makes it impossible
//! **by accident**. A second monomorphisation now requires someone to write a
//! macro invocation naming a stack type, in a file the unsafe gate and code
//! review both look at, rather than writing two turbofish arguments at a call
//! site. That is the same class of guarantee `TaskExclusive`'s sealing gives.
//!
//! # Why not just name the concrete type in OSTD
//!
//! It cannot: `KernelStack` and `UnsafeStack` live in `sched`. And the impl
//! cannot be written there either — `unsafe impl PcrTaskType for TaskInner<…>`
//! outside OSTD is E0117, because local type *arguments* do not make a foreign
//! ADT head local. The macro sidesteps that by impl'ing the foreign marker for
//! `sched`'s *own* `TaskStack<_>`, which is always legal, and OSTD derives
//! `PcrTaskType` from there through a blanket impl it owns.
//!
//! [`CurrentTask::get`]: crate::task::CurrentTask::get

use crate::task::kernel_task::TaskInner;

/// A stack-handle type the running kernel instantiates `TaskInner` with.
///
/// # Safety
///
/// The implementor must be one of the two stack-handle types the live kernel
/// actually builds tasks from. Declaring anything else and then naming it in a
/// `CurrentTask` turbofish would reinterpret every task body the PCR hands
/// back. Declare through [`declare_pcr_stack_type!`] rather than by hand.
pub unsafe trait PcrStackTy {}

/// A `TaskInner` monomorphisation that may be read back out of the PCR.
///
/// Sealed by construction: the blanket impl below is the only one, so a type is
/// a `PcrTaskType` exactly when both of its stack parameters are declared
/// [`PcrStackTy`].
///
/// # Safety
///
/// Implemented only by that blanket impl; see [`PcrStackTy`] for the obligation
/// it rests on.
pub unsafe trait PcrTaskType {}

// SAFETY: both parameters carry `PcrStackTy`, which is exactly the claim "this
// is a stack-handle type the live kernel instantiates `TaskInner` with". A
// reader naming this monomorphisation therefore names the one the publisher
// wrote.
unsafe impl<K: PcrStackTy, U: PcrStackTy> PcrTaskType for TaskInner<K, U> {}

/// Declare a caller-local type as one of the kernel's task stack handles.
///
/// Takes a `$ty:ty` fragment rather than an ident because the kernel spells its
/// stack handles as aliases (`pub type KernelStack = TaskStack<KstackRegion>`),
/// and the impl that is legal to write outside OSTD is the one whose head is
/// the caller's own `TaskStack<_>`.
///
/// The `unsafe impl` lives in this expansion, not at the invocation site, so an
/// invoking crate keeps `#![forbid(unsafe_code)]` — the same arrangement as
/// `write_field!` and `hermetic_state!`.
///
/// # Safety
///
/// The invoking crate asserts the [`PcrStackTy`] obligation for the named type.
#[macro_export]
macro_rules! declare_pcr_stack_type {
    ($ty:ty) => {
        // SAFETY: the invoking crate asserts this is a stack-handle type the
        // live kernel instantiates `TaskInner` with.
        unsafe impl $crate::task::PcrStackTy for $ty {}
    };
}

/// The stack-handle type OSTD's own tests instantiate `TaskInner` with.
///
/// Not a stack. It exists because `TaskStack` lives in `sched` and cannot be
/// linked into a host or Miri test, and because `()` must **not** be a
/// `PcrStackTy` — `CurrentTask::<(), ()>::get()` compiling is the exact hole
/// this module closes, so the tests get a type of their own rather than a
/// blanket exemption.
#[cfg(any(test, feature = "test-helpers"))]
pub struct HostStack;

// SAFETY: nothing ever publishes a `TaskInner<HostStack, HostStack>` into a
// PCR — the host tests that name it run with no PCR at all, which is what they
// assert — so no reader can observe a mismatch.
#[cfg(any(test, feature = "test-helpers"))]
unsafe impl PcrStackTy for HostStack {}
