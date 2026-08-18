//! Task identity as an address: comparable, never dereferenceable.
//!
//! Reading through a pointer to a task another CPU owns races that CPU's
//! switch tail, which releases the outgoing dispatch reference and may free
//! it. [`TaskAddr`] supports exactly `==` and `Debug`; nothing converts one
//! back into a pointer.
//!
//! An address is not a lifetime: ids are never reused but addresses are, so
//! every consumer must be an identity comparison whose two sides are sampled
//! close together and whose wrong answer is tolerable. Use a task id when the
//! answer must survive the task, a `KArc<TaskInner>` when the task must
//! survive the answer.
//!
//! [`TaskAddr::current_of`] is one atomic load, so a foreign CPU's value is a
//! snapshot. It deliberately applies **no bootstrap-stub filter**: the
//! publisher retires the id to `INVALID_TASK_ID` *before* storing the new
//! pointer, so an id-keyed filter would report "no task" during the window in
//! which the slot still names the real outgoing task.

use core::fmt;
use core::num::NonZeroUsize;

use crate::cpu::x86_64::pcr;
use crate::task::kernel_task::TaskInner;

/// The identity of a task, as an address. See the [module docs](self) for
/// what this does and does not promise.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskAddr(NonZeroUsize);

impl TaskAddr {
    /// The identity of a task you already hold a borrow of; the borrow is
    /// what proves the task is there.
    #[inline]
    pub fn of<K, U>(task: &TaskInner<K, U>) -> Self {
        let addr = core::ptr::from_ref(task) as usize;
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

    /// Wrap a type-erased PCR slot value. Private: a raw pointer must not be a
    /// way *in* from outside this module.
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
