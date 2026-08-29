use core::ffi::c_int;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;

use slopos_abi::event::{KernelEvent, TaskSlot};
use slopos_arch::cpu;
use slopos_ostd::sync::BUS;
use slopos_ostd::sync::PreemptGuard;

use slopos_ostd::kdiag_timestamp;
use slopos_ostd::{klog_info, klog_warn};

use slopos_kernel_services::platform;

use core::sync::atomic::AtomicBool;

/// Per-CPU: this CPU's idle path armed a LAPIC one-shot for the next sleep-queue
/// deadline. The first timer ISR restores periodic mode and clears the flag.
static ONESHOT_ARMED: [AtomicBool; slopos_arch::MAX_CPUS] = {
    const FALSE: AtomicBool = AtomicBool::new(false);
    [FALSE; slopos_arch::MAX_CPUS]
};

/// Must match `boot/src/boot_drivers.rs::LAPIC_TIMER_PERIOD_MS`.
const LAPIC_TIMER_PERIOD_MS: u32 = 10;

/// Arm a LAPIC one-shot for the soonest sleep-queue deadline that falls inside
/// the current periodic tick window, so a 1 ms sleep wakes at 1 ms rather than
/// at the next 10 ms boundary. Idempotent; the next ISR restores periodic mode.
pub fn arm_tickless_idle_if_due() {
    let now = super::sleep::sleep_queue_now_ms();
    let Some(deadline) = sleep_queue_next_deadline_ms(now) else {
        return;
    };
    let delta = deadline.wrapping_sub(now);
    // An already-due deadline lands in the upper (past) half of the ms space;
    // the next periodic tick wakes it, so skip arming.
    if delta == 0 || delta >= (1u64 << 63) {
        return;
    }
    if delta >= LAPIC_TIMER_PERIOD_MS as u64 {
        return;
    }
    let ms_until = delta as u32;
    if platform::timer_program_next_wakeup_ms(ms_until) {
        let cpu_id = slopos_arch::pcr::get_current_cpu();
        if cpu_id < slopos_arch::MAX_CPUS {
            ONESHOT_ARMED[cpu_id].store(true, Ordering::Release);
        }
    }
}

#[inline]
fn restore_periodic_if_armed() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    if cpu_id >= slopos_arch::MAX_CPUS {
        return;
    }
    if ONESHOT_ARMED[cpu_id].swap(false, Ordering::AcqRel) {
        platform::timer_restore_periodic();
    }
}

pub use super::lifecycle::{
    boot_step_idle_task, boot_step_scheduler_init, boot_step_task_manager_init,
    get_percpu_scheduler_stats, get_scheduler_stats, get_total_ready_tasks_all_cpus,
    init_scheduler_for_ap, scheduler_enable, scheduler_shutdown, send_reschedule_ipi,
    stop_scheduler,
};
use super::per_cpu;
pub use super::runtime::{
    create_idle_task, create_idle_task_for_cpu, enter_scheduler,
    scheduler_register_idle_wakeup_callback,
};
pub use super::sleep::{
    block_current_task_with_timeout, cancel_sleep, poll_block_current_timeout,
    sleep_current_task_ms,
};
use super::sleep::{sleep_queue_next_deadline_ms, wake_due_sleepers};
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_USER_MODE, Task, TaskPriority, TaskRef, TaskStatus, task_record_context_switch,
    task_record_yield, task_set_state, task_transition_from,
};
pub use super::trap::{
    RescheduleReason, TrapExitSource, save_preempt_context, scheduler_handle_post_irq,
    scheduler_handle_timer_interrupt, scheduler_handoff_on_trap_exit, scheduler_request_reschedule,
    scheduler_request_reschedule_from_interrupt,
};
const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;
const USER_SPACE_TOP: u64 = 0xffff_8000_0000_0000;

#[inline]
fn kernel_text_range() -> (u64, u64) {
    let r = slopos_ostd::arch::x86_64::linker::text_range();
    (r.start as u64, r.end as u64)
}

use core::sync::atomic::AtomicU8;
use slopos_ostd::task::{SchedPlacement, TaskAddr};
/// `pub(crate)` for `test_hermetic::SchedulerEnabledFlag`'s snapshot/restore;
/// everything else goes through `set_scheduler_enabled` / `scheduler_is_enabled`.
pub(crate) static SCHEDULER_ENABLED: AtomicU8 = AtomicU8::new(0);
static PREEMPTION_ENABLED: AtomicU8 = AtomicU8::new(SCHEDULER_PREEMPTION_DEFAULT);

pub(crate) fn set_scheduler_enabled(enabled: bool) {
    let value = if enabled { 1 } else { 0 };
    SCHEDULER_ENABLED.store(value, Ordering::Release);
}

#[cfg(feature = "test-hooks")]
pub fn set_scheduler_enabled_for_test(enabled: bool) {
    set_scheduler_enabled(enabled);
}

#[inline]
pub(crate) fn is_scheduling_active() -> bool {
    SCHEDULER_ENABLED.load(Ordering::Acquire) != 0
        && PREEMPTION_ENABLED.load(Ordering::Acquire) != 0
}

use slopos_mm::process_vm::{
    process_vm_activate_by_handle, process_vm_get_cr3_phys_by_handle, unpack_process_vm_handle,
};
use slopos_mm::tlb;
use slopos_ostd::handle::HandleError;

use slopos_ostd::cpu::x86_64::xsave::active_xcr0;
use slopos_ostd::task::switch::switch_context;

use super::ffi_boundary::kernel_stack_top;
use crate::task_struct::{Current, Idle};

fn get_default_time_slice() -> u64 {
    SCHED_DEFAULT_TIME_SLICE as u64
}

fn reset_task_quantum(task: &Task) {
    let slice = match task.time_slice() {
        0 => get_default_time_slice(),
        s => s,
    };
    task.set_time_slice(slice);
    task.set_time_slice_remaining(slice);
}

// Task-scoped state living in per-CPU PCR slots, so it must ride every switch: a
// non-zero in-flight count left on a departed CPU would make a later
// `AbortOnUnwind` drop there abort a healthy kernel.
#[inline]
fn save_live_recovery_depth(task: &Task) {
    task.set_recovery_depth(slopos_arch::pcr::recovery_depth());
    task.set_panic_in_flight(slopos_arch::pcr::panic_in_flight_depth());
}

#[inline]
fn restore_live_recovery_depth(task: &Task) {
    slopos_arch::pcr::recovery_depth_store(task.recovery_depth());
    slopos_arch::pcr::panic_in_flight_store(task.panic_in_flight());
}

#[inline]
fn scheduler_ready_count(cpu_id: usize) -> u32 {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0)
}

/// Atomically install `task` as the task running on `cpu_id`: the single write
/// site for `PCR.current_task`, `PCR.syscall_pid`, `Task.state` -> Running and
/// the per-CPU switch counter.
///
/// # Preconditions
///
/// - `cpu_id == slopos_arch::pcr::get_current_cpu()`: SafeStack reads only the
///   *local* PCR via GS, so a cross-CPU dispatch corrupts the remote CPU's
///   unsafe-SP resolution.
/// - `task` is registry-owned (or a bootstrap stub) with `unsafe_stack_sp` primed.
/// - Preemption disabled, or inside the interrupts-off context-switch window.
#[inline]
pub(crate) fn dispatch(cpu_id: usize, task: &Task) {
    debug_assert!(
        cpu_id == slopos_arch::pcr::get_current_cpu(),
        "dispatch() must run on the target CPU (SafeStack slot is gs-local)"
    );

    // The id and priority ride along so a peer asking who is running never
    // dereferences a task whose switch tail may be destroying it.
    slopos_arch::pcr::set_current_task_typed(
        core::ptr::from_ref(task).cast_mut(),
        task.task_id,
        published_priority(task),
    );
    restore_live_recovery_depth(task);

    // Keep `PCR.syscall_pid` in sync so `copy_from_user` resolves the correct
    // address space even after preemption.
    let pid = task.process_id;
    if let Some(pcr) = slopos_arch::pcr::current_pcr_local() {
        pcr.syscall_pid.store(pid, Ordering::Release);
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_switches();
    });

    let current_status = task.status();
    debug_assert!(
        matches!(current_status, TaskStatus::Ready | TaskStatus::Running),
        "dispatch: invariant broken — task {} in unexpected state {:?}",
        task.task_id,
        current_status,
    );
    if !matches!(current_status, TaskStatus::Ready | TaskStatus::Running) {
        // Skip rather than coerce: a wait-protocol half-state task forced into
        // Running would run with a corrupted user-mode RIP.
        return;
    }
    let _ = task.sched_placement_compare_exchange(SchedPlacement::None, SchedPlacement::OnCpu);
    if !task.set_status(TaskStatus::Running) {
        return;
    }
}

/// Test-only [`dispatch`], carrying the same preconditions. Takes an id so the
/// registry guard pinning the task across the dispatch is held here rather than
/// by the caller; `false` if the id names no live task.
#[cfg(feature = "test-hooks")]
pub fn dispatch_task_for_test(cpu_id: usize, task_id: u32) -> bool {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return false;
    };
    dispatch(cpu_id, &task);
    true
}

/// Whether a task name is one `create_idle_task_for_cpu` would have produced.
fn name_looks_idle(name: &[u8]) -> bool {
    let Some(rest) = name.strip_prefix(b"idle") else {
        return false;
    };
    match rest.first() {
        None | Some(0) | Some(b'_') => true,
        Some(b'/') => rest.get(1).is_some_and(u8::is_ascii_digit),
        _ => false,
    }
}

