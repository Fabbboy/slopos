//! Exclusive access to a task's register state, witnessed by a type.
//!
//! A published task is reachable through `KArc<Task>`, which yields only
//! `&TaskInner`. So the fields the kernel *must* still write after publication —
//! the saved register context, the FPU area, the user-mode round-trip slots —
//! cannot be reached through a Rust `&mut`. They live in [`TaskOwnCell`], and
//! writing one requires a value that proves the writer has exclusive access.
//!
//! # Why a witness rather than a lock
//!
//! These fields are written on the context-switch path, which runs with
//! interrupts off and must not acquire anything: the allocator's reuse path
//! performs synchronous cross-CPU work, and a lock taken in the switch window
//! is the shape of the known slab/LUF deadlock. Exclusivity here is not
//! *arranged* by taking a lock, it is a fact about the CPU that already holds:
//! only the CPU running a task touches that task's registers, and only the CPU
//! performing a switch touches either endpoint's. The witness makes that fact
//! checkable instead of commented.
//!
//! # The two witnesses
//!
//! - [`CurrentTask`] — this CPU is running the task. Minted from the PCR.
//! - [`SwitchWindow`] — this CPU is switching between two tasks and owns both
//!   endpoints' dispatch references for the duration.
//!
//! Both are `!Send` and `!Sync`, so a witness cannot be observed from a CPU
//! other than the one that minted it, and the trait is sealed, so no crate
//! outside OSTD can forge a third.
//!
//! # What a witness does *not* authorise
//!
//! Not the atomic fields — they need no witness — and not the states that
//! merely *look* exclusive. In particular a registered-but-unpublished task is
//! **not** exclusive: `SchedPlacement::Nascent` proves it is unschedulable, not
//! that it is unobservable, and a nascent task is still reachable through every
//! registry lookup, the active-task walk, the cr3 scan, the job-control
//! handles, and the diagnostic dump. Exclusive access before publication comes
//! from `KArc::get_mut` on the sole strong reference instead, which proves
//! uniqueness rather than asserting it.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr::NonNull;

use slopos_abi::task::INVALID_TASK_ID;

use crate::KArc;
use crate::cpu::x86_64::pcr;
use crate::task::kernel_task::TaskInner;
use crate::task::placement::task_placement_clone;

mod sealed {
    pub trait Sealed {}
}

/// Proof that the holder has exclusive access to one specific task's register
/// state.
///
/// # Safety
///
/// An implementor must genuinely have exclusive access to the task
/// [`witnessed`](TaskExclusive::witnessed) names, for as long as the witness is
/// alive, and must be `!Send`/`!Sync` so that exclusivity cannot be observed
/// from another CPU. The trait is sealed; [`CurrentTask`] and [`SwitchWindow`]
/// are the only implementors.
pub unsafe trait TaskExclusive<K, U>: sealed::Sealed {
    /// The task this witness authorises. Compared against the cell's owner so a
    /// witness for one task cannot be used to write another's state.
    fn witnessed(&self) -> *const TaskInner<K, U>;
}

/// A task field that only its exclusive owner may write.
///
/// `#[repr(transparent)]`, so wrapping a field changes neither `Task`'s size
/// nor its alignment and the layout razors keep holding.
#[repr(transparent)]
pub struct TaskOwnCell<T> {
    value: UnsafeCell<T>,
}

// No `unsafe impl Sync`: `TaskInner` is already neither `Send` nor `Sync` (it
// carries raw pointers), and every cross-CPU hand-off of a task launders
// through `KernelSync` or a raw placement pointer. Adding an `UnsafeCell` field
// therefore costs nothing and adds no unsafe trait impl.

