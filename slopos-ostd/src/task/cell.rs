//! Exclusive access to a task's register state, witnessed by a type.
//!
//! A published task is reachable only through `KArc<Task>`, which yields
//! `&TaskInner`, so the fields the kernel must still write after publication —
//! the saved register context, the FPU area, the user-mode round-trip slots —
//! live in [`TaskOwnCell`] and need a value proving exclusive access to write.
//!
//! A witness rather than a lock because the context-switch path runs with
//! interrupts off and must not acquire anything; exclusivity there is already a
//! fact about the CPU, and the witness makes it checkable. [`CurrentTask`] (this
//! CPU runs the task) and [`SwitchWindow`] (this CPU owns both switch endpoints'
//! dispatch references) are the only implementors — sealed, `!Send`, `!Sync`.
//!
//! [`IdleTask`] also lives here and is deliberately *not* one of them: it
//! borrows this CPU's idle task, which is usually not the task this CPU is
//! running, so it proves liveness and identity but authorises no write.
//!
//! A registered-but-unpublished task is not exclusive either — it is still
//! reachable through every registry lookup, the active-task walk, the cr3 scan,
//! the job-control handles and the diagnostic dump — so exclusive access before
//! publication comes from `KArc::get_mut` on the sole strong reference.

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

// No `unsafe impl Sync`: `TaskInner` is already neither `Send` nor `Sync`, and
// every cross-CPU hand-off of a task launders through `KernelSync` or a raw
// placement pointer.

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
    ///   the accesses are disjoint. `UnsafeCell` memory carries
    ///   `SharedReadWrite` provenance, which composes with itself, whereas
    ///   forming a `&mut` pushes a `Unique` that pops its sibling.
    ///
    /// Forming the raw pointer carries no caller obligation; every obligation
    /// belongs to the *dereference*, which carries its own `SAFETY:` note. The
    /// returned pointer is valid for as long as `self` is.
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
    /// a log line or a stack-walk seed.
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

// `#[repr(transparent)]` is what makes wrapping a `Task` field free, and
// `sched/src/task_struct.rs`'s offset-delta razor silently depends on it — but
// that razor does not catch a `#[repr(C)]` regression here, since both offsets
// would move together. So assert the properties the hardware needs beside the
// definition that could break them.
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
    // XSAVE64/XRSTOR64 require a 64-byte-aligned save area.
    assert!(core::mem::align_of::<TaskOwnCell<FpuState>>() >= 64);

    assert!(
        core::mem::size_of::<TaskOwnCell<UserContext>>() == core::mem::size_of::<UserContext>()
    );
    assert!(
        core::mem::align_of::<TaskOwnCell<UserContext>>() == core::mem::align_of::<UserContext>()
    );
};

