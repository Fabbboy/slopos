use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_arch::cpu;
use slopos_sync::preempt::PreemptGuard;

use slopos_utils::kdiag_timestamp;
use slopos_utils::klog_info;

use slopos_kernel_services::platform;

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
    TASK_FLAG_USER_MODE, TASK_PRIORITY_IDLE, Task, TaskStatus, task_get_info, task_is_blocked,
    task_is_invalid, task_is_ready, task_is_running, task_is_terminated, task_is_will_block,
    task_pointer_is_valid, task_record_context_switch, task_record_yield, task_set_current,
    task_set_state, task_try_transition_from,
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
static SCHEDULER_ENABLED: AtomicU8 = AtomicU8::new(0);
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

use slopos_mm::paging::paging_get_kernel_directory;
use slopos_mm::process_vm::{process_vm_get_cr3_phys, process_vm_sync_kernel_mappings};
use slopos_mm::tlb;
use slopos_mm::user_copy;

use super::ffi_boundary::kernel_stack_top;
use super::switch_asm::{fpu_restore, fpu_save, switch_registers};

fn current_task_process_id() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return crate::task::INVALID_PROCESS_ID;
    }
    unsafe { (*task).process_id }
}

fn get_default_time_slice() -> u64 {
    SCHED_DEFAULT_TIME_SLICE as u64
}

fn reset_task_quantum(task: *mut Task) {
    if task.is_null() {
        return;
    }
    let slice = unsafe {
        if (*task).time_slice != 0 {
            (*task).time_slice
        } else {
            get_default_time_slice()
        }
    };
    unsafe {
        (*task).time_slice = slice;
        (*task).time_slice_remaining = slice;
    }
}

#[inline]
fn scheduler_tasks_for_cpu(cpu_id: usize) -> (*mut Task, *mut Task) {
    let mut current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    let mut idle =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());

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
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_current_task(current);
        });
        task_set_current(current);
    }

    (current, idle)
}

#[inline]
fn scheduler_ready_count(cpu_id: usize) -> u32 {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0)
}

#[inline]
fn set_scheduler_current_task(cpu_id: usize, task: *mut Task) {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(task);
    });
    task_set_current(task);
}

#[allow(dead_code)]
fn requeue_running_task(cpu_id: usize, current: *mut Task) {
    if current.is_null() {
        return;
    }

    unsafe {
        // Requeue if Running OR WillBlock.  A WillBlock task is still
        // executing its pre-block critical section (condition check,
        // wait-queue enqueue).  If preempted here it must go back to
        // the Ready queue so it can finish and call block_current_task.
        if (task_is_running(current) || task_is_will_block(current))
            && task_set_state((*current).task_id, TaskStatus::Ready) == 0
        {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.enqueue_local(current);
            });
        }
    }
}

fn switch_to_kernel_address_space(_task: *mut Task) {
    tlb::enter_lazy_tlb(slopos_arch::pcr::get_current_cpu());
    unsafe {
        let kernel_dir = paging_get_kernel_directory();
        if !(*kernel_dir).pml4_phys.is_null() {
            let kd_phys = (*kernel_dir).pml4_phys.as_u64();
            let current_cr3 = cpu::read_cr3() & !0xFFF;
            if kd_phys != current_cr3 {
                cpu::write_cr3(kd_phys);
            }
        }
    }
}

#[inline]
fn task_name_looks_idle(task: *mut Task) -> bool {
    if task.is_null() {
        return false;
    }

    unsafe {
        let name = &(*task).name;
        name[0] == b'i'
            && name[1] == b'd'
            && name[2] == b'l'
            && name[3] == b'e'
            && (name[4] == 0 || name[4] == b'_')
    }
}

