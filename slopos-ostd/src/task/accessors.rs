//! Safe accessors over `*mut Task` / `*const Task` raw pointers.
//!
//! The kernel's exception/IRQ glue receives task pointers from the
//! scheduler/dispatcher without carrying a reference's lifetime
//! information. These helpers absorb the unsafe deref into a single
//! crate so call-site files (`boot/`, `mm/`) stay in safe Rust.
//!
//! Each accessor null-checks the pointer; `Task` field reads use
//! [`core::ptr::read_unaligned`] where the field lives inside a struct
//! that carries no `repr(C, packed)` annotation yet may be read while
//! another CPU is mid-update, so an aligned read could tear on x86_64.
//! Plain field reads (`(*p).field`) are used where the field is a
//! naturally-aligned scalar.
//!
//! All helpers return `Option<T>`; the caller threads the `None` case
//! through their existing diagnostics.

use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use crate::sync::{BUS, LinkError};
use crate::user::context::UserContext;
use slopos_abi::event::{KernelEvent, TaskSlot};
use slopos_abi::signal::{NSIG, SIG_DFL, SIG_IGN, SigSet, sig_bit};
use slopos_abi::syscall::TtyIndex;
use slopos_abi::task::{TaskExitReason, TaskFaultReason, TaskPriority, TaskStatus};

use crate::KArc;
use crate::task::job_control::{ProcessGroup, Session};
use crate::task::kernel_task::{SchedPlacement, TaskInner};
use crate::task::link_roles::SiblingRole;

pub const TASK_EXIT_CLEANUP_RESOURCES: u8 = 1 << 0;
pub const TASK_EXIT_CLEANUP_VM: u8 = 1 << 1;
/// The task's `num_tasks` accounting decrement has been applied. Exit cleanup
/// may run from both an external `task_terminate` and the owning CPU's
/// post-switch path; this bit keeps the decrement exactly-once.
pub const TASK_EXIT_CLEANUP_ACCOUNTED: u8 = 1 << 2;

/// The child-exit event for a task id. Parents blocked in `waitpid`-style
/// waits park on this; the task's exit path publishes it. Public so the
/// `slopos-pidfd` crate can subscribe a pidfd poller to the same event.
#[inline]
pub fn child_exit_event(task_id: u32) -> KernelEvent {
    KernelEvent::ChildExit {
        task: TaskSlot(task_id),
    }
}

/// The signal-pending event for a task id. A `signalfd` poller subscribes
/// here (via the fd's `poll_wait`); every signal-raise site publishes it so a
/// raised signal becomes an in-band ring/poll wakeup instead of relying on the
/// out-of-band interrupt path.
#[inline]
pub fn signal_pending_event(task_id: u32) -> KernelEvent {
    KernelEvent::SignalPending {
        task: TaskSlot(task_id),
    }
}

// ===========================================================================
// Generated accessors.
//
// Every one of these is a thin shim: null-check via `task_borrow` /
// `task_borrow_mut`, then a plain field access or a `&self` method call on the
// resulting reference. Concentrating the `unsafe` deref in exactly those two
// functions is what keeps the whole layer down to two audited derefs instead of
// one per accessor.
//
// A shim exists only to serve call sites that hold a raw pointer rather than a
// `&Task`; each disappears with its last caller.
// ===========================================================================

