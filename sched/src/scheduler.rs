use core::ffi::c_int;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::event::{KernelEvent, TaskSlot};
use slopos_arch::cpu;
use slopos_ostd::KBTreeMap;
use slopos_ostd::sync::BUS;
use slopos_ostd::sync::PreemptGuard;
use slopos_ostd::sync::{KernelSync, LOCK_LEVEL_RESOURCE, SpinLock};

use slopos_ostd::kdiag_timestamp;
use slopos_ostd::klog_info;

use slopos_kernel_services::platform;

// ---------------------------------------------------------------------------
// NMI watchdog: per-CPU alive timestamp (updated every timer tick)
// ---------------------------------------------------------------------------
use core::sync::atomic::AtomicBool;

/// Per-CPU flag set when the idle path armed the LAPIC timer in
/// one-shot mode for the next sleep-queue deadline. The first
/// timer ISR after that consults the flag, restores periodic mode
/// via `platform::timer_restore_periodic`, and clears it. This
/// converges back to the 100 Hz baseline whenever any tick fires
/// (whether the one-shot we armed or any unrelated IRQ that
/// raced it).
static ONESHOT_ARMED: [AtomicBool; slopos_arch::MAX_CPUS] = {
    const FALSE: AtomicBool = AtomicBool::new(false);
    [FALSE; slopos_arch::MAX_CPUS]
};

/// Periodic LAPIC tick interval. Mirrors the constant in
/// `boot/src/boot_drivers.rs::LAPIC_TIMER_PERIOD_MS`. Used by the
/// tickless-idle path to skip arming a one-shot when the next
/// deadline is already at or past the periodic boundary.
const LAPIC_TIMER_PERIOD_MS: u32 = 10;

/// Convert sleep-queue tick delta to a millisecond deadline,
/// rounding up so we never wake one tick early and busy-loop.
#[inline]
fn ticks_to_ms_ceil(delta_ticks: u64) -> u32 {
    let freq = platform::timer_frequency() as u64;
    if freq == 0 {
        return 0;
    }
    // ceil((delta * 1000) / freq). Both the multiply and the round-up
    // add saturate: a caller that passes an enormous delta (e.g. the
    // wraparound result of an already-past deadline) must not overflow
    // here — it simply pins to the `u32::MAX` clamp below.
    let ms = delta_ticks.saturating_mul(1000).saturating_add(freq - 1) / freq;
    if ms > u32::MAX as u64 {
        u32::MAX
    } else {
        ms as u32
    }
}

/// Idle-loop entry helper: peek the soonest sleep-queue deadline
/// and, if it falls inside the current periodic tick window, arm
/// the LAPIC in one-shot mode for it. The next ISR — whether the
/// one we armed or any unrelated IRQ — restores periodic mode in
/// `scheduler_timer_tick`. Idempotent: callable from every idle
/// iteration with no harm if the deadline hasn't changed.
///
/// This is what lets a `KernelIo` task that sleeps for 1 ms
/// actually wake at 1 ms instead of waiting for the next 10 ms
/// periodic boundary.
pub fn arm_tickless_idle_if_due() {
    let now = platform::timer_ticks();
    let Some(deadline) = sleep_queue_next_deadline_ticks(now) else {
        return;
    };
    let delta = deadline.wrapping_sub(now);
    // The soonest deadline may already be due: between a sleeper's
    // deadline passing and the next periodic tick removing it, the idle
    // loop can observe a `deadline <= now`, whose `wrapping_sub` lands in
    // the upper (past) half of the tick space. Such a deadline needs no
    // one-shot — the next periodic tick wakes it — so skip arming rather
    // than convert a near-`u64::MAX` delta to milliseconds.
    if delta == 0 || delta >= (1u64 << 63) {
        return;
    }
    let ms_until = ticks_to_ms_ceil(delta);
    if ms_until == 0 || ms_until >= LAPIC_TIMER_PERIOD_MS {
        // Either already due (next periodic tick will catch it) or
        // farther out than one periodic period — nothing to gain.
        return;
    }
    if platform::timer_program_next_wakeup_ms(ms_until) {
        let cpu_id = slopos_arch::pcr::get_current_cpu();
        if cpu_id < slopos_arch::MAX_CPUS {
            ONESHOT_ARMED[cpu_id].store(true, Ordering::Release);
        }
    }
}

/// Tick-time helper: if this CPU's idle loop previously armed a
/// LAPIC one-shot, restore periodic mode. Called unconditionally
/// at the head of `scheduler_timer_tick`; cheap fast-path when
/// the flag is clear.
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

static WATCHDOG_TICKS: [AtomicU64; slopos_arch::MAX_CPUS] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; slopos_arch::MAX_CPUS]
};

/// Returns the last timer-tick timestamp recorded by `cpu_id`.
/// Used by the cross-CPU watchdog monitor in the scheduler idle loop.
pub fn watchdog_last_tick(cpu_id: usize) -> u64 {
    if cpu_id < WATCHDOG_TICKS.len() {
        WATCHDOG_TICKS[cpu_id].load(Ordering::Relaxed)
    } else {
        0
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
pub use super::sleep::{block_current_task_with_timeout, cancel_sleep, sleep_current_task_ms};
use super::sleep::{reset_sleep_queue, sleep_queue_next_deadline_ticks, wake_due_sleepers};
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_USER_MODE, Task, TaskPriority, TaskRef, TaskStatus, task_put,
    task_record_context_switch, task_record_yield, task_set_state, task_transition_from,
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
/// Global scheduler-enabled flag. `pub(crate)` so the
/// `test_hermetic::SchedulerEnabledFlag` HermeticState impl can
/// snapshot/restore it. External code should go through
/// `set_scheduler_enabled` / `scheduler_is_enabled`.
pub(crate) static SCHEDULER_ENABLED: AtomicU8 = AtomicU8::new(0);
static PREEMPTION_ENABLED: AtomicU8 = AtomicU8::new(SCHEDULER_PREEMPTION_DEFAULT);

pub(crate) fn set_scheduler_enabled(enabled: bool) {
    let value = if enabled { 1 } else { 0 };
    SCHEDULER_ENABLED.store(value, Ordering::Release);
}

#[inline]
pub(crate) fn is_scheduling_active() -> bool {
    SCHEDULER_ENABLED.load(Ordering::Acquire) != 0
        && PREEMPTION_ENABLED.load(Ordering::Acquire) != 0
}

use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
use slopos_mm::process_vm::{
    process_vm_activate_by_handle, process_vm_get_cr3_phys_by_handle,
    process_vm_sync_kernel_mappings, unpack_process_vm_handle,
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

// Recovery depth and panic in-flight depth are task-scoped state whose
// live values sit in per-CPU PCR slots (read directly by the panic
// handler and `AbortOnUnwind`). Both must be saved into the task on every
// switch-out and reinstalled on switch-in: an unwinding task runs
// interrupts-on and can migrate, and a leaked non-zero in-flight count on
// the departed CPU would make any later `AbortOnUnwind` drop there abort
// a healthy kernel.
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

/// Atomically install `task` as the task running on `cpu_id`.
///
/// Single source of truth for "which task is on this CPU":
///   - `PCR.current_task`  — SafeStack reads via `gs:[CURRENT_TASK]`
///     on every instrumented function prologue.
///   - `PCR.syscall_pid`   — `copy_from_user` page-dir resolution.
///   - `Task.state`        — (Ready | Running) → Running.
///   - `sched.total_switches` — observability counter.
///
/// # Preconditions
///
/// - `cpu_id == slopos_arch::pcr::get_current_cpu()`.  SafeStack only
///   reads the *local* PCR via GS; cross-CPU dispatch would write
///   the wrong PCR and corrupt the remote CPU's unsafe-SP resolution.
/// - `task` is non-null, is registry-owned (or is a bootstrap
///   stub), and has its `unsafe_stack_sp` primed.
/// - Caller runs with preemption disabled OR inside the
///   interrupts-off context-switch window.
#[inline]
pub(crate) fn dispatch(cpu_id: usize, task: &Task) {
    debug_assert!(
        cpu_id == slopos_arch::pcr::get_current_cpu(),
        "dispatch() must run on the target CPU (SafeStack slot is gs-local)"
    );

    // SafeStack reads this on every instrumented prologue. The id and the
    // priority ride along so callers that want only "who is running" or "would
    // a newcomer preempt it" never dereference the task — least of all from
    // another CPU, where the switch tail may be destroying it.
    slopos_arch::pcr::set_current_task_typed(
        core::ptr::from_ref(task).cast_mut(),
        task.task_id,
        published_priority(task),
    );
    restore_live_recovery_depth(task);

    // Keep PCR.syscall_pid in sync so copy_from_user always resolves
    // the correct address space, even after preemption.
    let pid = task.process_id;
    // Safe surface: the local-CPU PCR lookup folds the GS resolution
    // behind a table read; the atomic store on `syscall_pid` is
    // race-free under the dispatch IRQs-off + on-this-CPU window.
    if let Some(pcr) = slopos_arch::pcr::current_pcr_local() {
        pcr.syscall_pid.store(pid, Ordering::Release);
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_switches();
    });

    // Lifecycle state transition — (Ready|Running) → Running.
    //
    // A task entering `dispatch` MUST be Ready or Running. Anything
    // else is an invariant violation: a Blocked task in a runqueue
    // means a wake path enqueued without first transitioning to
    // Ready, or a state transition raced the dispatcher. Either is
    // a bug we want surfaced loudly in debug, not silently coerced.
    let current_status = task.status();
    debug_assert!(
        matches!(current_status, TaskStatus::Ready | TaskStatus::Running),
        "dispatch: invariant broken — task {} in unexpected state {:?}",
        task.task_id,
        current_status,
    );
    if !matches!(current_status, TaskStatus::Ready | TaskStatus::Running) {
        // Production fallback: skip dispatch and let the caller pick
        // a different task. The pre-Phase-1 code logged + coerced to
        // Running, which produced the `0xdfdedddcdbdad9d8`-shape page
        // faults in CI (a wait-protocol-half-state task forced into
        // Running runs with a corrupted user-mode RIP). Skipping is
        // the safe move.
        return;
    }
    let _ = task.sched_placement_compare_exchange(SchedPlacement::None, SchedPlacement::OnCpu);
    task.set_status(TaskStatus::Running);
}

/// Cross-crate, test-only entry point into [`dispatch`] for hermetic
/// fixtures that live outside `slopos-sched` (notably
/// `core/src/syscall/tests.rs`). Carries the same safety preconditions
/// as [`dispatch`] — only invoke from a fixture that has primed
/// `unsafe_stack_sp` and is running with preemption disabled on the
/// target CPU.
///
/// Takes an id rather than a pointer so the registry guard that keeps the
/// task alive across the dispatch is held here, where the dispatch happens,
/// instead of being something the caller has to remember to hold.
/// Returns whether the id still named a live task.
#[cfg(feature = "test-hooks")]
pub fn dispatch_task_for_test(cpu_id: usize, task_id: u32) -> bool {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return false;
    };
    dispatch(cpu_id, &task);
    true
}

/// Whether a task name is one `create_idle_task_for_cpu` would have produced.
///
/// Scheduler policy, so it lives beside the publisher rather than on the task:
/// what makes a task "the idle task" is that a CPU installed it as one, and
/// this is the shape check on the name that install stamps.
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

/// Install `task` as `cpu_id`'s idle task.  Writes `PCR.idle_task` —
/// the single source of truth for "idle task on CPU N".
/// Called once per CPU by `create_idle_task_for_cpu`.
///
/// The "is this really an idle task" screen lives here, at the one-shot
/// publisher, rather than on every dispatch: the slot has a single production
/// writer, so a property checked once at install holds for every later reader.
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

fn switch_to_kernel_address_space() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    tlb::enter_lazy_tlb(cpu_id);
    // Safe-wrapper entry: KERNEL_VM_SPACE is the canonical kernel
    // master PML4; the kernel-half invariant is trivially satisfied
    // when we're switching onto the master itself.
    kernel_vm_space().lock().activate_kernel_master();
}