/// Install `task` as `cpu_id`'s idle task, writing `PCR.idle_task`. Called once
/// per CPU, which is why the shape screen runs here rather than on every dispatch.
#[inline]
pub(super) fn install_idle_task(cpu_id: usize, task: &Task) {
    debug_assert!(
        task.task_id != INVALID_TASK_ID,
        "install_idle_task() must receive a registered task"
    );
    debug_assert!(
        task.priority == TaskPriority::Idle,
        "install_idle_task() must receive an Idle-priority task"
    );
    debug_assert!(
        (task.flags & TASK_FLAG_KERNEL_MODE != 0),
        "install_idle_task() must receive a kernel-mode task"
    );
    debug_assert!(
        name_looks_idle(task.name_bytes()),
        "install_idle_task() must receive a task named idle/<n>"
    );
    slopos_arch::pcr::set_idle_task(cpu_id, core::ptr::from_ref(task).cast::<()>().cast_mut());
}

/// Pre-switch housekeeping. Must run with interrupts disabled, from the scheduler
/// hot path only, so the sequencing matches the dispatch state machine.
fn prepare_switch_to(
    cpu_id: usize,
    prev_window: Option<&crate::task_struct::Switching<'_>>,
    next_window: &crate::task_struct::Switching<'_>,
) {
    let next = next_window.task();
    let xcr0 = active_xcr0();
    let is_user_mode = next.flags & TASK_FLAG_USER_MODE != 0;

    slopos_ostd::task::switch::pcr_round_trip_swap(prev_window, next_window);

    // The kernel is built `+soft-float`, so a kernel-mode task owns nothing in
    // the XCR0 register file. This elides work with no subject; deferring a
    // user task's save to a trap would be lazy FPU switching (CVE-2018-3665).
    if let Some(prev_window) = prev_window
        && prev_window.task().flags & TASK_FLAG_USER_MODE != 0
    {
        prev_window.task().fpu_save_current(prev_window, xcr0);
    }

    let new_pid = if is_user_mode {
        next.process_id
    } else {
        INVALID_PROCESS_ID
    };
    let next_vm = if is_user_mode {
        slopos_mm::process_vm::unpack_process_vm_handle(next.process_vm_handle_raw())
    } else {
        None
    };
    let next_key = next_vm.and_then(|handle| tlb::TlbProcessKey::from_slot(handle.slot()));
    tlb::notify_mm_switch(next_key, new_pid, cpu_id);
    if is_user_mode {
        tlb::exit_lazy_tlb(cpu_id);
    } else {
        tlb::enter_lazy_tlb(cpu_id);
    }

    // Both are read only on a ring-3 → ring-0 transition, which cannot happen
    // while a kernel-mode task is current, and the next user switch writes them.
    if is_user_mode {
        let raw = next.fs_base();
        let fs = if raw == 0 || slopos_abi::addr::VirtAddr::is_canonical(raw) {
            raw
        } else {
            0
        };
        slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, fs);

        let kernel_rsp = match next.kernel_stack_top {
            kst if kst != 0 => kst,
            _ => kernel_stack_top() as u64,
        };
        platform::gdt_set_kernel_rsp0(kernel_rsp);
    }

    let _ = cpu_id;
    if let Some(handle) = next_vm {
        // The handle fails to resolve if its slot was rebound to another process
        // since this task was built; fall back to the kernel master.
        if !matches!(process_vm_activate_by_handle(handle), Ok(true)) {
            slopos_ostd::mm::vm_space::activate_kernel_master_cr3();
        }
    } else {
        slopos_ostd::mm::vm_space::activate_kernel_master_cr3();
    }

    // A rejected image is already reset to the init state; the CPU is mid-switch
    // with interrupts off, so stopping here would be a machine-wide halt.
    if is_user_mode && !next_window.task().fpu_restore_to_cpu(next_window, xcr0) {
        klog_warn!(
            "SCHED: CPU {} rejected task {} FPU image; reset to init state",
            cpu_id,
            next.task_id
        );
    }
}

fn ensure_idle_switch_ctx_valid(idle_task: &Task) -> bool {
    let (rip, rsp) = idle_task.switch_ctx_rip_rsp();

    let (text_start, text_end) = kernel_text_range();
    let rip_ok = rip >= text_start && rip < text_end;
    let rsp_ok = rsp >= USER_SPACE_TOP;

    if rip_ok && rsp_ok {
        return true;
    }

    klog_info!(
        "SCHED: CPU {} idle task {} has corrupt switch_ctx: rip=0x{:x} (ok={}) rsp=0x{:x} (ok={}) — refusing switch",
        slopos_arch::pcr::get_current_cpu(),
        idle_task.task_id,
        rip,
        rip_ok,
        rsp,
        rsp_ok,
    );
    false
}

fn switch_from_current_to_idle(cpu_id: usize, current: Option<&Task>, idle_task: &Task) {
    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, Some(idle_task), timestamp);

    // Validate before publishing as current_task: a peer must never observe the
    // slot naming an unusable idle context.
    if !ensure_idle_switch_ctx_valid(idle_task) {
        klog_info!(
            "SCHED: CPU {} cannot recover idle switch_ctx for task {}",
            cpu_id,
            idle_task.task_id
        );
        return;
    }

    // Stays armed across the switch: the switch span mutates current_task, the
    // PCR and per-task context as one transition, and a descheduled frame cannot
    // unwind until its task resumes.
    let switch_abort_guard = slopos_ostd::panic::AbortOnUnwind::new();
    if let Some(current) = current {
        save_live_recovery_depth(current);
    }

    // Idle owns no reference, so this clears the slot and hands back whatever
    // the outgoing task held.
    let prev_raw = slopos_arch::pcr::install_current_task(core::ptr::null_mut());
    if let Some(prev_node) = NonNull::new(prev_raw.cast::<Task>()) {
        assert!(
            slopos_arch::pcr::defer_previous_task(prev_node.as_ptr().cast()).is_ok(),
            "previous-task slot was not drained before the next switch"
        );
    }

    slopos_ostd::task::run_switch(
        current,
        idle_task,
        || {
            dispatch(cpu_id, idle_task);
            slopos_ostd::sync::rcu_note_qs();
        },
        |prev_window, next_window| {
            prepare_switch_to(cpu_id, prev_window, next_window);
            let prev_ctx =
                prev_window.map_or(core::ptr::null_mut(), |w| w.task().switch_ctx_ptr(w));
            let next_ctx = next_window.task().switch_ctx_ptr(next_window).cast_const();
            switch_context(prev_ctx, next_ctx);
        },
    );
    switch_abort_guard.disarm();
}

/// Switch `current` directly into `next_ref`, a task already claimed by
/// [`claim_next_task`].
fn switch_to_claimed_task(cpu_id: usize, current: &Task, next_ref: TaskRef) {
    if let Some(returned) = execute_task_owned(cpu_id, Some(current), next_ref) {
        abandon_claim(cpu_id, returned);
    }
}

/// [`execute_task`] taking ownership of the incoming task's reference, handing
/// it back if the switch is refused.
fn execute_task_owned(
    cpu_id: usize,
    from_task: Option<&Task>,
    next_ref: TaskRef,
) -> Option<TaskRef> {
    let node = next_ref.node();
    // A borrow with no destructor, unlike a second guard: a count held here
    // would go unreleased for as long as this task is descheduled — or forever.
    slopos_ostd::task::with_parked_node(node, |to_task: &Task| {
        execute_task(cpu_id, from_task, to_task, next_ref)
    })
}

#[inline]
fn task_has_no_preempt_flag(task: &Task) -> bool {
    task.flags & TASK_FLAG_NO_PREEMPT != 0
}

#[inline]
fn consume_time_slice(current: &Task) -> bool {
    let remaining = current.time_slice_remaining();
    if remaining > 0 {
        current.set_time_slice_remaining(remaining - 1);
    }
    current.time_slice_remaining() > 0
}

#[inline]
fn mark_preempt_if_ready(cpu_id: usize) {
    if scheduler_ready_count(cpu_id) > 0 {
        scheduler_request_reschedule(RescheduleReason::TimerTick);
    }
}

fn placement_is_durable_owner(placement: SchedPlacement) -> bool {
    matches!(
        placement,
        SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating
            | SchedPlacement::Held
    )
}

fn task_has_durable_owner(task: &Task) -> bool {
    placement_is_durable_owner(task.sched_placement())
}

/// A task with no readable priority publishes `PRIORITY_NONE`, so a CPU parked
/// on one always loses the preemption comparison.
#[inline]
fn published_priority(task: &Task) -> u8 {
    Some(task.priority).map_or(slopos_arch::pcr::PRIORITY_NONE, |p| p.as_u8())
}

/// Whether `new` should preempt whatever `cpu` is running.
///
/// Reads the priority `cpu` published in its own PCR: dereferencing its
/// `current_task` races that CPU's `drain_previous_task`, which can run the
/// task's destructor.
pub(crate) fn newcomer_outranks_current(cpu: usize, new: &Task) -> bool {
    new.priority.as_u8() < slopos_arch::pcr::current_task_priority_for(cpu)
}