/// Generate `Option<T>` getters that null-check then read one field.
/// `None` on a null pointer.
macro_rules! task_scalar_getters {
    ($( $(#[$meta:meta])* $name:ident -> $ty:ty = $field:ident ),+ $(,)?) => {$(
        $(#[$meta])*
        #[inline]
        pub fn $name<K, U>(task: *const TaskInner<K, U>) -> Option<$ty> {
            Some(task_borrow(task)?.$field)
        }
    )+};
}

/// Generate setters that null-check then write one field. No-op on a null
/// pointer.
///
/// Writes through the raw pointer rather than through a `&mut TaskInner`,
/// deliberately. A `&mut` to the whole task would retag the entire allocation
/// and invalidate every outstanding reference to *any* other field — so a
/// setter called while some caller holds an unrelated borrow would be aliasing
/// UB, even though the two touch disjoint fields. A raw field write forms no
/// reference and cannot invalidate anything. The getters are free to take a
/// shared reference because shared borrows coexist.
macro_rules! task_scalar_setters {
    ($( $(#[$meta:meta])* $name:ident = $field:ident : $ty:ty ),+ $(,)?) => {$(
        $(#[$meta])*
        #[inline]
        pub fn $name<K, U>(task: *mut TaskInner<K, U>, value: $ty) {
            if task.is_null() {
                return;
            }
            // SAFETY: non-null checked above; the caller holds a live task.
            // `addr_of_mut!` keeps this a raw place write — no reference to the
            // task is formed, so no outstanding borrow is invalidated.
            unsafe { core::ptr::addr_of_mut!((*task).$field).write(value) };
        }
    )+};
}

/// Generate `Option<T>` getters that null-check then call a `&self` method
/// (typically an internally-atomic load). `None` on a null pointer.
macro_rules! task_method_getters {
    ($( $(#[$meta:meta])* $name:ident -> $ty:ty = $method:ident ),+ $(,)?) => {$(
        $(#[$meta])*
        #[inline]
        pub fn $name<K, U>(task: *const TaskInner<K, U>) -> Option<$ty> {
            Some(task_borrow(task)?.$method())
        }
    )+};
}

/// Generate `Option<u64>` getters that read a `TaskContext` register field
/// with `read_unaligned` (the saved context is `repr(C, packed)`).
macro_rules! task_context_getters {
    ($( $(#[$meta:meta])* $name:ident = $field:ident ),+ $(,)?) => {$(
        $(#[$meta])*
        #[inline]
        pub fn $name<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
            let ctx = task_borrow(task)?.context.as_ptr_racy();
            // SAFETY: `as_ptr_racy` addresses the in-Task context, which
            // outlives this read. A concurrent write by the owning CPU may tear
            // the value; every consumer is a log line or a stack-walk seed, and
            // no reference is formed.
            Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).$field)) })
        }
    )+};
}

/// Generate `bool` state predicates that null-check (returning `false`) then
/// call a `&self` predicate method on the task.
macro_rules! task_state_predicates {
    ($( $(#[$meta:meta])* $name:ident = $method:ident ),+ $(,)?) => {$(
        $(#[$meta])*
        #[inline]
        pub fn $name<K, U>(task: *const TaskInner<K, U>) -> bool {
            task_borrow(task).is_some_and(|task| task.$method())
        }
    )+};
}

task_scalar_getters! {
    /// Read the task's stable `task_id`.
    task_id_of -> u32 = task_id,
    /// Read the task's `process_id`.
    task_process_id -> u32 = process_id,
    /// Read the task's `flags` bitfield.
    task_flags -> u16 = flags,
    /// Read the task's user-mode `entry_point` virtual address.
    task_entry_point -> u64 = entry_point,
    /// Read `task->cpu_affinity`.
    task_cpu_affinity -> u32 = cpu_affinity,
    /// Read `task->pgid` (process-group id).
    task_pgid -> u32 = pgid,
    /// Read `task->time_slice`.
    task_time_slice -> u64 = time_slice,
    /// Read `task->time_slice_remaining`.
    task_time_slice_remaining -> u64 = time_slice_remaining,
    /// Read `task->priority`. The field is a `Copy` enum stored in a single
    /// naturally-aligned byte slot.
    task_priority -> TaskPriority = priority,
    /// Read `task->sid` (session id).
    task_sid -> u32 = sid,
    /// Read `task->kernel_stack_top` directly (the dispatcher hot path needs
    /// only `top` for TSS RSP0 programming).
    task_kernel_stack_top -> u64 = kernel_stack_top,
    /// Read `task->fs_base`.
    /// Read `task->tgid` (thread-group id).
    task_tgid -> u32 = tgid,
    /// Read `task->parent_task_id`.
    task_parent_task_id -> u32 = parent_task_id,
}

task_scalar_setters! {
    /// Set the task's `parent_task_id` field. The pointer-validity check and
    /// `task_id`-to-pointer lookup live on the kernel-side shim (the only
    /// caller of this writer); this just exposes the store inside OSTD.
    task_set_parent_task_id = parent_task_id: u32,
    /// Stamp `task->cpu_affinity`.
    task_set_cpu_affinity = cpu_affinity: u32,
    /// Stamp `task->time_slice`.
    task_set_time_slice = time_slice: u64,
    /// Stamp `task->time_slice_remaining`.
    task_set_time_slice_remaining = time_slice_remaining: u64,
    /// Stamp `task->kernel_stack_top`. Used by tests that simulate a missing
    /// kernel-stack-top error path.
    task_set_kernel_stack_top = kernel_stack_top: u64,
}

task_method_getters! {
    /// Read `task->fs_base`. See [`TaskInner::fs_base`] for the ordering.
    task_fs_base -> u64 = fs_base,
    /// Read the task's atomic status (`Ready`, `Running`, `Blocked`, …).
    task_status -> TaskStatus = status,
    /// Read the task's current block reason via the `&self` method.
    task_load_block_reason -> slopos_abi::task::BlockReason = load_block_reason,
    /// Read `task->signal_blocked` (`SigSet = u64`).
    task_signal_blocked -> slopos_abi::signal::SigSet = signal_blocked,
}

task_context_getters! {
    /// Read the task's saved CR3 from its `TaskContext`.
    task_context_cr3 = cr3,
    /// Read the task's saved instruction pointer from its `TaskContext`.
    task_context_rip = rip,
    /// Read the task's saved stack pointer from its `TaskContext`.
    task_context_rsp = rsp,
    /// Read the task's saved user code selector from its `TaskContext`.
    task_context_cs = cs,
    /// Read the task's saved user stack selector from its `TaskContext`.
    task_context_ss = ss,
    /// Read the task's saved RFLAGS from its `TaskContext`.
    task_context_rflags = rflags,
}

task_state_predicates! {
    /// True iff the task is in the `Ready` state (null pointer → `false`).
    task_is_ready = is_ready,
    /// True iff the task is in the `Running` state (null pointer → `false`).
    task_is_running = is_running,
    /// True iff the task is in the `Blocked` state (null pointer → `false`).
    task_is_blocked = is_blocked,
    /// True iff the task is in the `Terminated` state (null pointer → `false`).
    task_is_terminated = is_terminated,
    /// `Zombie` or `Terminated` — the task has exited and is no longer
    /// schedulable. Use this anywhere a "task has stopped running" check is
    /// needed; reserve `task_is_terminated` for the strict, reapable variant.
    task_is_exited = is_exited,
}

/// True iff the task pointer is null **or** the task is in the `Invalid`
/// state (an uninitialized task). Preserves the historical
/// `task_get_state(...) == Invalid` semantics where a null pointer collapsed
/// to `Invalid`.
#[inline]
pub fn task_is_invalid<K, U>(task: *const TaskInner<K, U>) -> bool {
    task_status(task).map_or(true, |s| s == TaskStatus::Invalid)
}

/// Borrow the raw task-name byte array. The bytes are 0-padded; the
/// caller usually slices on the first NUL with `iter().position(|&b|
/// b == 0)`.
#[inline]
pub fn task_name_bytes<'a, K, U>(task: *const TaskInner<K, U>) -> Option<&'a [u8]> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `name` is a fixed-length byte
    // array embedded in `Task`. The returned slice's lifetime is
    // bounded by the caller's frame.
    Some(unsafe { &(*task).name })
}

/// Stamp `task->unsafe_stack_sp` with `sp`. Used by the safestack
/// bootstrap-stub seeding path during pre-SMP init, where the writer is
/// the only observer of the field. No-op on a null pointer.
#[inline]
pub fn task_set_unsafe_stack_sp<K, U>(task: *mut TaskInner<K, U>, sp: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated `task`; field is a naturally-aligned
    // u64 inside the Task struct. Pre-SMP single-writer access precludes
    // races on this field.
    unsafe {
        (*task).abi.unsafe_stack_sp = sp;
    }
}

/// Drive `task->set_status(...)`. The atomic-state setter lives on
/// `Task` itself; this helper centralises the unsafe deref.
#[inline]
pub fn task_set_status<K, U>(task: *mut TaskInner<K, U>, status: TaskStatus) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `set_status` is `&self` so the
    // field write is internally atomic.
    unsafe {
        (*task).set_status(status);
    }
}

/// Raw pointer to a running task's user-mode register snapshot.
///
/// The user-mode round-trip loop (`core::syscall::user_loop`) holds the task as
/// a raw pointer and hands this straight to `UserMode`, which keeps it across
/// an iretq/syscall round trip. Restructuring that loop to carry a
/// `CurrentTask` witness is C12's job, not this step's; until then this is the
/// bridge, and unlike the `task_user_ctx_mut` it replaces it fabricates no
/// lifetime — the caller gets a pointer and must justify its own borrow.
#[inline]
pub fn task_user_ctx_ptr<K, U>(task: *mut TaskInner<K, U>) -> *mut UserContext {
    if task.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller pre-validated; the cell is an in-Task field whose address
    // is stable for the task's lifetime.
    unsafe { (*task).user_ctx.as_ptr_nascent() }
}

/// Reborrow `*const Task` as `&Task`. Used by callers that need a
/// few field reads but not the named getter helpers above.
#[inline]
pub fn task_borrow<'a, K, U>(task: *const TaskInner<K, U>) -> Option<&'a TaskInner<K, U>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the borrow's lifetime is bounded
    // by the caller's frame.
    Some(unsafe { &*task })
}

/// Reborrow `*mut Task` as `&mut Task`. Mirrors [`task_borrow`].
#[inline]
pub fn task_borrow_mut<'a, K, U>(task: *mut TaskInner<K, U>) -> Option<&'a mut TaskInner<K, U>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the borrow's lifetime is bounded
    // by the caller's frame.
    Some(unsafe { &mut *task })
}