/// Pre-switch housekeeping: FPU save(prev), TLB flush, FS_BASE, TSS RSP0,
/// CR3 load, FPU restore(next).  Replaces the big unsafe block that lived
/// inside the old `execute_task`.
///
/// # Caller invariants
///
/// Every stage reaches its task through one of the two switch windows, which
/// are mintable only inside `run_switch`. Must still be called with interrupts
/// disabled and only by the scheduler hot path, so the FPU / TLB / FS_BASE /
/// TSS / CR3 sequencing matches the dispatch state machine.
fn prepare_switch_to(
    cpu_id: usize,
    prev_window: Option<&crate::task_struct::Switching<'_>>,
    next_window: &crate::task_struct::Switching<'_>,
) {
    let next = next_window.task();
    // Cache the active XCR0 mask once for the whole switch — the OSTD
    // `fpu_xsave` / `fpu_xrstor` primitives take it as a parameter
    // (the static is set at boot by `slopos_ostd::cpu::x86_64::xsave::init`).
    let xcr0 = active_xcr0();

    // --- Save/restore per-CPU PCR user-mode round-trip slots ---
    slopos_ostd::task::switch::pcr_round_trip_swap(prev_window, next_window);

    // --- FPU save (prev) ---
    if let Some(prev_window) = prev_window {
        // The witness discharges the exclusive-access half of the contract and
        // the scheduler's IRQs-off window the ordering half. This is a
        // switch-out, so the save also hands the register file back — which is
        // what lets the restore below pass its own precondition.
        prev_window.task().fpu_save_current(prev_window, xcr0);
    }

    // --- TLB / address-space switch ---
    let is_user_mode = next.flags & TASK_FLAG_USER_MODE != 0;
    let new_pid = if is_user_mode {
        next.process_id
    } else {
        INVALID_PROCESS_ID
    };
    // No address-space lookup anywhere on this path. The task carries a
    // handle that names its slot outright, the shootdown key is that
    // slot, and the outgoing side reads back the key this CPU stored when
    // it switched in.
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

    // --- FS_BASE ---
    let fs = if is_user_mode {
        let raw = next.fs_base();
        if raw == 0 || slopos_abi::addr::VirtAddr::is_canonical(raw) {
            raw
        } else {
            0
        }
    } else {
        0
    };
    slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, fs);

    // --- TSS RSP0 ---
    let kernel_rsp = if is_user_mode {
        match next.kernel_stack_top {
            kst if kst != 0 => kst,
            _ => kernel_stack_top() as u64,
        }
    } else {
        kernel_stack_top() as u64
    };
    platform::gdt_set_kernel_rsp0(kernel_rsp);

    // --- CR3 ---
    //
    // Routes through `VmSpace::activate`, the only sanctioned CR3 write
    // path. `activate` lazily resyncs kernel-half from
    // the master PML4 (KERNEL_MASTER_GEN bump propagation), fires the
    // registered `CursorUnmapHook::on_activate` callback, and writes
    // CR3 with PCID + NOFLUSH=1. Cold-path PCID rotation is OSTD's
    // concern; consumers see only the activate call.
    //
    // `_ = cpu_id;` — `mmu::select_cr3` plumbing is unreachable from
    // this hot path; the per-CPU ASID pool retires when the legacy
    // paging surface deletes.
    let _ = cpu_id;
    if let Some(handle) = next_vm {
        process_vm_sync_kernel_mappings(new_pid);
        // Scheduler-invariant safe entry: IRQs disabled by caller,
        // kernel-half maintained by `activate`'s internal resync.
        // Falls back to kernel master if the slot holds no VmSpace or has
        // been rebound to another process since this task was built.
        if !matches!(process_vm_activate_by_handle(handle), Ok(true)) {
            kernel_vm_space().lock().activate_kernel_master();
        }
    } else {
        // Idle / kernel-only task installs the kernel master.
        kernel_vm_space().lock().activate_kernel_master();
    }

    // --- FPU restore (next) ---
    // Safe wrapper: `&FpuState` keeps the buffer read-only borrowed;
    // XRSTOR64 only reads. Scheduler upholds Inv. 8 (no concurrent
    // mutator on another CPU).
    next_window.task().fpu_restore_to_cpu(next_window, xcr0);
}

/// Validate that the idle task's switch_ctx has a sane RIP (in kernel .text)
/// and RSP (above USER_SPACE_TOP).
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

    // Validate the idle context BEFORE publishing it as current_task.
    // Otherwise, other CPUs could observe current_task pointing at an
    // unusable idle context if validation fails.
    if !ensure_idle_switch_ctx_valid(idle_task) {
        klog_info!(
            "SCHED: CPU {} cannot recover idle switch_ctx for task {}",
            cpu_id,
            idle_task.task_id
        );
        return;
    }

    // The dispatch/switch span mutates current_task, PCR, and per-task
    // context as one multi-step transition; an unwind through it would
    // leave the CPU's scheduling state torn. The guard stays armed inside
    // the descheduled frame while another task runs — harmless, since a
    // descheduled frame cannot unwind until its task resumes.
    let switch_abort_guard = slopos_ostd::panic::AbortOnUnwind::new();
    if let Some(current) = current {
        save_live_recovery_depth(current);
    }

    // Scheduler hot path: IRQs disabled by caller; the safe-fn shims
    // for `prepare_switch_to` and `switch_registers` capture the
    // per-call validity through the now-installed dispatch target.
    // `run_switch` is the sole `SwitchWindow` construction site: OSTD proves
    // the exclusivity precondition and lends the witness in. The switch itself
    // stays inside the window, so the register-state pointers never outlive
    // the proof that authorised them. `dispatch` runs inside it because
    // publishing the incoming task also swaps the SafeStack data stack, and
    // the window's own frame has to be allocated before that happens.
    slopos_ostd::task::run_switch(
        current,
        idle_task,
        || {
            dispatch(cpu_id, idle_task);
            slopos_ostd::sync::rcu_note_qs();
        },
        |prev_window, next_window| {
            // prepare_switch_to handles FPU, TLB, FS_BASE, TSS, CR3
            prepare_switch_to(cpu_id, prev_window, next_window);
            let prev_ctx =
                prev_window.map_or(core::ptr::null_mut(), |w| w.task().switch_ctx_ptr(w));
            let next_ctx = next_window.task().switch_ctx_ptr(next_window).cast_const();
            // switch_context swaps the per-task preempt-count with the PCR around
            // the register switch (migration-safe accounting), then switches.
            switch_context(prev_ctx, next_ctx);
        },
    );
    // NOTE: code here runs when the TASK resumes (not on idle path).
    // All post-switch cleanup happens in run_ready_task_from_idle
    // after execute_task returns — that IS the idle resumption point.
    switch_abort_guard.disarm();
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