#[inline]
fn task_is_idle_candidate(task: *mut Task) -> bool {
    if task.is_null() || !task_pointer_is_valid(task) {
        return false;
    }

    unsafe {
        if (*task).task_id == INVALID_TASK_ID {
            return false;
        }
        if (*task).priority != TASK_PRIORITY_IDLE {
            return false;
        }
        if ((*task).flags & TASK_FLAG_KERNEL_MODE) == 0 {
            return false;
        }
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
    // --- FPU save (prev) ---
    if !prev.is_null() {
        unsafe {
            fpu_save(&raw mut (*prev).fpu_state);
        }
    }

    // --- TLB / address-space switch ---
    let is_user_mode = unsafe { (*next).flags & TASK_FLAG_USER_MODE != 0 };
    let old_pid = if !prev.is_null() {
        unsafe { (*prev).process_id }
    } else {
        INVALID_PROCESS_ID
    };
    let new_pid = if is_user_mode {
        let pid = unsafe { (*next).process_id };
        if pid != INVALID_PROCESS_ID {
            pid
        } else {
            INVALID_PROCESS_ID
        }
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
    if is_user_mode {
        let fs = unsafe { (*next).fs_base };
        if fs == 0 || slopos_abi::addr::VirtAddr::is_canonical(fs) {
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, fs);
        } else {
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, 0);
        }
    } else {
        slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, 0);
    }

    // --- TSS RSP0 ---
    let kernel_rsp = if is_user_mode {
        let kst = unsafe { (*next).kernel_stack_top };
        if kst != 0 {
            kst
        } else {
            kernel_stack_top() as u64
        }
    } else {
        kernel_stack_top() as u64
    };
    platform::gdt_set_kernel_rsp0(kernel_rsp);

    // --- CR3 ---
    let next_pid = unsafe { (*next).process_id };
    if next_pid != INVALID_PROCESS_ID {
        process_vm_sync_kernel_mappings(next_pid);
        let cr3_phys = process_vm_get_cr3_phys(next_pid);
        if cr3_phys != 0 {
            let current_cr3 = cpu::read_cr3() & !0xFFF;
            if cr3_phys != current_cr3 {
                cpu::write_cr3(cr3_phys);
            }
        }
    } else {
        unsafe {
            let kernel_dir = paging_get_kernel_directory();
            let kd_phys = (*kernel_dir).pml4_phys.as_u64();
            if kd_phys != 0 {
                let current_cr3 = cpu::read_cr3() & !0xFFF;
                if kd_phys != current_cr3 {
                    cpu::write_cr3(kd_phys);
                }
            }
        }
    }

    // --- FPU restore (next) ---
    unsafe {
        fpu_restore(&raw const (*next).fpu_state);
    }
}

/// Validate that the idle task's switch_ctx has a sane RIP (in kernel .text)
/// and RSP (above USER_SPACE_TOP).
fn ensure_idle_switch_ctx_valid(idle_task: *mut Task) -> bool {
    if idle_task.is_null() {
        return false;
    }
    unsafe {
        let rip = (*idle_task).switch_ctx.rip;
        let rsp = (*idle_task).switch_ctx.rsp;

        let (text_start, text_end) = kernel_text_range();
        let rip_ok = rip >= text_start && rip < text_end;
        let rsp_ok = rsp >= USER_SPACE_TOP;

        if rip_ok && rsp_ok {
            return true;
        }

        klog_info!(
            "SCHED: CPU {} idle task {} has corrupt switch_ctx: rip=0x{:x} (ok={}) rsp=0x{:x} (ok={}) — refusing switch",
            slopos_arch::pcr::get_current_cpu(),
            (*idle_task).task_id,
            rip,
            rip_ok,
            rsp,
            rsp_ok,
        );
        false
    }
}

fn switch_from_current_to_idle(cpu_id: usize, current: *mut Task, idle_task: *mut Task) {
    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, idle_task, timestamp);

    unsafe {
        // Validate the idle context BEFORE publishing it as current_task.
        // Otherwise, other CPUs could observe current_task pointing at an
        // unusable idle context if validation fails.
        if !ensure_idle_switch_ctx_valid(idle_task) {
            klog_info!(
                "SCHED: CPU {} cannot recover idle switch_ctx for task {}",
                cpu_id,
                (*idle_task).task_id
            );
            return;
        }
    }

    set_scheduler_current_task(cpu_id, idle_task);
    slopos_sync::rcu_note_qs();

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
    !task.is_null() && (unsafe { (*task).flags } & TASK_FLAG_NO_PREEMPT != 0)
}

#[inline]
fn consume_time_slice(current: *mut Task) -> bool {
    unsafe {
        if (*current).time_slice_remaining > 0 {
            (*current).time_slice_remaining -= 1;
        }
        (*current).time_slice_remaining > 0
    }
}

#[inline]
fn mark_preempt_if_ready(cpu_id: usize) {
    if scheduler_ready_count(cpu_id) > 0 {
        scheduler_request_reschedule(RescheduleReason::TimerTick);
    }
}

pub fn clear_scheduler_current_task() {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    set_scheduler_current_task(cpu_id, ptr::null_mut());
}

