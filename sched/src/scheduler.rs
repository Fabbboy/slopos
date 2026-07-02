use core::ffi::c_int;
use core::ptr;
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
    TASK_FLAG_USER_MODE, Task, TaskPriority, TaskStatus, task_controlling_tty, task_exit_info_ref,
    task_fpu_state_mut, task_fs_base, task_has_flag, task_id_of, task_inc_ref, task_is_exited,
    task_is_invalid, task_is_ready, task_is_running, task_kernel_stack_top,
    task_last_run_timestamp_volatile, task_name_looks_idle, task_on_cpu_load,
    task_pcr_round_trip_swap, task_pgid, task_pointer_is_valid, task_priority, task_process_id,
    task_record_context_switch, task_record_yield, task_recovery_depth_load,
    task_recovery_depth_store, task_remote_inbox_is_linked, task_sched_placement_compare_exchange,
    task_sched_placement_load, task_sched_placement_store, task_set_controlling_tty,
    task_set_on_cpu, task_set_state, task_set_status, task_set_time_slice,
    task_set_time_slice_remaining, task_sid, task_status, task_switch_ctx_ptr,
    task_switch_ctx_ptr_mut, task_switch_ctx_rip_rsp, task_time_slice, task_time_slice_remaining,
    task_try_transition_from,
};
pub use super::trap::{
    RescheduleReason, TrapExitSource, save_preempt_context, save_task_context_from_interrupt_frame,
    scheduler_handle_post_irq, scheduler_handle_timer_interrupt, scheduler_handoff_on_trap_exit,
    scheduler_request_reschedule, scheduler_request_reschedule_from_interrupt,
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
use slopos_ostd::task::SchedPlacement;
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
    process_vm_activate, process_vm_get_cr3_phys, process_vm_sync_kernel_mappings,
};
use slopos_mm::tlb;

use slopos_ostd::cpu::x86_64::xsave::active_xcr0;
use slopos_ostd::task::switch::switch_context;

use super::ffi_boundary::kernel_stack_top;

fn get_default_time_slice() -> u64 {
    SCHED_DEFAULT_TIME_SLICE as u64
}

fn reset_task_quantum(task: *mut Task) {
    if task.is_null() {
        return;
    }
    let slice = match task_time_slice(task) {
        Some(0) | None => get_default_time_slice(),
        Some(s) => s,
    };
    task_set_time_slice(task, slice);
    task_set_time_slice_remaining(task, slice);
}

#[inline]
fn save_live_recovery_depth(task: *mut Task) {
    if task.is_null() {
        return;
    }
    task_recovery_depth_store(task, slopos_arch::pcr::recovery_depth());
}

#[inline]
fn restore_live_recovery_depth(task: *mut Task) {
    slopos_arch::pcr::recovery_depth_store(task_recovery_depth_load(task));
}