impl<T> TaskOwnCell<T> {
    #[inline]
    pub const fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }

    /// Exclusive pointer to the contents, authorised by `witness`.
    ///
    /// Returns `*mut T`, never `&mut T` and never `T`:
    ///
    /// - not `T`, because one `FpuState` by value is 2.6 KiB on the caller's
    ///   frame and the 2 KiB stack-frame gate would reject it;
    /// - not `&mut T`, because two witnesses for the same task can legitimately
    ///   coexist in nested frames (an interrupt handler above a syscall on the
    ///   same task), and two `&mut` to one field would be aliasing UB even when
    ///   the accesses are disjoint.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid for as long as `self` is, and the caller
    /// must not retain it past the witness. The witness must name the task this
    /// cell belongs to; a debug assertion checks it at every call site inside
    /// OSTD.
    #[inline]
    pub(crate) fn get_ptr<K, U>(&self, _witness: &impl TaskExclusive<K, U>) -> *mut T {
        self.value.get()
    }

    /// Exclusive access proven by `&mut self` rather than by a witness.
    ///
    /// Reachable only where a `&mut TaskInner` is, which after publication is
    /// nowhere: `KArc::get_mut` on the sole pre-registration strong reference,
    /// and `Drop`.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Unsynchronised read pointer, for diagnostics only.
    ///
    /// The contents may be written concurrently by the owning CPU, so the
    /// caller must read through `read_unaligned`/`read_volatile` and must never
    /// form a `&T`. Torn values are expected and acceptable: every consumer is
    /// a log line or a stack-walk seed. This is the pointer accessors' existing
    /// behaviour, named rather than implied.
    #[inline]
    pub fn as_ptr_racy(&self) -> *const T {
        self.value.get().cast_const()
    }
}

impl<T: Default> Default for TaskOwnCell<T> {
    #[inline]
    fn default() -> Self {
        Self::new(T::default())
    }
}

// ---------------------------------------------------------------------------
// CurrentTask — invariant I5: `current` is a borrow, never an owned handle
// ---------------------------------------------------------------------------

/// Borrow of the task running on this CPU.
///
/// Takes no reference count. The task cannot be freed underneath it because a
/// task's own execution pins its allocation: the reap gate declines while
/// `task_is_dispatch_pinned` holds, and that predicate's second disjunct tests
/// whether the task is any CPU's `PCR.current_task` — which is exactly the
/// condition under which this guard exists. **Deleting that disjunct deletes
/// this guard's soundness proof**, so it is spelled out at the gate too.
///
/// `!Send` and `!Sync`, which is the whole enforcement: a guard cannot travel
/// to a CPU whose PCR names a different task. It deliberately does *not* hold a
/// preemption guard — several paths that read the current task go on to block,
/// and a held preempt guard across a deschedule trips the switch assertion.
/// Migration is safe without one: the guard travels with the task's own frames,
/// and those only execute while that task is scheduled.
pub struct CurrentTask<K, U> {
    ptr: NonNull<TaskInner<K, U>>,
    id: u32,
    /// Opts out of `Send`/`Sync`.
    _not_send: PhantomData<*mut ()>,
}

impl<K, U> CurrentTask<K, U> {
    /// The task running on this CPU, or `None` when there is none.
    ///
    /// `None` covers every case in which `PCR.current_task` does not name a
    /// heap task: GS_BASE not yet installed, and the pre-heap bootstrap stub a
    /// CPU parks on before its first dispatch. The discriminator is the id
    /// rather than the pointer's address, because `set_current_task` is the one
    /// publisher of the pair and writes both in the same call — so an id that
    /// is not `INVALID_TASK_ID` guarantees the pointer names a real task.
    ///
    /// # Type parameters
    ///
    /// `K` and `U` must be the stack-handle types the running kernel
    /// instantiated `TaskInner` with. The PCR slot is type-erased, so naming
    /// different ones would reinterpret the task body. The kernel spells this
    /// through its `Current` alias, which fixes both; there is exactly one
    /// `TaskInner` monomorphisation reachable from the PCR, and the switch and
    /// spawner surfaces already assume the same thing.
    #[inline]
    pub fn get() -> Option<Self> {
        let id = pcr::current_task_id();
        if id == INVALID_TASK_ID {
            return None;
        }
        // A valid id and a bootstrap-stub pointer are contradictory: every site
        // that parks a CPU on a stub publishes `INVALID_TASK_ID` with it, and
        // `set_current_task` is the only publisher of the pair. So the id check
        // above is also the stub filter.
        let ptr = NonNull::new(pcr::get_current_task().cast::<TaskInner<K, U>>())?;
        Some(Self {
            ptr,
            id,
            _not_send: PhantomData,
        })
    }

