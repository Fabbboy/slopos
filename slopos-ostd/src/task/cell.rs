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
    /// The second reason is the load-bearing one, and it is not a local
    /// judgement call: memory inside an `UnsafeCell` carries `SharedReadWrite`
    /// provenance, which composes with itself, whereas forming a `&mut` pushes
    /// a `Unique` that pops its sibling. Rust-for-Linux reached the identical
    /// fork with `Opaque<T>` and chose the same signature, `get(&self) ->
    /// *mut T`, listing "no uniqueness for mutable references: it is fine to
    /// have multiple `&mut Opaque<T>` point to the same value" as a design
    /// property rather than a caveat. The `task::cell` unit tests hold both
    /// aliasing models — Stacked and Tree Borrows — to that claim.
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

    /// Unsynchronised **write** pointer for the paths that hold a task as a raw
    /// pointer and have no witness to offer.
    ///
    /// One remains: the switch path's `prepare_switch_to` copies the outgoing
    /// task's kernel return context with interrupts off and both tasks
    /// dispatch-pinned. No witness is obtainable there — [`CurrentTask`] names
    /// the incoming task by then, the [`SwitchWindow`]s cover the endpoints
    /// rather than the round-trip slots, and `KArc::get_mut` fails against a
    /// registered task's existence reference.
    ///
    /// So this is a named debt rather than a disguised one — greppable, and
    /// deleted as each of those paths gains a witness it can carry, at which
    /// point its caller moves to [`get_ptr`](Self::get_ptr) or
    /// [`get_mut`](Self::get_mut).
    ///
    /// # Correctness
    ///
    /// The caller must have exclusive access to the task by an argument the
    /// signature cannot carry, and must not retain the pointer past it.
    #[inline]
    pub fn as_ptr_nascent(&self) -> *mut T {
        self.value.get()
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

// Layout razors for the register-state payloads.
//
// `#[repr(transparent)]` over `UnsafeCell<T>` (itself `repr(transparent)`) is
// what makes wrapping a `Task` field free, and `sched/src/task_struct.rs`'s
// `offset_of!(Task, fpu_state) - offset_of!(Task, context)` razor silently
// depends on it. That razor does NOT catch a regression here: if someone wrote
// `#[repr(C)]` on the cell, both offsets would move together and the delta
// would still land in range, while `FpuState` quietly lost its 64-byte
// alignment and `XSAVE64` started faulting with a #GP at the first context
// switch. So assert the properties the hardware actually needs, here, beside
// the definition that could break them.
const _: () = {
    use crate::task::fpu::FpuState;
    use crate::task::task::TaskContext;
    use crate::user::context::UserContext;

    assert!(
        core::mem::size_of::<TaskOwnCell<TaskContext>>() == core::mem::size_of::<TaskContext>()
    );
    assert!(
        core::mem::align_of::<TaskOwnCell<TaskContext>>() == core::mem::align_of::<TaskContext>()
    );

    assert!(core::mem::size_of::<TaskOwnCell<FpuState>>() == core::mem::size_of::<FpuState>());
    assert!(core::mem::align_of::<TaskOwnCell<FpuState>>() == core::mem::align_of::<FpuState>());
    // The one the hardware enforces: XSAVE64/XRSTOR64 require a 64-byte-aligned
    // save area, and the cell must not erode it.
    assert!(core::mem::align_of::<TaskOwnCell<FpuState>>() >= 64);

    assert!(
        core::mem::size_of::<TaskOwnCell<UserContext>>() == core::mem::size_of::<UserContext>()
    );
    assert!(
        core::mem::align_of::<TaskOwnCell<UserContext>>() == core::mem::align_of::<UserContext>()
    );
};

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// These live inside the module rather than in `tests/task_cells.rs` because
// [`TaskOwnCell::get_ptr`] is `pub(crate)` and must stay that way: handing a
// `*mut T` to a `#![forbid(unsafe_code)]` crate is exactly the surface the
// witness exists to remove. Testing through a `test-helpers` shim would test a
// *different function* — the regression guarded against here is a future edit
// changing `get_ptr`'s return type, and a shim can keep its own signature while
// the production one changes. So the test calls the production signature.
//
// `just check-miri` runs with `-Zmiri-ignore-leaks`, which suppresses leak
// reports only. Every property below is a borrow-model or value property, so
// the flag hides nothing here.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::kernel_task::TaskInner;

    type HostTask = TaskInner<(), ()>;

    fn fresh() -> KArc<HostTask> {
        KArc::try_new(HostTask::invalid()).expect("task allocation")
    }

    /// Open a switch window over `task`.
    ///
    /// # Safety
    /// Single-threaded host test: this "CPU" owns the switch, holds the only
    /// reference to `task`, and the window cannot be re-entered.
    fn window(task: &HostTask) -> SwitchWindow<'_, (), ()> {
        unsafe { SwitchWindow::new(task) }
    }

    /// Off-PCR there is no current task, which is *why* [`SwitchWindow`] is the
    /// only witness a host test can mint — `pcr::current_task_id` short-circuits
    /// on `GS_BASE_SET` and reports `INVALID_TASK_ID`.
    #[test]
    fn current_task_is_none_without_a_pcr() {
        assert!(CurrentTask::<(), ()>::get().is_none());
    }

    /// THE test this module exists for.
    ///
    /// Two witnesses for one task may legitimately coexist — an interrupt
    /// handler above a syscall, both on the same task — and both may hold a
    /// live pointer into the same cell at once.
    ///
    /// The write ordering is load-bearing: derive `a`, derive `b`, then write
    /// through `a` *again*. `UnsafeCell::get()` yields a `SharedReadWrite`
    /// derivation, so interleaving is defined. If `get_ptr` ever returns
    /// `&mut T`, deriving `b` pops `a` off the borrow stack and that third
    /// write becomes instant Miri UB — which is the whole point: the hazard
    /// turns into a hard test failure instead of sitting latent.
    ///
    /// That claim was checked, not assumed. Temporarily giving the accessor the
    /// `&mut T` shape and re-running this body under Miri fails with
    /// "<tag> was created by a Unique retag … later invalidated … by a Unique
    /// retag", pointing at exactly the third write. Re-do that probe if you
    /// ever doubt this test still bites.
    ///
    /// Run under **both** aliasing models — plain `cargo miri test` and
    /// `MIRIFLAGS=-Zmiri-tree-borrows` — because the two differ precisely on
    /// raw-pointer retagging, which is the thing under test.
    #[test]
    fn two_witnesses_for_one_task_may_write_the_same_cell() {
        let task = fresh();

        let outer = window(&task);
        let inner = window(&task);

        let a = task.cwd.get_ptr(&outer).cast::<u8>();
        let b = task.cwd.get_ptr(&inner).cast::<u8>();
        assert_eq!(a, b, "both witnesses address the same storage");

        // SAFETY: both pointers address the task's 256-byte `cwd` array, which
        // outlives them, and both are `SharedReadWrite` derivations of the same
        // `UnsafeCell`, so interleaved writes are defined.
        unsafe {
            a.write(b'/');
            b.add(1).write(b'a');
            // Written after `b` was derived: the access that would be UB if
            // `get_ptr` handed out `&mut T`.
            a.add(2).write(b'b');
            assert_eq!(a.read(), b'/');
            assert_eq!(b.add(1).read(), b'a');
            assert_eq!(b.add(2).read(), b'b');
        }
    }

    /// A witness authorises exactly one task. Writing another's state through
    /// it is what the owner check in `set_cwd`/`with_cwd` refuses.
    #[test]
    fn a_witness_names_exactly_one_task() {
        let first = fresh();
        let second = fresh();
        let w = window(&first);

        assert_eq!(w.witnessed(), core::ptr::from_ref(&*first));
        assert_ne!(
            w.witnessed(),
            core::ptr::from_ref(&*second),
            "a witness for one task must not name another"
        );
    }

    /// The witnessed path and the `&mut self` path address the same storage —
    /// so pre-publication construction through `get_mut` and post-publication
    /// writes through a witness cannot disagree about where a field lives.
    #[test]
    fn get_mut_and_get_ptr_address_the_same_storage() {
        let mut task = fresh();
        {
            let unique = KArc::get_mut(&mut task).expect("sole strong reference");
            unique.cwd.get_mut()[0] = b'/';
        }
        let w = window(&task);
        let via_witness = task.cwd.get_ptr(&w).cast::<u8>();
        // SAFETY: addresses the task's own `cwd` array, which outlives the read.
        assert_eq!(unsafe { via_witness.read() }, b'/');
    }

    /// `as_ptr_racy` is the diagnostics read: same address, no witness, and
    /// deliberately no `&T` — a torn read is acceptable, an aliasing violation
    /// is not.
    #[test]
    fn racy_read_addresses_the_same_storage() {
        let task = fresh();
        let w = window(&task);
        assert_eq!(
            task.cwd.as_ptr_racy().cast::<u8>(),
            task.cwd.get_ptr(&w).cast::<u8>().cast_const()
        );
    }
}
