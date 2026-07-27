//! Task identity as an address: comparable, never dereferenceable.
//!
//! Some scheduler questions are about *which* task, not about its state — "is
//! the task I am about to publish already running on that CPU?", "is this
//! pointer one the registry issued?", "is CPU 3 on its idle task?". Answering
//! them used to mean handing out a `*mut Task` naming a task that another CPU
//! owns, and nothing in the type stopped the next edit from reading through
//! it. Such a read races the owning CPU's switch tail, which reclaims and
//! releases the outgoing dispatch reference and can run the allocator-heavy
//! destructor — so the value read may come from freed memory.
//!
//! [`TaskAddr`] is the same answer with the hazard removed rather than
//! commented. It carries an address and supports exactly two operations, `==`
//! and `Debug`. There is no `as_ptr`, no `Deref`, no `upgrade`, and nothing
//! anywhere converts one back into a pointer, so a foreign-task dereference is
//! not a rule reviewers enforce — it is a program that does not compile.
//!
//! # What it does *not* promise
//!
//! An address is not a lifetime. A `TaskAddr` may name a task that has since
//! been reaped and freed, and — because ids are never reused but *addresses*
//! are — a later task can be allocated at the same address. Every consumer is
//! therefore an identity comparison whose two sides are sampled close together
//! and whose wrong answer is tolerable. Use a task id when the answer must
//! survive the task, and a `KArc<TaskInner>` when the task must survive the
//! answer.
//!
//! # Foreign CPUs are a snapshot
//!
//! [`TaskAddr::current_of`] reads another CPU's slot with a single atomic load
//! and reports what it found. That CPU may switch immediately afterwards, so
//! the value is a snapshot, exactly as the raw-pointer form it replaces was.
//! Deliberately, this type applies **no bootstrap-stub filter**: the publisher
//! retires the id to `INVALID_TASK_ID` *before* it stores the new pointer
//! (`set_current_task`), so an id-keyed filter would report "no task" during
//! the window in which the slot still names the real outgoing task. Callers
//! that must exclude the pre-heap stubs keep doing so by address, where a
//! false negative is not the difference between reaping a live task and not.

use core::fmt;
use core::num::NonZeroUsize;

use crate::cpu::x86_64::pcr;
use crate::task::kernel_task::TaskInner;

/// The identity of a task, as an address. Comparable, never dereferenceable.
///
/// See the [module docs](self) for what this does and does not promise.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskAddr(NonZeroUsize);

impl TaskAddr {
    /// The identity of a task you already hold a borrow of.
    ///
    /// The only minting path that involves a task body, and the reason it is
    /// sound is that the caller's borrow already proves the task is there.
    #[inline]
    pub fn of<K, U>(task: &TaskInner<K, U>) -> Self {
        let addr = core::ptr::from_ref(task) as usize;
        // A reference is never null.
        Self(NonZeroUsize::new(addr).expect("a task borrow is never null"))
    }

    /// The identity of the task this CPU is running, or `None` when the slot
    /// is empty or GS_BASE is not yet installed.
    #[inline]
    pub fn current() -> Option<Self> {
        Self::from_raw(pcr::get_current_task())
    }

    /// The identity of the task `cpu_id` is running, or `None` when that CPU
    /// has no PCR or has not dispatched yet.
    ///
    /// A snapshot: see the [module docs](self).
    #[inline]
    pub fn current_of(cpu_id: usize) -> Option<Self> {
        Self::from_raw(pcr::get_current_task_for(cpu_id))
    }

    /// The identity of `cpu_id`'s idle task, or `None` before it is installed.
    #[inline]
    pub fn idle_of(cpu_id: usize) -> Option<Self> {
        Self::from_raw(pcr::get_idle_task(cpu_id))
    }

    /// Wrap a type-erased PCR slot value. Private: the whole point of the type
    /// is that a raw pointer is not a way *in* from outside this module, and an
    /// address is never a way out.
    #[inline]
    fn from_raw(raw: *mut ()) -> Option<Self> {
        NonZeroUsize::new(raw as usize).map(Self)
    }
}

impl fmt::Debug for TaskAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TaskAddr({:#x})", self.0.get())
    }
}