fn publish_ready_fallback(task: &TaskRef) -> c_int {
    let body: &Task = task;
    if !body.is_ready() {
        return -1;
    }

    match body.sched_placement() {
        SchedPlacement::ReadyQueue
        | SchedPlacement::RemoteWake
        | SchedPlacement::OnCpu
        | SchedPlacement::Migrating
        | SchedPlacement::Held => return 0,
        SchedPlacement::Nascent => return -1,
        SchedPlacement::None | SchedPlacement::Waking => {}
    }

    // A strict-pinned task must not be enqueued outside its mask just because a
    // permitted CPU's enqueue momentarily raced; only when no permitted CPU
    // accepts it does the last resort below relax. `affinity == 0` permits all.
    let affinity = body.cpu_affinity();
    let current_cpu = slopos_arch::pcr::get_current_cpu();
    let cpu_count = slopos_arch::pcr::get_cpu_count();

    if per_cpu::affinity_allows_cpu(affinity, current_cpu)
        && per_cpu::with_cpu_scheduler(current_cpu, |sched| sched.enqueue_local(task)) == Some(0)
    {
        if newcomer_outranks_current(current_cpu, body) {
            scheduler_request_reschedule(RescheduleReason::InterruptWake);
        }
        return 0;
    }

    for cpu_id in 0..cpu_count {
        if cpu_id == current_cpu || !per_cpu::affinity_allows_cpu(affinity, cpu_id) {
            continue;
        }
        if per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(task)) == Some(0) {
            if slopos_arch::pcr::is_cpu_online(cpu_id) {
                send_reschedule_ipi(cpu_id);
            }
            return 0;
        }
        match body.sched_placement() {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating
            | SchedPlacement::Held => return 0,
            SchedPlacement::Nascent => return -1,
            SchedPlacement::None | SchedPlacement::Waking => {}
        }
    }

    // Last resort: relax affinity rather than strand a runnable task.
    for cpu_id in 0..cpu_count {
        if per_cpu::affinity_allows_cpu(affinity, cpu_id) {
            continue;
        }
        if per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(task)) == Some(0) {
            klog_info!(
                "SCHED: relaxed affinity 0x{:x} for task {} onto cpu {} (no permitted CPU accepted)",
                affinity,
                body.task_id,
                cpu_id,
            );
            if cpu_id != current_cpu && slopos_arch::pcr::is_cpu_online(cpu_id) {
                send_reschedule_ipi(cpu_id);
            }
            return 0;
        }
        match body.sched_placement() {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating
            | SchedPlacement::Held => return 0,
            SchedPlacement::Nascent => return -1,
            SchedPlacement::None | SchedPlacement::Waking => {}
        }
    }

    klog_info!(
        "SCHED: publish fallback failed task={} status={:?} placement={:?} current_cpu={} cpu_count={}",
        body.task_id,
        body.status(),
        body.sched_placement(),
        current_cpu,
        cpu_count,
    );
    -1
}

fn publish_reserved_waking_ready(task: &TaskRef, task_id: u32, context: &str) -> c_int {
    let body: &Task = task;
    if !body.is_ready() {
        if body.sched_placement() == SchedPlacement::Waking {
            let restore = if body.on_cpu() {
                SchedPlacement::OnCpu
            } else {
                SchedPlacement::None
            };
            let _ = body.sched_placement_compare_exchange(SchedPlacement::Waking, restore);
        }
        return if body.is_exited() || (body.status() == TaskStatus::Invalid) || !body.is_ready() {
            0
        } else {
            -1
        };
    }

    let rc = schedule_task_from_placement(task, SchedPlacement::Waking, false);
    if rc == 0
        || matches!(
            body.sched_placement(),
            SchedPlacement::ReadyQueue
                | SchedPlacement::RemoteWake
                | SchedPlacement::OnCpu
                | SchedPlacement::Migrating
                | SchedPlacement::Held
        )
    {
        return 0;
    }

    klog_info!(
        "SCHED: {} failed to publish READY task {} rc={} status={:?} placement={:?}",
        context,
        task_id,
        rc,
        body.status(),
        body.sched_placement(),
    );
    rc
}

fn publish_ready_from_current_owner(task: &TaskRef, task_id: u32, context: &str) -> c_int {
    let body: &Task = task;
    for _ in 0..4 {
        match body.sched_placement() {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::Migrating
            | SchedPlacement::Held => {
                return 0;
            }
            SchedPlacement::Waking => return publish_reserved_waking_ready(task, task_id, context),
            // Never published: a wake that reached a nascent task is its
            // caller's bug, not a race to spin on.
            SchedPlacement::Nascent => return -1,
            SchedPlacement::OnCpu => {
                if body
                    .sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::Waking)
                {
                    return publish_reserved_waking_ready(task, task_id, context);
                }
            }
            SchedPlacement::None => {
                if body
                    .sched_placement_compare_exchange(SchedPlacement::None, SchedPlacement::Waking)
                {
                    return publish_reserved_waking_ready(task, task_id, context);
                }
            }
        }
    }

    if task_has_durable_owner(body) { 0 } else { -1 }
}

fn schedule_task_from_placement(task: &TaskRef, from: SchedPlacement, new_task: bool) -> c_int {
    let body: &Task = task;
    if !body.is_ready() {
        return -1;
    }

    // Conclusive, not a failure: the hold owns the task and publishes it on release.
    if crate::task::kernel_io_hold_claim(body, from) {
        return 0;
    }

    if body.time_slice_remaining() == 0 {
        reset_task_quantum(task);
    }

    let target_cpu = if new_task {
        per_cpu::select_target_cpu_for_new(body)
    } else {
        per_cpu::select_target_cpu(body)
    };
    let Some(target_cpu) = target_cpu else {
        return publish_ready_fallback(task);
    };
    let current_cpu = slopos_arch::pcr::get_current_cpu();

    if target_cpu == current_cpu {
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| match from {
            SchedPlacement::None => sched.enqueue_local(task),
            SchedPlacement::Waking => sched.enqueue_waking(task),
            SchedPlacement::OnCpu => sched.enqueue_from_on_cpu(task),
            // The queue parks its own membership reference; the caller keeps
            // whatever handle it carried.
            SchedPlacement::Migrating => sched.enqueue_migrated_borrowed(task),
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake => 0,
            // A nascent task holds no reservation to transfer into a queue.
            SchedPlacement::Nascent | SchedPlacement::Held => -1,
        });

        if result != Some(0) {
            return publish_ready_fallback(task);
        }
        // Local path: the preempt-pending flag makes the trap-exit handoff
        // dispatch the newcomer before HLT re-engages; the remote path IPIs.
        if newcomer_outranks_current(current_cpu, body) {
            scheduler_request_reschedule(RescheduleReason::InterruptWake);
        }
        0
    } else {
        let push_result = per_cpu::with_cpu_scheduler(target_cpu, |sched| match from {
            SchedPlacement::None => {
                sched.push_remote_wake(task);
                0
            }
            SchedPlacement::Waking => sched.push_remote_wake_waking(task),
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake | SchedPlacement::OnCpu => 0,
            SchedPlacement::Migrating | SchedPlacement::Nascent | SchedPlacement::Held => -1,
        });
        if !matches!(push_result, Some(0) | Some(1)) {
            return publish_ready_fallback(task);
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if slopos_arch::pcr::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        0
    }
}

/// A publication reservation: the placement to publish `from`.
struct Reservation {
    from: SchedPlacement,
    /// Set only when this reservation moved the task out of `Nascent` and
    /// therefore owes it a rollback.
    restore_nascent: bool,
}

/// Reserve scheduler ownership for an explicit publication. `None` if the task
/// is nascent and someone else won the promotion.
///
/// A reservation that took a task out of `Nascent` **must** be released by
/// [`release_publication`] if the publication does not complete: `Waking` is a
/// state `wake_blocked_task` publishes from, so a task left there hands a later
/// wake the half-built task `Nascent` exists to protect.
#[inline]
fn reserve_publication(task: &Task) -> Option<Reservation> {
    match task.sched_placement() {
        SchedPlacement::Waking => Some(Reservation {
            from: SchedPlacement::Waking,
            restore_nascent: false,
        }),
        SchedPlacement::Nascent => {
            if task
                .sched_placement_compare_exchange(SchedPlacement::Nascent, SchedPlacement::Waking)
            {
                Some(Reservation {
                    from: SchedPlacement::Waking,
                    restore_nascent: true,
                })
            } else {
                None
            }
        }
        _ => Some(Reservation {
            from: SchedPlacement::None,
            restore_nascent: false,
        }),
    }
}

/// Undo a reservation whose publication failed, so "never published" stays
/// spelled `Nascent`.
#[inline]
fn release_publication(task: &Task, reservation: &Reservation) {
    if reservation.restore_nascent {
        let _ =
            task.sched_placement_compare_exchange(SchedPlacement::Waking, SchedPlacement::Nascent);
    }
}

pub fn schedule_task(task: &TaskRef) -> c_int {
    let Some(reservation) = reserve_publication(task) else {
        return 0;
    };
    let rc = schedule_task_from_placement(task, reservation.from, false);
    if rc != 0 {
        release_publication(task, &reservation);
    }
    rc
}

/// Put a freshly created task into the placement a *published, then blocked* task
/// has: owned by nothing, but past construction. False if the task is gone or had
/// already left `Nascent`.
#[cfg(feature = "test-hooks")]
pub fn clear_nascent_for_test(task_id: u32) -> bool {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return false;
    };
    task.sched_placement_compare_exchange(SchedPlacement::Nascent, SchedPlacement::None)
}

/// Schedule a **newly created** task (fork, spawn, exec): picks the globally
/// idlest CPU to spread new processes out, where [`schedule_task()`] prefers
/// `last_cpu` for cache affinity.
pub fn schedule_new_task(task: &TaskRef) -> c_int {
    let Some(reservation) = reserve_publication(task) else {
        return 0;
    };
    let rc = schedule_task_from_placement(task, reservation.from, true);
    if rc != 0 {
        release_publication(task, &reservation);
    }
    rc
}

