//! Safe accessors over `*mut Task` / `*const Task` raw pointers.
//!
//! The kernel's exception/IRQ glue receives task pointers from the
//! scheduler/dispatcher without carrying a reference's lifetime
//! information. These helpers absorb the unsafe deref into a single
//! crate so call-site files (`boot/`, `mm/`) stay in safe Rust.
//!
//! Each accessor null-checks the pointer; `Task` field reads use
//! [`core::ptr::read_unaligned`] for fields that the legacy
//! `task_struct` does not annotate as `repr(C, packed)` but where
//! callers historically used unaligned reads to be safe against
//! mid-update tearing on x86_64. Plain field reads (`(*p).field`) are
//! used where the field is a naturally-aligned scalar.
//!
//! All helpers return `Option<T>`; the caller threads the `None` case
//! through their existing diagnostics.

use crate::sync::BUS;
use crate::task::fpu::FpuState;
use crate::user::context::UserContext;
use slopos_abi::event::{KernelEvent, TaskSlot};
use slopos_abi::syscall::TtyIndex;
use slopos_abi::task::{TaskExitReason, TaskFaultReason, TaskPriority, TaskStatus};

use crate::task::kernel_task::{SchedPlacement, TaskInner};

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

/// Read the task's stable `task_id`.
#[inline]
pub fn task_id_of<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).task_id })
}

/// Set the task's `parent_task_id` field. The pointer-validity check
/// and the `task_id`-to-pointer lookup live on the kernel-side shim
/// (which is the only caller of this writer); we just expose the
/// naturally-aligned u32 store here so the unsafe deref lives inside
/// OSTD.
#[inline]
pub fn task_set_parent_task_id<K, U>(task: *mut TaskInner<K, U>, parent_task_id: u32) {
    if task.is_null() {
        return;
    }
    // SAFETY: lock-free write of a naturally-aligned u32; consumers
    // tolerate stale-vs-fresh reads of this field.
    unsafe { (*task).parent_task_id = parent_task_id };
}

/// Read the task's `process_id`.
#[inline]
pub fn task_process_id<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).process_id })
}

/// Read the task's `flags` bitfield.
#[inline]
pub fn task_flags<K, U>(task: *const TaskInner<K, U>) -> Option<u16> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u16.
    Some(unsafe { (*task).flags })
}

/// Read the task's user-mode `entry_point` virtual address.
#[inline]
pub fn task_entry_point<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u64.
    Some(unsafe { (*task).entry_point })
}

/// Read the task's kernel-stack `(base, top)` pair.
#[inline]
pub fn task_kernel_stack_bounds<K, U>(task: *const TaskInner<K, U>) -> Option<(u64, u64)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; both fields are u64.
    let base = unsafe { (*task).kernel_stack_base };
    let top = unsafe { (*task).kernel_stack_top };
    Some((base, top))
}

/// Read the task's saved CR3 from its `TaskContext`. Uses
/// [`core::ptr::read_unaligned`] for parity with existing callers.
#[inline]
pub fn task_context_cr3<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; addr_of! produces a valid pointer
    // to the cr3 field; read_unaligned is safe regardless of the
    // surrounding context's alignment.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.cr3)) })
}

/// Read the task's saved instruction pointer from its `TaskContext`.
#[inline]
pub fn task_context_rip<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; read_unaligned is safe.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.rip)) })
}

/// Read the task's saved stack pointer from its `TaskContext`.
#[inline]
pub fn task_context_rsp<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; read_unaligned is safe.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.rsp)) })
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

/// Stamp `task->entry_point` with `entry`. Used by the exec path
/// when re-targeting an existing task at a freshly-loaded ELF entry.
#[inline]
pub fn task_set_entry_point<K, U>(task: *mut TaskInner<K, U>, entry: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).entry_point = entry;
    }
}

/// Stamp `task->fs_base` with `fs_base` (TLS thread pointer).
#[inline]
pub fn task_set_fs_base<K, U>(task: *mut TaskInner<K, U>, fs_base: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).fs_base = fs_base;
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