// ---------------------------------------------------------------------------
// Owner-list mechanism.
//
// A parent's `children` list and each task's `sibling_link` are the intrusive
// membership machinery; these accessors are the safe surface over it. They are
// pure mechanism — the strong-reference ownership (park on link, reclaim on
// unlink) and the serialising registry lock are the scheduler crate's policy.
// All list operations are `&self`, so a shared `task_borrow` suffices.
// ---------------------------------------------------------------------------

/// Link `child` into `parent`'s children list. `Err(AlreadyLinked)` if the
/// child is already a member of some owner list; `Err(NotPresent)` on a null
/// parent.
#[inline]
pub fn task_children_push<K, U>(
    parent: *const TaskInner<K, U>,
    child: NonNull<TaskInner<K, U>>,
) -> Result<(), LinkError> {
    match task_borrow(parent) {
        Some(p) => p.children.push_back(child),
        None => Err(LinkError::NotPresent),
    }
}

/// Detach and return one child from the head of `parent`'s children list, or
/// `None` when the list is empty (or the parent pointer is null).
#[inline]
pub fn task_children_pop<K, U>(parent: *const TaskInner<K, U>) -> Option<NonNull<TaskInner<K, U>>> {
    task_borrow(parent)?.children.pop_front()
}

/// The head of `parent`'s children list without detaching it, so a caller can
/// decide a child's fate and unlink it in the same critical section.
#[inline]
pub fn task_children_peek<K, U>(
    parent: *const TaskInner<K, U>,
) -> Option<NonNull<TaskInner<K, U>>> {
    task_borrow(parent)?.children.peek_front()
}

/// Remove a specific `child` from `parent`'s children list. `Err(NotPresent)`
/// if the child is not in that list (e.g. a concurrent drain already detached
/// it) or the parent pointer is null.
#[inline]
pub fn task_children_remove<K, U>(
    parent: *const TaskInner<K, U>,
    child: NonNull<TaskInner<K, U>>,
) -> Result<(), LinkError> {
    match task_borrow(parent) {
        Some(p) => p.children.remove(child),
        None => Err(LinkError::NotPresent),
    }
}

/// Whether `parent`'s children list is empty (also true for a null pointer).
#[inline]
pub fn task_children_is_empty<K, U>(parent: *const TaskInner<K, U>) -> bool {
    task_borrow(parent).is_none_or(|p| p.children.is_empty())
}

/// Number of children currently linked under `parent`.
#[inline]
pub fn task_children_len<K, U>(parent: *const TaskInner<K, U>) -> usize {
    task_borrow(parent).map_or(0, |p| p.children.len())
}

/// Detach `task` from whichever owner list currently holds it, without the
/// caller having to know which one that is. Returns whether it was linked.
///
/// This is what makes retirement uniform: a task is dropped from its owner list
/// identically whether that list is a parent's `children` or the parentless-task
/// root list. The caller must hold the lock serialising every owner list.
#[inline]
pub fn task_owner_unlink<K, U>(task: NonNull<TaskInner<K, U>>) -> bool {
    crate::sync::dlist_unlink::<TaskInner<K, U>, SiblingRole>(task)
}

/// Whether `task` is currently a member of some owner list. With ownership
/// total, this is equivalent to "is registered", and the two are cross-checked
/// by debug assertions at the registration and retirement sites.
#[inline]
pub fn task_owner_is_linked<K, U>(task: *const TaskInner<K, U>) -> bool {
    task_borrow(task).is_some_and(|t| t.sibling_link.is_linked())
}

/// Stamp `task->fs_base`. Takes a borrow rather than a pointer: every caller
/// already has one, and a raw parameter here would be a task handle with no
/// owner for no gain.
#[inline]
pub fn task_set_fs_base<K, U>(task: &TaskInner<K, U>, value: u64) {
    task.set_fs_base(value);
}