#[inline]
fn scheduler_tasks_for_cpu(cpu_id: usize) -> (*mut Task, *mut Task) {
    let mut current = scheduler_get_current_task_for(cpu_id);
    let mut idle = scheduler_get_idle_task_for(cpu_id);

    if !idle.is_null() && !task_pointer_is_valid(idle) {
        klog_info!(
            "SCHED: CPU {} has corrupted idle task pointer {:p}; disabling scheduler view",
            cpu_id,
            idle
        );
        idle = ptr::null_mut();
    }

    if !current.is_null() && !task_pointer_is_valid(current) {
        klog_info!(
            "SCHED: CPU {} has corrupted current task pointer {:p}; recovering",
            cpu_id,
            current
        );
        current = if idle.is_null() {
            ptr::null_mut()
        } else {
            idle
        };
        // Quiet log-and-continue semantic: `dispatch()` isn't
        // appropriate here because its non-null assert would panic
        // on a null fallback when both current and idle are corrupt.
        // PCR is the source of truth for SafeStack readers.
        if !current.is_null() {
            slopos_arch::pcr::set_current_task(current as *mut ());
            restore_live_recovery_depth(current);
            task_set_status(current, TaskStatus::Running);
        }
    }

    (current, idle)
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
/// - `task` is non-null, lives in the task pool (or is a bootstrap
///   stub), and has its `unsafe_stack_sp` primed.
/// - Caller runs with preemption disabled OR inside the
///   interrupts-off context-switch window.
#[inline]
pub(crate) fn dispatch(cpu_id: usize, task: *mut Task) {
    debug_assert!(!task.is_null(), "dispatch() must receive a non-null task");
    debug_assert!(
        cpu_id == slopos_arch::pcr::get_current_cpu(),
        "dispatch() must run on the target CPU (SafeStack slot is gs-local)"
    );

    // SafeStack reads this on every instrumented prologue.
    slopos_arch::pcr::set_current_task(task as *mut ());
    restore_live_recovery_depth(task);

    // Keep PCR.syscall_pid in sync so copy_from_user always resolves
    // the correct process page directory, even after preemption.
    let pid = task_process_id(task).unwrap_or(INVALID_PROCESS_ID);
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
    // After Phase 1 (durable exit_info + per-task waiters WaitQueue),
    // a task entering `dispatch` MUST be Ready or Running. Anything
    // else is an invariant violation: a Blocked task in a runqueue
    // means a wake path enqueued without first transitioning to
    // Ready, or a state transition raced the dispatcher. Either is
    // a bug we want surfaced loudly in debug, not silently coerced.
    let current_status = task_status(task);
    debug_assert!(
        matches!(
            current_status,
            Some(TaskStatus::Ready) | Some(TaskStatus::Running)
        ),
        "dispatch: invariant broken — task {} in unexpected state {:?}",
        task_id_of(task).unwrap_or(INVALID_TASK_ID),
        current_status,
    );
    if !matches!(
        current_status,
        Some(TaskStatus::Ready) | Some(TaskStatus::Running)
    ) {
        // Production fallback: skip dispatch and let the caller pick
        // a different task. The pre-Phase-1 code logged + coerced to
        // Running, which produced the `0xdfdedddcdbdad9d8`-shape page
        // faults in CI (a wait-protocol-half-state task forced into
        // Running runs with a corrupted user-mode RIP). Skipping is
        // the safe move.
        return;
    }
    let _ =
        task_sched_placement_compare_exchange(task, SchedPlacement::None, SchedPlacement::OnCpu);
    task_set_status(task, TaskStatus::Running);
}

/// Cross-crate, test-only entry point into [`dispatch`] for hermetic
/// fixtures that live outside `slopos-sched` (notably
/// `core/src/syscall/tests.rs`). Carries the same safety preconditions
/// as [`dispatch`] — only invoke from a fixture that has primed
/// `unsafe_stack_sp` and is running with preemption disabled on the
/// target CPU.
#[cfg(feature = "test-hooks")]
#[inline]
pub fn dispatch_for_test(cpu_id: usize, task: *mut Task) {
    dispatch(cpu_id, task);
}

/// Install `task` as `cpu_id`'s idle task.  Writes `PCR.idle_task` —
/// the single source of truth for "idle task on CPU N".
/// Called once per CPU by `create_idle_task_for_cpu`.
#[inline]
pub(super) fn install_idle_task(cpu_id: usize, task: *mut Task) {
    debug_assert!(
        !task.is_null(),
        "install_idle_task() must receive a non-null task"
    );
    slopos_arch::pcr::set_idle_task(cpu_id, task as *mut ());
}

fn switch_to_kernel_address_space(_task: *mut Task) {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    tlb::enter_lazy_tlb(cpu_id);
    // Safe-wrapper entry: KERNEL_VM_SPACE is the canonical kernel
    // master PML4; the kernel-half invariant is trivially satisfied
    // when we're switching onto the master itself.
    kernel_vm_space().lock().activate_kernel_master();
}

#[inline]
fn task_is_idle_candidate(task: *mut Task) -> bool {
    if task.is_null() || !task_pointer_is_valid(task) {
        return false;
    }

    if task_id_of(task) == Some(INVALID_TASK_ID) {
        return false;
    }
    if task_priority(task) != Some(TaskPriority::Idle) {
        return false;
    }
    if !task_has_flag(task, TASK_FLAG_KERNEL_MODE) {
        return false;
    }

    task_name_looks_idle(task)
}

/// Pre-switch housekeeping: FPU save(prev), TLB flush, FS_BASE, TSS RSP0,
/// CR3 load, FPU restore(next).  Replaces the big unsafe block that lived
/// inside the old `execute_task`.
///
/// # Caller invariants
///
/// Routed through safe-fn wrappers that take `*mut Task` (handled
/// internally by null-and-validity checks) and `&mut FpuState`
/// (witnessed by the safe FPU helpers). Must still be called with
/// interrupts disabled and only by the scheduler hot path so the
/// FPU / TLB / FS_BASE / TSS / CR3 sequencing matches the dispatch
/// state machine.
fn prepare_switch_to(cpu_id: usize, prev: *mut Task, next: *mut Task) {
    // Cache the active XCR0 mask once for the whole switch — the OSTD
    // `fpu_xsave` / `fpu_xrstor` primitives take it as a parameter
    // (the static is set at boot by `slopos_ostd::cpu::x86_64::xsave::init`).
    let xcr0 = active_xcr0();

    // --- Save/restore per-CPU PCR user-mode round-trip slots ---
    //
    // `pcr.user_ctx_ptr` and `pcr.kernel_return_ctx` are written by
    // `slopos_ostd::user::mode::user_mode_round_trip_asm` before iretq
    // and read by `__ostd_user_return` on the next user→kernel SYSCALL.
    // The slots are per-CPU but the data they carry belongs to the
    // user-mode round trip in flight on the *running task*.  When
    // another task is scheduled in between the iretq and the SYSCALL
    // back, that task overwrites the PCR slots with its own values; on
    // resume the original task's trampoline would otherwise jump into
    // the wrong saved RIP/RSP.  Mirror them onto the per-task `Task`
    // struct here so each task carries its own copy across switches.
    task_pcr_round_trip_swap(prev, next);

    // --- FPU save (prev) ---
    if let Some(prev_fpu) = task_fpu_state_mut(prev) {
        // Safe wrapper: `&mut FpuState` discharges the exclusive-write
        // half of the contract; the scheduler's IRQs-off + Inv. 8
        // discharge the remaining ordering requirement.
        prev_fpu.save_current(xcr0);
    }

    // --- TLB / address-space switch ---
    let is_user_mode = task_has_flag(next, TASK_FLAG_USER_MODE);
    let old_pid = task_process_id(prev).unwrap_or(INVALID_PROCESS_ID);
    let new_pid = if is_user_mode {
        task_process_id(next).unwrap_or(INVALID_PROCESS_ID)
    } else {
        INVALID_PROCESS_ID
    };
    tlb::notify_mm_switch(old_pid, new_pid, cpu_id);
    if is_user_mode {
        tlb::exit_lazy_tlb(cpu_id);
    } else {
        tlb::enter_lazy_tlb(cpu_id);
    }

    // --- FS_BASE ---
    let fs = if is_user_mode {
        let raw = task_fs_base(next).unwrap_or(0);
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
        match task_kernel_stack_top(next) {
            Some(kst) if kst != 0 => kst,
            _ => kernel_stack_top() as u64,
        }
    } else {
        kernel_stack_top() as u64
    };
    platform::gdt_set_kernel_rsp0(kernel_rsp);

    // --- CR3 ---
    //
    // Routes through `VmSpace::activate`, the only sanctioned CR3 write
    // path post-framekernel. `activate` lazily resyncs kernel-half from
    // the master PML4 (KERNEL_MASTER_GEN bump propagation), fires the
    // registered `CursorUnmapHook::on_activate` callback, and writes
    // CR3 with PCID + NOFLUSH=1. Cold-path PCID rotation is OSTD's
    // concern; consumers see only the activate call.
    //
    // `_ = cpu_id;` — `mmu::select_cr3` plumbing is unreachable from
    // this hot path; the per-CPU ASID pool retires when the legacy
    // paging surface deletes.
    let _ = cpu_id;
    let next_pid = task_process_id(next).unwrap_or(INVALID_PROCESS_ID);
    if next_pid != INVALID_PROCESS_ID {
        process_vm_sync_kernel_mappings(next_pid);
        // Scheduler-invariant safe entry: IRQs disabled by caller,
        // kernel-half maintained by `activate`'s internal resync.
        // Falls back to kernel master if the process has no VmSpace
        // bound (early creation, slot reset).
        let activated = process_vm_activate(next_pid);
        if !activated {
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
    let next_fpu = task_fpu_state_mut(next).expect("next must be non-null");
    next_fpu.restore_to_cpu(xcr0);
}

/// Validate that the idle task's switch_ctx has a sane RIP (in kernel .text)
/// and RSP (above USER_SPACE_TOP).
fn ensure_idle_switch_ctx_valid(idle_task: *mut Task) -> bool {
    if idle_task.is_null() {
        return false;
    }
    let (rip, rsp) = task_switch_ctx_rip_rsp(idle_task).unwrap_or((0, 0));

    let (text_start, text_end) = kernel_text_range();
    let rip_ok = rip >= text_start && rip < text_end;
    let rsp_ok = rsp >= USER_SPACE_TOP;

    if rip_ok && rsp_ok {
        return true;
    }

    klog_info!(
        "SCHED: CPU {} idle task {} has corrupt switch_ctx: rip=0x{:x} (ok={}) rsp=0x{:x} (ok={}) — refusing switch",
        slopos_arch::pcr::get_current_cpu(),
        task_id_of(idle_task).unwrap_or(INVALID_TASK_ID),
        rip,
        rip_ok,
        rsp,
        rsp_ok,
    );
    false
}

fn switch_from_current_to_idle(cpu_id: usize, current: *mut Task, idle_task: *mut Task) {
    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, idle_task, timestamp);

    // Validate the idle context BEFORE publishing it as current_task.
    // Otherwise, other CPUs could observe current_task pointing at an
    // unusable idle context if validation fails.
    if !ensure_idle_switch_ctx_valid(idle_task) {
        klog_info!(
            "SCHED: CPU {} cannot recover idle switch_ctx for task {}",
            cpu_id,
            task_id_of(idle_task).unwrap_or(INVALID_TASK_ID)
        );
        return;
    }

    save_live_recovery_depth(current);
    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    // Scheduler hot path: IRQs disabled by caller; the safe-fn shims
    // for `prepare_switch_to` and `switch_registers` capture the
    // per-call validity through the now-installed dispatch target.
    let prev_ctx = task_switch_ctx_ptr_mut(current);
    let next_ctx = task_switch_ctx_ptr(idle_task);
    // prepare_switch_to handles FPU, TLB, FS_BASE, TSS, CR3
    prepare_switch_to(cpu_id, current, idle_task);
    // switch_context swaps the per-task preempt-count with the PCR around
    // the register switch (migration-safe accounting), then switches.
    switch_context(prev_ctx, next_ctx);
    // NOTE: code here runs when the TASK resumes (not on idle path).
    // All post-switch cleanup happens in run_ready_task_from_idle
    // after execute_task returns — that IS the idle resumption point.
}

#[inline]
fn task_has_no_preempt_flag(task: *mut Task) -> bool {
    task_has_flag(task, TASK_FLAG_NO_PREEMPT)
}

#[inline]
fn consume_time_slice(current: *mut Task) -> bool {
    let remaining = task_time_slice_remaining(current).unwrap_or(0);
    if remaining > 0 {
        task_set_time_slice_remaining(current, remaining - 1);
    }
    task_time_slice_remaining(current).unwrap_or(0) > 0
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

fn task_has_durable_owner(task: *mut Task) -> bool {
    placement_is_durable_owner(task_sched_placement_load(task))
}

fn newcomer_outranks_current(cpu: usize, new: *mut Task) -> bool {
    let Some(new_prio) = task_priority(new) else {
        return false;
    };
    let current = scheduler_get_current_task_for(cpu);
    if current.is_null() {
        return true; // CPU idle (or about to be)
    }
    let Some(current_prio) = task_priority(current) else {
        return true;
    };
    (new_prio.as_u8()) < (current_prio.as_u8())
}

fn publish_ready_fallback(task: *mut Task) -> c_int {
    if task.is_null() || !task_is_ready(task) {
        return -1;
    }

    match task_sched_placement_load(task) {
        SchedPlacement::ReadyQueue
        | SchedPlacement::RemoteWake
        | SchedPlacement::OnCpu
        | SchedPlacement::Migrating => return 0,
        SchedPlacement::None | SchedPlacement::Waking => {}
    }

    let current_cpu = slopos_arch::pcr::get_current_cpu();
    if per_cpu::with_cpu_scheduler(current_cpu, |sched| sched.enqueue_local(task)) == Some(0) {
        if newcomer_outranks_current(current_cpu, task) {
            scheduler_request_reschedule(RescheduleReason::InterruptWake);
        }
        return 0;
    }

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if cpu_id == current_cpu {
            continue;
        }
        if per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enqueue_local(task)) == Some(0) {
            if slopos_arch::pcr::is_cpu_online(cpu_id) {
                send_reschedule_ipi(cpu_id);
            }
            return 0;
        }
        match task_sched_placement_load(task) {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating => return 0,
            SchedPlacement::None | SchedPlacement::Waking => {}
        }
    }

    klog_info!(
        "SCHED: publish fallback failed task={} status={:?} placement={:?} current_cpu={} cpu_count={}",
        task_id_of(task).unwrap_or(INVALID_TASK_ID),
        task_status(task),
        task_sched_placement_load(task),
        current_cpu,
        cpu_count,
    );
    -1
}

fn publish_reserved_waking_ready(task: *mut Task, task_id: u32, context: &str) -> c_int {
    if !task_is_ready(task) {
        if task_sched_placement_load(task) == SchedPlacement::Waking {
            let restore = if task_on_cpu_load(task) {
                SchedPlacement::OnCpu
            } else {
                SchedPlacement::None
            };
            let _ = task_sched_placement_compare_exchange(task, SchedPlacement::Waking, restore);
        }
        return if task_is_exited(task) || task_is_invalid(task) || !task_is_ready(task) {
            0
        } else {
            -1
        };
    }

    let rc = schedule_task_from_placement(task, SchedPlacement::Waking, false);
    if rc == 0
        || matches!(
            task_sched_placement_load(task),
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
        task_status(task),
        task_sched_placement_load(task),
    );
    rc
}

fn publish_ready_from_current_owner(task: *mut Task, task_id: u32, context: &str) -> c_int {
    for _ in 0..4 {
        match task_sched_placement_load(task) {
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake | SchedPlacement::Migrating => {
                return 0;
            }
            SchedPlacement::Waking => return publish_reserved_waking_ready(task, task_id, context),
            SchedPlacement::OnCpu => {
                if task_sched_placement_compare_exchange(
                    task,
                    SchedPlacement::OnCpu,
                    SchedPlacement::Waking,
                ) {
                    return publish_reserved_waking_ready(task, task_id, context);
                }
            }
            SchedPlacement::None => {
                if task_sched_placement_compare_exchange(
                    task,
                    SchedPlacement::None,
                    SchedPlacement::Waking,
                ) {
                    return publish_reserved_waking_ready(task, task_id, context);
                }
            }
        }
    }

    if task_has_durable_owner(task) { 0 } else { -1 }
}

fn schedule_task_from_placement(task: *mut Task, from: SchedPlacement, new_task: bool) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    if task_time_slice_remaining(task) == Some(0) {
        reset_task_quantum(task);
    }

    let target_cpu = if new_task {
        per_cpu::select_target_cpu_for_new(task)
    } else {
        per_cpu::select_target_cpu(task)
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
            SchedPlacement::Migrating => sched.enqueue_migrated(task),
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake => 0,
        });

        if result != Some(0) {
            return publish_ready_fallback(task);
        }
        // Self-CPU reschedule: the remote-CPU path below sends an IPI;
        // for the local path we set the per-CPU preempt-pending flag so
        // `scheduler_handoff_on_trap_exit` (called from the trap-exit
        // path / idle loop) dispatches the new task before HLT re-engages.
        if newcomer_outranks_current(current_cpu, task) {
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
            SchedPlacement::Migrating => -1,
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

pub fn schedule_task(task: *mut Task) -> c_int {
    let from = if task_sched_placement_load(task) == SchedPlacement::Waking {
        SchedPlacement::Waking
    } else {
        SchedPlacement::None
    };
    schedule_task_from_placement(task, from, false)
}

/// Schedule a **newly created** task (fork, spawn, exec).
///
/// Uses the `SD_BALANCE_FORK`-style slow path: bypasses `last_cpu` and
/// finds the globally idlest CPU with round-robin tie-breaking.  This
/// spreads new processes across CPUs at creation time, matching Linux's
/// `wake_up_new_task()` → `select_task_rq(WF_FORK)` path.
///
/// Regular wakeups from sleep/block should use [`schedule_task()`] instead,
/// which preserves cache affinity by preferring the last CPU.
pub fn schedule_new_task(task: *mut Task) -> c_int {
    let from = if task_sched_placement_load(task) == SchedPlacement::Waking {
        SchedPlacement::Waking
    } else {
        SchedPlacement::None
    };
    schedule_task_from_placement(task, from, true)
}

/// Publish a fully-initialized new task as runnable without ever exposing
/// `Ready + no scheduler owner`.
///
/// This is SlopOS's `wake_up_new_task()` equivalent: reserve scheduler
/// placement first (`Waking`), publish `TaskStatus::Ready`, then transfer the
/// reservation into a runqueue or remote inbox.
pub fn publish_new_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    match task_sched_placement_load(task) {
        SchedPlacement::None => {
            if !task_sched_placement_compare_exchange(
                task,
                SchedPlacement::None,
                SchedPlacement::Waking,
            ) {
                return if task_has_durable_owner(task)
                    || task_sched_placement_load(task) == SchedPlacement::Waking
                {
                    0
                } else {
                    -1
                };
            }
        }
        SchedPlacement::Waking => {}
        _ => {
            return if task_has_durable_owner(task) { 0 } else { -1 };
        }
    }
    let previous_status = task_status(task).unwrap_or(TaskStatus::Blocked);
    task_set_status(task, TaskStatus::Ready);
    let rc = schedule_task_from_placement(task, SchedPlacement::Waking, true);
    if rc != 0 {
        let _ = task_sched_placement_compare_exchange(
            task,
            SchedPlacement::Waking,
            SchedPlacement::None,
        );
        task_set_status(task, previous_status);
    }
    rc
}