/// True if `new` has strictly higher priority (numerically lower) than
/// the task currently running on `cpu`. Idle/null current always
/// counts as lower-priority so a wake onto an idle CPU triggers an
/// immediate reschedule on the next IRQ return.
fn placement_is_durable_owner(placement: SchedPlacement) -> bool {
    matches!(
        placement,
        SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating
    )
}

fn task_has_durable_owner(task: &Task) -> bool {
    placement_is_durable_owner(task.sched_placement())
}

/// The value `dispatch()` publishes in `PCR.current_task_priority` for `task`.
///
/// A task with no readable priority — a bootstrap stub, a null — publishes
/// [`PRIORITY_NONE`], so a CPU parked on one always loses the preemption
/// comparison.
#[inline]
fn published_priority(task: &Task) -> u8 {
    Some(task.priority).map_or(slopos_arch::pcr::PRIORITY_NONE, |p| p.as_u8())
}

/// Whether `new` should preempt whatever `cpu` is running.
///
/// Reads the priority `cpu` published in its own PCR rather than dereferencing
/// its `current_task`. The dereference raced that CPU's `drain_previous_task`,
/// which reclaims and releases the outgoing dispatch reference and can run the
/// task's allocator-heavy destructor — so the priority read could land in freed
/// memory. A published scalar cannot dangle.
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
        | SchedPlacement::Migrating => return 0,
        SchedPlacement::Nascent => return -1,
        SchedPlacement::None | SchedPlacement::Waking => {}
    }

    // Honor affinity in the fallback too: a strict-pinned task must never be
    // enqueued on a CPU outside its mask just because a permitted CPU's enqueue
    // momentarily raced. Try the local CPU (if permitted), then every other
    // permitted CPU (waking it via IPI). Only if no permitted CPU accepts the
    // task do we relax onto any CPU — Linux's `select_fallback_rq` last resort,
    // which in practice means the permitted CPUs are all offline. `affinity == 0`
    // permits every CPU, so the common case walks all CPUs as before.
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
            | SchedPlacement::Migrating => return 0,
            SchedPlacement::Nascent => return -1,
            SchedPlacement::None | SchedPlacement::Waking => {}
        }
    }

    // Last resort: no permitted CPU accepted the task. Relax affinity onto any
    // CPU rather than strand a runnable task, and log the relaxation.
    for cpu_id in 0..cpu_count {
        if per_cpu::affinity_allows_cpu(affinity, cpu_id) {
            continue; // already attempted above
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
            | SchedPlacement::Migrating => return 0,
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
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake | SchedPlacement::Migrating => {
                return 0;
            }
            SchedPlacement::Waking => return publish_reserved_waking_ready(task, task_id, context),
            // Never published, so there is nothing to re-publish. A wake path
            // that reached a nascent task is a bug in its caller, not a race
            // to spin on.
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
            // Raw re-publish of a migrating task: the queue parks its own
            // membership reference and the caller keeps whatever handle it
            // carried.
            SchedPlacement::Migrating => sched.enqueue_migrated_borrowed(task),
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake => 0,
            // `from` is a reservation the caller already holds; a nascent task
            // has none, so there is nothing to transfer into a queue.
            SchedPlacement::Nascent => -1,
        });

        if result != Some(0) {
            return publish_ready_fallback(task);
        }
        // Self-CPU reschedule: the remote-CPU path below sends an IPI;
        // for the local path we set the per-CPU preempt-pending flag so
        // `scheduler_handoff_on_trap_exit` (called from the trap-exit
        // path / idle loop) dispatches the new task before HLT re-engages.
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
            SchedPlacement::Migrating | SchedPlacement::Nascent => -1,
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

/// A publication reservation: the placement to publish `from`, plus the
/// placement to restore if the publication fails.
struct Reservation {
    from: SchedPlacement,
    /// `Some` only when this reservation moved the task out of `Nascent` and
    /// therefore owes it a rollback.
    restore_nascent: bool,
}

/// Reserve scheduler ownership for an explicit publication.
///
/// A never-published task must leave `Nascent` through a CAS before anything
/// downstream sees it, so the queue machinery never has to reason about the
/// state at all. Returns `None` if the task is nascent and someone else won the
/// promotion — that winner owns the publication.
///
/// A reservation that took a task out of `Nascent` **must** be released by
/// [`release_publication`] if the publication does not complete. Leaving a task
/// parked in `Waking` after a failed publish is not merely a leak: `Waking` is a
/// state `wake_blocked_task` publishes from, so it would hand any later signal
/// exactly the half-built task that `Nascent` exists to protect.
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
                // Lost the promotion: whoever won it owns the publication.
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

/// Put a freshly created task into the placement a *published, then blocked*
/// task has: owned by nothing, but past construction.
///
/// Tests that exercise wake and inbox machinery need a task in that state, and
/// a task straight out of `task_create` is not in it — it is `Nascent`, which
/// every wake path deliberately refuses. Reaching the state for real means
/// publishing the task and letting it run and block, which those tests are not
/// about. Returns false if the task is gone or had already left `Nascent`.
#[cfg(feature = "test-hooks")]
pub fn clear_nascent_for_test(task_id: u32) -> bool {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return false;
    };
    task.sched_placement_compare_exchange(SchedPlacement::Nascent, SchedPlacement::None)
}

/// Schedule a **newly created** task (fork, spawn, exec).
///
/// Fork-placement slow path: bypasses `last_cpu` and finds the globally
/// idlest CPU with round-robin tie-breaking, spreading new processes
/// across CPUs at creation time.
///
/// Regular wakeups from sleep/block should use [`schedule_task()`] instead,
/// which preserves cache affinity by preferring the last CPU.
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
///
/// This is SlopOS's `wake_up_new_task()` equivalent: reserve scheduler
/// placement first (`Waking`), publish `TaskStatus::Ready`, then transfer the
/// reservation into a runqueue or remote inbox.
pub fn publish_new_task(task: &TaskRef) -> c_int {
    let body: &Task = task;
    // The sole sanctioned exit from `Nascent`: this is what makes a
    // never-published task schedulable, and every other path refuses the
    // transition. `None` stays accepted for a task that was unscheduled back to
    // no owner and is being re-published.
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
    body.set_status(TaskStatus::Ready);
    let rc = schedule_task_from_placement(task, SchedPlacement::Waking, true);
    if rc != 0 {
        // Roll back to whichever unpublished state we reserved from, so a
        // failed publication leaves "never published" still spelled `Nascent`
        // and a retry — or a later `task_terminate` — sees a coherent state.
        let _ = body.sched_placement_compare_exchange(SchedPlacement::Waking, reserved_from);
        body.set_status(previous_status);
    }
    rc
}

pub fn unschedule_task(task: &Task) -> c_int {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.remove_task(task);
        });
    }

    0
}

/// Re-place a task after its CPU-affinity mask changes so the new mask actually
/// governs where it runs — mirroring Linux `sched_setaffinity`, where stamping
/// the mask is followed by a migrate if the task is no longer allowed on its
/// current CPU. Idempotent when the task is still permitted where it is.
///
/// - A **Ready** task sitting in a now-disallowed CPU's queue is pulled out
///   (`unschedule_task`, which reclaims its parked ref) and re-scheduled onto a
///   permitted CPU (`schedule_task` → `select_target_cpu`, affinity-honoring).
/// - A **Running** task on a now-disallowed CPU is asked to reschedule; the
///   switch-out tail then repatriates it (a local flag for the calling CPU, a
///   reschedule IPI for a remote one).
/// - A **blocked / remote-inbox / migrating** task needs nothing: its next wake
///   or drain re-selects via `select_target_cpu`, which now honors the mask.
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
        SchedPlacement::None
        | SchedPlacement::Waking
        | SchedPlacement::RemoteWake
        | SchedPlacement::Migrating
        | SchedPlacement::Nascent => {}
    }
}

/// Whether `pid` is an id the process-VM allocator can have issued.
///
/// Nothing downstream indexes an array by process id any more, so this is
/// a statement about the allocator rather than a bound anything depends
/// on. It stays because it is the property a churn test can assert
/// directly on a freshly built task, without dispatching it — and because
/// an id outside this range would mean the allocator handed out something
/// it has no record of.
pub(crate) fn dispatch_pid_ok(pid: u32) -> bool {
    pid == INVALID_PROCESS_ID || pid <= slopos_mm::memory_layout_defs::MAX_PROCESS_ID
}