/// Record a user-mode-fault exit on `task`: sets `exit_reason`,
/// `fault_reason`, and `exit_code`, then returns the task's id so the
/// caller can drive `task_terminate(tid)`.
///
/// Returns `None` if `task` is null. Used by the user-fault retire
/// path in `boot/src/user_fault.rs`.
#[inline]
pub fn task_record_user_fault_exit<K, U>(
    task: *mut TaskInner<K, U>,
    reason: TaskFaultReason,
) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated `task`; the per-task spinlock that
    // gates context-switch is not held here, but the user-fault path
    // is single-threaded for the faulting CPU and the task is not
    // dispatchable while we mutate its exit state.
    unsafe {
        (*task)
            .exit_reason
            .store(TaskExitReason::UserFault.as_u16(), Ordering::Release);
        (*task)
            .fault_reason
            .store(reason.as_u16(), Ordering::Release);
        (*task).exit_code.store(1, Ordering::Release);
        Some((*task).task_id)
    }
}

/// Save the task's panic-recovery nesting depth while it is not running.
#[inline]
pub fn task_recovery_depth_store<K, U>(task: *mut TaskInner<K, U>, depth: u32) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `recovery_depth` is an atomic scalar.
    unsafe {
        (*task)
            .recovery_depth
            .store(depth, core::sync::atomic::Ordering::Release);
    }
}

/// Load the task's saved panic-recovery nesting depth.
#[inline]
pub fn task_recovery_depth_load<K, U>(task: *const TaskInner<K, U>) -> u32 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `recovery_depth` is an atomic scalar.
    unsafe {
        (*task)
            .recovery_depth
            .load(core::sync::atomic::Ordering::Acquire)
    }
}

/// Save the task's panic in-flight depth while it is not running.
#[inline]
pub fn task_panic_in_flight_store<K, U>(task: *mut TaskInner<K, U>, depth: u32) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `panic_in_flight` is an atomic scalar.
    unsafe {
        (*task)
            .panic_in_flight
            .store(depth, core::sync::atomic::Ordering::Release);
    }
}

/// Load the task's saved panic in-flight depth.
#[inline]
pub fn task_panic_in_flight_load<K, U>(task: *const TaskInner<K, U>) -> u32 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `panic_in_flight` is an atomic scalar.
    unsafe {
        (*task)
            .panic_in_flight
            .load(core::sync::atomic::Ordering::Acquire)
    }
}

/// Mark exit-cleanup bits and return the bits that were newly set.
#[inline]
pub fn task_exit_cleanup_mark<K, U>(task: *mut TaskInner<K, U>, bits: u8) -> u8 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `exit_cleanup_flags` is an atomic scalar.
    let previous = unsafe {
        (*task)
            .exit_cleanup_flags
            .fetch_or(bits, core::sync::atomic::Ordering::AcqRel)
    };
    bits & !previous
}

/// Read `task->controlling_tty`.
#[inline]
pub fn task_controlling_tty<K, U>(task: *const TaskInner<K, U>) -> Option<TtyIndex> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the read is an atomic load.
    unsafe { (*task).controlling_tty() }
}

/// Stamp `task->controlling_tty`. Used by the session-leader
/// disposition path when a TTY is hung up.
#[inline]
pub fn task_set_controlling_tty<K, U>(task: *const TaskInner<K, U>, tty: Option<TtyIndex>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; the write is an atomic store.
    unsafe {
        (*task).set_controlling_tty(tty);
    }
    true
}

/// Read `task->flags`. (`task_flags` already exists as `u16`; this
/// helper exposes a typed bit-test the scheduler uses.)
#[inline]
pub fn task_has_flag<K, U>(task: *const TaskInner<K, U>, flag: u16) -> bool {
    task_flags(task).is_some_and(|f| (f & flag) != 0)
}

/// Clone this task's strong process-group membership handle, if any.
#[inline]
pub fn task_process_group<K, U>(task: *const TaskInner<K, U>) -> Option<KArc<ProcessGroup>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the field is an `RcuArcSlot`, whose `load`
    // mints the caller's own reference under an RCU read-side section, so a
    // concurrent `setpgid` on this task cannot release the group underneath it.
    unsafe { (*task).process_group.load() }
}

/// Clone this task's session handle, resolved through its process group.
#[inline]
pub fn task_session<K, U>(task: *const TaskInner<K, U>) -> Option<KArc<Session>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; see `task_process_group` — the group handle
    // this borrows the session from is one `load` minted for us.
    unsafe { (*task).process_group.load() }.map(|pg| pg.session().clone())
}

/// Read `task->name[..]` and probe whether the first 5 bytes
/// match the kernel-internal `idle/<digit>` or `idle\0`/`idle_`
/// prefix used to identify per-CPU idle tasks. Encapsulates the
/// hot-path byte-poke from `scheduler.rs::task_name_looks_idle`.
#[inline]
pub fn task_name_looks_idle<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `name` is an in-Task fixed array.
    let name = unsafe { &(*task).name };
    if name[0] != b'i' || name[1] != b'd' || name[2] != b'l' || name[3] != b'e' {
        return false;
    }
    match name[4] {
        0 | b'_' => true,
        b'/' => name[5].is_ascii_digit(),
        _ => false,
    }
}

/// Stamp `task->cpu_affinity` and `task->last_cpu` from a single
/// boot-time idle-task install. Wraps two field writes to keep the
/// caller's site free of `unsafe`.
#[inline]
pub fn task_install_idle_affinity<K, U>(task: *mut TaskInner<K, U>, mask: u32, last_cpu: u8) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; the affinity mask is a scalar write and
    // the last-CPU hint is an atomic store.
    unsafe {
        (*task).cpu_affinity = mask;
        (*task).set_last_cpu(last_cpu);
    }
}