/// Publish a fully-initialized new task as runnable without ever exposing
/// `Ready + no scheduler owner`.
pub fn publish_new_task(task: &TaskRef) -> c_int {
    let body: &Task = task;
    // The sole sanctioned exit from `Nascent`; every other path refuses the
    // transition. `None` covers re-publishing a task unscheduled back to no owner.
    let reserved_from = match body.sched_placement() {
        from @ (SchedPlacement::Nascent | SchedPlacement::None) => {
            if !body.sched_placement_compare_exchange(from, SchedPlacement::Waking) {
                return if task_has_durable_owner(body)
                    || body.sched_placement() == SchedPlacement::Waking
                {
                    0
                } else {
                    -1
                };
            }
            from
        }
        SchedPlacement::Waking => SchedPlacement::Waking,
        _ => {
            return if task_has_durable_owner(body) { 0 } else { -1 };
        }
    };
    let previous_status = body.status();
    if !body.set_status(TaskStatus::Ready) {
        let _ = body.sched_placement_compare_exchange(SchedPlacement::Waking, reserved_from);
        return -1;
    }
    let rc = schedule_task_from_placement(task, SchedPlacement::Waking, true);
    if rc != 0 {
        let _ = body.sched_placement_compare_exchange(SchedPlacement::Waking, reserved_from);
        let _ = body.set_status(previous_status);
    }
    rc
}

/// Strip `task` from whichever ready queue holds it.
///
/// The placements refused below hold no queue membership: enqueue CASes to
/// `ReadyQueue` before linking. `Migrating` is not among them — a thief CASes
/// out of `ReadyQueue` before unlinking, so it may still be linked.
///
/// A wake racing this call is the caller's `status()` re-check to catch, not
/// this function's.
pub fn unschedule_task(task: &Task) -> c_int {
    match task.sched_placement() {
        SchedPlacement::None
        | SchedPlacement::OnCpu
        | SchedPlacement::Waking
        | SchedPlacement::RemoteWake
        | SchedPlacement::Nascent
        | SchedPlacement::Held => return 0,
        SchedPlacement::ReadyQueue | SchedPlacement::Migrating => {}
    }

    // Stamped before the link, under the same lock `remove_task` takes.
    let last_cpu = task.last_cpu() as usize;
    if per_cpu::with_cpu_scheduler(last_cpu, |sched| sched.remove_task(task)) == Some(0) {
        return 0;
    }

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if cpu_id == last_cpu {
            continue;
        }
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.remove_task(task);
        });
    }

    0
}

/// Re-place a task after its CPU-affinity mask changes. Idempotent when the task
/// is still permitted where it is.
pub fn task_apply_affinity(task: &TaskRef, new_affinity: u32) {
    let body: &Task = task;
    let last_cpu = body.last_cpu() as usize;
    if per_cpu::affinity_allows_cpu(new_affinity, last_cpu) {
        return;
    }
    match body.sched_placement() {
        SchedPlacement::ReadyQueue => {
            let _ = unschedule_task(body);
            let _ = schedule_task(task);
        }
        SchedPlacement::OnCpu => {
            let current_cpu = slopos_arch::pcr::get_current_cpu();
            if last_cpu == current_cpu {
                scheduler_request_reschedule(RescheduleReason::InterruptWake);
            } else if slopos_arch::pcr::is_cpu_online(last_cpu) {
                send_reschedule_ipi(last_cpu);
            }
        }
        // The release re-runs `select_target_cpu`, so a held task needs no repatriation.
        SchedPlacement::None
        | SchedPlacement::Waking
        | SchedPlacement::RemoteWake
        | SchedPlacement::Migrating
        | SchedPlacement::Nascent
        | SchedPlacement::Held => {}
    }
}

/// Whether `pid` is an id the process-VM allocator can have issued: a tripwire
/// on the allocator, not a bound anything downstream depends on.
pub(crate) fn dispatch_pid_ok(pid: u32) -> bool {
    pid == INVALID_PROCESS_ID || pid <= slopos_mm::memory_layout_defs::MAX_PROCESS_ID
}

/// Switch `cpu_id` from `from_task` into `to_task`.
///
/// Returns `handover` back when the switch is **refused** — a corrupt
/// `switch_ctx` or an unusable address space — in which case `to_task` has been
/// terminated and no register switch occurred; the caller unwinds the claim.
///
/// # Ownership across a switch
///
/// On the switched path this does not return until the *calling* task is
/// dispatched again, which may be never. No handle may outlive the call: a
/// `TaskRef` local would run its `Drop` on resume, releasing a count for a task
/// the frame no longer describes. `handover` is therefore moved into
/// `PCR.current_task_ref` below and the reference it displaces parked in
/// `previous_task`, since the frame that would otherwise hold either belongs to
/// a task the successor may outlive.
fn execute_task(
    cpu_id: usize,
    from_task: Option<&Task>,
    to_task: &Task,
    handover: TaskRef,
) -> Option<TaskRef> {
    let pid = to_task.process_id;
    let to_id = to_task.task_id;

    debug_assert!(
        dispatch_pid_ok(pid),
        "the process-VM allocator issued an id outside its own space"
    );

    // switch_ctx.rip must be in kernel .text: the entry trampoline, the user
    // first-run wrapper and every schedule resume point live there.
    let (rip, rsp) = to_task.switch_ctx_rip_rsp();
    let (text_start, text_end) = kernel_text_range();
    if rip < text_start || rip >= text_end {
        klog_info!(
            "SCHED: refusing to dispatch task {} with switch_ctx.rip=0x{:x} outside .text (0x{:x}..0x{:x})",
            to_id,
            rip,
            text_start,
            text_end,
        );
        let _ = crate::task::task_terminate(to_id);
        return Some(handover);
    }
    if rsp < USER_SPACE_TOP {
        klog_info!(
            "SCHED: refusing to dispatch task {} with switch_ctx.rsp=0x{:x} below kernel space",
            to_id,
            rsp,
        );
        let _ = crate::task::task_terminate(to_id);
        return Some(handover);
    }

    // A handle whose slot now belongs to another process, or one resolving to a
    // slot with no address space: switching into either would run this task in
    // someone else's page tables or in none.
    if pid != INVALID_PROCESS_ID {
        let resolved = match unpack_process_vm_handle(to_task.process_vm_handle_raw()) {
            Some(handle) => process_vm_get_cr3_phys_by_handle(handle),
            None => Err(HandleError::NoEntry),
        };
        match resolved {
            Ok(cr3_phys) if cr3_phys != 0 => {}
            Ok(_) => {
                klog_info!(
                    "SCHED: refusing to dispatch task {} (pid {}) with cr3_phys=0",
                    to_id,
                    pid,
                );
                let _ = crate::task::task_terminate(to_id);
                return Some(handover);
            }
            Err(err) => {
                klog_info!(
                    "SCHED: refusing to dispatch task {} (pid {}): address space handle {:?}",
                    to_id,
                    pid,
                    err,
                );
                let _ = crate::task::task_terminate(to_id);
                return Some(handover);
            }
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, Some(to_task), timestamp);

    let switch_abort_guard = slopos_ostd::panic::AbortOnUnwind::new();
    // `on_cpu` is what makes a peer dispatcher requeue this task rather than
    // dispatch it a second time.
    to_task.set_on_cpu(true);
    if let Some(from_task) = from_task {
        save_live_recovery_depth(from_task);
    }

    slopos_ostd::task::run_switch(
        from_task,
        to_task,
        || {
            // The last point still executing as the outgoing task, so the only
            // place the handover can happen.
            let next_node = handover.into_placement();
            let prev_raw = slopos_arch::pcr::install_current_task(next_node.as_ptr().cast());
            if let Some(prev_node) = NonNull::new(prev_raw.cast::<Task>()) {
                assert!(
                    slopos_arch::pcr::defer_previous_task(prev_node.as_ptr().cast()).is_ok(),
                    "previous-task slot was not drained before the next switch"
                );
            }
            dispatch(cpu_id, to_task);
            slopos_ostd::sync::rcu_note_qs();
        },
        |prev_window, next_window| {
            prepare_switch_to(cpu_id, prev_window, next_window);
            let prev_ctx =
                prev_window.map_or(core::ptr::null_mut(), |w| w.task().switch_ctx_ptr(w));
            let next_ctx = next_window.task().switch_ctx_ptr(next_window).cast_const();
            switch_context(prev_ctx, next_ctx);
        },
    );
    switch_abort_guard.disarm();
    None
}

/// Dequeue and claim the highest-priority ready task for `cpu_id`.
///
/// On success the returned reference carries the CPU's exclusive dispatch
/// claim: the task is `Running`, `on_cpu`, and `executing_task` is set. On
/// failure every one of those is unwound and the reference released, so a
/// caller that gets `None` owes nothing.
fn claim_next_task(cpu_id: usize) -> Option<TaskRef> {
    // Drain cross-core wakes before the pick: a just-pushed remote wake is not
    // yet visible in the ready queues. `cpu_id` is the owning CPU, which is
    // `drain_remote_inbox`'s single-consumer contract.
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.drain_remote_inbox());

    // The dequeue hands over the queue's owning reference rather than releasing
    // it, so the task stays pinned for the whole dispatch window below —
    // including the unbounded `on_cpu` spin.
    let dispatch_ref =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority()).flatten()?;
    let next_task: &Task = &dispatch_ref;

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(true);
    });

    if per_cpu::should_pause_scheduler_loop(cpu_id) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_from_on_cpu(&dispatch_ref);
            sched.set_executing_task(false);
        });
        core::hint::spin_loop();
        super::task::task_put(dispatch_ref);
        return None;
    }

    if next_task.is_exited() || !next_task.is_ready() {
        let _ =
            next_task.sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        super::task::task_put(dispatch_ref);
        return None;
    }

    // Wait out the prior CPU's switch-out tail rather than publish a second
    // queue membership. Dispatch runs interrupts-off, so this spin takes no IPI:
    // `spin_relax` services cross-CPU work by hand, since the prior CPU may be
    // stalled behind a shootdown it needs *us* to acknowledge.
    while next_task.on_cpu() {
        slopos_ostd::sync::spin_relax();
        core::hint::spin_loop();
    }

    // Single-winner dispatch claim: only one CPU may run a Ready task.
    let next_task_id = next_task.task_id;
    if task_set_state(next_task_id, TaskStatus::Running) != 0 {
        // A still-Ready task holds no other scheduler placement, so dropping the
        // dequeue would strand it; a claimed one is the winner's responsibility.
        if next_task.is_ready() {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_from_on_cpu(&dispatch_ref);
            });
        } else {
            let _ = next_task
                .sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
        }
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        super::task::task_put(dispatch_ref);
        return None;
    }

    // Before the validation in execute_task: a concurrent `task_terminate` must
    // see this task on-CPU for the whole dispatch, or it frees the kernel stack
    // this CPU is about to run on.
    next_task.set_on_cpu(true);

    Some(dispatch_ref)
}