pub fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.remove_task(task);
        });
    }

    0
}

/// Unified task execution for all CPUs.
/// Handles switch_ctx validation, prepare_switch_to, and switch_registers.
fn execute_task(cpu_id: usize, from_task: *mut Task, to_task: *mut Task) {
    if to_task.is_null() {
        return;
    }

    if !task_pointer_is_valid(to_task) {
        klog_info!(
            "SCHED: refusing to dispatch invalid task pointer {:p}",
            to_task
        );
        return;
    }

    let pid = task_process_id(to_task).unwrap_or(INVALID_PROCESS_ID);
    let pid_ok =
        pid == INVALID_PROCESS_ID || (pid as usize) < slopos_mm::memory_layout_defs::MAX_PROCESSES;

    let to_id = task_id_of(to_task).unwrap_or(INVALID_TASK_ID);

    if !pid_ok {
        klog_info!(
            "SCHED: refusing to dispatch task {} with invalid pid {}",
            to_id,
            pid
        );
        let _ = crate::task::task_terminate(to_id);
        return;
    }

    // Validate switch_ctx.rip — must be in kernel .text (the OSTD
    // task-entry trampoline / user_task_first_run wrapper / a
    // schedule resume point all live there).
    let (rip, rsp) = task_switch_ctx_rip_rsp(to_task).unwrap_or((0, 0));
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

    // Validate CR3 for tasks with a process VM.  cr3_phys == 0 means
    // the process VM was destroyed or never created — switching into
    // that address space would fault immediately.
    if pid != INVALID_PROCESS_ID {
        let cr3_phys = process_vm_get_cr3_phys(pid);
        if cr3_phys == 0 {
            klog_info!(
                "SCHED: refusing to dispatch task {} (pid {}) with cr3_phys=0",
                to_id,
                pid,
            );
            let _ = crate::task::task_terminate(to_id);
            return;
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, to_task, timestamp);

    // Mark task as physically on this CPU. The dispatcher's re-enqueue
    // check at run_ready_task_from_idle (below) reads this flag before
    // dispatching a task — if it's still on_cpu on another CPU, the
    // task is requeued rather than dispatched twice. (Linux's
    // p->on_cpu pattern; pre-Phase-6 schedule_task also spin-waited
    // on this flag, but that spin was made redundant by Phase 1's
    // lock-pair barrier and removed.)
    task_set_on_cpu(to_task, true);
    save_live_recovery_depth(from_task);

    // Single source-of-truth install: writes PCR.current_task
    // (SafeStack slot), PCR.syscall_pid, task.state = Running, and
    // the per-CPU switch counter in one place.
    dispatch(cpu_id, to_task);
    slopos_ostd::sync::rcu_note_qs();

    // Scheduler hot path: IRQs disabled by caller; switch_ctx pointers
    // were freshly validated above, both safe shims accept the
    // raw-task arguments and route through the OSTD safe-fn surfaces.
    let prev_ctx = task_switch_ctx_ptr_mut(from_task);
    let next_ctx = task_switch_ctx_ptr(to_task);
    // prepare_switch_to handles FPU, TLB, FS_BASE, TSS RSP0, CR3
    prepare_switch_to(cpu_id, from_task, to_task);
    // switch_context swaps the per-task preempt-count with the PCR around
    // the register switch (migration-safe accounting), then switches.
    switch_context(prev_ctx, next_ctx);
}

pub(crate) fn run_ready_task_from_idle(cpu_id: usize, idle_task: *mut Task) -> bool {
    let canonical_idle = scheduler_get_idle_task_for(cpu_id);
    let mut idle_task = idle_task;

    if !task_is_idle_candidate(idle_task) && task_is_idle_candidate(canonical_idle) {
        idle_task = canonical_idle;
    }

    if !task_is_idle_candidate(idle_task) {
        return false;
    }

    let next_task = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority())
        .unwrap_or(ptr::null_mut());

    if next_task.is_null() {
        return false;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(true);
    });

    if per_cpu::should_pause_scheduler_loop(cpu_id) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_from_on_cpu(next_task);
            sched.set_executing_task(false);
        });
        core::hint::spin_loop();
        return false;
    }

    if !task_pointer_is_valid(next_task) {
        klog_info!(
            "SCHED: dropped invalid ready-queue task pointer {:p}",
            next_task
        );
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    if task_is_exited(next_task) || !task_is_ready(next_task) {
        let _ = task_sched_placement_compare_exchange(
            next_task,
            SchedPlacement::OnCpu,
            SchedPlacement::None,
        );
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    // Guard: if the task is still physically on another CPU (context switch
    // in progress), put it back and skip. Re-enqueue rather than spin —
    // the idle loop must not block, and Phase 6 removed the schedule_task
    // spin in favour of this check + the WaitQueue lock-pair barrier.
    if task_on_cpu_load(next_task) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_from_on_cpu(next_task);
            sched.set_executing_task(false);
        });
        return false;
    }

    // Single-winner dispatch claim: only one CPU may run a READY task.
    // If another CPU already claimed it (or state changed), drop this dequeue.
    let next_task_id = task_id_of(next_task).unwrap_or(INVALID_TASK_ID);
    if task_set_state(next_task_id, TaskStatus::Running) != 0 {
        // Lost the claim — but if the task is *still* Ready we hold its only
        // scheduler placement, so dropping the dequeue would strand it READY.
        // Put it back; a claimed (Running/exited) task is the winner's
        // responsibility and is correctly dropped.
        if task_is_ready(next_task) {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_from_on_cpu(next_task);
            });
        } else {
            let _ = task_sched_placement_compare_exchange(
                next_task,
                SchedPlacement::OnCpu,
                SchedPlacement::None,
            );
        }
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    // Publish on_cpu with the dispatch claim, before the reference bump and
    // the validation in execute_task. A concurrent task_terminate() must
    // observe this task as on-CPU for the whole dispatch so it defers cleanup
    // (kernel-stack free) to this CPU's post-switch path rather than freeing
    // the stack while this CPU is about to run on it — a use-after-free.
    task_set_on_cpu(next_task, true);

    // Hold a reference while the task is dispatched on this CPU, so its stack
    // survives until this CPU's post-switch cleanup releases the reference.
    let _ = task_inc_ref(next_task);

    execute_task(cpu_id, idle_task, next_task);

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    switch_to_kernel_address_space(idle_task);
    super::task::cleanup_current_task_after_switch(next_task);

    // Re-enqueue the task if it was preempted (Running) or already
    // woken (Ready) before its yield completed. Keep `on_cpu=true`
    // until after any Ready task has a queue/inbox membership. This is
    // the Linux `on_rq`/`on_cpu` ordering: a peer may observe a task as
    // runnable while it is still completing a switch-out, but it must
    // never observe Ready + off-CPU + unqueued. Peer dispatchers that
    // pick such a task hit the `on_cpu` guard above and requeue it.
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
    if !task_is_exited(next_task) {
        let already_ready = task_is_ready(next_task);
        let needs_ready_transition = task_is_running(next_task);
        let should_enqueue = if already_ready {
            true
        } else if needs_ready_transition {
            task_set_state(next_task_id, TaskStatus::Ready) == 0
        } else {
            false
        };
        if should_enqueue {
            let rc =
                per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enqueue_from_on_cpu(next_task))
                    .unwrap_or(-1);
            ready_published = rc == 0
                || matches!(
                    task_sched_placement_load(next_task),
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
        if task_is_ready(next_task) {
            let rc = publish_ready_from_current_owner(next_task, next_task_id, "finish_switch");
            if rc != 0 {
                klog_info!(
                    "SCHED: finish_switch failed final READY publish id={} rc={} placement={:?}",
                    next_task_id,
                    rc,
                    task_sched_placement_load(next_task),
                );
            }
        } else {
            let _ = task_sched_placement_compare_exchange(
                next_task,
                SchedPlacement::OnCpu,
                SchedPlacement::None,
            );
            if task_is_ready(next_task) {
                let rc = publish_ready_from_current_owner(next_task, next_task_id, "finish_switch");
                if rc != 0 {
                    klog_info!(
                        "SCHED: finish_switch failed raced READY publish id={} rc={} placement={:?}",
                        next_task_id,
                        rc,
                        task_sched_placement_load(next_task),
                    );
                }
            }
        }
    }

    // Context switch OUT is complete and every still-Ready task has been
    // published to a runqueue (or was already in a remote inbox). Only now
    // clear on_cpu so peer CPUs may claim it. Pairs with peer Acquire loads
    // of `on_cpu`, mirroring Linux `finish_task()`.
    task_set_on_cpu(next_task, false);

    // Release the dispatch reference.  If the task was re-enqueued above,
    // the queue holds its own reference so the refcnt stays > 0.  If the
    // task was terminated/blocked, this may drop refcnt to 0, allowing the
    // zombie reaper to safely reclaim its resources on the next pass.
    let _ = super::task::task_dec_ref(next_task);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });

    true
}

