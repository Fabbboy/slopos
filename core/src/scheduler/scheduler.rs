use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::cpu;
use slopos_ostd::sync::PreemptGuard;

use slopos_utils::kdiag_timestamp;
use slopos_utils::klog_info;

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
    TASK_FLAG_USER_MODE, Task, TaskPriority, TaskStatus, task_controlling_tty, task_fpu_state_mut,
    task_fs_base, task_get_info, task_has_flag, task_id_of, task_inc_ref, task_is_blocked,
    task_is_invalid, task_is_ready, task_is_running, task_is_terminated, task_is_will_block,
    task_kernel_stack_top, task_name_looks_idle, task_pgid, task_pointer_is_valid, task_priority,
    task_process_id, task_record_context_switch, task_record_yield, task_set_controlling_tty,
    task_set_on_cpu, task_set_state, task_set_status, task_set_time_slice,
    task_set_time_slice_remaining, task_set_waiting_on, task_sid, task_status, task_time_slice,
    task_time_slice_remaining, task_try_transition_from, task_wait_off_cpu, task_waiting_on_cas,
};
pub use super::trap::{
    RescheduleReason, TrapExitSource, save_preempt_context, save_task_context_from_interrupt_frame,
    scheduler_handle_post_irq, scheduler_handle_timer_interrupt, scheduler_handoff_on_trap_exit,
    scheduler_request_reschedule, scheduler_request_reschedule_from_interrupt,
};
const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;
const USER_SPACE_TOP: u64 = 0xffff_8000_0000_0000;

unsafe extern "C" {
    static _text_start: u8;
    static _text_end: u8;
}

#[inline]
fn kernel_text_range() -> (u64, u64) {
    unsafe {
        (
            &_text_start as *const u8 as u64,
            &_text_end as *const u8 as u64,
        )
    }
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
use slopos_ostd::task::fpu::{fpu_xrstor, fpu_xsave};
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
    // SAFETY: `current_pcr` is `unsafe fn` because the per-CPU PCR
    // pointer is only valid on the current CPU; `dispatch` runs with
    // interrupts disabled on the target CPU per the function's
    // preconditions, so the PCR is stable for this read+store window.
    unsafe {
        slopos_arch::pcr::current_pcr()
            .syscall_pid
            .store(pid, Ordering::Release);
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_switches();
    });

    // Lifecycle state transition — (Ready|Running) → Running.
    let current_status = task_status(task);
    if current_status != Some(TaskStatus::Ready) && current_status != Some(TaskStatus::Running) {
        klog_info!(
            "dispatch: unexpected state {} for task {}",
            current_status.map(|s| s.as_u8()).unwrap_or(0xFF) as u32,
            task_id_of(task).unwrap_or(INVALID_TASK_ID)
        );
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

#[allow(dead_code)]
fn requeue_running_task(cpu_id: usize, current: *mut Task) {
    if current.is_null() {
        return;
    }

    // Requeue if Running OR WillBlock.  A WillBlock task is still
    // executing its pre-block critical section (condition check,
    // wait-queue enqueue).  If preempted here it must go back to
    // the Ready queue so it can finish and call block_current_task.
    let id = task_id_of(current).unwrap_or(INVALID_TASK_ID);
    if (task_is_running(current) || task_is_will_block(current))
        && task_set_state(id, TaskStatus::Ready) == 0
    {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.enqueue_local(current);
        });
    }
}