/// Release a dispatch claim taken by [`claim_next_task`] for a switch that did
/// not happen, putting the task back where a later dispatch can find it.
fn abandon_claim(cpu_id: usize, dispatch_ref: TaskRef) {
    let task: &Task = &dispatch_ref;
    let task_id = task.task_id;
    if !task.is_exited() && task.is_running() {
        let _ = task_set_state(task_id, TaskStatus::Ready);
    }
    if task.is_ready() {
        let rc =
            per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enqueue_from_on_cpu(&dispatch_ref))
                .unwrap_or(-1);
        if rc != 0 {
            let _ = publish_ready_from_current_owner(&dispatch_ref, task_id, "abandon_claim");
        }
    } else {
        let _ = task.sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
    }
    // After the republish, as everywhere else: a peer must never see the task
    // Ready, off-CPU and unqueued.
    task.set_on_cpu(false);
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });
    super::task::task_put(dispatch_ref);
}

pub(crate) fn run_ready_task_from_idle(cpu_id: usize, idle_task: &Task) -> bool {
    let Some(dispatch_ref) = claim_next_task(cpu_id) else {
        return false;
    };
    // Idle owns no reference of its own — it is pinned by
    // `task_is_dispatch_pinned`'s idle disjunct — so nothing is displaced here.
    if let Some(returned) = execute_task_owned(cpu_id, Some(idle_task), dispatch_ref) {
        abandon_claim(cpu_id, returned);
        return false;
    }

    // Reached only when a task switched back to idle and parked itself for us.
    let timestamp = kdiag_timestamp();
    task_record_context_switch(None, Some(idle_task), timestamp);

    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    // No CR3 reload: `prepare_switch_to(next = idle)` already loaded the master.
    debug_assert!(
        slopos_ostd::mm::vm_space::kernel_master_pml4().is_none_or(|master| {
            slopos_arch::cpu::control_regs::read_cr3() & 0x000F_FFFF_FFFF_F000 == master.as_u64()
        }),
        "idle resumed on an address space that is not the kernel master",
    );

    finish_pending_switch(cpu_id);
    true
}

/// The post-switch tail: run by the CPU that has just switched *into* a task,
/// on behalf of the task it switched *away from*. IRQ-off, so the heavy
/// cleanup a dead task needs is left to [`drain_previous_task`].
fn finish_switch(cpu_id: usize, dispatch_ref: TaskRef) {
    let next_task: &Task = &dispatch_ref;
    let next_task_id = next_task.task_id;

    // Re-enqueue a task preempted or already woken before its yield completed,
    // keeping `on_cpu=true` until any Ready task has a queue/inbox membership: a
    // peer may observe a task as runnable while it is still completing a
    // switch-out, but never Ready + off-CPU + unqueued. Blocked/Zombie/Terminated
    // tasks are woken by their own event paths instead.
    let mut ready_published = false;
    if !next_task.is_exited() {
        let already_ready = next_task.is_ready();
        let needs_ready_transition = next_task.is_running();
        let should_enqueue = if already_ready {
            true
        } else if needs_ready_transition {
            task_set_state(next_task_id, TaskStatus::Ready) == 0
        } else {
            false
        };
        if should_enqueue {
            // A mask that changed while the task ran here must repatriate it
            // rather than re-queue it locally forever; `on_cpu` is still cleared
            // below, so the target CPU's dispatcher waits for us.
            let allowed = per_cpu::affinity_allows_cpu(next_task.cpu_affinity(), cpu_id);
            let rc = if allowed {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.enqueue_from_on_cpu(&dispatch_ref)
                })
                .unwrap_or(-1)
            } else {
                publish_ready_from_current_owner(&dispatch_ref, next_task_id, "affinity_migrate")
            };
            ready_published = rc == 0
                || matches!(
                    next_task.sched_placement(),
                    SchedPlacement::ReadyQueue
                        | SchedPlacement::RemoteWake
                        | SchedPlacement::Migrating
                        | SchedPlacement::Held
                );
        }
    }
    if !ready_published {
        // Only a non-ready task may drop `OnCpu` to `None`; a Ready one is
        // published from a `Waking` token, leaving no Ready+None interval.
        if next_task.is_ready() {
            let rc = publish_ready_from_current_owner(&dispatch_ref, next_task_id, "finish_switch");
            if rc != 0 {
                klog_info!(
                    "SCHED: finish_switch failed final READY publish id={} rc={} placement={:?}",
                    next_task_id,
                    rc,
                    next_task.sched_placement(),
                );
            }
        } else {
            let _ = next_task
                .sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
            if next_task.is_ready() {
                let rc =
                    publish_ready_from_current_owner(&dispatch_ref, next_task_id, "finish_switch");
                if rc != 0 {
                    klog_info!(
                        "SCHED: finish_switch failed raced READY publish id={} rc={} placement={:?}",
                        next_task_id,
                        rc,
                        next_task.sched_placement(),
                    );
                }
            }
        }
    }

    // Cleared only after every still-Ready publication above; pairs with peers'
    // Acquire loads of `on_cpu`.
    next_task.set_on_cpu(false);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });

    // Re-parked rather than released: this runs interrupts-off inside the
    // switch window, and a dead task's release needs a context that can take
    // locks and wait for TLB acks. `drain_previous_task`, called by every path
    // once it leaves the window, is that context.
    let parked = dispatch_ref.into_placement();
    assert!(
        slopos_arch::pcr::defer_previous_task(parked.as_ptr().cast()).is_ok(),
        "previous-task slot was not drained before the next switch"
    );
}

/// Finish the predecessor's switch, if this CPU has one pending. Called
/// interrupts-off from every resume point; see [`execute_task`] for why the
/// handover travels through the PCR rather than a stack frame.
pub(crate) fn finish_pending_switch(cpu_id: usize) {
    let previous = slopos_arch::pcr::take_previous_task().cast::<Task>();
    let Some(node) = NonNull::new(previous) else {
        return;
    };
    finish_switch(cpu_id, TaskRef::from_placement(node));
}

/// Scoped interrupt-enable window for the idle dispatcher's deferred-drop work,
/// which needs IF set where the scheduler loop leaves it clear. Sound only on
/// the CPU's non-migrating idle stack.
pub(crate) struct RestoreInterruptState {
    disable_on_drop: bool,
}

impl RestoreInterruptState {
    #[inline]
    pub(crate) fn open_window() -> Self {
        let was_enabled = slopos_ostd::cpu::x86_64::interrupts::are_interrupts_enabled();
        if !was_enabled {
            slopos_arch::cpu::enable_interrupts();
        }
        Self {
            disable_on_drop: !was_enabled,
        }
    }
}

impl Drop for RestoreInterruptState {
    #[inline]
    fn drop(&mut self) {
        if self.disable_on_drop {
            slopos_arch::cpu::disable_interrupts();
        }
    }
}

/// Release the outgoing dispatch reference from this CPU's deferred slot;
/// `false` if none was parked.
///
/// Called once each switch path has left the interrupts-off window, which is
/// what makes it the right place for a dead task's cleanup.
#[inline]
pub(crate) fn drain_previous_task() -> bool {
    let previous = slopos_arch::pcr::take_previous_task().cast::<Task>();
    let Some(node) = NonNull::new(previous) else {
        return false;
    };

    // Sampled before the release — afterwards the pointer must not be touched.
    let dispatch_ref = TaskRef::from_placement(node);
    let needs_cleanup = matches!(
        dispatch_ref.status(),
        TaskStatus::Terminated | TaskStatus::Zombie
    );
    if needs_cleanup {
        // Not in the switch tail: `destroy_process_vm` takes a lock and waits
        // for cross-CPU TLB acks, neither of which is legal with interrupts
        // off. Runs on the successor's stack, so the dying task's is free (I3).
        let _window = RestoreInterruptState::open_window();
        slopos_ostd::task::run_off_lock(|| {
            super::task::cleanup_current_task_after_switch(&dispatch_ref);
        });
    }
    super::task::task_put(dispatch_ref);
    if needs_cleanup {
        super::task::arm_deferred_reap();
    }
    true
}

/// Reap terminated tasks whose reap was refused while they were dispatch-pinned.
/// Runs under the same idle-stack interrupt-window contract as
/// [`drain_previous_task`].
pub(crate) fn drain_deferred_task_reclaim() {
    let retire = super::task::task_reap_pending();
    let destroy = super::task::task_graveyard_pending();
    if !retire && !destroy {
        return;
    }
    let _restore_interrupts = RestoreInterruptState::open_window();
    if retire {
        slopos_ostd::task::run_off_lock(super::task::task_reap_dispatch_pinned);
    }
    // Destroy after retiring: a retirement can drop the last reference and so
    // park a fresh corpse, and draining second collects it in the same pass.
    if super::task::task_graveyard_pending() {
        slopos_ostd::task::run_off_lock(super::task::task_graveyard_drain);
    }
}