/// Unified task execution for all CPUs.
/// Handles switch_ctx validation, prepare_switch_to, and switch_registers.
fn execute_task(cpu_id: usize, from_task: Option<&Task>, to_task: &Task) {
    let pid = to_task.process_id;
    let to_id = to_task.task_id;

    // The structural refusal for a task whose address space is gone is
    // the CR3 check below, which asks the address space itself rather
    // than reasoning about the id's numeric value. This is a tripwire on
    // the allocator, not a guard the dispatch depends on.
    debug_assert!(
        dispatch_pid_ok(pid),
        "the process-VM allocator issued an id outside its own space"
    );

    // Validate switch_ctx.rip — must be in kernel .text (the OSTD
    // task-entry trampoline / user_task_first_run wrapper / a
    // schedule resume point all live there).
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
        return;
    }
    // RSP must be in kernel space (above USER_SPACE_TOP)
    if rsp < USER_SPACE_TOP {
        klog_info!(
            "SCHED: refusing to dispatch task {} with switch_ctx.rsp=0x{:x} below kernel space",
            to_id,
            rsp,
        );
        let _ = crate::task::task_terminate(to_id);
        return;
    }

    // Validate the address space of a task that has one. Two distinct
    // refusals, both fatal to the task: the handle no longer resolves —
    // the slot it was built against now belongs to a different process —
    // or it resolves to a slot with no address space, destroyed or never
    // created. Switching into either would run this task in someone
    // else's page tables or in none.
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
                return;
            }
            Err(err) => {
                klog_info!(
                    "SCHED: refusing to dispatch task {} (pid {}): address space handle {:?}",
                    to_id,
                    pid,
                    err,
                );
                let _ = crate::task::task_terminate(to_id);
                return;
            }
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, Some(to_task), timestamp);

    // Mark task as physically on this CPU. The dispatcher's re-enqueue
    // check at run_ready_task_from_idle (below) reads this flag before
    // dispatching a task — if it's still on_cpu on another CPU, the
    // task is requeued rather than dispatched twice.
    //
    // The on_cpu-through-switch span mutates current_task, PCR, and
    // per-task context as one multi-step transition; an unwind through it
    // would leave the CPU's scheduling state torn. The guard stays armed
    // inside the descheduled frame while another task runs — harmless,
    // since a descheduled frame cannot unwind until its task resumes.
    let switch_abort_guard = slopos_ostd::panic::AbortOnUnwind::new();
    to_task.set_on_cpu(true);
    if let Some(from_task) = from_task {
        save_live_recovery_depth(from_task);
    }

    // Scheduler hot path: IRQs disabled by caller; switch_ctx pointers
    // were freshly validated above, both safe shims accept the
    // raw-task arguments and route through the OSTD safe-fn surfaces.
    // `run_switch` is the sole `SwitchWindow` construction site: OSTD proves
    // the exclusivity precondition and lends the witness in. The switch itself
    // stays inside the window, so the register-state pointers never outlive
    // the proof that authorised them. `dispatch` runs inside it because
    // publishing the incoming task also swaps the SafeStack data stack, and
    // the window's own frame has to be allocated before that happens.
    slopos_ostd::task::run_switch(
        from_task,
        to_task,
        || {
            // Single source-of-truth install: writes PCR.current_task
            // (SafeStack slot), PCR.syscall_pid, task.state = Running, and
            // the per-CPU switch counter in one place.
            dispatch(cpu_id, to_task);
            slopos_ostd::sync::rcu_note_qs();
        },
        |prev_window, next_window| {
            // prepare_switch_to handles FPU, TLB, FS_BASE, TSS, CR3
            prepare_switch_to(cpu_id, prev_window, next_window);
            let prev_ctx =
                prev_window.map_or(core::ptr::null_mut(), |w| w.task().switch_ctx_ptr(w));
            let next_ctx = next_window.task().switch_ctx_ptr(next_window).cast_const();
            // switch_context swaps the per-task preempt-count with the PCR around
            // the register switch (migration-safe accounting), then switches.
            switch_context(prev_ctx, next_ctx);
        },
    );
    // Runs when the switched-out task resumes on a later dispatch.
    switch_abort_guard.disarm();
}

/// Dispatch one ready task from this CPU's idle context, returning whether one
/// ran.
///
/// `idle_task` is the borrow both callers mint from the PCR idle slot, so it is
/// canonical by construction — there is no second candidate to reconcile
/// against, and no pointer to re-validate.
pub(crate) fn run_ready_task_from_idle(cpu_id: usize, idle_task: &Task) -> bool {
    // Flush cross-core wakes parked in this CPU's remote inbox into the ready
    // queue *before* the pick. Every dispatch entry point funnels through here
    // — the idle loop, the timer-preempt handoff, and crucially the
    // reschedule-IPI trap-exit path, whose only job is to run a just-pushed
    // remote wake but which would otherwise pick from ready queues that cannot
    // yet see the inbox task. This mirrors Linux running `sched_ttwu_pending()`
    // before `pick_next_task`. `cpu_id` is the current (owning) CPU, satisfying
    // `drain_remote_inbox`'s single-consumer contract; draining an empty inbox
    // is a no-op, so this is idempotent with the idle-loop and tick drains.
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.drain_remote_inbox());

    // The dequeue hands over the queue's owning reference rather than releasing
    // it, so the task is pinned for the whole dispatch window below — including
    // the unbounded `on_cpu` spin. It becomes this CPU's dispatch reference.
    let Some(dispatch_ref) =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority()).flatten()
    else {
        return false;
    };
    // The dequeue's own reference is what makes this borrow sound — it is held
    // across the whole window below, including the unbounded `on_cpu` spin — so
    // there is nothing left to validate about the pointer it came from.
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
        return false;
    }

    if next_task.is_exited() || !next_task.is_ready() {
        let _ =
            next_task.sched_placement_compare_exchange(SchedPlacement::OnCpu, SchedPlacement::None);
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        super::task::task_put(dispatch_ref);
        return false;
    }

    // A runnable task may become visible on this CPU while its prior CPU is
    // still completing the switch-out tail. The dequeue already transferred
    // scheduler placement to this CPU, so wait for the prior CPU's Release
    // store instead of publishing a second queue membership.
    while next_task.on_cpu() {
        core::hint::spin_loop();
    }

    // Single-winner dispatch claim: only one CPU may run a READY task.
    // If another CPU already claimed it (or state changed), drop this dequeue.
    let next_task_id = next_task.task_id;
    if task_set_state(next_task_id, TaskStatus::Running) != 0 {
        // Lost the claim — but if the task is *still* Ready we hold its only
        // scheduler placement, so dropping the dequeue would strand it READY.
        // Put it back; a claimed (Running/exited) task is the winner's
        // responsibility and is correctly dropped.
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
        return false;
    }

    // Publish on_cpu with the dispatch claim, before the reference bump and
    // the validation in execute_task. A concurrent task_terminate() must
    // observe this task as on-CPU for the whole dispatch so it defers cleanup
    // (kernel-stack free) to this CPU's post-switch path rather than freeing
    // the stack while this CPU is about to run on it — a use-after-free.
    next_task.set_on_cpu(true);

    execute_task(cpu_id, Some(idle_task), next_task);

    let timestamp = kdiag_timestamp();
    task_record_context_switch(Some(next_task), Some(idle_task), timestamp);

    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    switch_to_kernel_address_space();
    super::task::cleanup_current_task_after_switch(&dispatch_ref);

    // Re-enqueue the task if it was preempted (Running) or already
    // woken (Ready) before its yield completed. Keep `on_cpu=true`
    // until after any Ready task has a queue/inbox membership. The
    // ordering invariant: a peer may observe a task as runnable while
    // it is still completing a switch-out, but it must never observe
    // Ready + off-CPU + unqueued. Peer dispatchers that pick such a
    // task hit the `on_cpu` guard above and requeue it.
    //
    // The Ready case covers the self-wakeup window: a wake from the
    // timer ISR transitions state Blocked→Ready and routes through
    // `enqueue_local` on this CPU; the in-progress block path then
    // `unschedule_task`s the entry, leaving the task Ready but in no
    // runqueue. Re-enqueueing here keeps it schedulable across the yield.
    //
    // task_wait_for goes through WaitQueue::wait_event, which
    // performs the Running→Blocked transition while the queue is
    // already holding our wait node — by the time `schedule()`
    // dispatches a peer, our state is committed Blocked and the
    // peer's `wake_one` will find us on the queue.
    //
    // Blocked/Zombie/Terminated tasks are NOT re-enqueued — they'll
    // be woken by their respective event paths. This runs after the
    // context is saved but before on_cpu is cleared, so there is no
    // Ready/off-CPU/unlinked window.
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
            // Repatriate a task whose affinity mask no longer permits this CPU
            // (it changed while the task ran here) instead of re-queueing it
            // locally forever. `publish_ready_from_current_owner` transitions the
            // OnCpu owner token to Waking and routes through `select_target_cpu`
            // (now affinity-honoring) to a permitted CPU's inbox + reschedule IPI
            // — the same proven publisher the raced-wake fallback below uses;
            // single-membership CAS prevents a second queue entry, and `on_cpu`
            // is still cleared below so the target dispatcher waits for us.
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
                );
        }
    }
    if !ready_published {
        // Close the final switch-out wake race without wake-side spinning and
        // without a Ready+None interval. If the task is Ready, transfer the
        // current CPU's placement ownership to Waking and publish from that
        // token. Only non-ready tasks may drop OnCpu to None.
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

    // Context switch OUT is complete and every still-Ready task has been
    // published to a runqueue (or was already in a remote inbox). Only now
    // clear on_cpu so peer CPUs may claim it. Pairs with peer Acquire loads
    // of `on_cpu`.
    next_task.set_on_cpu(false);

    // Park the owning dispatch reference in the CPU-local deferred slot. Its
    // successor reclaims and releases it after this IRQ-off switch window has
    // ended and while executing on the idle stack.
    let parked = dispatch_ref.into_placement();
    assert!(
        slopos_arch::pcr::defer_previous_task(parked.as_ptr().cast()).is_ok(),
        "previous-task slot was not drained before the next switch"
    );

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });

    true
}