/// Set `task->on_cpu = on`. The dispatcher uses `true` before
/// switching in, then clears to `false` after the outgoing context
/// save completes.
#[inline]
pub fn task_set_on_cpu<K, U>(task: *mut TaskInner<K, U>, on: bool) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `on_cpu` is `AtomicBool`.
    unsafe {
        (*task)
            .on_cpu
            .store(on, core::sync::atomic::Ordering::Release);
    }
}

/// Take ownership of `task->test_reports`, leaving the slot `None`.
///
/// The taker is a foreign task draining a corpse, while the owner lazily
/// installs the ring on its first report — so the two need mutual exclusion,
/// and the `SpinLock` is what makes it structural rather than a convention the
/// caller has to remember.
///
/// The `KBox` leaves with the return value, so it is dropped by the caller
/// *after* the guard is released: freeing a ring under a lock would put an
/// allocator call inside the critical section for no reason.
#[inline]
pub fn task_take_test_reports<K, U>(
    task: *const TaskInner<K, U>,
) -> Option<crate::KBox<crate::task::test_reports::TestReportRing>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the lock makes the take race-free against
    // the owner's lazy install.
    unsafe { (*task).test_reports.lock().take() }
}

/// Read `task->exit_info.is_set()`.
#[inline]
pub fn task_exit_info_is_set<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `is_set` is an atomic-load on the
    // AtomicCell.
    unsafe { (*task).exit_info.is_set() }
}

/// Read `task->signal_pending` as a single `u64`.
#[inline]
pub fn task_signal_pending<K, U>(task: *const TaskInner<K, U>) -> u64 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `AtomicU64::load` is sound from any context.
    unsafe {
        (*task)
            .signal_pending
            .load(core::sync::atomic::Ordering::Acquire)
    }
}

/// Clear `mask` from `task->signal_pending` with `AcqRel`; returns the
/// previous bitmask. Used by `signalfd` `read` to consume (drain) the
/// signals it reports, mirroring how `deliver_pending_signal` clears a bit
/// once it has handled a signal.
#[inline]
pub fn task_signal_pending_clear<K, U>(task: *const TaskInner<K, U>, mask: u64) -> u64 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `signal_pending` is `AtomicU64`.
    unsafe {
        (*task)
            .signal_pending
            .fetch_and(!mask, core::sync::atomic::Ordering::AcqRel)
    }
}

/// Number of tasks blocked waiting for this task to exit.
#[inline]
pub fn task_waiter_count<K, U>(task: *const TaskInner<K, U>) -> usize {
    match task_id_of(task) {
        Some(id) => BUS.waiter_count(child_exit_event(id)),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// Scheduler / driver / signal / stats hot paths. Each absorbs one
// `unsafe { (*task).<field> }` deref so its call sites stay in safe Rust.
// ---------------------------------------------------------------------------

/// Load `task->on_cpu` as `bool` with `Acquire` ordering.
#[inline]
pub fn task_on_cpu_load<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `on_cpu` is `AtomicBool`.
    unsafe { (*task).on_cpu.load(core::sync::atomic::Ordering::Acquire) }
}

/// Borrow `task->exit_info` as `&AtomicCell<ExitInfo>`. The returned
/// borrow's lifetime is bounded by the caller's borrow of `task`.
#[inline]
pub fn task_exit_info_ref<'a, K, U>(
    task: *const TaskInner<K, U>,
) -> Option<&'a crate::sync::AtomicCell<crate::task::exit_info::ExitInfo>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `exit_info` is in-Task.
    Some(unsafe { &(*task).exit_info })
}

/// Read `task->switch_ctx.rbp` via `read_unaligned`. The saved frame
/// pointer of a descheduled task — the anchor for walking its parked
/// call chain in diagnostics (SysRq task dump).
#[inline]
pub fn task_switch_ctx_rbp<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; addr_of! produces a valid pointer
    // into the in-Task crate::task::TaskContext; read_unaligned is safe.
    unsafe {
        let ctx = (*task).switch_ctx.as_ptr_racy();
        Some(core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).rbp)))
    }
}

/// Read `task->switch_ctx.rflags` via `read_unaligned`. Diagnostics and the
/// context test-suite; a torn read is acceptable for both.
#[inline]
pub fn task_switch_ctx_rflags<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `as_ptr_racy` addresses the in-Task
    // context and no reference is formed.
    unsafe {
        let ctx = (*task).switch_ctx.as_ptr_racy();
        Some(core::ptr::read_unaligned(core::ptr::addr_of!(
            (*ctx).rflags
        )))
    }
}

/// Read `task->switch_ctx.(rip, rsp)` as `(u64, u64)` via `read_unaligned`:
/// `SwitchContext` carries no alignment guarantee, and the read may land while
/// the owning CPU is mid-switch.
#[inline]
pub fn task_switch_ctx_rip_rsp<K, U>(task: *const TaskInner<K, U>) -> Option<(u64, u64)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; addr_of! produces a valid pointer
    // into the in-Task crate::task::TaskContext; read_unaligned is safe.
    unsafe {
        let ctx = (*task).switch_ctx.as_ptr_racy();
        Some((
            core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).rip)),
            core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).rsp)),
        ))
    }
}

/// Read the task's scheduler placement owner.
#[inline]
pub fn task_sched_placement_load<K, U>(task: *const TaskInner<K, U>) -> SchedPlacement {
    if task.is_null() {
        return SchedPlacement::None;
    }
    // SAFETY: caller pre-validated; `sched_placement` is an in-task atomic.
    SchedPlacement::from_u8(unsafe {
        (*task)
            .sched_placement
            .load(core::sync::atomic::Ordering::Acquire)
    })
}

