use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::cpu;
use slopos_ostd::sync::PreemptGuard;

use slopos_ostd::kdiag_timestamp;
use slopos_ostd::klog_info;

use slopos_kernel_services::platform;

// ---------------------------------------------------------------------------
// NMI watchdog: per-CPU alive timestamp (updated every timer tick)
// ---------------------------------------------------------------------------
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
    create_idle_task, create_idle_task_for_cpu, enter_scheduler, scheduler_register_bottom_half,
    scheduler_register_idle_wakeup_callback, scheduler_run_bottom_halves,
    spawn_kernel_task_from_driver,
};
pub use super::sleep::{block_current_task_with_timeout, cancel_sleep, sleep_current_task_ms};
use super::sleep::{reset_sleep_queue, wake_due_sleepers};
use super::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_USER_MODE, Task, TaskPriority, TaskStatus, task_controlling_tty, task_exit_info_ref,
    task_fpu_state_mut, task_fs_base, task_has_flag, task_id_of, task_inc_ref, task_is_exited,
    task_is_invalid, task_is_ready, task_is_running, task_kernel_stack_top, task_name_looks_idle,
    task_on_cpu_load, task_pcr_round_trip_swap, task_pgid, task_pointer_is_valid, task_priority,
    task_process_id, task_record_context_switch, task_record_yield, task_set_controlling_tty,
    task_set_on_cpu, task_set_state, task_set_status, task_set_time_slice,
    task_set_time_slice_remaining, task_sid, task_status, task_switch_ctx_ptr,
    task_switch_ctx_ptr_mut, task_switch_ctx_rip_rsp, task_time_slice, task_time_slice_remaining,
    task_try_transition_from, task_waiters_ref,
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
use slopos_ostd::task::switch::switch_registers;

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
    task_set_status(task, TaskStatus::Running);
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

    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    // Scheduler hot path: IRQs disabled by caller; the safe-fn shims
    // for `prepare_switch_to` and `switch_registers` capture the
    // per-call validity through the now-installed dispatch target.
    let prev_ctx = task_switch_ctx_ptr_mut(current);
    let next_ctx = task_switch_ctx_ptr(idle_task);
    // prepare_switch_to handles FPU, TLB, FS_BASE, TSS, CR3
    prepare_switch_to(cpu_id, current, idle_task);
    switch_registers(prev_ctx, next_ctx);
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

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    if task_time_slice_remaining(task) == Some(0) {
        reset_task_quantum(task);
    }

    let Some(target_cpu) = per_cpu::select_target_cpu(task) else {
        return -1;
    };
    let current_cpu = slopos_arch::pcr::get_current_cpu();

    if target_cpu == current_cpu {
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));

        if result != Some(0) {
            return -1;
        }
        0
    } else {
        let push_result = per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
            0
        });
        if push_result != Some(0) {
            return -1;
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if slopos_arch::pcr::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        0
    }
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
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    if task_time_slice_remaining(task) == Some(0) {
        reset_task_quantum(task);
    }

    let Some(target_cpu) = per_cpu::select_target_cpu_for_new(task) else {
        return -1;
    };
    let current_cpu = slopos_arch::pcr::get_current_cpu();

    if target_cpu == current_cpu {
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));

        if result != Some(0) {
            return -1;
        }
        0
    } else {
        let push_result = per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
            0
        });
        if push_result != Some(0) {
            return -1;
        }

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if slopos_arch::pcr::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        0
    }
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
    switch_registers(prev_ctx, next_ctx);
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
            let _ = sched.enqueue_local(next_task);
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
            let _ = sched.enqueue_local(next_task);
            sched.set_executing_task(false);
        });
        return false;
    }

    // Single-winner dispatch claim: only one CPU may run a READY task.
    // If another CPU already claimed it (or state changed), drop this dequeue.
    let next_task_id = task_id_of(next_task).unwrap_or(INVALID_TASK_ID);
    if task_set_state(next_task_id, TaskStatus::Running) != 0 {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    // Hold a reference while the task is dispatched on this CPU.
    // Without this, refcnt is 0 after dequeue and a concurrent
    // task_terminate() on another CPU can kfree() the kernel stack
    // while we are still executing on it — a use-after-free.
    let _ = task_inc_ref(next_task);

    execute_task(cpu_id, idle_task, next_task);

    // Context switch OUT is complete — the task's registers are fully saved.
    // Clear on_cpu so schedule_task() on other CPUs can dispatch this task.
    task_set_on_cpu(next_task, false);

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    dispatch(cpu_id, idle_task);
    slopos_ostd::sync::rcu_note_qs();

    switch_to_kernel_address_space(idle_task);

    // Re-enqueue the task if it was preempted (Running) or already
    // woken (Ready) before its yield completed. The Ready case covers
    // the self-wakeup window: a wake from the timer ISR transitions
    // state Blocked→Ready and routes through `enqueue_local` on this
    // CPU; the in-progress block path then `unschedule_task`s the
    // entry, leaving the task Ready but in no runqueue. Re-enqueueing
    // here keeps it schedulable across the yield.
    //
    // task_wait_for goes through WaitQueue::wait_event, which
    // performs the Running→Blocked transition while the queue is
    // already holding our wait node — by the time `schedule()`
    // dispatches a peer, our state is committed Blocked and the
    // peer's `wake_one` will find us on the queue.
    //
    // Blocked/Zombie/Terminated tasks are NOT re-enqueued — they'll
    // be woken by their respective event paths.
    // This runs AFTER on_cpu=false and context save, so no SMP race.
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
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_local(next_task);
            });
        }
    }

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
pub fn yield_blocked_task() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if task_id_of(current).unwrap_or(INVALID_TASK_ID) == INVALID_TASK_ID {
        return;
    }
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        unschedule_task(current);
        if !matches!(task_status(current), Some(TaskStatus::Blocked)) {
            // Wake raced in. Either the wake's `schedule_task` happened
            // before our `unschedule_task` (we removed the entry) or
            // after (we left the entry). Force `Running` and scrub the
            // residual enqueue defensively.
            task_set_status(current, TaskStatus::Running);
            unschedule_task(current);
            return;
        }
        schedule();
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
    super::sleep::arm_blocked_timeout(task_id, timeout_ms);
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        unschedule_task(current);
        if !matches!(task_status(current), Some(TaskStatus::Blocked)) {
            task_set_status(current, TaskStatus::Running);
            unschedule_task(current);
            return;
        }
        schedule();
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
        task_set_status(current, TaskStatus::Running);
        // Remove any runqueue presence a racing wake's
        // `schedule_task` may have added — we are about to keep
        // running on this CPU, the task must not also be eligible
        // for dispatch from a ready queue.
        unschedule_task(current);
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

    let _ref_guard = super::task::TaskRefGuard::new(target);

    // `_ref_guard` keeps `target`'s slot alive across the potential
    // yield. The waiters queue and exit_info cell are valid for the
    // duration of `_ref_guard`. Memory ordering: the producer's
    // `try_set` is Release; `is_set` (Acquire, evaluated under the
    // WaitQueue's SpinLock) is the matching consumer. The SpinLock
    // pair on both sides supplies the bidirectional full barrier.
    let Some(waiters) = task_waiters_ref(target) else {
        return 0;
    };
    let Some(exit_cell) = task_exit_info_ref(target) else {
        return 0;
    };

    // The `task_is_exited` fallback covers the case where the
    // target's status flips to Zombie/Terminated via a path that has
    // not (yet) published exit_info — defensive, but cheap.
    let _ = waiters.wait_event(|| exit_cell.is_set() || task_is_exited(target));
    0
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let task_id = task_id_of(task).unwrap_or(INVALID_TASK_ID);

    // Phase 5 collapsed the WillBlock state; the only blockable
    // intermediate is `Blocked` itself. Wake-side just transitions
    // Blocked → Ready; the caller's wait-queue lock-pair guarantees
    // that the waiter committed to Blocked before we observed it.
    if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
        super::sleep::cancel_sleep(task_id);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        return schedule_task(task);
    }

    // Task is in some other state (Running, Ready, Zombie, Terminated) —
    // nothing to do. A Running waker that races a Blocked→Ready CAS sees
    // Running here; that's a benign no-op — the waiter never actually slept.
    if task_is_exited(task) || task_is_invalid(task) {
        return -1;
    }
    0
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
}

fn deferred_reschedule_callback() {
    if PreemptGuard::is_active() || !is_scheduling_active() {
        return;
    }

    let current = scheduler_get_current_task();
    if task_has_no_preempt_flag(current) {
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
    crate::scheduler::task::task_clear_controlling_tty_for_session(session_id, tty)
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
    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // NMI watchdog: record that this CPU is alive before touching any lock.
    WATCHDOG_TICKS[cpu_id].store(crate::irq::get_timer_ticks(), Ordering::Relaxed);

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