fn switch_to_kernel_address_space(_task: *mut Task) {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    tlb::enter_lazy_tlb(cpu_id);
    // SAFETY: irqs disabled by caller; KERNEL_VM_SPACE is the canonical
    // kernel master PML4, kernel-half invariant always holds.
    unsafe {
        kernel_vm_space().lock().activate();
    }
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
/// # Safety
/// Both task pointers must be valid (or null for `prev`).  Must be called
/// with interrupts disabled.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn prepare_switch_to(cpu_id: usize, prev: *mut Task, next: *mut Task) {
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
    unsafe {
        let pcr = slopos_arch::pcr::current_pcr();
        if !prev.is_null() {
            (*prev).saved_user_ctx_ptr = pcr.user_ctx_ptr.load(Ordering::Acquire);
            core::ptr::copy_nonoverlapping(
                pcr.kernel_return_ctx.get(),
                &raw mut (*prev).saved_kernel_return_ctx,
                1,
            );
        }
        pcr.user_ctx_ptr
            .store((*next).saved_user_ctx_ptr, Ordering::Release);
        core::ptr::copy_nonoverlapping(
            &raw const (*next).saved_kernel_return_ctx,
            pcr.kernel_return_ctx.get(),
            1,
        );
    }

    // --- FPU save (prev) ---
    if let Some(prev_fpu) = task_fpu_state_mut(prev) {
        // SAFETY: caller serialises (irqs disabled).  Inv. 8 — prev
        // is currently on this CPU and only this CPU.
        unsafe {
            fpu_xsave(prev_fpu as *mut _, xcr0);
        }
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
        // SAFETY: irqs disabled by caller (`prepare_switch_to`'s
        // contract); kernel-half invariant maintained by VmSpace's
        // own resync. Falls back to kernel master if the process has
        // no VmSpace bound (early creation, slot reset).
        let activated = unsafe { process_vm_activate(next_pid) };
        if !activated {
            unsafe { kernel_vm_space().lock().activate() };
        }
    } else {
        // SAFETY: same as above; idle / kernel-only task installs the
        // kernel master.
        unsafe { kernel_vm_space().lock().activate() };
    }

    // --- FPU restore (next) ---
    // SAFETY: caller serialises (irqs disabled).  Inv. 8 — next is
    // becoming the running task on this CPU and no other CPU touches it.
    let next_fpu = task_fpu_state_mut(next).expect("next must be non-null");
    unsafe {
        fpu_xrstor(next_fpu as *const _, xcr0);
    }
}

