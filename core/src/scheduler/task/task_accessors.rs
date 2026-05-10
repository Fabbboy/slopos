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

use slopos_abi::syscall::TtyIndex;
use slopos_abi::task::{TaskExitReason, TaskFaultReason, TaskPriority, TaskStatus};
use slopos_ostd::task::fpu::FpuState;
use slopos_ostd::user::context::UserContext;

use super::Task;
use super::task_pointer_is_valid;

/// Validate the pointer through [`task_pointer_is_valid`]. Wraps the
/// downstream null/whitelist check so callers can short-circuit
/// before touching any field.
#[inline]
pub fn task_validate(task: *const Task) -> Option<*const Task> {
    if task.is_null() {
        None
    } else if task_pointer_is_valid(task) {
        Some(task)
    } else {
        None
    }
}

/// Read the task's stable `task_id`.
#[inline]
pub fn task_id_of(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).task_id })
}

/// Read the task's `process_id`.
#[inline]
pub fn task_process_id(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).process_id })
}

/// Read the task's `flags` bitfield.
#[inline]
pub fn task_flags(task: *const Task) -> Option<u16> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u16.
    Some(unsafe { (*task).flags })
}

/// Read the task's user-mode `entry_point` virtual address.
#[inline]
pub fn task_entry_point(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u64.
    Some(unsafe { (*task).entry_point })
}

