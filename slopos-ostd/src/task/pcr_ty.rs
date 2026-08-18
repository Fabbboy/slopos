//! Which `TaskInner` monomorphisation the PCR's current-task slot holds.
//!
//! The slot is a `*mut ()`: nothing in its type stops a reader casting it to a
//! `TaskInner<A, B>` the writer never published, and both sites are safe code
//! in crates that forbid `unsafe`. [`CurrentTask::get`] and the typed publisher
//! therefore both require [`PcrTaskType`], which holds exactly when both stack
//! parameters are declared [`PcrStackTy`].
//!
//! [`declare_pcr_stack_type!`] is exported, so a mismatched cast is not
//! *impossible* — only impossible **by accident**: a second monomorphisation
//! takes a macro invocation in a file the unsafe gate and review both look at.
//!
//! OSTD cannot name the concrete type itself. `KernelStack` and `UnsafeStack`
//! live in `sched`, and `unsafe impl PcrTaskType for TaskInner<…>` outside OSTD
//! is E0117 because local type *arguments* do not make a foreign ADT head
//! local. The macro impls the marker for `sched`'s *own* `TaskStack<_>`
//! instead, and a blanket impl here derives `PcrTaskType` from that.
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

// SAFETY: both parameters carry `PcrStackTy`, so a reader naming this
// monomorphisation names the one the publisher wrote.
unsafe impl<K: PcrStackTy, U: PcrStackTy> PcrTaskType for TaskInner<K, U> {}

/// Declare a caller-local type as one of the kernel's task stack handles.
///
/// Takes a `$ty:ty` fragment rather than an ident because the kernel spells its
/// stack handles as aliases (`pub type KernelStack = TaskStack<KstackRegion>`),
/// and the impl legal outside OSTD is the one whose head is the caller's own
/// `TaskStack<_>`.
///
/// The `unsafe impl` lives in this expansion, not at the invocation site, so an
/// invoking crate keeps `#![forbid(unsafe_code)]`.
///
/// # Safety
///
/// The invoking crate asserts the [`PcrStackTy`] obligation for the named type.
#[macro_export]
#[allow_internal_unsafe]
macro_rules! declare_pcr_stack_type {
    ($ty:ty) => {
        // SAFETY: the invoking crate asserts this is a stack-handle type the
        // live kernel instantiates `TaskInner` with.
        unsafe impl $crate::task::PcrStackTy for $ty {}
    };
}

/// The stack-handle type OSTD's own tests instantiate `TaskInner` with.
///
/// Not a stack. `TaskStack` lives in `sched` and cannot be linked into a host
/// or Miri test, and `()` must **not** be a `PcrStackTy` —
/// `CurrentTask::<(), ()>::get()` compiling is the exact hole this module
/// closes.
#[cfg(any(test, feature = "test-helpers"))]
pub struct HostStack;

// SAFETY: nothing ever publishes a `TaskInner<HostStack, HostStack>` into a
// PCR — the host tests that name it run with no PCR at all — so no reader can
// observe a mismatch.
#[cfg(any(test, feature = "test-helpers"))]
unsafe impl PcrStackTy for HostStack {}