fn schedule_internal() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let irq_flags = cpu::save_flags_cli();

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0 {
        cpu::restore_flags(irq_flags);
        return;
    }

    let (current, idle_task) = scheduler_tasks_for_cpu(cpu_id);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_schedule_calls();
    });

    if idle_task.is_null() {
        cpu::restore_flags(irq_flags);
        return;
    }

    // Invariant gate (Linux `__schedule_bug`/`schedule_debug` analogue):
    // every context switch funnels through here, and a switch may only
    // happen at the running baseline (preempt_count == 0). Descheduling
    // with a preempt/lock guard held would make the held
    // SpinLock/PreemptMutex travel with the blocked task (contenders spin
    // unpreemptibly) and unbalance the per-task count swap in
    // `switch_context`. Fail loud at the offending call chain rather than
    // corrupting the count into a later, context-free underflow panic.
    assert_switch_preempt_safe();

    if current == idle_task {
        let _ = run_ready_task_from_idle(cpu_id, idle_task);
        cpu::restore_flags(irq_flags);
        return;
    }

    // Do NOT re-enqueue before the context switch — that is the
    // "wake-before-switch-complete" SMP race.  The re-enqueue happens in
    // run_ready_task_from_idle (the idle resumption point) AFTER
    // execute_task returns and on_cpu is cleared.
    switch_from_current_to_idle(cpu_id, current, idle_task);
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
    let current = scheduler_get_current_task();
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_yields();
    });
    task_record_yield(current);
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
    let current = scheduler_get_current_task();
    if current.is_null() {
        return false;
    }
    let task_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);
    if task_id == INVALID_TASK_ID {
        return false;
    }
    task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked) == 0
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
/// Scheduling-while-atomic guard (Linux's `__schedule_bug` / BUG on
/// `in_atomic()`): a task must never deschedule while preemption is
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
pub(crate) fn consume_ready_wake_for_current(current: *mut Task) {
    task_set_status(current, TaskStatus::Running);
    unschedule_task(current);
    task_sched_placement_store(current, SchedPlacement::OnCpu);
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
pub(crate) fn commit_blocked_deschedule(current: *mut Task) -> bool {
    unschedule_task(current);
    if !matches!(task_status(current), Some(TaskStatus::Blocked)) {
        consume_ready_wake_for_current(current);
        return false;
    }
    true
}

pub fn yield_blocked_task() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if task_id_of(current).unwrap_or(INVALID_TASK_ID) == INVALID_TASK_ID {
        return;
    }
    assert_not_blocking_while_atomic();
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if commit_blocked_deschedule(current) {
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
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    let task_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);
    if task_id == INVALID_TASK_ID {
        return;
    }
    assert_not_blocking_while_atomic();
    super::sleep::arm_blocked_timeout(task_id, timeout_ms);
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if commit_blocked_deschedule(current) {
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
/// been dropped (Linux's `set_current_state(TASK_RUNNING)` in
/// `finish_wait`).
///
/// Idempotent vs. a concurrent producer-side `wake_*`: a wake that
/// already CAS'd us to `Ready` and enqueued us on a runqueue is
/// absorbed here by the unconditional state store + `unschedule_task`
/// removal, so the next scheduler dispatch will not try to
/// double-dispatch the still-executing task.
///
/// # Force-store idempotency (Linux `include/linux/sched.h:201-208`)
///
/// > Wakeup will do: if (@state & p->__state) p->__state =
/// > TASK_RUNNING, that is, once it observes the
/// > TASK_UNINTERRUPTIBLE store the waking CPU can issue a
/// > TASK_RUNNING store which can collide with
/// > `__set_current_state(TASK_RUNNING)`. […] Losing that store is
/// > not a problem either because that will result in one extra go
/// > around the loop and our @cond test will save the day.
///
/// SlopOS's argument is the same: the wake-side CAS
/// `Blocked → Ready` and this function's store `→ Running` are
/// indistinguishable for the purpose of "task is no longer blocked
/// on this wait-queue"; whichever order they land in, the
/// `wait_event_until` loop's condition recheck closes the residual
/// race via the data lock's own happens-before chain.
pub fn set_current_runnable() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        // Force-set state to Running. `set_status` is a plain
        // store (force_set on the underlying packed TaskState atomic),
        // so it deterministically overrides whatever transient state
        // a racing wake left behind.
        // Remove any runqueue presence a racing wake may have added and
        // restore the scheduler owner to OnCpu — we are about to keep running
        // on this CPU, the task must not also be eligible for dispatch.
        consume_ready_wake_for_current(current);
    });
}