/// Atomically transition the scheduler placement owner.
#[inline]
pub fn task_sched_placement_compare_exchange<K, U>(
    task: *const TaskInner<K, U>,
    expected: SchedPlacement,
    target: SchedPlacement,
) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `sched_placement` is an in-task atomic.
    unsafe {
        (*task)
            .sched_placement
            .compare_exchange(
                expected.as_u8(),
                target.as_u8(),
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Force-store the scheduler placement owner. Intended for exclusive reset /
/// recovery paths; normal scheduler publication should use
/// [`task_sched_placement_compare_exchange`].
#[inline]
pub fn task_sched_placement_store<K, U>(task: *const TaskInner<K, U>, placement: SchedPlacement) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `sched_placement` is an in-task atomic.
    unsafe {
        (*task)
            .sched_placement
            .store(placement.as_u8(), core::sync::atomic::Ordering::Release);
    }
}

/// Read the task's remote-inbox successor as `*mut Task` with `Acquire`
/// ordering.
#[inline]
pub fn task_next_inbox_load<K, U>(task: *const TaskInner<K, U>) -> *mut TaskInner<K, U> {
    if task.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.load() }
}

/// Store the task's remote-inbox successor with `Relaxed` ordering. Used by the
/// remote-inbox lock-free push (the head CAS itself supplies the AcqRel
/// barrier).
#[inline]
pub fn task_next_inbox_store_relaxed<K, U>(
    task: *const TaskInner<K, U>,
    next: *mut TaskInner<K, U>,
) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.store_relaxed(next) };
}

/// Store the task's remote-inbox successor with `Release` ordering. Used by the
/// remote-inbox drain when clearing the link before re-queueing.
#[inline]
pub fn task_next_inbox_store_release<K, U>(
    task: *const TaskInner<K, U>,
    next: *mut TaskInner<K, U>,
) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.store(next) };
}

/// Try to claim membership in a remote wake inbox.
///
/// This is the remote-inbox equivalent of `ready_link`'s `linked` bit:
/// a one-element inbox stack has a null successor while still queued, so the
/// successor pointer alone cannot distinguish "not queued" from "queued as
/// tail". The bit lives in the role-typed `Link<Task, RemoteWakeRole>` slot so
/// duplicate remote wakes use the same single-membership primitive as other
/// intrusive scheduler lists.
#[inline]
pub fn task_remote_inbox_try_link<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.try_mark_linked() }
}

/// Clear remote wake inbox membership after the owner CPU has detached the
/// task from its Treiber stack and either queued or dropped it.
#[inline]
pub fn task_remote_inbox_unlink<K, U>(task: *const TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.mark_unlinked() };
}

// ---------------------------------------------------------------------------
// Reclaim-link mechanism (the task graveyard).
//
// Same Treiber-stack shape as the remote-wake inbox, but the invariant is
// inverted: a node here has a strong count of zero and the pusher owns the
// allocation outright, having won the final release. Nothing else may touch a
// parked node, which is why the single-membership claim below can never
// contend.
// ---------------------------------------------------------------------------

/// Read the task's graveyard successor with `Acquire` ordering.
#[inline]
pub fn task_reclaim_next_load<K, U>(task: *const TaskInner<K, U>) -> *mut TaskInner<K, U> {
    if task.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller uniquely owns the parked allocation; `reclaim_link` is an
    // in-task `Link` whose storage outlives the destructor call that follows.
    unsafe { (*task).reclaim_link.load() }
}

/// Store the task's graveyard successor with `Relaxed` ordering. The head CAS
/// supplies the `AcqRel` barrier.
#[inline]
pub fn task_reclaim_next_store_relaxed<K, U>(
    task: *const TaskInner<K, U>,
    next: *mut TaskInner<K, U>,
) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller uniquely owns the parked allocation.
    unsafe { (*task).reclaim_link.store_relaxed(next) };
}

/// Claim graveyard membership. Fails only if the node is already parked, which
/// would mean two threads believed they won the same final release.
#[inline]
pub fn task_reclaim_try_link<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller uniquely owns the parked allocation.
    unsafe { (*task).reclaim_link.try_mark_linked() }
}

/// Release graveyard membership once a drain has detached the node, before its
/// destructor runs (the destructor frees the memory the link lives in).
#[inline]
pub fn task_reclaim_unlink<K, U>(task: *const TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller uniquely owns the parked allocation.
    unsafe { (*task).reclaim_link.mark_unlinked() };
}

/// True iff the task is currently claimed by some CPU's remote wake inbox.
#[inline]
pub fn task_remote_inbox_is_linked<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.is_linked() }
}

/// Wake every task currently blocked waiting for this task to exit.
/// Caller must hold the task pointer stable (e.g. via the task-manager
/// lock or an owning `KArc`) long enough to resolve its id; the event
/// bus queue's internal SpinLock makes the publish interrupt-safe and
/// serialises against any concurrent waiter registration.
#[inline]
pub fn task_wake_all_waiters<K, U>(task: *const TaskInner<K, U>) {
    if let Some(id) = task_id_of(task) {
        BUS.publish(child_exit_event(id));
    }
}

/// Store `value` into `task->signal_pending` with `Release` ordering.
/// Used by tests that reset the pending mask between assertions.
#[inline]
pub fn task_signal_pending_store<K, U>(task: *const TaskInner<K, U>, value: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `signal_pending` is `AtomicU64`.
    unsafe {
        (*task)
            .signal_pending
            .store(value, core::sync::atomic::Ordering::Release)
    }
}

/// Raise `mask` on `task`'s pending set **and** wake any `signalfd` poller
/// registered on it (publishes [`signal_pending_event`]). This is the
/// signal-raise chokepoint for in-band (signalfd / ring) delivery: a masked
/// signal stays pending (so it does not interrupt a wait with EINTR) yet a
/// poller subscribed to its `SignalPending` queue is still woken to drain it.
/// Returns the previous pending bitmask.
pub fn task_signal_raise<K, U>(task: *const TaskInner<K, U>, mask: u64) -> u64 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `signal_pending` is `AtomicU64`.
    let prev = unsafe {
        (*task)
            .signal_pending
            .fetch_or(mask, core::sync::atomic::Ordering::AcqRel)
    };
    if let Some(id) = task_id_of(task) {
        BUS.publish(signal_pending_event(id));
    }
    prev
}