/// Stamp `task->context.{rip,rsp}` with the unaligned-write pattern
/// the exec path requires. Used by ELF load to seed the user-mode
/// entry RIP/RSP before activation.
#[inline]
pub fn task_set_context_rip_rsp<K, U>(task: *mut TaskInner<K, U>, rip: u64, rsp: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; `context` is in-Task; both fields
    // are u64. `write_unaligned` lifts the alignment requirement to
    // match the legacy exec path's discipline.
    unsafe {
        core::ptr::write_unaligned(core::ptr::addr_of_mut!((*task).context.rip), rip);
        core::ptr::write_unaligned(core::ptr::addr_of_mut!((*task).context.rsp), rsp);
    }
}

/// Reborrow `task->user_ctx` as `&mut UserContext`. The returned
/// borrow's lifetime is bounded by the caller's borrow of `task`.
#[inline]
pub fn task_user_ctx_mut<'a, K, U>(task: *mut TaskInner<K, U>) -> Option<&'a mut UserContext> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `user_ctx` is an in-Task field
    // whose pin-stability matches the rest of the Task struct.
    Some(unsafe { &mut (*task).user_ctx })
}

/// Read `task->cpu_affinity`.
#[inline]
pub fn task_cpu_affinity<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    Some(unsafe { (*task).cpu_affinity })
}

/// Stamp `task->cpu_affinity`.
#[inline]
pub fn task_set_cpu_affinity<K, U>(task: *mut TaskInner<K, U>, mask: u32) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    unsafe {
        (*task).cpu_affinity = mask;
    }
}

/// Read `task->pgid` (process-group id).
#[inline]
pub fn task_pgid<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    Some(unsafe { (*task).pgid })
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
        (*task).exit_reason = TaskExitReason::UserFault;
        (*task).fault_reason = reason;
        (*task).exit_code = 1;
        Some((*task).task_id)
    }
}

// Scheduler-hot-path accessors. Each absorbs one
// `unsafe { (*task).<field> }` per-field access pattern formerly
// scattered across `core/src/scheduler/{scheduler,per_cpu,task/*}.rs`.

/// Read `task->time_slice`.
#[inline]
pub fn task_time_slice<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).time_slice })
}

/// Stamp `task->time_slice`.
#[inline]
pub fn task_set_time_slice<K, U>(task: *mut TaskInner<K, U>, slice: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).time_slice = slice;
    }
}

/// Read `task->time_slice_remaining`.
#[inline]
pub fn task_time_slice_remaining<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).time_slice_remaining })
}

/// Stamp `task->time_slice_remaining`.
#[inline]
pub fn task_set_time_slice_remaining<K, U>(task: *mut TaskInner<K, U>, remaining: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).time_slice_remaining = remaining;
    }
}

/// Bump `task->refcnt`. Returns the post-increment count, mirroring
/// `Task::inc_ref`. Returns `None` for null pointers.
#[inline]
pub fn task_inc_ref<K, U>(task: *mut TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `inc_ref` takes `&self` and
    // performs the atomic add internally.
    Some(unsafe { (*task).inc_ref() })
}

/// Decrement `task->refcnt`. Returns `Some(true)` if the count
/// dropped to zero (caller is the last reference), mirroring
/// `Task::dec_ref`. Returns `None` for null pointers.
#[inline]
pub fn task_dec_ref<K, U>(task: *mut TaskInner<K, U>) -> Option<bool> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `dec_ref` takes `&self` and
    // performs the atomic sub internally.
    Some(unsafe { (*task).dec_ref() })
}

/// Read `task->refcnt`.
#[inline]
pub fn task_ref_count<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `ref_count` takes `&self` and
    // performs an atomic load internally.
    Some(unsafe { (*task).ref_count() })
}

/// Reborrow `task->fpu_state` as `&mut FpuState`. The returned
/// borrow's lifetime is bounded by the caller's borrow of `task`.
#[inline]
pub fn task_fpu_state_mut<'a, K, U>(task: *mut TaskInner<K, U>) -> Option<&'a mut FpuState> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `fpu_state` is an in-Task field
    // whose pin-stability matches the rest of the Task struct.
    Some(unsafe { &mut (*task).fpu_state })
}

/// Read `task->priority`. The field is a copy-`Copy` enum stored as
/// a `u8` discriminant inside a one-byte slot; reading it through
/// the pointer is naturally aligned and atomic on x86_64.
#[inline]
pub fn task_priority<K, U>(task: *const TaskInner<K, U>) -> Option<TaskPriority> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u8.
    Some(unsafe { (*task).priority })
}