    /// This task's registry id, without dereferencing it.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Borrow the task. The returned reference cannot outlive the guard.
    #[inline]
    pub fn task(&self) -> &TaskInner<K, U> {
        // SAFETY: the guard exists only while this CPU's PCR names the task,
        // and a task that is some CPU's current cannot be reaped — the reap
        // gate declines while `task_is_dispatch_pinned`, whose second disjunct
        // is exactly that condition.
        unsafe { self.ptr.as_ref() }
    }

    /// Mint an owning handle for a caller that must outlive the borrow.
    ///
    /// Storing the current task anywhere requires this explicit clone: the
    /// guard itself is never an owned handle.
    #[inline]
    pub fn to_owned(&self) -> KArc<TaskInner<K, U>> {
        task_placement_clone(self.ptr)
    }

    /// The underlying pointer.
    ///
    /// Transitional: it exists so call sites can migrate to the guard before
    /// the surfaces they feed have been retyped, and goes away with the last of
    /// them.
    #[inline]
    pub fn as_ptr(&self) -> *mut TaskInner<K, U> {
        self.ptr.as_ptr()
    }
}

impl<K, U> sealed::Sealed for CurrentTask<K, U> {}

// SAFETY: the guard is minted only from this CPU's PCR, is `!Send`/`!Sync`, and
// names the task this CPU is running — which no other CPU may touch the
// register state of.
unsafe impl<K, U> TaskExclusive<K, U> for CurrentTask<K, U> {
    #[inline]
    fn witnessed(&self) -> *const TaskInner<K, U> {
        self.ptr.as_ptr()
    }
}

// ---------------------------------------------------------------------------
// SwitchWindow — exclusivity over both endpoints of a context switch
// ---------------------------------------------------------------------------

/// Exclusive access to the task being switched *away from*, held by the CPU
/// performing the switch.
///
/// [`CurrentTask`] does not cover it: the dispatcher publishes the incoming
/// task into the PCR *before* the outgoing task's registers are saved, so by
/// the time the FPU area and the round-trip slots are written, the outgoing
/// task is no longer this CPU's current. It is still exclusively this CPU's —
/// the dispatch reference is held across the whole window and no other CPU may
/// dispatch a task while its `on_cpu` is set — which is what this witness
/// names.
pub struct SwitchWindow<'a, K, U> {
    task: &'a TaskInner<K, U>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, K, U> SwitchWindow<'a, K, U> {
    /// Open a switch window over `task`.
    ///
    /// # Safety
    ///
    /// The caller must be the CPU performing the switch, must hold `task`'s
    /// dispatch reference for the whole lifetime of the returned witness, and
    /// must run with interrupts disabled so the window cannot be re-entered.
    #[inline]
    pub unsafe fn new(task: &'a TaskInner<K, U>) -> Self {
        Self {
            task,
            _not_send: PhantomData,
        }
    }

    /// The task this window covers.
    #[inline]
    pub fn task(&self) -> &'a TaskInner<K, U> {
        self.task
    }
}

impl<K, U> sealed::Sealed for SwitchWindow<'_, K, U> {}

// SAFETY: constructed only through the unsafe `new`, whose contract is that the
// caller owns the switch and the task's dispatch reference; `!Send`/`!Sync`.
unsafe impl<K, U> TaskExclusive<K, U> for SwitchWindow<'_, K, U> {
    #[inline]
    fn witnessed(&self) -> *const TaskInner<K, U> {
        core::ptr::from_ref(self.task)
    }
}