/// Post `signum` to `task`, honouring its disposition at the send site.
///
/// This is the disposition-aware raise chokepoint every signal *send*
/// (kill, process-group, session, parent-notify) routes through. A
/// signal that would be discarded anyway — handler is `SIG_IGN`, or
/// `SIG_DFL` with a default of [`SigDefault::Ignore`] — and is **not
/// blocked** is dropped here instead of being left pending, so it
/// never spuriously wakes a blocked task only to be consumed as a
/// no-op at the delivery point. Blocked signals always pend
/// regardless of disposition: a `signalfd` reader or a later-installed
/// handler may still drain them after unblocking.
///
/// Returns `true` when the signal was made pending (the caller should
/// then wake/unblock the target); `false` when it was dropped or the
/// arguments were invalid.
pub fn task_signal_post<K, U>(task: *const TaskInner<K, U>, signum: u8) -> bool {
    let bit = slopos_abi::signal::sig_bit(signum);
    if task.is_null() || bit == 0 {
        return false;
    }
    if task_signal_pending(task) & bit != 0 {
        return false;
    }
    let blocked = task_signal_blocked(task).unwrap_or(0);
    if (blocked & bit) == 0 {
        let handler = task_signal_handler(task, (signum - 1) as usize);
        let ignored = match handler {
            Some(h) if h == slopos_abi::signal::SIG_IGN => true,
            Some(h) if h == slopos_abi::signal::SIG_DFL => {
                slopos_abi::signal::sig_default_ignores(signum)
            }
            _ => false,
        };
        if ignored {
            return false;
        }
    }
    task_signal_raise(task, bit);
    true
}

/// Drive `task->store_block_reason(reason)` via the existing `&self`
/// method (atomic store internally).
#[inline]
pub fn task_store_block_reason<K, U>(
    task: *const TaskInner<K, U>,
    reason: slopos_abi::task::BlockReason,
) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `store_block_reason` is `&self`.
    unsafe { (*task).store_block_reason(reason) };
}

/// Read `task->last_run_timestamp` via `read_volatile` to defeat
/// compiler reordering across the context-switch boundary.
#[inline]
pub fn task_last_run_timestamp<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the read is an atomic load.
    Some(unsafe { (*task).last_run_timestamp() })
}

/// Stamp `task->last_run_timestamp`.
#[inline]
pub fn task_set_last_run_timestamp<K, U>(task: *const TaskInner<K, U>, timestamp: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; the write is an atomic store.
    unsafe { (*task).set_last_run_timestamp(timestamp) };
}

/// Read `task->last_cpu`, the placement hint for the CPU this task last ran on.
#[inline]
pub fn task_last_cpu<K, U>(task: *const TaskInner<K, U>) -> Option<u8> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the read is an atomic load.
    Some(unsafe { (*task).last_cpu() })
}

/// Stamp `task->last_cpu` (set when the scheduler enqueues the task onto a
/// particular CPU's run queue or remote-wake inbox).
#[inline]
pub fn task_set_last_cpu<K, U>(task: *const TaskInner<K, U>, cpu: u8) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; the write is an atomic store.
    unsafe { (*task).set_last_cpu(cpu) };
}

/// Bump `task->total_runtime` by `delta`, saturating.
#[inline]
pub fn task_add_total_runtime<K, U>(task: *const TaskInner<K, U>, delta: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; the update is an atomic read-modify-write.
    unsafe { (*task).add_total_runtime(delta) };
}

/// Clear `task->controlling_tty` if `(sid, tty)` matches. Returns
/// `true` if a clear occurred. Used by the session-leader TTY-hangup
/// hook.
#[inline]
pub fn task_clear_controlling_tty_for<K, U>(
    task: *const TaskInner<K, U>,
    sid: u32,
    tty: TtyIndex,
) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; the session id is a scalar read and the
    // clear is a compare-and-clear, so a task that has meanwhile moved to a
    // different terminal keeps it.
    unsafe { (*task).sid == sid && (*task).clear_controlling_tty_if(tty) }
}

/// PCR↔Task mirror used by the scheduler context-switch path.
/// Saves the per-CPU PCR's user-mode round-trip slots onto `prev`,
/// then loads `next`'s saved slots back into the PCR. Single source
/// of truth for the "switch the user-mode round-trip" half of
/// `prepare_switch_to`. Operates only on the current CPU's PCR.
///
/// # Preconditions
/// - Caller has interrupts disabled.
/// - `prev` and `next` are Task pointers pinned by their dispatch
///   references (or null for `prev` on bootstrap entry).
#[inline]
pub fn task_pcr_round_trip_swap<K, U>(prev: *mut TaskInner<K, U>, next: *mut TaskInner<K, U>) {
    use core::sync::atomic::Ordering;
    // SAFETY: interrupts disabled by caller; the per-CPU PCR is
    // stable for this CPU during a switch window. Each `(*prev)` /
    // `(*next)` access is to a dispatch-pinned Task whose memory is
    // valid for the duration of `prepare_switch_to`.
    unsafe {
        let pcr = crate::cpu::x86_64::pcr::current_pcr();
        if !prev.is_null() {
            (*prev)
                .saved_user_ctx_ptr
                .store(pcr.user_ctx_ptr.load(Ordering::Acquire), Ordering::Release);
            core::ptr::copy_nonoverlapping(
                pcr.kernel_return_ctx.get(),
                (*prev).saved_kernel_return_ctx.as_ptr_nascent(),
                1,
            );
        }
        if !next.is_null() {
            pcr.user_ctx_ptr.store(
                (*next).saved_user_ctx_ptr.load(Ordering::Acquire),
                Ordering::Release,
            );
            core::ptr::copy_nonoverlapping(
                (*next).saved_kernel_return_ctx.as_ptr_racy(),
                pcr.kernel_return_ctx.get(),
                1,
            );
        }
    }
}