fn schedule_internal() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let irq_flags = cpu::save_flags_cli();

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0 {
        cpu::restore_flags(irq_flags);
        return;
    }

    // Minted here, never inside `run_switch`'s closures: a guard is
    // address-taken, so its frame must be allocated while the *outgoing* task is
    // still published, or the SafeStack reservation is released against the wrong
    // data stack. This frame straddles the switch.
    let idle = Idle::current();
    let current = Current::get();

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_schedule_calls();
    });

    let Some(idle) = idle else {
        cpu::restore_flags(irq_flags);
        return;
    };

    assert_switch_preempt_safe();

    // A CPU with no current task is *not* running idle: it is parked on a
    // pre-heap bootstrap stub, whose first switch takes the `prev = None` path.
    if current.as_ref().is_some_and(|c| c.addr() == idle.addr()) {
        let _ = run_ready_task_from_idle(cpu_id, idle.task());
        // Before re-enabling interrupts: otherwise a timer-driven re-entrant
        // dispatch parks a second reference into the still-occupied slot.
        let _ = drain_previous_task();
        cpu::restore_flags(irq_flags);
        return;
    }

    // Direct task→task handoff, skipping the second `prepare_switch_to` and the
    // third dispatch that going through idle would cost. A failed claim falls
    // through to the idle switch below.
    if let Some(current_ref) = current.as_ref() {
        if let Some(next_ref) = claim_next_task(cpu_id) {
            let next: &Task = &next_ref;
            if next.task_id != current_ref.id() {
                switch_to_claimed_task(cpu_id, current_ref.task(), next_ref);
                // Reached on resume; the tail is for whatever this task
                // displaced then. Before re-enabling interrupts, or a
                // timer-driven dispatch parks into the still-occupied slot.
                finish_pending_switch(cpu_id);
                let _ = drain_previous_task();
                cpu::restore_flags(irq_flags);
                return;
            }
            abandon_claim(cpu_id, next_ref);
        }
    }

    // Re-enqueueing before the switch is the wake-before-switch-complete SMP
    // race; it happens in `finish_switch` once `on_cpu` is cleared.
    switch_from_current_to_idle(
        cpu_id,
        current.as_ref().map(|current| current.task()),
        idle.task(),
    );
    finish_pending_switch(cpu_id);
    let _ = drain_previous_task();
    cpu::restore_flags(irq_flags);
}

pub(crate) fn schedule_from_trap_exit() {
    schedule_internal();
}

pub fn schedule() {
    schedule_internal();
}

pub fn r#yield() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_yields();
    });
    if let Some(current) = Current::get() {
        task_record_yield(current.task());
    }
    schedule();
}

pub fn yield_() {
    r#yield();
}

/// CAS the current task from `Running` to `Blocked` without yielding.
///
/// Called from inside the wait queue's SpinLock, so a `wake_*` taking the same
/// lock observes either an empty queue or the task queued and `Blocked` — never
/// `Running`-and-on-queue. The matching yield is [`yield_blocked_task`], after
/// the lock is dropped.
pub fn mark_current_blocked() -> bool {
    let task_id = slopos_arch::pcr::current_task_id();
    if task_id == INVALID_TASK_ID {
        return false;
    }
    // Stamp the reason explicitly: `try_transition_from` keeps the previous one,
    // and the invariant is `Blocked(Sleep)` ⇔ a deadline is armed.
    super::task::task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        slopos_abi::task::BlockReason::Generic,
    ) == 0
}

/// A task must never deschedule with preemption disabled: the held
/// `SpinLock`/`PreemptMutex` would travel with the blocked task and every
/// contender would spin unpreemptibly until a wake that may itself need the lock.
#[inline]
fn assert_not_blocking_while_atomic() {
    if PreemptGuard::is_active() {
        panic!("scheduler: blocking wait entered with preemption disabled (spinning lock held?)");
    }
}

/// A switch may only occur at the running baseline (`preempt_count == 0`): a held
/// guard would unbalance `switch_context`'s per-task count swap. Sited at the
/// universal chokepoint, so the panic lands at the real caller.
#[inline]
fn assert_switch_preempt_safe() {
    let count = PreemptGuard::count();
    if count != 0 {
        panic!(
            "scheduler: context switch attempted with preempt_count={count} \
             (a SpinLock/PreemptMutex/PreemptGuard is held across a blocking or yielding call)"
        );
    }
}

/// Consume a wake that raced the current task's block path. The task is still
/// executing here, so its placement must end as `OnCpu`; a stale remote-inbox
/// node is harmless, as the owner CPU drops it on the next drain.
///
/// Returns `false` when the task went terminal while parked — it is then left
/// with no scheduler owner rather than restored, because `Running` over a
/// published `Zombie` makes deferred cleanup a permanent no-op.
pub(crate) fn consume_ready_wake_for_current(current: &Current) -> bool {
    let body = current.task();
    if !body.set_status(TaskStatus::Running) {
        unschedule_task(body);
        body.set_sched_placement(SchedPlacement::None);
        return false;
    }
    unschedule_task(body);
    body.set_sched_placement(SchedPlacement::OnCpu);
    true
}

#[cfg(feature = "test-hooks")]
pub fn consume_ready_wake_for_current_for_test(current: &Current) -> bool {
    consume_ready_wake_for_current(current)
}

/// Commit a `Blocked` deschedule: strip `current` from every runqueue, then
/// re-confirm it is still `Blocked`. `true` if the caller may `schedule()`.
///
/// The lost-wakeup guard every blocking primitive must funnel through: a peer
/// may CAS `Blocked → Ready` and enqueue between the caller's Blocked-CAS and
/// the `unschedule_task` here, which just stripped that enqueue. Descheduling
/// anyway would strand the task Ready in no runqueue forever, so on a detected
/// race the wake is consumed and the caller must not deschedule.
///
/// A wake landing after this returns `true` but before the caller's
/// `schedule()` is not lost either: it enqueues via its own `schedule_task`,
/// so the task is dispatched on a later tick rather than stranded.
///
/// Must be called with IRQs disabled, after the caller committed
/// `Running → Blocked`.
pub(crate) fn commit_blocked_deschedule(current: &Current) -> bool {
    let body = current.task();
    unschedule_task(body);
    if body.status() != TaskStatus::Blocked {
        let _ = consume_ready_wake_for_current(current);
        return false;
    }
    true
}

/// Yield a task already CAS-flipped to `Blocked` by [`mark_current_blocked`].
/// Must be called outside any SpinLock.
pub fn yield_blocked_task() {
    let Some(current) = Current::get() else {
        return;
    };
    assert_not_blocking_while_atomic();
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if commit_blocked_deschedule(&current) {
            schedule();
        }
    });
}

/// Yield a task already CAS-flipped to `Blocked` and arm a millisecond-resolution
/// timeout. Carries the same contract as [`yield_blocked_task`]: a wake that
/// raced us since `mark_current_blocked` restores `Running` without descheduling.
pub fn yield_blocked_task_with_timeout(timeout_ms: u32) {
    let Some(current) = Current::get() else {
        return;
    };
    let task_id = current.id();
    assert_not_blocking_while_atomic();
    if !super::sleep::arm_blocked_timeout(task_id, timeout_ms) {
        // Nothing armed to wake the caller's committed `Blocked`: undo the commit
        // so the wait degrades to a yield loop rather than a lost wake.
        report_unarmed_timeout(task_id);
        set_current_runnable();
        yield_();
        return;
    }
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if commit_blocked_deschedule(&current) {
            schedule();
        }
    });
    super::sleep::cancel_sleep(task_id);
}

/// Report the first timed wait that could not arm a deadline. Once per boot: the
/// degraded wait spins, so per-occurrence logs would bury the line that matters.
fn report_unarmed_timeout(task_id: u32) {
    static REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, Ordering::Relaxed) {
        slopos_ostd::klog_info!(
            "SCHED: task {} timed wait could not arm a deadline; wait degraded to a yield loop",
            task_id
        );
    }
}

/// Force the current task back to `Running` and strip any stale runqueue
/// presence, cancelling a committed `Running → Blocked` CAS whose wait condition
/// became observable after the queue's SpinLock was dropped.
///
/// Idempotent against a racing `wake_*`: whichever of that CAS and this store
/// lands last, the caller's condition recheck closes the race, at a cost of at
/// most one extra trip around the wait loop.
pub fn set_current_runnable() {
    let Some(current) = Current::get() else {
        return;
    };
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let _ = consume_ready_wake_for_current(&current);
    });
}

pub fn task_wait_for(task_id: u32) -> c_int {
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    let Some(target_guard) = super::task::task_find_by_id(task_id) else {
        // Already gone — waitpid semantics treat this as success.
        return 0;
    };
    if target_guard.status() == TaskStatus::Invalid {
        return 0;
    }

    let waiter_id = slopos_ostd::cpu::x86_64::pcr::current_task_id();
    if waiter_id == task_id {
        return -1;
    }

    let target_id = target_guard.task_id;
    // `target_guard` held across the wait is what keeps the node — and the exit
    // cell the predicate reads — from being recycled underneath it.
    let target = target_guard.node();

    // The exit-cell re-check makes a colliding `ChildExit` bucket harmless.
    let waited = BUS
        .subscribe(KernelEvent::ChildExit {
            task: TaskSlot(target_id),
        })
        .wait_event(|| slopos_ostd::task::parked_task_has_exited(target));

    if waited.is_err() {
        return -(slopos_abi::Errno::EINTR.raw());
    }
    0
}