pub fn task_wait_for(task_id: u32) -> c_int {
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    let mut target: *mut Task = ptr::null_mut();
    if super::task::task_get_info(task_id, &mut target) != 0 || target.is_null() {
        // Already gone — waitpid semantics treat this as success.
        return 0;
    }

    let current = scheduler_get_current_task();
    if !current.is_null() && task_id_of(current) == Some(task_id) {
        return -1; // self-wait rejected
    }
    let waiter_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);

    let Some(target_id) = task_id_of(target) else {
        return 0;
    };
    let Some(exit_cell) = task_exit_info_ref(target) else {
        return 0;
    };

    // Hold a reference on `target` for the whole wait so its slot — and the
    // `exit_cell` we read in the predicate below — cannot be recycled while
    // we are parked. Memory ordering: the producer's `try_set` is Release;
    // `is_set` (Acquire, evaluated under the event-bus queue's SpinLock) is
    // the matching consumer; the SpinLock pair supplies the full barrier.
    //
    // The reference is recorded in WAIT_REFS keyed by this waiter so it is
    // released exactly once — either here on the normal wake path
    // (`wait_ref_release`), or by `release_wait_ref` from the task-teardown
    // path if this waiter is SIGKILL'd while parked. SlopOS tears a blocked
    // task down asynchronously (its kernel stack, and any RAII guard on it,
    // is never unwound on async kill), so a plain stack `TaskRefGuard` would
    // leak its incref and pin the target's zombie slot forever
    // (`reap_zombies` gates on refcnt==0). Tying the release to the task's
    // kernel-object lifecycle — the map entry IS the reference, and the
    // atomic `remove` elects the single releaser — mirrors `futex_remove_task`
    // and is the correct pattern under this kill model.
    wait_ref_acquire(waiter_id, target);

    // The `task_is_exited` fallback covers the case where the target's status
    // flips to Zombie/Terminated via a path that has not (yet) published
    // exit_info — defensive, but cheap. The exit-cell re-check also makes a
    // colliding `ChildExit` bucket harmless.
    let _ = BUS
        .subscribe(KernelEvent::ChildExit {
            task: TaskSlot(target_id),
        })
        .wait_event(|| exit_cell.is_set() || task_is_exited(target));

    wait_ref_release(waiter_id);
    0
}