/// Scoped interrupt-enable window for the idle dispatcher's deferred-drop
/// work. The scheduler loop normally returns from `sti; hlt; cli` with IF
/// clear. This code runs on the CPU's non-migrating idle stack, so it may
/// open the required interruptible drop window and then restore the loop's
/// prior state without an IRQ-driven migration moving that state to another
/// CPU.
struct RestoreInterruptState {
    disable_on_drop: bool,
}

impl RestoreInterruptState {
    #[inline]
    fn open_window() -> Self {
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

/// Release the outgoing dispatch reference from this CPU's deferred slot.
/// Returns whether a reference was present.
#[inline]
pub(crate) fn drain_previous_task() -> bool {
    let previous = slopos_arch::pcr::take_previous_task().cast::<Task>();
    let Some(node) = NonNull::new(previous) else {
        return false;
    };

    // Reclaim the parked dispatch reference and release it. The task still holds
    // its own existence reference until it is reaped, so this is a bare atomic
    // decrement — safe here with interrupts disabled and no lock discipline —
    // and `task_put` parks rather than destroys should it ever be the last.
    //
    // A terminated task's allocator-heavy reap is deliberately not run in this
    // switch tail; arming the latch defers it to the idle dispatcher, which
    // frees the outgoing stack later on the successor's stack with interrupts
    // enabled and no lock held. Sampled before the release: afterwards the
    // pointer must not be touched.
    let dispatch_ref = TaskRef::from_placement(node);
    let terminated = dispatch_ref.status() == TaskStatus::Terminated;
    super::task::task_put(dispatch_ref);
    if terminated {
        super::task::arm_deferred_reap();
    }
    true
}

/// Reap terminated tasks whose reap was refused while they were dispatch-pinned. Runs under the same idle-stack
/// interrupt-window contract as [`drain_previous_task`]; a cheap no-op
/// unless a deferred attempt armed the retry latch.
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

    // Both endpoints of the switch this CPU may be about to make, minted here
    // and never inside `run_switch`'s closures: a guard is address-taken, so
    // the frame that holds it must be allocated while the *outgoing* task is
    // still published or its SafeStack reservation is released against the
    // wrong data stack. This frame straddles the switch, so it is the right
    // place; see `slopos_ostd::task::run_switch`.
    let idle = Idle::current();
    let current = Current::get();

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_schedule_calls();
    });

    let Some(idle) = idle else {
        cpu::restore_flags(irq_flags);
        return;
    };

    // Invariant gate:
    // every context switch funnels through here, and a switch may only
    // happen at the running baseline (preempt_count == 0). Descheduling
    // with a preempt/lock guard held would make the held
    // SpinLock/PreemptMutex travel with the blocked task (contenders spin
    // unpreemptibly) and unbalance the per-task count swap in
    // `switch_context`. Fail loud at the offending call chain rather than
    // corrupting the count into a later, context-free underflow panic.
    assert_switch_preempt_safe();

    // A CPU with no current task is *not* running idle: it is parked on a
    // pre-heap bootstrap stub, and its first switch out of the boot context
    // takes the `prev = None` path below rather than the idle dispatcher.
    if current.as_ref().is_some_and(|c| c.addr() == idle.addr()) {
        let _ = run_ready_task_from_idle(cpu_id, idle.task());
        // Drain the deferred reference before re-enabling interrupts. The drain
        // clears the CPU-local previous-task slot while interrupts are still
        // disabled, then drops the reference in its own interrupt window.
        // Re-enabling interrupts first would open a window in which a
        // timer-driven re-entrant dispatch parks a second reference into the
        // still-occupied slot.
        let _ = drain_previous_task();
        cpu::restore_flags(irq_flags);
        return;
    }

    // Do NOT re-enqueue before the context switch — that is the
    // "wake-before-switch-complete" SMP race.  The re-enqueue happens in
    // run_ready_task_from_idle (the idle resumption point) AFTER
    // execute_task returns and on_cpu is cleared.
    switch_from_current_to_idle(
        cpu_id,
        current.as_ref().map(|current| current.task()),
        idle.task(),
    );
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

/// CAS the current task's status from `Running` to `Blocked` without
/// yielding. Returns `true` on CAS success.
///
/// Used by the wait-queue protocol from inside the queue's SpinLock
/// so a `wake_*` taking the same lock necessarily observes either
/// (a) the queue empty (we haven't pushed yet), or (b) the task in
/// the queue and `Blocked` — never `Running`-and-on-queue. The
/// matching yield happens after the lock is dropped via
/// [`yield_blocked_task`].
pub fn mark_current_blocked() -> bool {
    // The PCR id answers this without dereferencing the task, which keeps the
    // wait-queue's under-lock park free of a task load.
    let task_id = slopos_arch::pcr::current_task_id();
    if task_id == INVALID_TASK_ID {
        return false;
    }
    // Stamp a truthful reason: a bare `try_transition_from` KEEPS the
    // previous reason, so a wait-queue park after an earlier timed sleep
    // masqueraded as `Blocked(Sleep)`-with-no-entry — indistinguishable
    // from a genuinely stranded sleeper. Timed wq waits re-stamp `Sleep`
    // in `arm_blocked_timeout`, keeping `Sleep ⇔ a deadline is armed`.
    super::task::task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        slopos_abi::task::BlockReason::Generic,
    ) == 0
}

/// Yield a task already CAS-flipped to `Blocked` by
/// [`mark_current_blocked`]. Must be called outside any SpinLock —
/// `schedule()` is not reentrant-safe under our locks.
///
/// # State-aware contract
///
/// The wait-queue protocol now evaluates `condition()` *outside*
/// the queue's internal SpinLock (see
/// [`slopos_ostd::sync::wait_queue::WaitQueue::wait_event`]). That
/// opens a race window: a producer's `wake_*` may CAS
/// `Blocked → Ready` between our prior `mark_current_blocked` and
/// our call into this function. If we blindly descheduled in that
/// case, the wake would be silently dropped (we'd be removed from
/// the runqueue with state `Ready` and nobody to dispatch us).
///
/// Scheduling-while-atomic guard: a task must never deschedule while preemption is
/// disabled — the held `SpinLock`/`PreemptMutex` would travel with the
/// blocked task and every contender would spin unpreemptibly until a
/// wake that may itself need the lock. Before scheduler-backed waits
/// landed in the block-device path this failure mode was silent (the
/// task blocked, the lock stayed held, the system wedged); fail loud
/// instead so the offending call chain is in the panic backtrace.
#[inline]
fn assert_not_blocking_while_atomic() {
    if PreemptGuard::is_active() {
        panic!("scheduler: blocking wait entered with preemption disabled (spinning lock held?)");
    }
}

/// Hard guard at the universal context-switch chokepoint
/// ([`schedule_internal`]): a switch may only occur at the running
/// baseline (`preempt_count == 0`). See the call site for the full
/// rationale. Promotes the historical WaitQueue-only
/// [`assert_not_blocking_while_atomic`] to *every* deschedule path —
/// direct `schedule()`/`yield()`, the `sleep`/`block` sleep-queue
/// primitives, and the trap-exit handoff — so a lock held across any
/// blocking call panics at the real caller instead of silently
/// corrupting the per-CPU count.
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

/// Consume a wake that raced with the current task's block path and keep the
/// scheduler ownership model honest.
///
/// The task is physically still executing on this CPU, so after absorbing the
/// wake its placement must be `OnCpu` even if the wake briefly linked a local
/// ready entry or a remote-inbox node. Local ready entries are removed here;
/// stale remote-inbox nodes are harmless because the owner CPU will unlink/drop
/// them when it drains and observes placement != `RemoteWake`.
pub(crate) fn consume_ready_wake_for_current(current: &Current) {
    // The guard already proves this CPU is running the task, so every read
    // below comes off it directly.
    let body = current.task();
    current.task().set_status(TaskStatus::Running);
    unschedule_task(body);
    body.set_sched_placement(SchedPlacement::OnCpu);
}