/// Read `task->signal_actions[idx].handler` if `idx` is in range.
#[inline]
pub fn task_signal_handler<K, U>(task: *const TaskInner<K, U>, idx: usize) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `signal_actions` is a fixed-size
    // in-Task array; bounds-check via `len()` keeps the index in range.
    unsafe {
        let actions = &(*task).signal_actions;
        if idx < actions.len() {
            Some(actions[idx].handler())
        } else {
            None
        }
    }
}

/// Reset `task->fpu_state` in place via the OSTD-side
/// `fpu_reset_in_place` routine. Caller holds exclusive `&mut Task`
/// access through `task` borrow / fresh slot.
#[inline]
pub fn task_reset_fpu_state<K, U>(task: &mut TaskInner<K, U>) {
    // SAFETY: `&mut Task` gives exclusive access to the in-Task
    // `fpu_state` field; the OSTD reset routine writes a fresh
    // `FpuState` value into the slot.
    unsafe {
        crate::task::kernel_task::fpu_reset_in_place(task.fpu_state.get_mut());
    }
}

/// Reset every *caught* signal (a handler other than `SIG_DFL`/`SIG_IGN`) to
/// `SIG_DFL`. This is the execve disposition reset: a stale handler pointer
/// must never survive into the new image, but POSIX keeps ignored signals
/// ignored, so `SIG_IGN` (and `SIG_DFL`) entries are left untouched. The
/// blocked mask and pending set are preserved across exec by the caller.
#[inline]
pub fn task_reset_caught_handlers<K, U>(task: &TaskInner<K, U>) {
    for action in task.signal_actions.iter() {
        let handler = action.handler();
        if handler != SIG_DFL && handler != SIG_IGN {
            action.reset();
        }
    }
}

/// Force every signal named in `mask` to `SIG_DFL`, overriding a caught
/// handler or `SIG_IGN`. Backs POSIX_SPAWN_SETSIGDEF (spawn) and the
/// `sigdefault` syscall (a forked child installing job-control defaults).
#[inline]
pub fn task_default_signals_in_mask<K, U>(task: &TaskInner<K, U>, mask: SigSet) {
    for signum in 1..=NSIG {
        if mask & sig_bit(signum as u8) != 0 {
            task.signal_actions[signum - 1].reset();
        }
    }
}

/// Write a kernel-mode trampoline return-address into the slot at
/// `kernel_stack_top - 8`. Used by `init_task_context` to seed the
/// first `ret` of a kernel task's switch frame. Caller must hold
/// exclusive access to the (just-allocated) kernel stack.
#[inline]
pub fn task_kernel_stack_seed_ret(kernel_stack_top: u64, trampoline: u64) {
    // SAFETY: `kernel_stack_top` points at the top of a kernel stack
    // the caller just allocated; the slot at `top - 8` is reserved
    // for the synthetic return address.
    unsafe {
        let ret_addr_ptr = (kernel_stack_top - 8) as *mut u64;
        core::ptr::write(ret_addr_ptr, trampoline);
    }
}

/// Clone `other` into `dest` in place via [`Task::clone_from_raw`].
/// Caller must hold exclusive `&mut Task` access to `dest` (e.g.
/// just-reserved slot) and ensure `other` aliases a different slot.
#[inline]
pub fn task_clone_from<K, U>(dest: &mut TaskInner<K, U>, other: &TaskInner<K, U>) {
    // SAFETY: caller's `&mut Task` is exclusive; `other` is a
    // distinct shared borrow; `clone_from_raw` is a bulk-copy
    // routine that maintains atomics' values.
    unsafe { dest.clone_from_raw(other) };
}

/// Save register state from `frame` into `task->context`, set the
/// segment selectors to USER_DATA, and stamp `context_from_user = 1`.
/// Optionally also stamp `user_started = 1`.
///
/// The field-by-field copy behind
/// `core::scheduler::trap::save_task_context_from_interrupt_frame`.
#[inline]
pub fn task_save_from_interrupt_frame<K, U>(
    task: *mut TaskInner<K, U>,
    frame: *const crate::irq::interrupt_frame::InterruptFrame,
    mark_user_started: bool,
) {
    if task.is_null() || frame.is_null() {
        return;
    }
    // SAFETY: caller pre-validated both pointers; the in-Task
    // `context` field and the caller-owned `InterruptFrame` are
    // exclusive for the duration of this call.
    unsafe {
        use crate::arch::x86_64::gdt::SegmentSelector;
        let ctx = &mut *(*task).context.as_ptr_nascent();
        let f = &*frame;
        ctx.rax = f.rax;
        ctx.rbx = f.rbx;
        ctx.rcx = f.rcx;
        ctx.rdx = f.rdx;
        ctx.rsi = f.rsi;
        ctx.rdi = f.rdi;
        ctx.rbp = f.rbp;
        ctx.r8 = f.r8;
        ctx.r9 = f.r9;
        ctx.r10 = f.r10;
        ctx.r11 = f.r11;
        ctx.r12 = f.r12;
        ctx.r13 = f.r13;
        ctx.r14 = f.r14;
        ctx.r15 = f.r15;
        ctx.rip = f.rip;
        ctx.rsp = f.rsp;
        ctx.rflags = f.rflags;
        ctx.cs = f.cs;
        ctx.ss = f.ss;
        ctx.ds = SegmentSelector::USER_DATA.bits() as u64;
        ctx.es = SegmentSelector::USER_DATA.bits() as u64;
        ctx.fs = 0;
        ctx.gs = 0;

        (*task).context_from_user = 1;
        if mark_user_started {
            (*task).user_started = 1;
        }
    }
}

/// Test whether `task->signal_pending & !task->signal_blocked` is non-zero,
/// i.e. there is at least one deliverable signal.
#[inline]
pub fn task_has_deliverable_signal<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; both fields are in-Task atomics /
    // scalars. The atomic load is internally synchronised.
    unsafe {
        let pending = (*task)
            .signal_pending
            .load(core::sync::atomic::Ordering::Acquire);
        let blocked = (*task).signal_blocked();
        (pending & !blocked) != 0
    }
}