/// Read the task's kernel-stack `(base, top)` pair.
#[inline]
pub fn task_kernel_stack_bounds(task: *const Task) -> Option<(u64, u64)> {
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
pub fn task_context_cr3(task: *const Task) -> Option<u64> {
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
pub fn task_context_rip(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; read_unaligned is safe.
    Some(unsafe { core::ptr::read_unaligned(core::ptr::addr_of!((*task).context.rip)) })
}

/// Read the task's saved stack pointer from its `TaskContext`.
#[inline]
pub fn task_context_rsp(task: *const Task) -> Option<u64> {
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
pub fn task_name_bytes<'a>(task: *const Task) -> Option<&'a [u8]> {
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
pub fn task_set_unsafe_stack_sp(task: *mut Task, sp: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated `task`; field is a naturally-aligned
    // u64 inside the Task struct. Pre-SMP single-writer access precludes
    // races on this field.
    unsafe {
        (*task).unsafe_stack_sp = sp;
    }
}

/// Stamp `task->entry_point` with `entry`. Used by the exec path
/// when re-targeting an existing task at a freshly-loaded ELF entry.
#[inline]
pub fn task_set_entry_point(task: *mut Task, entry: u64) {
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
pub fn task_set_fs_base(task: *mut Task, fs_base: u64) {
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
pub fn task_set_status(task: *mut Task, status: TaskStatus) {
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
pub fn task_set_context_rip_rsp(task: *mut Task, rip: u64, rsp: u64) {
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
pub fn task_user_ctx_mut<'a>(task: *mut Task) -> Option<&'a mut UserContext> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `user_ctx` is an in-Task field
    // whose pin-stability matches the rest of the Task struct.
    Some(unsafe { &mut (*task).user_ctx })
}

/// Read `task->cpu_affinity`.
#[inline]
pub fn task_cpu_affinity(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    Some(unsafe { (*task).cpu_affinity })
}

/// Stamp `task->cpu_affinity`.
#[inline]
pub fn task_set_cpu_affinity(task: *mut Task, mask: u32) {
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
pub fn task_pgid(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u32.
    Some(unsafe { (*task).pgid })
}

/// Reborrow `*const Task` as `&Task`. Used by callers that need a
/// few field reads but not the named getter helpers above.
#[inline]
pub fn task_borrow<'a>(task: *const Task) -> Option<&'a Task> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the borrow's lifetime is bounded
    // by the caller's frame.
    Some(unsafe { &*task })
}

/// Reborrow `*mut Task` as `&mut Task`. Mirrors [`task_borrow`].
#[inline]
pub fn task_borrow_mut<'a>(task: *mut Task) -> Option<&'a mut Task> {
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
pub fn task_record_user_fault_exit(task: *mut Task, reason: TaskFaultReason) -> Option<u32> {
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
pub fn task_time_slice(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).time_slice })
}

/// Stamp `task->time_slice`.
#[inline]
pub fn task_set_time_slice(task: *mut Task, slice: u64) {
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
pub fn task_time_slice_remaining(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).time_slice_remaining })
}

/// Stamp `task->time_slice_remaining`.
#[inline]
pub fn task_set_time_slice_remaining(task: *mut Task, remaining: u64) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    unsafe {
        (*task).time_slice_remaining = remaining;
    }
}

/// Read `task->next_ready` — the intrusive-list link used by
/// `ReadyQueue` and `ZombieList`.
#[inline]
pub fn task_next_ready(task: *const Task) -> Option<*mut Task> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; the link slot's atomic load is
    // internally synchronised, the owning queue's lock orders this
    // with concurrent push/pop operations.
    Some(unsafe { (*task).next_ready.load() })
}

/// Stamp `task->next_ready`. Used by the queue-mutation paths in
/// `ReadyQueue::{enqueue,dequeue,remove}` and `ZombieList::push`.
#[inline]
pub fn task_set_next_ready(task: *mut Task, next: *mut Task) {
    if task.is_null() {
        return;
    }
    // SAFETY: caller pre-validated; the link slot's atomic store is
    // internally synchronised; ordering with concurrent readers is
    // upheld by the owning queue's lock.
    unsafe {
        (*task).next_ready.store(next);
    }
}

/// Bump `task->refcnt`. Returns the post-increment count, mirroring
/// `Task::inc_ref`. Returns `None` for null pointers.
#[inline]
pub fn task_inc_ref(task: *mut Task) -> Option<u32> {
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
pub fn task_dec_ref(task: *mut Task) -> Option<bool> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `dec_ref` takes `&self` and
    // performs the atomic sub internally.
    Some(unsafe { (*task).dec_ref() })
}

/// Read `task->refcnt`.
#[inline]
pub fn task_ref_count(task: *const Task) -> Option<u32> {
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
pub fn task_fpu_state_mut<'a>(task: *mut Task) -> Option<&'a mut FpuState> {
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
pub fn task_priority(task: *const Task) -> Option<TaskPriority> {
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
pub fn task_set_last_cpu(task: *mut Task, cpu_id: u8) {
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
pub fn task_status(task: *const Task) -> Option<TaskStatus> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `status` is a `&self` method
    // that performs an atomic load internally.
    Some(unsafe { (*task).status() })
}

/// Read `task->sid` (session id).
#[inline]
pub fn task_sid(task: *const Task) -> Option<u32> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is a naturally-aligned u32.
    Some(unsafe { (*task).sid })
}

/// Read `task->controlling_tty`.
#[inline]
pub fn task_controlling_tty(task: *const Task) -> Option<TtyIndex> {
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
pub fn task_set_controlling_tty(task: *mut Task, tty: Option<TtyIndex>) -> bool {
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
pub fn task_kernel_stack_top(task: *const Task) -> Option<u64> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; field is naturally-aligned u64.
    Some(unsafe { (*task).kernel_stack_top })
}

/// Read `task->flags`. (`task_flags` already exists as `u16`; this
/// helper exposes a typed bit-test the scheduler uses.)
#[inline]
pub fn task_has_flag(task: *const Task, flag: u16) -> bool {
    task_flags(task).is_some_and(|f| (f & flag) != 0)
}

/// Read `task->fs_base`.
#[inline]
pub fn task_fs_base(task: *const Task) -> Option<u64> {
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
pub fn task_name_looks_idle(task: *const Task) -> bool {
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
pub fn task_install_idle_affinity(task: *mut Task, mask: u32, last_cpu: u8) {
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
pub fn task_inc_ref_with_id(task: *mut Task) -> Option<(u32, u32)> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; both ops are atomic / scalar.
    let id = unsafe { (*task).task_id };
    let count = unsafe { (*task).inc_ref() };
    Some((id, count))
}

/// Spin-wait until the task's `on_cpu` flag goes false. Used by
/// `schedule_task` to avoid dispatching a task that another CPU is
/// still finishing its outgoing context switch on.
///
/// Self-wakeup short-circuit: if `task` is the currently-executing
/// task on this CPU, its `on_cpu` flag will not clear until we yield
/// — but we cannot yield while spinning here (the caller is the
/// timer ISR's wake_due_sleepers). Skip the wait; the caller's
/// `enqueue_local` ensures the task is back in a runqueue, and the
/// idle-resume path's re-enqueue check (now extended to cover the
/// `Ready` state) keeps the task schedulable across the yield.
#[inline]
pub fn task_wait_off_cpu(task: *const Task) {
    if task.is_null() {
        return;
    }
    let cur = crate::scheduler::scheduler::scheduler_get_current_task();
    if !cur.is_null() && cur as *const Task == task {
        return;
    }
    // SAFETY: caller pre-validated; `on_cpu` is `AtomicBool`.
    unsafe {
        while (*task).on_cpu.load(core::sync::atomic::Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

/// Set `task->on_cpu = on`. The dispatcher uses `true` before
/// switching in, then clears to `false` after the outgoing context
/// save completes.
#[inline]
pub fn task_set_on_cpu(task: *mut Task, on: bool) {
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
pub fn task_has_test_reports(task: *const Task) -> bool {
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
pub fn task_take_test_reports(
    task: *mut Task,
) -> Option<slopos_ostd::KBox<crate::scheduler::test_reports::TestReportRing>> {
    if task.is_null() {
        return None;
    }
    // SAFETY: caller pre-validated; `Option::take` is a single
    // word swap on the in-Task field.
    unsafe { (*task).test_reports.take() }
}

/// RAII increment of `task->refcnt`. `new` bumps the count; `Drop`
/// decrements. Used by callers that hold a `*mut Task` across a
/// scheduler yield (e.g. `task_wait_for`) so the pool slot cannot be
/// reset by the zombie reaper while the borrow is live — `reap_zombies`
/// requires `task_ref_count(raw) == Some(0)` before recycling.
pub struct TaskRefGuard {
    task: *mut Task,
}

impl TaskRefGuard {
    pub fn new(task: *mut Task) -> Self {
        if !task.is_null() {
            let _ = task_inc_ref(task);
        }
        Self { task }
    }
}

impl Drop for TaskRefGuard {
    fn drop(&mut self) {
        if !self.task.is_null() {
            let _ = task_dec_ref(self.task);
        }
    }
}