// ── waitpid wait-reference tracking (kill-safe) ─────────────────────────────
//
// `task_wait_for` increfs its target for the duration of the wait. Because a
// blocked task that is killed never unwinds its own stack, that incref cannot
// be released by a stack guard — it must be released from the task-teardown
// path. WAIT_REFS records the held reference per waiting task; whoever removes
// the entry (normal wake or kill teardown) performs the lone `dec_ref`.
static WAIT_REFS: SpinLock<KBTreeMap<u32, KernelSync<*mut Task>>> =
    SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE);

fn wait_ref_acquire(waiter_id: u32, target: *mut Task) {
    if waiter_id == INVALID_TASK_ID || target.is_null() {
        return;
    }
    let _ = task_inc_ref(target);
    let stale = {
        let mut map = WAIT_REFS.lock();
        map.insert(waiter_id, KernelSync::new(target))
    };
    // A task can only be parked in one wait at a time; a pre-existing entry
    // would be a bug, but release it defensively rather than leak.
    if let Some(prev) = stale {
        let prev = *prev;
        if !prev.is_null() {
            let _ = super::task::task_dec_ref(prev);
        }
    }
}

fn wait_ref_release(waiter_id: u32) {
    let entry = { WAIT_REFS.lock().remove(&waiter_id) };
    if let Some(target) = entry {
        let target = *target;
        if !target.is_null() {
            let _ = super::task::task_dec_ref(target);
        }
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

pub(crate) fn wake_blocked_task(task: *mut Task, task_id: u32) -> c_int {
    // Phase 5 collapsed the WillBlock state; the only blockable intermediate
    // is `Blocked` itself. Wake-side must either observe an existing scheduler
    // owner (ready queue / remote inbox / migration), or acquire the explicit
    // `Waking` publication token before it publishes `TaskStatus::Ready`.
    //
    // Importantly, `OnCpu` is no longer treated as a sufficient Ready
    // publication owner. It proves the task is physically executing or in a
    // switch window, but the producer that wins `Blocked -> Ready` still owns
    // runnable publication via `Waking`. The separate `on_cpu` bit prevents a
    // queued still-switching task from being dispatched twice.
    for _ in 0..8 {
        match task_sched_placement_load(task) {
            SchedPlacement::OnCpu => {
                // `OnCpu` is also the dispatcher's transient claim after a
                // ready-queue dequeue and before Ready->Running. A wake that
                // targets an already-Ready/already-Running task is a no-op and
                // must not steal that claim. Only a genuinely Blocked current
                // task is converted to an explicit Waking publisher token.
                if task_status(task) != Some(TaskStatus::Blocked) {
                    return if task_is_exited(task) || task_is_invalid(task) {
                        -1
                    } else {
                        0
                    };
                }
                if !task_sched_placement_compare_exchange(
                    task,
                    SchedPlacement::OnCpu,
                    SchedPlacement::Waking,
                ) {
                    continue;
                }
                if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
                    super::sleep::cancel_sleep(task_id);
                    return publish_reserved_waking_ready(task, task_id, "oncpu wake");
                }
                let restore = if task_on_cpu_load(task) {
                    SchedPlacement::OnCpu
                } else {
                    SchedPlacement::None
                };
                let _ =
                    task_sched_placement_compare_exchange(task, SchedPlacement::Waking, restore);
                return if task_is_exited(task) || task_is_invalid(task) {
                    -1
                } else {
                    0
                };
            }
            SchedPlacement::None => {
                if !task_sched_placement_compare_exchange(
                    task,
                    SchedPlacement::None,
                    SchedPlacement::Waking,
                ) {
                    continue;
                }
                if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) != 0 {
                    if task_is_ready(task) {
                        return publish_reserved_waking_ready(task, task_id, "unblock_task");
                    }
                    let _ = task_sched_placement_compare_exchange(
                        task,
                        SchedPlacement::Waking,
                        SchedPlacement::None,
                    );
                    return if task_is_exited(task) || task_is_invalid(task) {
                        -1
                    } else {
                        0
                    };
                }
                super::sleep::cancel_sleep(task_id);
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                return publish_reserved_waking_ready(task, task_id, "unblock_task");
            }
            SchedPlacement::Waking => {
                // `Waking` is an explicit publisher reservation. The CAS below
                // is single-winner; duplicate wakes either see Ready already
                // published/owned or leave the owner to finish.
                if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
                    super::sleep::cancel_sleep(task_id);
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                if task_is_ready(task) {
                    return publish_reserved_waking_ready(task, task_id, "waking wake");
                }
                return if task_is_exited(task) || task_is_invalid(task) {
                    -1
                } else {
                    0
                };
            }
            SchedPlacement::ReadyQueue | SchedPlacement::RemoteWake | SchedPlacement::Migrating => {
                // Scheduler ownership already exists. If a direct state mutator
                // or test fixture parked the task while leaving that ownership
                // in place, the wake still performs the state CAS; the existing
                // queue/inbox/migration owner then becomes runnable again.
                if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
                    super::sleep::cancel_sleep(task_id);
                    return 0;
                }
                return if task_is_exited(task) || task_is_invalid(task) {
                    -1
                } else {
                    0
                };
            }
        }
    }

    if task_is_exited(task) || task_is_invalid(task) {
        -1
    } else {
        0
    }
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let task_id = task_id_of(task).unwrap_or(INVALID_TASK_ID);
    wake_blocked_task(task, task_id)
}