/// Borrow of the task running on this CPU. Invariant I5: `current` is a borrow,
/// never an owned handle.
///
/// Takes no reference count: the reap gate declines while
/// `task_is_dispatch_pinned` holds, and that predicate's *current-task* disjunct
/// tests whether the task is any CPU's `PCR.current_task` — exactly the
/// condition under which this guard exists. **Deleting that disjunct deletes
/// this guard's soundness proof.** [`IdleTask`] rests on the gate's idle
/// disjunct the same way.
///
/// `!Send` and `!Sync` is the whole enforcement: a guard cannot travel to a CPU
/// whose PCR names a different task. It deliberately holds no preemption guard —
/// several paths that read the current task go on to block, and a held preempt
/// guard across a deschedule trips the switch assertion — and needs none, since
/// the guard travels with frames that only execute while the task is scheduled.
pub struct CurrentTask<K, U> {
    ptr: NonNull<TaskInner<K, U>>,
    id: u32,
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
    /// `K` and `U` must be the stack-handle types the running kernel
    /// instantiated `TaskInner` with — the PCR slot is type-erased, so naming
    /// different ones would reinterpret the task body. That is what the
    /// `PcrTaskType` bound holds.
    #[inline]
    pub fn get() -> Option<Self>
    where
        TaskInner<K, U>: crate::task::PcrTaskType,
    {
        let id = pcr::current_task_id();
        if id == INVALID_TASK_ID {
            return None;
        }
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

    /// This task's compare-only address, for identity tests that must not
    /// dereference either side.
    #[inline]
    pub fn addr(&self) -> crate::task::TaskAddr {
        crate::task::TaskAddr::of(self.task())
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

    /// Mint an owning handle: the guard itself is never one, so storing the
    /// current task anywhere requires this explicit clone.
    #[inline]
    pub fn to_owned(&self) -> KArc<TaskInner<K, U>> {
        task_placement_clone(self.ptr)
    }

    /// The underlying pointer. Transitional: it exists so call sites can adopt
    /// the guard before the surfaces they feed have been retyped.
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

/// Borrow of this CPU's idle task.
///
/// Takes no reference count, for the same reason [`CurrentTask`] does not: the
/// reap gate declines while `task_is_dispatch_pinned` holds, and that
/// predicate's *idle* disjunct tests whether the task is any CPU's PCR idle
/// task — exactly the condition under which this guard exists. **Deleting that
/// disjunct deletes this guard's soundness proof.**
///
/// Local-CPU only by construction: [`current`](Self::current) takes no CPU
/// index, because reading another CPU's task races its switch tail and a
/// `debug_assert!` on an index would evaporate in release exactly where the
/// hazard is real. `TaskAddr::idle_of` stays the compare-only answer for foreign
/// CPUs.
///
/// `None` covers the two states in which the slot names nothing: an idle task
/// not yet installed, and the test fixtures that null the slot and restore it.
/// `install_idle_task` is the only other writer, so a non-null slot names a real
/// task and no id sidecar is needed.
///
/// Deliberately not a [`TaskExclusive`]: a CPU's idle task is frequently *not*
/// the task that CPU is running, so this proves identity and liveness, never
/// exclusivity. Its register state is written only inside `run_switch`, under
/// the [`SwitchWindow`] over that endpoint.
pub struct IdleTask<K, U> {
    ptr: NonNull<TaskInner<K, U>>,
    _not_send: PhantomData<*mut ()>,
}

impl<K, U> IdleTask<K, U> {
    /// This CPU's idle task, or `None` when its slot names nothing.
    #[inline]
    pub fn current() -> Option<Self>
    where
        TaskInner<K, U>: crate::task::PcrTaskType,
    {
        let cpu_id = pcr::get_current_cpu();
        let ptr = NonNull::new(pcr::get_idle_task(cpu_id).cast::<TaskInner<K, U>>())?;
        Some(Self {
            ptr,
            _not_send: PhantomData,
        })
    }

    /// [`current`](Self::current) for a caller that already carries its own CPU
    /// index. Prefer `current`; this debug-asserts the index is this CPU's
    /// rather than trusting it.
    #[inline]
    pub fn get(cpu_id: usize) -> Option<Self>
    where
        TaskInner<K, U>: crate::task::PcrTaskType,
    {
        debug_assert_eq!(
            cpu_id,
            pcr::get_current_cpu(),
            "IdleTask is a local-CPU guard"
        );
        Self::current()
    }

    /// Borrow the idle task. The returned reference cannot outlive the guard.
    #[inline]
    pub fn task(&self) -> &TaskInner<K, U> {
        // SAFETY: the guard exists only while this CPU's PCR idle slot names
        // the task, and such a task cannot be reaped — the reap gate declines
        // while `task_is_dispatch_pinned`, whose idle disjunct is exactly that
        // condition.
        unsafe { self.ptr.as_ref() }
    }

    /// This task's compare-only address.
    #[inline]
    pub fn addr(&self) -> crate::task::TaskAddr {
        crate::task::TaskAddr::of(self.task())
    }
}

/// Exclusive access to the task being switched *away from*, held by the CPU
/// performing the switch.
///
/// [`CurrentTask`] does not cover it: the dispatcher publishes the incoming
/// task into the PCR *before* the outgoing task's registers are saved, so by
/// the time the FPU area and the round-trip slots are written, the outgoing
/// task is no longer this CPU's current. It is still exclusively this CPU's —
/// the dispatch reference is held across the whole window and no other CPU may
/// dispatch a task while its `on_cpu` is set.
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

// These live inside the module because [`TaskOwnCell::get_ptr`] is `pub(crate)`
// and must stay that way: the regression guarded against here is a future edit
// changing its return type, and a `test-helpers` shim could keep its own
// signature while the production one changed.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::kernel_task::TaskInner;

    type HostTask = TaskInner<crate::task::HostStack, crate::task::HostStack>;

    fn fresh() -> KArc<HostTask> {
        KArc::try_new(HostTask::invalid()).expect("task allocation")
    }

    /// # Safety
    /// Single-threaded host test: this "CPU" owns the switch, holds the only
    /// reference to `task`, and the window cannot be re-entered.
    fn window(task: &HostTask) -> SwitchWindow<'_, crate::task::HostStack, crate::task::HostStack> {
        unsafe { SwitchWindow::new(task) }
    }

    /// `pcr::current_task_id` short-circuits on `GS_BASE_SET` and reports
    /// `INVALID_TASK_ID`, which is *why* [`SwitchWindow`] is the only witness a
    /// host test can mint.
    #[test]
    fn current_task_is_none_without_a_pcr() {
        assert!(CurrentTask::<crate::task::HostStack, crate::task::HostStack>::get().is_none());
    }

    /// The write ordering is load-bearing: derive `a`, derive `b`, then write
    /// through `a` *again*. `UnsafeCell::get()` yields a `SharedReadWrite`
    /// derivation, so interleaving is defined. If `get_ptr` ever returns
    /// `&mut T`, deriving `b` pops `a` off the borrow stack and that third
    /// write becomes instant Miri UB.
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
            a.add(2).write(b'b');
            assert_eq!(a.read(), b'/');
            assert_eq!(b.add(1).read(), b'a');
            assert_eq!(b.add(2).read(), b'b');
        }
    }

    /// The owner check in `set_cwd`/`with_cwd` refuses a witness that names
    /// another task.
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

    /// Pre-publication construction through `get_mut` and post-publication
    /// writes through a witness must not disagree about where a field lives.
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