/// Stamp `task->last_cpu`. The field is updated by the scheduler
/// when a task is enqueued onto a particular CPU's run queue or
/// remote-wake inbox.
#[inline]
pub fn task_set_last_cpu<K, U>(task: *mut TaskInner<K, U>, cpu_id: u8) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is a single byte; the
    // owning queue's lock orders this against the dispatcher read.
    unsafe {
        (*task).last_cpu = cpu_id;
    }
}

/// Read the task's atomic status (`Ready`, `Running`, `Blocked`, …).
#[inline]
pub fn task_status<K, U>(task: *const TaskInner<K, U>) -> Option<TaskStatus> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `status` is a `&self` method
    // that performs an atomic load internally.
    Some(unsafe { (*task).status() })
}

/// Read `task->sid` (session id).
#[inline]
pub fn task_sid<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).sid })
}

/// Read `task->controlling_tty`.
#[inline]
pub fn task_controlling_tty<K, U>(task: *const TaskInner<K, U>) -> Option<TtyIndex> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the field is `Option<TtyIndex>`,
    // both variants are 1-byte / 2-byte naturally-aligned scalars.
    unsafe { (*task).controlling_tty }
}

/// Stamp `task->controlling_tty`. Used by the session-leader
/// disposition path when a TTY is hung up.
#[inline]
pub fn task_set_controlling_tty<K, U>(task: *mut TaskInner<K, U>, tty: Option<TtyIndex>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; field write is naturally aligned.
    unsafe {
        (*task).controlling_tty = tty;
    }
    true
}

/// Read `task->kernel_stack_top` directly. The full
/// `task_kernel_stack_bounds` accessor returns `(base, top)`; the
/// dispatcher hot path only needs `top` for TSS RSP0 programming.
#[inline]
pub fn task_kernel_stack_top<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).kernel_stack_top })
}

/// Read `task->flags`. (`task_flags` already exists as `u16`; this
/// helper exposes a typed bit-test the scheduler uses.)
#[inline]
pub fn task_has_flag<K, U>(task: *const TaskInner<K, U>, flag: u16) -> bool {
    task_flags(task).is_some_and(|f| (f & flag) != 0)
}

/// Read `task->fs_base`.
#[inline]
pub fn task_fs_base<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).fs_base })
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
    // SAFETY: caller pre-validated; both fields are scalars.
    unsafe {
        (*task).cpu_affinity = mask;
        (*task).last_cpu = last_cpu;
    }
}

/// Read `task->task_id` and increment its refcount in one shot.
/// Returns the (post-inc) refcount, or `None` if `task` is null.
#[inline]
pub fn task_inc_ref_with_id<K, U>(task: *mut TaskInner<K, U>) -> Option<(u32, u32)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; both ops are atomic / scalar.
    let id = unsafe { (*task).task_id };
    let count = unsafe { (*task).inc_ref() };
    Some((id, count))
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

/// Read `task->test_reports.is_some()`.
#[inline]
pub fn task_has_test_reports<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `Option<KBox<..>>` is one
    // pointer-sized slot, the niche-optimised None matches null.
    unsafe { (*task).test_reports.is_some() }
}

/// Take ownership of `task->test_reports`, leaving the slot as
/// `None`. Caller-required invariant: invoke after the task has
/// exited so no further `SYSCALL_TEST_REPORT` push races the take.
#[inline]
pub fn task_take_test_reports<K, U>(
    task: *mut TaskInner<K, U>,
) -> Option<crate::KBox<crate::task::test_reports::TestReportRing>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `Option::take` is a single
    // word swap on the in-Task field.
    unsafe { (*task).test_reports.take() }
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

/// Read `task->slot_index`.
#[inline]
pub fn task_slot_index<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `slot_index` is a `u32` field.
    Some(unsafe { (*task).slot_index })
}

/// Read `task->last_cpu`.
#[inline]
pub fn task_last_cpu<K, U>(task: *const TaskInner<K, U>) -> Option<u8> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `last_cpu` is a `u8` field.
    Some(unsafe { (*task).last_cpu })
}

/// Read `task->context.rflags`.
#[inline]
pub fn task_context_rflags<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `rflags` is a u64 field.
    Some(unsafe { (*task).context.rflags })
}