/// Defence: at entry, `unschedule_task` strips us from every
/// runqueue (serialised against any racing wake's `schedule_task`
/// via the per-CPU `queue_lock`). Then re-load the task state. If
/// the state is no longer `Blocked` (a wake CAS happened-before our
/// Acquire-load), force state back to `Running`, scrub any
/// residual runqueue presence, and return without context-switching.
/// The caller's `wait_event` loop will re-check the condition on
/// the next iteration and observe whatever data the producer stored
/// before its `wake_*`.
///
/// If the state is still `Blocked`, no wake has been observed; we
/// call `schedule()` to context-switch. A wake that fires after the
/// state-load but before the context-switch still enqueues us
/// (via its own `schedule_task`), so we are dispatched on a later
/// scheduler tick — no lost wakeup.
/// Commit a `Blocked` deschedule: strip `current` from every runqueue, then
/// re-confirm it is still `Blocked`. Returns `true` if the caller may
/// proceed to `schedule()`.
///
/// This is THE lost-wakeup guard for every blocking primitive: a peer's
/// `unblock_task` may CAS `Blocked → Ready` and enqueue between the
/// caller's Blocked-CAS and the `unschedule_task` here — which just
/// stripped that fresh enqueue. Descheduling anyway would strand the task
/// READY in no runqueue forever (every later wake no-ops on a Ready task,
/// and the sleep timer's wake gates on Blocked). On a detected race the
/// wake is consumed instead: status is forced back to `Running`, any
/// residual enqueue (a wake that landed after the first unschedule) is
/// scrubbed, and the caller must NOT deschedule. Every blocking path —
/// `yield_blocked_task`, `yield_blocked_task_with_timeout`,
/// `sleep_current_task_ms`, `block_current_task_with_timeout` — funnels
/// through this one definition; a new blocking primitive must too.
///
/// Must be called with IRQs disabled, after the caller committed
/// `Running → Blocked`.
pub(crate) fn commit_blocked_deschedule(current: &Current) -> bool {
    let body = current.task();
    unschedule_task(body);
    if body.status() != TaskStatus::Blocked {
        consume_ready_wake_for_current(current);
        return false;
    }
    true
}

pub fn yield_blocked_task() {
    // `Current::get()` folds the null-pointer and invalid-id checks into one:
    // it yields `None` unless the PCR names a task with a valid id.
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

/// Yield a task already CAS-flipped to `Blocked` and arm a
/// millisecond-resolution timeout. The sleep-queue entry will fire
/// `unblock_task` (CAS `Blocked → Ready`) when the deadline passes;
/// if a peer `wake_*` arrives first, that path's
/// `cancel_sleep` removes the entry to keep the timer from firing
/// spuriously against the (now-`Ready`) task.
///
/// Carries the same state-aware contract as [`yield_blocked_task`]:
/// if a wake or a sleep deadline raced us between
/// `mark_current_blocked` and entry here, we restore `Running` and
/// return without descheduling.
pub fn yield_blocked_task_with_timeout(timeout_ms: u32) {
    let Some(current) = Current::get() else {
        return;
    };
    let task_id = current.id();
    assert_not_blocking_while_atomic();
    super::sleep::arm_blocked_timeout(task_id, timeout_ms);
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if commit_blocked_deschedule(&current) {
            schedule();
        }
    });
    super::sleep::cancel_sleep(task_id);
}

/// Force the current task's state back to `Running` and remove any
/// stale runqueue presence. Used by
/// [`slopos_ostd::sync::wait_queue::WaitQueue::wait_event_until`] to
/// cancel a previously committed `Running → Blocked` CAS when the
/// wait condition becomes observable after the queue's SpinLock has
/// been dropped.
///
/// Idempotent vs. a concurrent producer-side `wake_*`: a wake that
/// already CAS'd us to `Ready` and enqueued us on a runqueue is
/// absorbed here by the unconditional state store + `unschedule_task`
/// removal, so the next scheduler dispatch will not try to
/// double-dispatch the still-executing task.
///
/// # Force-store idempotency
///
/// The wake-side CAS `Blocked → Ready` and this function's store
/// `→ Running` are indistinguishable for the purpose of "task is no
/// longer blocked on this wait-queue"; whichever order they land in,
/// the `wait_event_until` loop's condition recheck closes the residual
/// race via the data lock's own happens-before chain. A lost store
/// costs at most one extra trip around the wait loop.
pub fn set_current_runnable() {
    let Some(current) = Current::get() else {
        return;
    };
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        // Force-set state to Running. `set_status` is a plain
        // store (force_set on the underlying packed TaskState atomic),
        // so it deterministically overrides whatever transient state
        // a racing wake left behind.
        // Remove any runqueue presence a racing wake may have added and
        // restore the scheduler owner to OnCpu — we are about to keep running
        // on this CPU, the task must not also be eligible for dispatch.
        consume_ready_wake_for_current(&current);
    });
}

pub fn task_wait_for(task_id: u32) -> c_int {
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    // The registry guard pins the target across the reads below, and is the
    // reference `wait_ref_acquire` clones into the wait map — the map entry has
    // to exist before the guard is dropped, or the last strong holder could
    // destroy the task in between.
    let Some(target_guard) = super::task::task_find_by_id(task_id) else {
        // Already gone — waitpid semantics treat this as success.
        return 0;
    };
    if target_guard.status() == TaskStatus::Invalid {
        return 0;
    }

    // Id-only: nothing here dereferences the current task, so read the id
    // straight out of the PCR rather than a pointer that would have to be
    // proven live.
    let waiter_id = slopos_ostd::cpu::x86_64::pcr::current_task_id();
    if waiter_id == task_id {
        return -1; // self-wait rejected
    }

    let target_id = target_guard.task_id;
    // The predicate below outlives this guard, so it keeps the node rather than
    // a borrow — the WAIT_REFS entry is what owns the target across the wait,
    // and `parked_task_has_exited` is the sanctioned read against that.
    let target = target_guard.node();

    // Hold a reference on `target` for the whole wait so the task — and the
    // `exit_cell` we read in the predicate below — cannot be recycled while
    // we are parked. Memory ordering: the producer's `try_set` is Release;
    // `is_set` (Acquire, evaluated under the event-bus queue's SpinLock) is
    // the matching consumer; the SpinLock pair supplies the full barrier.
    //
    // The owning reference is recorded in WAIT_REFS keyed by this waiter so it
    // is released exactly once — either here on the normal wake path
    // (`wait_ref_release`), or by `release_wait_ref` from the task-teardown
    // path if this waiter is SIGKILL'd while parked. SlopOS tears a blocked
    // task down asynchronously (its kernel stack, and any RAII guard on it,
    // is never unwound on async kill), so a plain stack guard would leak its
    // reference and pin the target forever. Tying the release to the task's
    // kernel-object lifecycle — the map entry IS the owning reference, and the
    // atomic `remove` elects the single releaser — mirrors `futex_remove_task`
    // and is the correct pattern under this kill model.
    wait_ref_acquire(waiter_id, &target_guard);

    // The WAIT_REFS entry now pins the target. Drop the registry guard
    // before parking: a waiter SIGKILL'd mid-wait never unwinds this stack,
    // and a leaked guard here would pin the target forever, while the map
    // entry is released by the teardown path.
    drop(target_guard);

    // The exited-status fallback covers a target whose status flips to
    // Zombie/Terminated via a path that has not (yet) published exit_info —
    // defensive, but cheap. The exit-cell re-check also makes a colliding
    // `ChildExit` bucket harmless.
    let _ = BUS
        .subscribe(KernelEvent::ChildExit {
            task: TaskSlot(target_id),
        })
        .wait_event(|| slopos_ostd::task::parked_task_has_exited(target));

    wait_ref_release(waiter_id);
    0
}

// ── waitpid wait-reference tracking (kill-safe) ─────────────────────────────
//
// `task_wait_for` holds an owning reference on its target for the duration of
// the wait. Because a blocked task that is killed never unwinds its own stack,
// that reference cannot be released by a stack guard — it must be released from
// the task-teardown path. WAIT_REFS owns one strong reference per waiting task;
// whoever removes the entry (normal wake or kill teardown) performs the lone
// drop, off the map lock.
static WAIT_REFS: SpinLock<KBTreeMap<u32, KernelSync<TaskRef>>> =
    SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE);

/// Record `waiter_id`'s owning reference on the task it is about to park on.
///
/// Takes the caller's guard rather than a pointer: `TaskRef::clone_of`'s
/// contract is that the caller already holds a live strong reference, and
/// `&TaskRef` is that contract written in the signature instead of in a comment
/// above the call.
fn wait_ref_acquire(waiter_id: u32, target: &TaskRef) {
    if waiter_id == INVALID_TASK_ID {
        return;
    }
    // The map entry owns a strong reference that pins the target — and the
    // `exit_info` cell the wait predicate reads — for the whole wait, even
    // after the target becomes a zombie.
    let target = TaskRef::clone_of(target.node());
    let stale = {
        let mut map = WAIT_REFS.lock();
        map.insert(waiter_id, KernelSync::new(target))
    };
    // A task can only be parked in one wait at a time; a pre-existing entry
    // would be a bug, but release it off-lock rather than leak.
    if let Some(prev) = stale {
        task_put(prev.into_inner());
    }
}

fn wait_ref_release(waiter_id: u32) {
    let entry = { WAIT_REFS.lock().remove(&waiter_id) };
    if let Some(target) = entry {
        task_put(target.into_inner());
    }
}

/// Release the wait reference (if any) held by a task being torn down.
///
/// Called from `mark_task_terminated` so a waiter SIGKILL'd while parked in
/// `task_wait_for` still drops the incref it holds on its target. No-op for
/// tasks that hold none. Mirrors `futex_remove_task`.
pub fn release_wait_ref(waiter_id: u32) {
    wait_ref_release(waiter_id);
}