/// Spin until the previous CPU finishes its outgoing context switch for
/// this task.  Matches Linux's `smp_cond_load_acquire(&p->on_cpu, !VAL)`
/// in `try_to_wake_up()`.  Without this, a woken task can be dispatched
/// on CPU B while CPU A is still saving its registers → corruption.
#[inline]
fn wait_task_off_cpu(task: *mut Task) {
    unsafe {
        while (*task).on_cpu.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    wait_task_off_cpu(task);

    if unsafe { (*task).time_slice_remaining } == 0 {
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

    if unsafe { (*task).time_slice_remaining } == 0 {
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

    unsafe {
        let pid = (*to_task).process_id;
        let pid_ok = pid == INVALID_PROCESS_ID
            || (pid as usize) < slopos_mm::memory_layout_defs::MAX_PROCESSES;

        if !pid_ok {
            klog_info!(
                "SCHED: refusing to dispatch task {} with invalid pid {}",
                (*to_task).task_id,
                pid
            );
            let _ = crate::task::task_terminate((*to_task).task_id);
            return;
        }

        // Validate switch_ctx.rip — for kernel tasks it must be in .text,
        // for user tasks ret_from_fork is also in .text.
        let rip = (*to_task).switch_ctx.rip;
        let rsp = (*to_task).switch_ctx.rsp;
        let (text_start, text_end) = kernel_text_range();
        if rip < text_start || rip >= text_end {
            klog_info!(
                "SCHED: refusing to dispatch task {} with switch_ctx.rip=0x{:x} outside .text (0x{:x}..0x{:x})",
                (*to_task).task_id,
                rip,
                text_start,
                text_end,
            );
            let _ = crate::task::task_terminate((*to_task).task_id);
            return;
        }
        // RSP must be in kernel space (above USER_SPACE_TOP)
        if rsp < USER_SPACE_TOP {
            klog_info!(
                "SCHED: refusing to dispatch task {} with switch_ctx.rsp=0x{:x} below kernel space",
                (*to_task).task_id,
                rsp,
            );
            let _ = crate::task::task_terminate((*to_task).task_id);
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
                    (*to_task).task_id,
                    pid,
                );
                let _ = crate::task::task_terminate((*to_task).task_id);
                return;
            }
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, to_task, timestamp);

    // Mark task as physically on this CPU.  schedule_task() spin-waits on
    // this flag before allowing the task to be dispatched elsewhere, preventing
    // the "wake-before-switch-complete" race (Linux p->on_cpu pattern).
    unsafe {
        (*to_task).on_cpu.store(true, Ordering::Release);
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(to_task);
        sched.increment_switches();
    });
    task_set_current(to_task);
    slopos_sync::rcu_note_qs();

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
    let canonical_idle =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
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
    if unsafe { (*next_task).on_cpu.load(Ordering::Acquire) } {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_local(next_task);
            sched.set_executing_task(false);
        });
        return false;
    }

    // Single-winner dispatch claim: only one CPU may run a READY task.
    // If another CPU already claimed it (or state changed), drop this dequeue.
    let next_task_id = unsafe { (*next_task).task_id };
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
    unsafe {
        (*next_task).inc_ref();
    }

    execute_task(cpu_id, idle_task, next_task);

    // Context switch OUT is complete — the task's registers are fully saved.
    // Clear on_cpu so schedule_task() on other CPUs can dispatch this task.
    unsafe {
        (*next_task).on_cpu.store(false, Ordering::Release);
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    set_scheduler_current_task(cpu_id, idle_task);
    slopos_sync::rcu_note_qs();

    switch_to_kernel_address_space(idle_task);

    // Re-enqueue the task if it was preempted (Running) or in a pre-block
    // critical section (WillBlock).  Blocked/Terminated tasks are NOT
    // re-enqueued — they'll be woken by their respective event paths.
    // This runs AFTER on_cpu=false and context save, so no SMP race.
    unsafe {
        if !task_is_terminated(next_task)
            && (task_is_running(next_task) || task_is_will_block(next_task))
            && task_set_state((*next_task).task_id, TaskStatus::Ready) == 0
        {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_local(next_task);
            });
        }
    }

    // Release the dispatch reference.  If the task was re-enqueued above,
    // the queue holds its own reference so the refcnt stays > 0.  If the
    // task was terminated/blocked, this may drop refcnt to 0, allowing the
    // zombie reaper to safely reclaim its resources on the next pass.
    unsafe {
        (*next_task).dec_ref();
    }

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
    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
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
    let task_id = unsafe { (*current).task_id };
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
    let task_id = unsafe { (*current).task_id };
    let _ = task_set_state(task_id, TaskStatus::Running);
}