/// RAII increment of `task->refcnt`. `new` bumps the count; `Drop`
/// decrements. Used by callers that hold a `*mut Task` across a
/// scheduler yield (e.g. `task_wait_for`) so the pool slot cannot be
/// reset by the zombie reaper while the borrow is live — `reap_zombies`
/// requires `task_ref_count(raw) == Some(0)` before recycling.
pub struct TaskRefGuard<K, U> {
    task: *mut TaskInner<K, U>,
}

impl<K, U> TaskRefGuard<K, U> {
    pub fn new(task: *mut TaskInner<K, U>) -> Self {
        if !task.is_null() {
            let _ = task_inc_ref(task);
        }
        Self { task }
    }
}

impl<K, U> Drop for TaskRefGuard<K, U> {
    fn drop(&mut self) {
        if !self.task.is_null() {
            let _ = task_dec_ref(self.task);
        }
    }
}
// ---------------------------------------------------------------------------
// Phase-2 accessors: scheduler / driver / signal / stats hot paths.
// Each absorbs an `unsafe { (*task).<field> }` deref formerly scattered
// across `core/src/scheduler/{scheduler,per_cpu,task/*}.rs`,
// `core/src/scheduler/sleep.rs`, `core/src/driver_hooks.rs`, etc.
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

/// Read `task->switch_ctx.(rip, rsp)` as `(u64, u64)` via
/// `read_unaligned`. Mirrors the legacy idiom in `scheduler.rs`.
#[inline]
pub fn task_switch_ctx_rip_rsp<K, U>(task: *const TaskInner<K, U>) -> Option<(u64, u64)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; addr_of! produces a valid pointer
    // into the in-Task crate::task::TaskContext; read_unaligned is safe.
    unsafe {
        let ctx = core::ptr::addr_of!((*task).switch_ctx);
        Some((
            core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).rip)),
            core::ptr::read_unaligned(core::ptr::addr_of!((*ctx).rsp)),
        ))
    }
}

/// Reborrow `task->switch_ctx` as `*mut crate::task::TaskContext`. Returns
/// `null_mut()` when the caller passes a null Task pointer. Used by
/// the scheduler dispatcher to feed `switch_registers`.
#[inline]
pub fn task_switch_ctx_ptr_mut<K, U>(task: *mut TaskInner<K, U>) -> *mut crate::task::TaskContext {
    if task.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller pre-validated; `switch_ctx` is in-Task; the
    // returned raw pointer's validity is bounded by the Task's
    // lifetime.
    unsafe { &raw mut (*task).switch_ctx }
}

/// Reborrow `task->switch_ctx` as `*const crate::task::TaskContext`. Mirror of
/// [`task_switch_ctx_ptr_mut`] for read-only callers.
#[inline]
pub fn task_switch_ctx_ptr<K, U>(task: *const TaskInner<K, U>) -> *const crate::task::TaskContext {
    if task.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller pre-validated; in-Task field reborrow.
    unsafe { &raw const (*task).switch_ctx }
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

/// True when the scheduler already owns the task in a runqueue, remote inbox,
/// dispatch/on-CPU window, or migration handoff.
#[inline]
pub fn task_sched_placement_is_owned<K, U>(task: *const TaskInner<K, U>) -> bool {
    task_sched_placement_load(task) != SchedPlacement::None
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

/// True iff the task is currently claimed by some CPU's remote wake inbox.
#[inline]
pub fn task_remote_inbox_is_linked<K, U>(task: *const TaskInner<K, U>) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; `remote_inbox_link` is an in-task `Link`.
    unsafe { (*task).remote_inbox_link.is_linked() }
}

/// Read `task->tgid` (thread-group id).
#[inline]
pub fn task_tgid<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).tgid })
}

/// Read `task->parent_task_id`.
#[inline]
pub fn task_parent_task_id<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).parent_task_id })
}

/// Read `task->task_id` without any pool-validity gate. Mirrors
/// [`task_id_of`] but skips the null-check.
#[inline]
pub fn task_task_id<K, U>(task: *const TaskInner<K, U>) -> Option<u32> {
    task_id_of(task)
}

/// Wake every task currently blocked waiting for this task to exit.
/// Caller must hold the task pointer stable (e.g. via the task-manager
/// lock or a `TaskRefGuard`) long enough to resolve its id; the event
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