pub(crate) fn wake_blocked_task(task: &TaskRef, task_id: u32) -> c_int {
    // `Blocked` is the only blockable intermediate state. Wake-side must either
    // observe an existing scheduler owner (ready queue / remote inbox /
    // migration), or acquire the explicit `Waking` publication token before it
    // publishes `TaskStatus::Ready`.
    //
    // `OnCpu` is deliberately *not* a sufficient Ready publication owner. It
    // proves the task is physically executing or in a switch window, but the
    // producer that wins `Blocked -> Ready` still owns runnable publication via
    // `Waking`. The separate `on_cpu` bit prevents a queued still-switching
    // task from being dispatched twice.
    //
    // TOTALITY CONTRACT (Linux-ttwu discipline): this function returns only
    // once the wake is conclusive — we published Ready, or the task is
    // observably no longer Blocked (a peer publisher won / it is running),
    // or it is exited/invalid. It must NEVER give up while the task is
    // still Blocked: wake sources are often one-shot (a popped sleep-queue
    // deadline, a masked-until-drained GPIO edge, a consumed event), so a
    // silently dropped wake strands the sleeper forever. Transient windows
    // — the dispatcher's `OnCpu` claim during switch-out, a peer's `Waking`
    // reservation — last microseconds and are waited out with `spin_loop`,
    // mirroring `smp_cond_load_acquire(&p->on_cpu, !VAL)` in Linux's
    // `try_to_wake_up`. (A prior 8-iteration cap here returned "0 = nothing
    // done" on transient collisions; that was the root cause of the
    // Blocked(Sleep)-with-no-entry kthread strands seen on hardware.)
    let body: &Task = task;
    loop {
        if body.is_exited() || (body.status() == TaskStatus::Invalid) {
            return -1;
        }
        if body.status() != TaskStatus::Blocked {
            // Already woken (or never blocked): the wake is a no-op.
            return 0;
        }
        match body.sched_placement() {
            // Registered but never published: its creator has not finished
            // building it, and publishing it here would put a half-constructed
            // task on a runqueue. `task_create` stamps `pgid = task_id` before
            // it registers, so a process-group signal reaches exactly this
            // window.
            //
            // Terminal, not a retry — and compatible with the totality
            // contract above, for three reasons. A nascent task has never
            // executed, so it holds no one-shot wake source: it has never
            // parked on a sleep queue, a futex bucket, a wait node, or the
            // event bus. The senders that *can* name it — kill, and the
            // process-group and session fanouts — are level-triggered: each
            // sets the durable `signal_pending` bit before waking, and that
            // bit survives this refusal to be consumed at the task's first
            // user-mode boundary. And every creation path reaches either
            // `publish_new_task` or `task_terminate`, so the window is bounded
            // by its creator's own progress.
            //
            // Returns 0 ("nothing to do"), never -1: -1 means the target is
            // gone, and `kill` would turn that into ESRCH for a task that very
            // much exists.
            SchedPlacement::Nascent => return 0,
            SchedPlacement::OnCpu => {
                // `OnCpu` is also the dispatcher's transient claim after a
                // ready-queue dequeue and before Ready->Running. A wake that
                // targets an already-Ready/already-Running task is a no-op and
                // must not steal that claim (the status check above filtered
                // those). Only a genuinely Blocked current task is converted
                // to an explicit Waking publisher token.
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
                // Status moved under our reservation; restore the placement
                // and re-observe from the top (exit if no longer Blocked).
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
                // `Waking` is an explicit publisher reservation. The CAS below
                // is single-winner; duplicate wakes either see Ready already
                // published/owned or wait for the owner to finish (its
                // reservation lasts microseconds).
                if task_transition_from(body, TaskStatus::Blocked, TaskStatus::Ready) {
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                if body.is_ready() {
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                core::hint::spin_loop();
                continue;
            }
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake | SchedPlacement::Migrating => {
                // Scheduler ownership already exists. If a direct state mutator
                // or test fixture parked the task while leaving that ownership
                // in place, the wake still performs the state CAS; the existing
                // queue/inbox/migration owner then becomes runnable again.
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

/// Wake the task named by `task_id`.
///
/// The permanent id-keyed entry point: most wake sources — a driver's wait
/// queue, a signal fanout, a poll notifier — hold only an id, and resolving it
/// here through the registry keeps the liveness-checked upgrade in one place
/// instead of once per caller. Returns `-1` when the id names no live task.
pub fn unblock_task_id(task_id: u32) -> c_int {
    let Some(task) = crate::task::task_find_by_id(task_id) else {
        return -1;
    };
    wake_blocked_task(&task, task_id)
}

/// Unified task exit for all CPUs.
/// Terminates the current task and switches to idle via schedule().
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
        // No current task - just schedule, which will switch to idle
        schedule();
        // `schedule()` returning here means the scheduler's task-pick
        // protocol broke; nothing actionable to do, but the CPU must
        // stay alive to ack TLB-shootdown / reschedule IPIs from
        // peers, otherwise the BSP's NMI watchdog declares this CPU
        // dead. Re-enable IF so HLT wakes on every timer tick and
        // IPI; without this the CPU sleeps with IF cleared and the
        // 500ms watchdog times it out.
        slopos_arch::cpu::enable_interrupts();
        slopos_ostd::cpu::x86_64::core::halt_loop();
    }

    if crate::task::task_terminate(u32::MAX) != 0 {
        klog_info!("scheduler_task_exit: Failed to terminate current task");
    }

    // Dying task stays in PCR.current_task until `schedule()` below
    // dispatches idle.  Its memory is pinned through the switch window:
    // `on_cpu` blocks reclaim until the switch tail publishes the
    // handoff, and the dispatch reference is released only by the
    // successor's `drain_previous_task`.  The primed `unsafe_stack_sp`
    // keeps SafeStack prologues happy through that window.
    schedule();

    klog_info!(
        "scheduler_task_exit: Schedule returned unexpectedly on CPU {}",
        cpu_id
    );
    slopos_arch::cpu::enable_interrupts();
    slopos_ostd::cpu::x86_64::core::halt_loop();
}

// OSTD task-exit hook.  Wraps `scheduler_task_exit_impl()` to expose
// it as `extern "sysv64" fn() -> !`, the type expected by
// [`slopos_ostd::task::switch::register_task_exit_hook`].  The OSTD
// `task_entry_trampoline` calls the registered hook when a kernel
// task's entry function returns.
extern "sysv64" fn ostd_task_exit_hook() -> ! {
    scheduler_task_exit_impl()
}

/// Install the OSTD task-exit hook.  Must be called once at boot, after
/// the scheduler is initialised but before any task can return from
/// its entry function (in practice: before `enter_scheduler`).
///
/// The `&BspToken<'brand>` witness binds the call to the BSP-init scope
/// opened by `slopos_ostd::sync::run_bsp_init`; OSTD's
/// [`register_task_exit_hook`] is one-shot and asserts on double-call.
pub fn install_ostd_task_exit_hook<'b>(token: &slopos_ostd::sync::BspToken<'b>) {
    slopos_ostd::task::switch::register_task_exit_hook(token, ostd_task_exit_hook);
    slopos_ostd::panic_recovery::register_oops_task_id_provider(current_task_id);
}

fn deferred_reschedule_callback() {
    if PreemptGuard::is_active() || !is_scheduling_active() {
        return;
    }

    // Both tests read the running task, and the guard is scoped to them:
    // `schedule()` below republishes `PCR.current_task`, so a guard held
    // across it would describe the successor. A CPU with no current task keeps
    // today's behaviour and falls through to the reschedule.
    //
    // SM_PREEMPT discipline: an involuntary reschedule must never park a task
    // that has committed `Running → Blocked` but is still executing its
    // blocking protocol. Every wait primitive CASes to Blocked under its queue
    // lock and only afterwards re-checks the condition / arms its sleep
    // timeout / calls the voluntary yield — and the queue guard's drop lands
    // exactly here when a reschedule went pending during the locked section.
    // Switching away at that point deschedules the task with no wake armed: a
    // producer whose event landed in the gap finds no waiter to wake and no
    // timeout exists yet, so the task is parked forever (the exec-time
    // blk-read hang). The task's own voluntary `schedule()` is at most a few
    // instructions away; skipping the preemption here costs nothing.
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
    reset_sleep_queue();

    0
}

/// Register the kernel scheduler's deferred-reschedule callback with
/// OSTD's preempt backend.  Called once from the BSP boot path
/// (`boot_step_scheduler_init`) — the `&BspToken<'brand>` witness
/// binds the call to the BSP-init scope opened by
/// `slopos_ostd::sync::run_bsp_init`. Kept separate from
/// [`init_scheduler`] so test-scope reinit (which lacks a `BspToken`
/// — `KernelTestScope` holds only a `BootCtx<'_, TestInit>`) can
/// rerun `init_scheduler` without contending with OSTD's one-shot
/// callback slot.
pub fn install_reschedule_callback<'b>(token: &slopos_ostd::sync::BspToken<'b>) {
    slopos_ostd::sync::register_reschedule_callback(token, deferred_reschedule_callback);
}

pub fn scheduler_is_enabled() -> c_int {
    SCHEDULER_ENABLED.load(Ordering::Acquire) as c_int
}

/// ID of the task running on this CPU, or 0 when there is none.
///
/// Reads the id `dispatch()` published in the PCR rather than dereferencing
/// `current_task`, so it stays correct — and cheap — while the slot names a
/// pre-heap bootstrap stub. The "no task" and "stub" cases both report the
/// `INVALID_TASK_ID` sentinel, which this collapses to 0 for callers that treat
/// zero as absent.
pub fn current_task_id() -> u32 {
    match slopos_arch::pcr::current_task_id() {
        INVALID_TASK_ID => 0,
        id => id,
    }
}

/// Id of the task running on this CPU for wait-queue parking, or
/// `INVALID_TASK_ID` when there is none.
///
/// Deliberately *not* [`current_task_id`], which collapses "absent" to 0 for
/// its own callers. A wait queue must be able to tell "no current task" apart
/// from a real id, and 0 is a value the sentinel check would let through — a
/// waiter would then park a task that does not exist and never be woken.
#[inline]
pub fn current_task_handle() -> u32 {
    slopos_arch::pcr::current_task_id()
}

pub fn current_task_pgid() -> u32 {
    Current::get().map_or(0, |c| c.task().pgid())
}

/// Get the current task's session ID (SID).
///
/// Returns 0 if there is no current task or the scheduler is not yet active.
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
    // If the idle loop on this CPU armed a LAPIC one-shot, restore
    // periodic mode now. Runs unconditionally so that even an IRQ
    // unrelated to the timer (e.g. a NIC RX IRQ that fires before
    // our one-shot fires) re-arms periodic — `scheduler_timer_tick`
    // is the natural funnel because every IRQ that pulls work into
    // this CPU eventually reaches a tick or trap-exit boundary.
    restore_periodic_if_armed();

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Drive the on-screen kernel-log (fblog) renderer from CPU 0's tick — a
    // single relaxed atomic load unless the framebuffer log console is shown.
    // Tick-driven so it renders even when userland (or the scheduler's own
    // dispatch) is wedged, which is exactly when it's needed.
    if cpu_id == 0 {
        slopos_ostd::fblog::on_timer_tick();
    }

    // NMI watchdog: record that this CPU is alive before touching any lock.
    WATCHDOG_TICKS[cpu_id].store(
        slopos_kernel_services::clock::get_timer_ticks(),
        Ordering::Relaxed,
    );

    // Conditional QS. A read-side critical section disables preemption but
    // NOT interrupts, so the timer ISR can land in the middle of one — which
    // is exactly why this cannot report unconditionally. Reporting from inside
    // a reader tells `synchronize_rcu` that reader has finished, and the object
    // it is still dereferencing is then freed underneath it.
    //
    // Declining is always safe: it delays a grace period, never shortens one.
    // The switch and idle sites stay unconditional and carry the liveness, so
    // a CPU that is preempt-disabled here simply reports at its next switch.
    slopos_ostd::sync::rcu_note_qs_from_interrupt();

    // Raise the deferred-callback softirq flag on CPU 0 only.
    // rcu_process_callbacks() runs later from the idle loop, not here.
    if cpu_id == 0 {
        slopos_ostd::sync::rcu_raise_softirq();
    }

    let idle = Idle::current();
    let current = Current::get();
    let running_idle = match (&current, &idle) {
        (Some(current), Some(idle)) => current.addr() == idle.addr(),
        _ => false,
    };

    // Unconditional tick accounting:
    // every timer interrupt is counted regardless of preemption state.
    // Idle time is categorised per-tick (not per-idle-loop-iteration) so
    // that idle_ticks and total_ticks stay in lockstep.
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

    wake_due_sleepers(platform::timer_ticks());

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0
        || PREEMPTION_ENABLED.load(Ordering::Acquire) == 0
    {
        return;
    }

    let Some(current) = current else {
        return;
    };

    if running_idle {
        mark_preempt_if_ready(cpu_id);
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

// ---------------------------------------------------------------------------
// Stranded-READY rescue sweep
// ---------------------------------------------------------------------------

/// Re-enqueue any task observed READY with no runqueue entry and not on a
/// CPU — the "stranded Ready" state in which every future `unblock_task`
/// no-ops (the task is already Ready) and the sleep timer's wake gates on
/// Blocked, so nothing ever dispatches it again.
///
/// The deschedule paths re-check for racing wakes after `unschedule_task`
/// and the idle dispatcher re-enqueues a still-Ready lost-claim dequeue, so
/// this sweep should find nothing; it is the belt-and-braces backstop that
/// turns any residual lost-enqueue race from a permanent interactive freeze
/// into a one-tick blip — and its klog line is the telemetry that exposes
/// such a race for root-causing. Called from the idle loop under a tick
/// cooldown.
///
/// A transiently Ready-and-unlinked task (mid-wake between the Ready-CAS
/// and the enqueue, mid-dispatch between dequeue and the Running claim,
/// or pending in a remote wake inbox) can in principle be observed by the
/// sweep. The first two windows must be short-lived; the third is now
/// explicit state (`remote_inbox_linked`) and is not a strand at all.
/// Therefore the rescue only fires after the same task is observed as a
/// true candidate across multiple consecutive sweeps.
pub(crate) fn rescue_stranded_ready_tasks() {
    // Cooldown: the sweep is a backstop, not a hot path — walking the task
    // registry (manager lock + scratch alloc) from every idle iteration on
    // every CPU would cost hundreds of walks per second at idle. One sweep
    // per RESCUE_COOLDOWN_TICKS across all CPUs bounds a genuine strand's
    // extra latency to ~100ms while keeping the steady-state cost near zero.
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
        return; // another CPU claimed this window
    }
    let seq = RESCUE_SWEEP_SEQ
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    CURRENT_RESCUE_SWEEP.store(seq, Ordering::Relaxed);
    super::task::task_for_each_active(rescue_check_task);
}

/// Consecutive-sweep strike tracking for genuinely stranded READY tasks.
///
/// Fresh task creation and normal wake/dispatch paths have small windows
/// where a task is Ready, off-CPU, and not yet on a ready queue. Rescue is
/// only safe once that observation persists across consecutive global
/// rescue sweeps. Slots are keyed by `task_id % N`; a collision at worst
/// delays a rescue by one window.
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

/// Whether any CPU is still executing `task`, still names it as its current
/// task, or holds it in its idle slot.
///
/// The reap gate and the destructor gate both key on this, so they can never
/// disagree. Unhashing a task that satisfies any disjunct takes back its
/// existence reference, and the last release that follows runs the
/// allocator-heavy destructor — freeing the kernel stack a CPU is executing on.
///
/// **Each disjunct is load-bearing on its own, and deleting one deletes a
/// guard's soundness proof.**
///
/// - `on_cpu` alone is not enough: `dispatch()` publishes `PCR.current_task`
///   without setting `on_cpu`, so a task can be a CPU's current without being
///   marked on-CPU. The second disjunct is what makes [`CurrentTask`] sound.
/// - The current-task disjunct alone is not enough either: a CPU's idle task is
///   not its current task while a ready task runs there, yet the idle task must
///   stay reapable-never — it is minted once at `create_idle_task_for_cpu` and
///   the PCR idle slot names it for the machine's whole life. The third
///   disjunct is what makes [`IdleTask`] sound, and it is also what discharges
///   `SwitchWindow::new`'s dispatch-reference precondition for the idle
///   endpoint of a switch, which no owning handle covers.
///
/// [`CurrentTask`]: slopos_ostd::task::CurrentTask
/// [`IdleTask`]: slopos_ostd::task::IdleTask
#[inline]
pub(crate) fn task_is_dispatch_pinned(task: &Task) -> bool {
    let addr = TaskAddr::of(task);
    task.on_cpu() || task_is_current_on_any_cpu(addr) || crate::per_cpu::is_idle_task(addr)
}

/// Address comparison only — the per-CPU current-task slots are raw pointers,
/// and [`TaskAddr`] is the compare-only view of one, so nothing here can
/// dereference a foreign CPU's task even by accident.
fn task_is_current_on_any_cpu(addr: TaskAddr) -> bool {
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    (0..cpu_count).any(|cpu_id| TaskAddr::current_of(cpu_id) == Some(addr))
}

fn rescue_check_task(guard: &crate::task::TaskRef) {
    let t: &Task = guard;
    if t.status() != TaskStatus::Ready {
        return;
    }
    // A never-published task is not stranded, it is unfinished. Rescuing one
    // onto a runqueue is the very thing `Nascent` exists to prevent, so the
    // sweep must decline even if something has forced it Ready.
    if t.sched_placement() == SchedPlacement::Nascent {
        return;
    }
    if t.on_cpu() || task_is_current_on_any_cpu(TaskAddr::of(t)) {
        return;
    }
    // `last_run_timestamp != 0` means the task is still accounted as the
    // running task on some CPU. A self-wakeup can temporarily make the
    // current task Ready before it yields back to idle; it is not stranded
    // until the context-switch-out accounting has cleared this timestamp.
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
    // Enqueue LOCALLY — never via `schedule_task`: this function is the
    // recovery path for a task that already lost the normal enqueue, so route
    // directly to the current CPU's ready queue. A leaked Waking reservation is
    // completed as Waking; Ready+Waking is not a durable scheduler owner.
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