pub(crate) fn wake_blocked_task(task: &TaskRef, task_id: u32) -> c_int {
    // `Waking` is the explicit publication token: the wake side must either
    // observe an existing scheduler owner (ready queue / remote inbox /
    // migration) or acquire that token before publishing `TaskStatus::Ready`.
    // `OnCpu` is deliberately not sufficient — it proves the task is executing,
    // not that this producer owns publication.
    //
    // Totality contract: this returns only once the wake is conclusive — Ready
    // published, the task observably no longer Blocked, or exited/invalid. Wake
    // sources are one-shot (a popped deadline, a masked-until-drained edge), so
    // giving up while the task is still Blocked strands the sleeper forever; the
    // transient `OnCpu` and `Waking` windows are waited out with `spin_loop`.
    let body: &Task = task;
    loop {
        if body.is_exited() || (body.status() == TaskStatus::Invalid) {
            return -1;
        }
        if body.status() != TaskStatus::Blocked {
            return 0;
        }
        match body.sched_placement() {
            // Registered but never published: its creator has not finished
            // building it. Terminal rather than a retry — a nascent task has
            // never executed, so it holds no one-shot wake source, and the
            // senders that can name it set the durable `signal_pending` bit,
            // which survives this refusal. Returns 0, not -1: -1 means gone, and
            // `kill` would turn that into ESRCH for a task that exists.
            SchedPlacement::Nascent => return 0,
            SchedPlacement::OnCpu => {
                // `OnCpu` is also the dispatcher's transient claim between a
                // dequeue and Ready->Running; the status check above already
                // filtered the tasks whose claim must not be stolen.
                if !body
                    .sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::Waking)
                {
                    core::hint::spin_loop();
                    continue;
                }
                if task_transition_from(body, TaskStatus::Blocked, TaskStatus::Ready) {
                    // No sleep-queue cancel here: only the owner and the
                    // generation-checked timer path remove entries (a
                    // waker-side cancel raced the owner's next re-arm).
                    return publish_reserved_waking_ready(task, task_id, "oncpu wake");
                }
                let restore = if body.on_cpu() {
                    SchedPlacement::OnCpu
                } else {
                    SchedPlacement::None
                };
                let _ = body.sched_placement_compare_exchange(SchedPlacement::Waking, restore);
                core::hint::spin_loop();
                continue;
            }
            SchedPlacement::None => {
                if !body
                    .sched_placement_compare_exchange(SchedPlacement::None, SchedPlacement::Waking)
                {
                    core::hint::spin_loop();
                    continue;
                }
                if !task_transition_from(body, TaskStatus::Blocked, TaskStatus::Ready) {
                    if body.is_ready() {
                        return publish_reserved_waking_ready(task, task_id, "unblock_task");
                    }
                    let _ = body.sched_placement_compare_exchange(
                        SchedPlacement::Waking,
                        SchedPlacement::None,
                    );
                    core::hint::spin_loop();
                    continue;
                }
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                return publish_reserved_waking_ready(task, task_id, "unblock_task");
            }
            SchedPlacement::Waking => {
                // Single-winner: a duplicate wake either sees Ready already
                // published or waits out the owner's microsecond reservation.
                if task_transition_from(body, TaskStatus::Blocked, TaskStatus::Ready) {
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                if body.is_ready() {
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                core::hint::spin_loop();
                continue;
            }
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::Migrating
            | SchedPlacement::Held => {
                // Scheduler ownership already exists: the state CAS alone makes
                // the existing queue/inbox/migration owner runnable again.
                if task_transition_from(body, TaskStatus::Blocked, TaskStatus::Ready) {
                    return 0;
                }
                core::hint::spin_loop();
                continue;
            }
        }
    }
}

pub fn unblock_task(task: &TaskRef) -> c_int {
    let task_id = task.task_id;
    wake_blocked_task(task, task_id)
}

/// Wake the task named by `task_id`. Resolving the id through the registry keeps
/// the liveness-checked upgrade in one place instead of once per caller. `-1`
/// when the id names no live task.
pub fn unblock_task_id(task_id: u32) -> c_int {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return -1;
    };
    wake_blocked_task(&task, task_id)
}

pub fn scheduler_task_exit_impl() -> ! {
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Scoped, and never held across the `schedule()` below: by then the PCR
    // names the successor and the guard would describe the wrong task.
    let recorded = if let Some(current) = Current::get() {
        task_record_context_switch(Some(current.task()), None, kdiag_timestamp());
        true
    } else {
        false
    };

    if !recorded {
        klog_info!("scheduler_task_exit: No current task on CPU {}", cpu_id);
        schedule();
        // Stay alive to ack shootdown / reschedule IPIs: halting with IF clear
        // lets the BSP's 500 ms NMI watchdog declare this CPU dead.
        slopos_arch::cpu::enable_interrupts();
        slopos_ostd::cpu::x86_64::core::halt_loop();
    }

    if crate::task::task_terminate(u32::MAX) != 0 {
        klog_info!("scheduler_task_exit: Failed to terminate current task");
    }

    // The dying task stays in PCR.current_task until `schedule()` dispatches
    // idle; `on_cpu` blocks reclaim until the switch tail publishes the handoff,
    // and its dispatch reference is released by the successor's
    // `drain_previous_task`.
    schedule();

    klog_info!(
        "scheduler_task_exit: Schedule returned unexpectedly on CPU {}",
        cpu_id
    );
    slopos_arch::cpu::enable_interrupts();
    slopos_ostd::cpu::x86_64::core::halt_loop();
}

// ABI shim for `slopos_ostd::task::switch::register_task_exit_hook`.
extern "sysv64" fn ostd_task_exit_hook() -> ! {
    scheduler_task_exit_impl()
}

/// ABI shim for `register_task_entry_hook`: completes the predecessor's switch
/// on a task's first dispatch, which has no switching frame to return into.
///
/// Runs interrupts-off — the trampoline calls it ahead of its `sti` — so the
/// tail and the release are ordered the same way as on every other resume path.
extern "sysv64" fn ostd_task_entry_hook() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    finish_pending_switch(cpu_id);
    let _ = drain_previous_task();
}

/// Install the OSTD task-exit hook. Must run once at boot, after the scheduler
/// is initialised and before any task can return from its entry function; the
/// OSTD registration is one-shot.
pub fn install_ostd_task_exit_hook<'b>(token: &slopos_ostd::sync::BspToken<'b>) {
    slopos_ostd::task::switch::register_task_exit_hook(token, ostd_task_exit_hook);
    slopos_ostd::task::switch::register_task_entry_hook(token, ostd_task_entry_hook);
    slopos_ostd::panic_recovery::register_oops_task_id_provider(current_task_id);
}

fn deferred_reschedule_callback() {
    if PreemptGuard::is_active() || !is_scheduling_active() {
        return;
    }

    // An involuntary reschedule must never park a task that has committed
    // `Running → Blocked` but is still executing its blocking protocol: it would
    // deschedule with no wake armed and no timeout yet, parking it forever.
    let skip = Current::get().is_some_and(|current| {
        task_has_no_preempt_flag(current.task()) || current.task().status() == TaskStatus::Blocked
    });
    if skip {
        return;
    }

    schedule();
}

pub fn init_scheduler() -> c_int {
    SCHEDULER_ENABLED.store(0, Ordering::Release);
    PREEMPTION_ENABLED.store(SCHEDULER_PREEMPTION_DEFAULT, Ordering::Release);

    per_cpu::init_all_percpu_schedulers();
    // Ensure rather than reset: kthreads parked on deadlines back in `drivers`
    // are still queued when this runs in the `services` phase.
    if !super::sleep::ensure_sleep_queue_allocated() {
        return -1;
    }

    0
}

/// Register the deferred-reschedule callback with OSTD's preempt backend, once
/// from the BSP boot path. Kept separate from [`init_scheduler`] so test-scope
/// reinit can rerun that without contending with OSTD's one-shot callback slot.
pub fn install_reschedule_callback<'b>(token: &slopos_ostd::sync::BspToken<'b>) {
    slopos_ostd::sync::register_reschedule_callback(token, deferred_reschedule_callback);
}

pub fn scheduler_is_enabled() -> c_int {
    SCHEDULER_ENABLED.load(Ordering::Acquire) as c_int
}

/// ID of the task running on this CPU, or 0 when there is none.
///
/// Reads the id `dispatch()` published in the PCR rather than dereferencing
/// `current_task`, so it stays correct while the slot names a pre-heap stub.
pub fn current_task_id() -> u32 {
    match slopos_arch::pcr::current_task_id() {
        INVALID_TASK_ID => 0,
        id => id,
    }
}

/// Id of the task running on this CPU for wait-queue parking, or
/// `INVALID_TASK_ID` when there is none.
///
/// Deliberately *not* [`current_task_id`], which collapses "absent" to 0: a wait
/// queue must tell the two apart or it parks a task that does not exist.
#[inline]
pub fn current_task_handle() -> u32 {
    slopos_arch::pcr::current_task_id()
}

pub fn current_task_pgid() -> u32 {
    Current::get().map_or(0, |c| c.task().pgid())
}

pub fn current_task_sid() -> u32 {
    Current::get().map_or(0, |c| c.task().sid())
}

pub fn current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex> {
    Current::get().and_then(|c| c.task().controlling_tty())
}

pub fn set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool {
    let Some(current) = Current::get() else {
        return false;
    };
    current.task().set_controlling_tty(tty);
    true
}

pub fn clear_session_controlling_tty(session_id: u32, tty: slopos_abi::syscall::TtyIndex) -> usize {
    crate::task::task_clear_controlling_tty_for_session(session_id, tty)
}