/// OR `mask` into `task->signal_pending` with `AcqRel` ordering.
/// Returns the previous bitmask. Used by signal-delivery hooks.
#[inline]
pub fn task_signal_pending_or<K, U>(task: *const TaskInner<K, U>, mask: u64) -> u64 {
    if task.is_null() {
        return 0;
    }
    // SAFETY: caller pre-validated; `signal_pending` is `AtomicU64`.
    unsafe {
        (*task)
            .signal_pending
            .fetch_or(mask, core::sync::atomic::Ordering::AcqRel)
    }
}

/// Raise `mask` on `task`'s pending set **and** wake any `signalfd` poller
/// registered on it (publishes [`signal_pending_event`]). This is the
/// signal-raise chokepoint for in-band (signalfd / ring) delivery: a masked
/// signal stays pending (so it does not interrupt a wait with EINTR) yet a
/// poller subscribed to its `SignalPending` queue is still woken to drain it.
/// Returns the previous pending bitmask.
pub fn task_signal_raise<K, U>(task: *const TaskInner<K, U>, mask: u64) -> u64 {
    let prev = task_signal_pending_or(task, mask);
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

/// Read `task->load_block_reason()` via the existing `&self` method.
#[inline]
pub fn task_load_block_reason<K, U>(
    task: *const TaskInner<K, U>,
) -> Option<slopos_abi::task::BlockReason> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `load_block_reason` is `&self`.
    Some(unsafe { (*task).load_block_reason() })
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

/// Bump `task->yield_count` with saturating-add semantics.
#[inline]
pub fn task_yield_count_inc<K, U>(task: *mut TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    unsafe {
        (*task).yield_count = (*task).yield_count.saturating_add(1);
    }
}

/// Stamp `task->last_run_timestamp`.
#[inline]
pub fn task_set_last_run_timestamp<K, U>(task: *mut TaskInner<K, U>, timestamp: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).last_run_timestamp = timestamp;
    }
}

/// Read `task->last_run_timestamp` via `read_volatile` to defeat
/// compiler reordering across the context-switch boundary.
#[inline]
pub fn task_last_run_timestamp_volatile<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*task).last_run_timestamp)) })
}

/// Plain (non-volatile) read of `task->last_run_timestamp`. Used by
/// callers that don't need ordering across the context-switch boundary
/// (e.g. work-stealing heuristic).
#[inline]
pub fn task_last_run_timestamp<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).last_run_timestamp })
}

/// Bump `task->total_runtime` by `delta`, saturating.
#[inline]
pub fn task_add_total_runtime<K, U>(task: *mut TaskInner<K, U>, delta: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).total_runtime = (*task).total_runtime.saturating_add(delta);
    }
}

/// Clear `task->controlling_tty` if `(sid, tty)` matches. Returns
/// `true` if a clear occurred. Used by the session-leader TTY-hangup
/// hook.
#[inline]
pub fn task_clear_controlling_tty_for<K, U>(
    task: *mut TaskInner<K, U>,
    sid: u32,
    tty: TtyIndex,
) -> bool {
    if task.is_null() {
        return false;
    }
    // SAFETY: caller pre-validated; both reads are scalar.
    unsafe {
        if (*task).sid == sid && (*task).controlling_tty == Some(tty) {
            (*task).controlling_tty = None;
            return true;
        }
    }
    false
}

/// Read `task->clear_child_tid` (futex-on-exit user-mode address).
#[inline]
pub fn task_clear_child_tid<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).clear_child_tid })
}

/// Stamp `task->clear_child_tid`.
#[inline]
pub fn task_set_clear_child_tid<K, U>(task: *mut TaskInner<K, U>, tid: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).clear_child_tid = tid;
    }
}

/// Run `f` against `task->user_ctx` borrowed as `&UserContext`.
/// Returns `None` for null `task`; otherwise returns `Some(f(...))`.
#[inline]
pub fn task_with_user_ctx<R, K, U>(
    task: *const TaskInner<K, U>,
    f: impl FnOnce(&UserContext) -> R,
) -> Option<R> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `user_ctx` is an in-Task field.
    Some(f(unsafe { &(*task).user_ctx }))
}