/// Validate that the idle task's switch_ctx has a sane RIP (in kernel .text)
/// and RSP (above USER_SPACE_TOP).
fn ensure_idle_switch_ctx_valid(idle_task: *mut Task) -> bool {
    if idle_task.is_null() {
        return false;
    }
    // SAFETY: caller pre-checked non-null; switch_ctx is an in-Task
    // field, naturally aligned u64 reads on x86_64 are atomic.
    let (rip, rsp) = unsafe {
        let ctx = &raw const (*idle_task).switch_ctx;
        (
            core::ptr::read_unaligned(&raw const (*ctx).rip),
            core::ptr::read_unaligned(&raw const (*ctx).rsp),
        )
    };

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

    // SAFETY: caller serialises (irqs disabled).  prepare_switch_to
    // and switch_registers both operate on the now-installed dispatch
    // target; the next-task switch_ctx pointer's stability is upheld
    // by `task_pointer_is_valid` checks earlier in the call chain.
    unsafe {
        // prepare_switch_to handles FPU, TLB, FS_BASE, TSS, CR3
        prepare_switch_to(cpu_id, current, idle_task);

        let prev_ctx = if !current.is_null() {
            &raw mut (*current).switch_ctx
        } else {
            ptr::null_mut()
        };
        let next_ctx = &raw const (*idle_task).switch_ctx;
        switch_registers(prev_ctx, next_ctx);
        // NOTE: code here runs when the TASK resumes (not on idle path).
        // All post-switch cleanup happens in run_ready_task_from_idle
        // after execute_task returns — that IS the idle resumption point.
    }
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

/// Spin until the previous CPU finishes its outgoing context switch for
/// this task.  Matches Linux's `smp_cond_load_acquire(&p->on_cpu, !VAL)`
/// in `try_to_wake_up()`.  Without this, a woken task can be dispatched
/// on CPU B while CPU A is still saving its registers → corruption.
#[inline]
fn wait_task_off_cpu(task: *mut Task) {
    task_wait_off_cpu(task);
}

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    wait_task_off_cpu(task);

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

    // New tasks have on_cpu=false from init, but be defensive.
    wait_task_off_cpu(task);

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
    // SAFETY: `to_task` is non-null and was just validated through
    // `task_pointer_is_valid`. The switch_ctx is an in-Task field;
    // its u64 word reads are atomic on x86_64.
    let (rip, rsp) = unsafe {
        let ctx = &raw const (*to_task).switch_ctx;
        (
            core::ptr::read_unaligned(&raw const (*ctx).rip),
            core::ptr::read_unaligned(&raw const (*ctx).rsp),
        )
    };
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

    // Mark task as physically on this CPU.  schedule_task() spin-waits on
    // this flag before allowing the task to be dispatched elsewhere, preventing
    // the "wake-before-switch-complete" race (Linux p->on_cpu pattern).
    task_set_on_cpu(to_task, true);

    // Single source-of-truth install: writes PCR.current_task
    // (SafeStack slot), PCR.syscall_pid, task.state = Running, and
    // the per-CPU switch counter in one place.
    dispatch(cpu_id, to_task);
    slopos_ostd::sync::rcu_note_qs();

    // SAFETY: caller serialises (irqs disabled). prepare_switch_to
    // and switch_registers operate on the freshly-validated
    // switch_ctx whose addresses are stable for this CPU.
    unsafe {
        // prepare_switch_to handles FPU, TLB, FS_BASE, TSS RSP0, CR3
        prepare_switch_to(cpu_id, from_task, to_task);

        let prev_ctx = if !from_task.is_null() {
            &raw mut (*from_task).switch_ctx
        } else {
            ptr::null_mut()
        };
        let next_ctx = &raw const (*to_task).switch_ctx;
        switch_registers(prev_ctx, next_ctx);
    }
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

    if task_is_terminated(next_task) || !task_is_ready(next_task) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    // Guard: if the task is still physically on another CPU (context switch
    // in progress), put it back and skip.  Matches the schedule_task() spin,
    // but here we re-enqueue instead of spinning (idle loop must not block).
    // SAFETY: `next_task` is non-null and was just validated through
    // `task_pointer_is_valid`; the `on_cpu` AtomicBool is internally
    // synchronised.
    if unsafe { (*next_task).on_cpu.load(Ordering::Acquire) } {
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

    // Re-enqueue the task if it was preempted (Running) or in a pre-block
    // critical section (WillBlock).  Blocked/Terminated tasks are NOT
    // re-enqueued — they'll be woken by their respective event paths.
    // This runs AFTER on_cpu=false and context save, so no SMP race.
    if !task_is_terminated(next_task)
        && (task_is_running(next_task) || task_is_will_block(next_task))
        && task_set_state(next_task_id, TaskStatus::Ready) == 0
    {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_local(next_task);
        });
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

pub fn prepare_to_wait() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if task_is_blocked(current) || task_is_will_block(current) {
        return;
    }
    let task_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);
    let _ = task_set_state(task_id, TaskStatus::WillBlock);
}

pub fn finish_wait() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if !task_is_will_block(current) {
        return;
    }
    let task_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);
    let _ = task_set_state(task_id, TaskStatus::Running);
}

pub fn block_current_task() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }

    let task_id = task_id_of(current).unwrap_or(INVALID_TASK_ID);

    // Atomic CAS(WillBlock, Blocked): only blocks if still in WillBlock.
    // If a concurrent unblock_task already set Running, the CAS fails and
    // we return immediately — the wakeup is preserved.
    if task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Blocked) != 0 {
        return;
    }
    unschedule_task(current);
    schedule();
}