pub fn scheduler_set_preemption_enabled(enabled: c_int) {
    let val = if enabled != 0 { 1u8 } else { 0u8 };
    PREEMPTION_ENABLED.store(val, Ordering::Release);
    if val == 0 {
        PreemptGuard::clear_reschedule_pending();
    }
    if val != 0 {
        platform::timer_enable_irq();
    } else {
        platform::timer_disable_irq();
    }
}

pub fn scheduler_is_preemption_enabled() -> c_int {
    PREEMPTION_ENABLED.load(Ordering::Acquire) as c_int
}

pub fn scheduler_timer_tick() {
    // Only the LAPIC timer vector reaches here, so this is the one-shot's own
    // ISR: it fired, and the CPU is back to needing a periodic tick.
    restore_periodic_if_armed();

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Tick-driven so the on-screen log renders even when dispatch is wedged,
    // which is exactly when it is needed.
    if cpu_id == 0 {
        slopos_ostd::fblog::on_timer_tick();
    }

    // Conditional: a reader disables preemption but not interrupts, so this ISR
    // can land inside one, and reporting there tells `synchronize_rcu` that
    // reader has finished while it is still dereferencing.
    slopos_ostd::sync::rcu_note_qs_from_interrupt();

    slopos_ostd::sync::rcu_gp_poll();

    slopos_ostd::sync::rcu_raise_softirq();

    let idle = Idle::current();
    let current = Current::get();
    let running_idle = match (&current, &idle) {
        (Some(current), Some(idle)) => current.addr() == idle.addr(),
        _ => false,
    };

    // Categorised per tick, not per idle-loop iteration, so `idle_ticks` and
    // `total_ticks` stay in lockstep.
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_ticks();
        if running_idle {
            sched.increment_idle_time();
        }
    });

    let preempt_active = PreemptGuard::is_active();

    if preempt_active && !running_idle {
        scheduler_request_reschedule(RescheduleReason::TimerTick);
        return;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });

    wake_due_sleepers(super::sleep::sleep_queue_now_ms());

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0
        || PREEMPTION_ENABLED.load(Ordering::Acquire) == 0
    {
        return;
    }

    // After the inbox drain, so a just-arrived remote wake counts toward this
    // CPU's load rather than being pushed straight back out.
    super::work_steal::periodic_balance();

    let Some(current) = current else {
        return;
    };

    if running_idle {
        mark_preempt_if_ready(cpu_id);
        return;
    }

    // Must precede the arms below: the no-preempt flag, an unspent slice and an
    // empty ready queue each return without a reschedule, so a killed task alone
    // on a CPU would keep executing as a Zombie indefinitely.
    if current.task().is_exited() {
        scheduler_request_reschedule(RescheduleReason::TimerTick);
        return;
    }

    if task_has_no_preempt_flag(current.task()) {
        return;
    }

    if consume_time_slice(current.task()) {
        return;
    }

    if scheduler_ready_count(cpu_id) == 0 {
        reset_task_quantum(current.task());
        return;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_preemptions();
    });
    scheduler_request_reschedule(RescheduleReason::TimerTick);
}

/// Re-enqueue any task observed Ready with no runqueue entry and not on a CPU:
/// every later `unblock_task` no-ops on an already-Ready task and the sleep
/// timer's wake gates on Blocked, so nothing would ever dispatch it again.
///
/// A backstop that should find nothing — its klog line is the telemetry that
/// exposes a residual lost-enqueue race. Transient Ready-and-unlinked windows
/// exist, so the rescue only fires once consecutive sweeps agree.
pub(crate) fn rescue_stranded_ready_tasks() {
    // Walking the registry (manager lock + scratch alloc) from every idle
    // iteration on every CPU would cost hundreds of walks per second at idle; one
    // sweep per cooldown bounds a genuine strand to ~100 ms of extra latency.
    const RESCUE_COOLDOWN_TICKS: u64 = 10;
    static LAST_RESCUE_TICK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let now = slopos_kernel_services::platform::timer_ticks();
    let last = LAST_RESCUE_TICK.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < RESCUE_COOLDOWN_TICKS {
        return;
    }
    if LAST_RESCUE_TICK
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let seq = RESCUE_SWEEP_SEQ
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    CURRENT_RESCUE_SWEEP.store(seq, Ordering::Relaxed);
    super::task::task_for_each_active(rescue_check_task);
}

/// Consecutive-sweep strike tracking: normal creation and wake/dispatch paths
/// have small Ready-off-CPU-unqueued windows, so a rescue is only safe once the
/// observation persists across consecutive sweeps. Slots are keyed by
/// `task_id % N`; a collision at worst delays a rescue by one window.
const RESCUE_STRIKE_SLOTS: usize = 64;
const RESCUE_STRIKE_THRESHOLD: u8 = 3;
static RESCUE_SWEEP_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static CURRENT_RESCUE_SWEEP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static RESCUE_STRIKE_IDS: [core::sync::atomic::AtomicU32; RESCUE_STRIKE_SLOTS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; RESCUE_STRIKE_SLOTS];
static RESCUE_STRIKE_SWEEPS: [core::sync::atomic::AtomicU64; RESCUE_STRIKE_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; RESCUE_STRIKE_SLOTS];
static RESCUE_STRIKES: [core::sync::atomic::AtomicU8; RESCUE_STRIKE_SLOTS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; RESCUE_STRIKE_SLOTS];

/// Count one stranded observation of `task_id`; true once the task has
/// been seen stranded in `RESCUE_STRIKE_THRESHOLD` consecutive sweeps.
fn rescue_strike(task_id: u32) -> bool {
    let seq = CURRENT_RESCUE_SWEEP.load(Ordering::Relaxed);
    let slot = task_id as usize % RESCUE_STRIKE_SLOTS;
    let same_task = RESCUE_STRIKE_IDS[slot].load(Ordering::Relaxed) == task_id;
    let prev_seq = RESCUE_STRIKE_SWEEPS[slot].load(Ordering::Relaxed);
    let consecutive = same_task && prev_seq.saturating_add(1) == seq;

    RESCUE_STRIKE_IDS[slot].store(task_id, Ordering::Relaxed);
    RESCUE_STRIKE_SWEEPS[slot].store(seq, Ordering::Relaxed);

    if !consecutive {
        RESCUE_STRIKES[slot].store(1, Ordering::Relaxed);
        return false;
    }

    let strikes = RESCUE_STRIKES[slot]
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    strikes >= RESCUE_STRIKE_THRESHOLD
}

/// Whether any CPU is still executing `task`, names it as its current task, or
/// holds it in its idle slot. The reap gate and the destructor gate both key on
/// this, so they can never disagree: unhashing a task that satisfies any
/// disjunct frees the kernel stack a CPU is executing on.
///
/// **Each disjunct is load-bearing.** `dispatch()` publishes `PCR.current_task`
/// without setting `on_cpu`, so the second is what makes `CurrentTask` sound; a
/// CPU's idle task is not its current task while a ready task runs there yet must
/// stay reapable-never, so the third is what makes `IdleTask` sound and
/// discharges `SwitchWindow::new`'s dispatch-reference precondition for the idle
/// endpoint of a switch.
#[inline]
pub(crate) fn task_is_dispatch_pinned(task: &Task) -> bool {
    let addr = TaskAddr::of(task);
    task.on_cpu() || task_is_current_on_any_cpu(addr) || crate::per_cpu::is_idle_task(addr)
}

/// Address comparison only: [`TaskAddr`] cannot dereference a foreign CPU's task.
fn task_is_current_on_any_cpu(addr: TaskAddr) -> bool {
    cpu_running_task(addr).is_some()
}

/// The CPU currently executing `addr`, if any.
///
/// Reads each CPU's published current-task slot rather than the task's
/// `last_cpu`, which is only an enqueue-time placement hint. Teardown only, so
/// the scan is off the hot path.
pub(crate) fn cpu_running_task(addr: TaskAddr) -> Option<usize> {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    (0..cpu_count).find(|&cpu_id| TaskAddr::current_of(cpu_id) == Some(addr))
}

fn rescue_check_task(guard: &crate::task::TaskRef) {
    let t: &Task = guard;
    if t.status() != TaskStatus::Ready {
        return;
    }
    // A never-published task is unfinished, not stranded: rescuing one onto a
    // runqueue is the very thing `Nascent` exists to prevent.
    if t.sched_placement() == SchedPlacement::Nascent {
        return;
    }
    if t.on_cpu() || task_is_current_on_any_cpu(TaskAddr::of(t)) {
        return;
    }
    // A non-zero `last_run_timestamp` means some CPU still accounts this task as
    // running; a self-wakeup can make the current task Ready before it yields.
    if t.last_run_timestamp() != 0 {
        return;
    }
    if t.ready_link.is_linked() {
        return;
    }
    if t.inbox_link().is_linked() {
        return;
    }
    let placement = t.sched_placement();
    if placement_is_durable_owner(placement) {
        return;
    }
    if !rescue_strike(t.task_id) {
        return;
    }
    // Local enqueue, never `schedule_task`: this is the recovery path for a task
    // that already lost the normal enqueue. A leaked `Waking` reservation is
    // completed as `Waking`; Ready+Waking is not a durable scheduler owner.
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let enqueue_status = per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        if placement == SchedPlacement::Waking {
            sched.enqueue_waking(guard)
        } else {
            sched.enqueue_local_with_status(guard)
        }
    })
    .unwrap_or(-1);
    if enqueue_status == 0 {
        klog_info!("SCHED: rescuing stranded READY task {}", t.task_id);
    } else if enqueue_status < 0 {
        klog_info!(
            "SCHED: failed to rescue stranded READY task {} (enqueue_status={})",
            t.task_id,
            enqueue_status
        );
    }
}