/// PCR↔Task mirror used by the scheduler context-switch path.
/// Saves the per-CPU PCR's user-mode round-trip slots onto `prev`,
/// then loads `next`'s saved slots back into the PCR. Single source
/// of truth for the "switch the user-mode round-trip" half of
/// `prepare_switch_to`. Operates only on the current CPU's PCR.
///
/// # Preconditions
/// - Caller has interrupts disabled.
/// - `prev` and `next` are pool-pinned Task pointers (or null for
///   `prev` on bootstrap entry).
#[inline]
pub fn task_pcr_round_trip_swap<K, U>(prev: *mut TaskInner<K, U>, next: *mut TaskInner<K, U>) {
    use core::sync::atomic::Ordering;
    // SAFETY: interrupts disabled by caller; the per-CPU PCR is
    // stable for this CPU during a switch window. Each `(*prev)` /
    // `(*next)` access is to a pool-pinned Task whose memory is
    // valid for the duration of `prepare_switch_to`.
    unsafe {
        let pcr = crate::cpu::x86_64::pcr::current_pcr();
        if !prev.is_null() {
            (*prev).saved_user_ctx_ptr = pcr.user_ctx_ptr.load(Ordering::Acquire);
            core::ptr::copy_nonoverlapping(
                pcr.kernel_return_ctx.get(),
                &raw mut (*prev).saved_kernel_return_ctx,
                1,
            );
        }
        if !next.is_null() {
            pcr.user_ctx_ptr
                .store((*next).saved_user_ctx_ptr, Ordering::Release);
            core::ptr::copy_nonoverlapping(
                &raw const (*next).saved_kernel_return_ctx,
                pcr.kernel_return_ctx.get(),
                1,
            );
        }
    }
}

/// Read `task->stack_pointer`.
#[inline]
pub fn task_stack_pointer<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).stack_pointer })
}

/// Read `task->stack_base`.
#[inline]
pub fn task_stack_base<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).stack_base })
}

/// Read `task->stack_size`.
#[inline]
pub fn task_stack_size<K, U>(task: *const TaskInner<K, U>) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).stack_size })
}

/// Read `task->signal_blocked` (`SigSet = u64`).
#[inline]
pub fn task_signal_blocked<K, U>(
    task: *const TaskInner<K, U>,
) -> Option<slopos_abi::signal::SigSet> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u64.
    Some(unsafe { (*task).signal_blocked })
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
            Some(actions[idx].handler)
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
        crate::task::kernel_task::fpu_reset_in_place(&raw mut task.fpu_state);
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
/// Centralises the field-by-field copy formerly inline in
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
        let ctx = &mut (*task).context;
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

/// Stamp `task->kernel_stack_top`. Used by tests that simulate a
/// missing kernel-stack-top error path.
#[inline]
pub fn task_set_kernel_stack_top<K, U>(task: *mut TaskInner<K, U>, top: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).kernel_stack_top = top;
    }
}

/// Bump `task->migration_count` with saturating-add semantics.
#[inline]
pub fn task_migration_count_inc<K, U>(task: *mut TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    unsafe {
        (*task).migration_count = (*task).migration_count.saturating_add(1);
    }
}

/// Reset a `Task` in place via [`Task::reset_in_place`]. No-op for
/// null pointers; caller must hold exclusive access to the slot
/// (typically via the task-manager lock).
#[inline]
pub fn task_reset_in_place<K, U>(task: *mut TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller guarantees exclusive access; `reset_in_place` is
    // the canonical slot-recycle entry point.
    unsafe { TaskInner::<K, U>::reset_in_place(task) };
}

/// Release the kernel-stack and SafeStack handles owned by `task`,
/// zero the adjacent plain-u64 fields, and clear the kernel-task
/// `stack_base` alias. Used by `free_task_stacks` to retire a
/// Terminated task's backing memory while keeping the slot
/// discoverable for idempotent terminate calls.
#[inline]
pub fn task_release_stacks<K, U>(task: *mut TaskInner<K, U>) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller holds the task-manager lock (or equivalent
    // exclusive access); dropping the handles releases VA slots +
    // physical frames; the plain-u64 mirrors are cleared so no
    // stray reader sees a dangling base/top.
    unsafe {
        (*task).kernel_stack = None;
        (*task).kernel_stack_base = 0;
        (*task).kernel_stack_top = 0;
        (*task).kernel_stack_size = 0;

        (*task).unsafe_stack = None;
        (*task).abi.unsafe_stack_sp = 0;

        if (*task).process_id == slopos_abi::task::INVALID_PROCESS_ID {
            (*task).stack_base = 0;
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
        let blocked = (*task).signal_blocked;
        (pending & !blocked) != 0
    }
}