pub fn task_wait_for(task_id: u32) -> c_int {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return -1;
    }
    if task_id == INVALID_TASK_ID || task_id_of(current) == Some(task_id) {
        return -1;
    }

    let mut target: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut target) != 0 || target.is_null() {
        task_set_waiting_on(current, INVALID_TASK_ID);
        return 0;
    }
    task_set_waiting_on(current, task_id);
    prepare_to_wait();
    block_current_task();
    finish_wait();
    task_set_waiting_on(current, INVALID_TASK_ID);
    0
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let task_id = task_id_of(task).unwrap_or(INVALID_TASK_ID);

    // Try WillBlock -> Running (task declared intent to block but hasn't yet).
    if task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Running) == 0 {
        // Cancel any pending sleep-queue entry so it doesn't fire later
        // and spuriously transition the (now-Running) task.
        super::sleep::cancel_sleep(task_id);
        return 0;
    }

    // Try Blocked -> Ready (task is fully blocked, wake it).
    if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
        super::sleep::cancel_sleep(task_id);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        return schedule_task(task);
    }

    // Task is in some other state (Running, Ready, Terminated) — nothing to do.
    if task_is_terminated(task) || task_is_invalid(task) {
        return -1;
    }
    0
}

/// Attempt to wake a task that was waiting on `completed_id`.
/// Returns true if THIS caller won the wake race and should handle the task.
/// Returns false if another caller already woke it or task wasn't waiting on this ID.
///
/// This is the key primitive for lock-free task termination - uses CAS to ensure
/// exactly one waker succeeds per waiting task.
pub fn try_wake_from_task_wait(task: *mut Task, completed_id: u32) -> bool {
    if task.is_null() || completed_id == INVALID_TASK_ID {
        return false;
    }

    // CAS: Atomically clear waiting_on only if it matches completed_id
    // Only ONE caller can succeed this CAS - the "winner"
    let won = task_waiting_on_cas(task, completed_id, INVALID_TASK_ID).unwrap_or(false);

    if won {
        // We won the CAS race. Now wake the task using the same safe
        // transitions as unblock_task() to avoid the WillBlock race.
        let task_id = task_id_of(task).unwrap_or(INVALID_TASK_ID);

        // WillBlock -> Running: task declared intent but hasn't blocked yet.
        // Cancel any pending sleep; no schedule_task (task is still running).
        if task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Running) == 0 {
            super::sleep::cancel_sleep(task_id);
            return true;
        }

        // Blocked -> Ready: task is fully blocked, wake it.
        if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
            super::sleep::cancel_sleep(task_id);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            schedule_task(task);
            return true;
        }

        // Task is in some other state (Running, Ready, Terminated).
        if task_is_terminated(task) || task_is_invalid(task) {
            return false;
        }
        true
    } else {
        // Lost race OR task is waiting on different ID
        // Either way, not our responsibility to wake
        false
    }
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
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
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
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
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
pub fn install_ostd_task_exit_hook() {
    // SAFETY: `ostd_task_exit_hook` does not return; the OSTD register
    // hook is one-shot and asserts on double-call.
    unsafe {
        slopos_ostd::task::switch::register_task_exit_hook(ostd_task_exit_hook);
    }
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

    slopos_ostd::sync::register_reschedule_callback(deferred_reschedule_callback);

    slopos_utils::panic_recovery::register_panic_cleanup(sched_panic_cleanup);

    0
}

fn sched_panic_cleanup() {
    // Called from the panic recovery path after longjmp. The
    // task-manager lock may have been held when the panic occurred
    // and the guard was lost. We poison-unlock to mark the data as
    // potentially inconsistent; the scheduler reinit path checks
    // `is_poisoned()` before accepting operations.
    scheduler_force_unlock();
    // SAFETY: panic-recovery path; the caller's longjmp invalidated
    // the lock guard, so a `poison_unlock` is the only legal way to
    // reset state without panicking again.
    unsafe { crate::task::task_manager_poison_unlock() };
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

/// No-op: the scheduler no longer holds a global mutex. Per-CPU
/// schedulers serialise via per-CPU `queue_lock` SpinLocks and
/// lock-free atomics; nothing global to release here. Kept as a
/// `pub fn` symbol for the panic-recovery path that historically
/// called it.
pub fn scheduler_force_unlock() {
    // No global scheduler mutex to unlock - per-CPU schedulers use lockless atomics
}