/// Unified task exit for all CPUs.
/// Terminates the current task and switches to idle via schedule().
pub fn scheduler_task_exit_impl() -> ! {
    let current = scheduler_get_current_task();
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    if current.is_null() {
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

    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, ptr::null_mut(), timestamp);

    if crate::task::task_terminate(u32::MAX) != 0 {
        klog_info!("scheduler_task_exit: Failed to terminate current task");
    }

    // Dying task stays in PCR.current_task until `schedule()` below
    // dispatches idle.  Task memory is pool-allocated (never freed),
    // and its primed `unsafe_stack_sp` keeps SafeStack prologues
    // happy through the switch window.
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

    let current = scheduler_get_current_task();
    if task_has_no_preempt_flag(current) {
        return;
    }

    // SM_PREEMPT discipline (Linux `__schedule(SM_PREEMPT)`): an
    // involuntary reschedule must never park a task that has committed
    // `Running → Blocked` but is still executing its blocking protocol.
    // Every wait primitive CASes to Blocked under its queue lock and only
    // afterwards re-checks the condition / arms its sleep timeout / calls
    // the voluntary yield — and the queue guard's drop lands exactly here
    // when a reschedule went pending during the locked section. Switching
    // away at that point deschedules the task with no wake armed: a
    // producer whose event landed in the gap finds no waiter to wake and
    // no timeout exists yet, so the task is parked forever (the exec-time
    // blk-read hang). The task's own voluntary `schedule()` is at most a
    // few instructions away; skipping the preemption here costs nothing.
    if matches!(task_status(current), Some(TaskStatus::Blocked)) {
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

pub fn scheduler_get_current_task() -> *mut Task {
    // PCR.current_task is the source of truth; written by `dispatch()`
    // on every context switch and read by SafeStack's naked
    // `__safestack_pointer_address` on every instrumented prologue.
    // Bootstrap stubs are filtered out — they are valid SafeStack
    // slot targets (they carry a primed `unsafe_stack_sp`) but are
    // semantically "no scheduled task" for scheduler-facing readers.
    let ptr = slopos_arch::pcr::get_current_task() as *mut Task;
    if super::safestack_rt::is_bootstrap_task_ptr(ptr) {
        core::ptr::null_mut()
    } else {
        ptr
    }
}

/// Cross-CPU variant — read `cpu_id`'s current task pointer.
#[inline]
pub fn scheduler_get_current_task_for(cpu_id: usize) -> *mut Task {
    let ptr = slopos_arch::pcr::get_current_task_for(cpu_id) as *mut Task;
    if super::safestack_rt::is_bootstrap_task_ptr(ptr) {
        core::ptr::null_mut()
    } else {
        ptr
    }
}

/// Cross-CPU idle-task getter.
#[inline]
pub fn scheduler_get_idle_task_for(cpu_id: usize) -> *mut Task {
    slopos_arch::pcr::get_idle_task(cpu_id) as *mut Task
}

pub fn current_task_id() -> u32 {
    task_id_of(scheduler_get_current_task()).unwrap_or(0)
}

pub fn current_task_pgid() -> u32 {
    task_pgid(scheduler_get_current_task()).unwrap_or(0)
}

/// Get the current task's session ID (SID).
///
/// Returns 0 if there is no current task or the scheduler is not yet active.
pub fn current_task_sid() -> u32 {
    task_sid(scheduler_get_current_task()).unwrap_or(0)
}

pub fn current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex> {
    task_controlling_tty(scheduler_get_current_task())
}

pub fn set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool {
    task_set_controlling_tty(scheduler_get_current_task(), tty)
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

    // Unconditional QS: the timer ISR firing proves this CPU is not
    // inside an RCU read-side critical section (those disable preemption
    // but not interrupts).  Matches Linux rcu_sched_clock_irq().
    slopos_ostd::sync::rcu_note_qs();

    // Raise the deferred-callback softirq flag on CPU 0 only.
    // rcu_process_callbacks() runs later from the idle loop, not here.
    if cpu_id == 0 {
        slopos_ostd::sync::rcu_raise_softirq();
    }

    let (current, idle_task) = scheduler_tasks_for_cpu(cpu_id);
    let running_idle = !current.is_null() && current == idle_task;

    // Unconditional tick accounting — like Linux's account_process_tick().
    // Every timer interrupt is counted regardless of preemption state.
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

    if current.is_null() {
        return;
    }

    if current == idle_task {
        mark_preempt_if_ready(cpu_id);
        return;
    }

    if task_has_no_preempt_flag(current) {
        return;
    }

    if consume_time_slice(current) {
        return;
    }

    if scheduler_ready_count(cpu_id) == 0 {
        reset_task_quantum(current);
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
    // pool (manager lock + scratch alloc) from every idle iteration on every
    // CPU would cost hundreds of pool walks per second at idle. One sweep
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
    super::task::task_iterate_active(Some(rescue_check_task), ptr::null_mut());
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

fn task_is_current_on_any_cpu(task: *mut Task) -> bool {
    if task.is_null() {
        return false;
    }
    let cpu_count = slopos_arch::pcr::get_cpu_count();
    for cpu_id in 0..cpu_count {
        if scheduler_get_current_task_for(cpu_id) == task {
            return true;
        }
    }
    false
}

fn rescue_check_task(task: *mut Task, _context: *mut core::ffi::c_void) {
    let Some(t) = super::task::task_borrow(task) else {
        return;
    };
    if t.status() != TaskStatus::Ready {
        return;
    }
    if slopos_ostd::task::accessors::task_on_cpu_load(task) || task_is_current_on_any_cpu(task) {
        return;
    }
    // `last_run_timestamp != 0` means the task is still accounted as the
    // running task on some CPU. A self-wakeup can temporarily make the
    // current task Ready before it yields back to idle; it is not stranded
    // until the context-switch-out accounting has cleared this timestamp.
    if task_last_run_timestamp_volatile(task).unwrap_or(0) != 0 {
        return;
    }
    if t.ready_link.is_linked() {
        return;
    }
    if task_remote_inbox_is_linked(task) {
        return;
    }
    let placement = task_sched_placement_load(task);
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
            sched.enqueue_waking(task)
        } else {
            sched.enqueue_local_with_status(task)
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