pub fn block_current_task() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }

    let task_id = unsafe { (*current).task_id };

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
    if task_id == INVALID_TASK_ID || unsafe { (*current).task_id } == task_id {
        return -1;
    }

    let mut target: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut target) != 0 || target.is_null() {
        unsafe {
            (*current)
                .waiting_on
                .store(INVALID_TASK_ID, Ordering::Release)
        };
        return 0;
    }
    unsafe { (*current).waiting_on.store(task_id, Ordering::Release) };
    prepare_to_wait();
    block_current_task();
    finish_wait();
    unsafe {
        (*current)
            .waiting_on
            .store(INVALID_TASK_ID, Ordering::Release)
    };
    0
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let task_id = unsafe { (*task).task_id };

    // Try WillBlock -> Running (task declared intent to block but hasn't yet).
    if task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Running) == 0 {
        return 0;
    }

    // Try Blocked -> Ready (task is fully blocked, wake it).
    if task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready) == 0 {
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
    let result = unsafe {
        (*task).waiting_on.compare_exchange(
            completed_id,      // expected: waiting on the completed task
            INVALID_TASK_ID,   // desired: no longer waiting
            Ordering::AcqRel,  // success: acquire prior writes, release our write
            Ordering::Acquire, // failure: just acquire to see current value
        )
    };

    match result {
        Ok(_) => {
            // We won the race! Now transition state and enqueue
            // CAS: BLOCKED -> READY (single-winner state transition)
            if task_set_state(unsafe { (*task).task_id }, TaskStatus::Ready) != 0 {
                // State changed unexpectedly - task may be terminated or already ready
                // Check if it's a real failure
                if task_is_terminated(task) || task_is_invalid(task) {
                    klog_info!(
                        "try_wake_from_task_wait: task {} state transition failed (terminated/invalid)",
                        unsafe { (*task).task_id }
                    );
                    return false;
                }
                // Task is already ready/running - that's fine, we still "won" the CAS
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            // Enqueue the task
            if schedule_task(task) != 0 {
                klog_info!(
                    "try_wake_from_task_wait: failed to schedule task {}",
                    unsafe { (*task).task_id }
                );
            }
            true
        }
        Err(_current) => {
            // Lost race OR task is waiting on different ID
            // Either way, not our responsibility to wake
            false
        }
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

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(ptr::null_mut());
    });
    task_set_current(ptr::null_mut());

    // All CPUs use the unified schedule() path which switches to idle
    schedule();

    klog_info!(
        "scheduler_task_exit: Schedule returned unexpectedly on CPU {}",
        cpu_id
    );
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
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

    user_copy::register_current_task_provider(current_task_process_id);

    per_cpu::init_all_percpu_schedulers();
    reset_sleep_queue();

    slopos_sync::preempt::register_reschedule_callback(deferred_reschedule_callback);

    slopos_utils::panic_recovery::register_panic_cleanup(sched_panic_cleanup);

    0
}

fn sched_panic_cleanup() {
    // SAFETY: Called from the panic recovery path after longjmp. The lock
    // may have been held when the panic occurred and the guard was lost.
    // We poison-unlock to mark the data as potentially inconsistent; the
    // scheduler reinit path checks is_poisoned() before accepting operations.
    unsafe {
        scheduler_force_unlock();
        crate::task::task_manager_poison_unlock();
    }
}

pub fn scheduler_is_enabled() -> c_int {
    SCHEDULER_ENABLED.load(Ordering::Acquire) as c_int
}

pub fn scheduler_get_current_task() -> *mut Task {
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task()).unwrap_or(ptr::null_mut())
}

pub fn current_task_id() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).task_id }
}

pub fn current_task_pgid() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).pgid }
}

/// Get the current task's session ID (SID).
///
/// Returns 0 if there is no current task or the scheduler is not yet active.
pub fn current_task_sid() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).sid }
}

pub fn current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex> {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return None;
    }
    unsafe { (*task).controlling_tty }
}

pub fn set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return false;
    }
    unsafe {
        (*task).controlling_tty = tty;
    }
    true
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
    // Unconditional QS: the timer ISR firing proves this CPU is not
    // inside an RCU read-side critical section (those disable preemption
    // but not interrupts).  Matches Linux rcu_sched_clock_irq().
    slopos_sync::rcu_note_qs();

    let cpu_id = slopos_arch::pcr::get_current_cpu();

    // Raise the deferred-callback softirq flag on CPU 0 only.
    // rcu_process_callbacks() runs later from the idle loop, not here.
    if cpu_id == 0 {
        slopos_sync::rcu_raise_softirq();
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

pub unsafe fn scheduler_force_unlock() {
    // No global scheduler mutex to unlock - per-CPU schedulers use lockless atomics
}
