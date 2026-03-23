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
    init_scheduler_for_ap, scheduler_shutdown, send_reschedule_ipi, stop_scheduler,
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
    TASK_FLAG_USER_MODE, TASK_PRIORITY_IDLE, Task, TaskContext, TaskStatus, task_get_info,
    task_is_blocked, task_is_invalid, task_is_ready, task_is_running, task_is_terminated,
    task_is_will_block, task_pointer_is_valid, task_record_context_switch, task_record_yield,
    task_set_current, task_set_state,
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
use slopos_mm::process_vm::{process_vm_get_page_dir, process_vm_sync_kernel_mappings};
use slopos_mm::tlb;
use slopos_mm::user_copy;

use super::ffi_boundary::{
    context_switch, context_switch_user, kernel_stack_top, task_entry_wrapper,
};

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

fn switch_to_kernel_address_space(task: *mut Task) {
    tlb::enter_lazy_tlb(slopos_arch::pcr::get_current_cpu());
    unsafe {
        let kernel_dir = paging_get_kernel_directory();
        if !(*kernel_dir).pml4_phys.is_null() && !task.is_null() {
            (*task).context.cr3 = (*kernel_dir).pml4_phys.as_u64();
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

fn switch_from_current_to_idle(
    cpu_id: usize,
    current: *mut Task,
    idle_task: *mut Task,
    allow_user_frame_resume: bool,
) {
    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, idle_task, timestamp);

    set_scheduler_current_task(cpu_id, idle_task);
    switch_to_kernel_address_space(idle_task);

    unsafe {
        let old_pid = if !current.is_null() {
            (*current).process_id
        } else {
            INVALID_PROCESS_ID
        };
        tlb::notify_mm_switch(old_pid, INVALID_PROCESS_ID, cpu_id);

        if !ensure_idle_context_valid(idle_task) {
            klog_info!(
                "SCHED: CPU {} cannot recover idle context for task {}",
                cpu_id,
                (*idle_task).task_id
            );
            return;
        }

        // Only skip saving kernel context when schedule() is running on an
        // interrupt-trap exit path and we have a valid user interrupt-frame
        // snapshot to resume from via IRET.
        //
        // For all non-trap call sites (yield/block/deferred callback), we must
        // save a kernel resume point here.
        let mut current_ctx = ptr::null_mut();
        if !current.is_null() {
            let is_user = (*current).flags & TASK_FLAG_USER_MODE != 0;
            let has_user_resume = is_user && (*current).context_from_user != 0;
            let in_syscall_block_path = is_user && ((*current).flags & TASK_FLAG_NO_PREEMPT != 0);
            let can_resume_from_user_frame =
                allow_user_frame_resume && has_user_resume && !in_syscall_block_path;

            if !can_resume_from_user_frame {
                if is_user {
                    (*current).context_from_user = 0;
                }
                current_ctx = &raw mut (*current).context;
            }
        }

        let idle_ctx = &raw const (*idle_task).context;
        context_switch(current_ctx, idle_ctx);
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

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

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
/// Handles page directory setup, TSS RSP0, context validation, and actual switch.
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
        let ctx = &(*to_task).context;
        let dispatch_user = (ctx.cs & 3) == 3;
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

        if dispatch_user {
            validate_user_context(ctx, to_task);
        } else if !validate_kernel_context(ctx, to_task) {
            klog_info!(
                "SCHED: refusing to dispatch task {} with invalid kernel context",
                (*to_task).task_id,
            );
            let _ = crate::task::task_terminate((*to_task).task_id);
            return;
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, to_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(to_task);
        sched.increment_switches();
    });
    task_set_current(to_task);

    unsafe {
        let is_user_mode = (*to_task).flags & TASK_FLAG_USER_MODE != 0;
        let old_pid = if !from_task.is_null() {
            (*from_task).process_id
        } else {
            INVALID_PROCESS_ID
        };
        let new_pid = if is_user_mode && (*to_task).process_id != INVALID_PROCESS_ID {
            (*to_task).process_id
        } else {
            INVALID_PROCESS_ID
        };
        tlb::notify_mm_switch(old_pid, new_pid, cpu_id);
        if is_user_mode {
            tlb::exit_lazy_tlb(cpu_id);
        } else {
            tlb::enter_lazy_tlb(cpu_id);
        }

        if is_user_mode {
            let fs = (*to_task).fs_base;
            if fs == 0 || slopos_abi::addr::VirtAddr::is_canonical(fs) {
                slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, fs);
            } else {
                slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, 0);
            }
        } else {
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, 0);
        }

        let kernel_rsp = if is_user_mode && (*to_task).kernel_stack_top != 0 {
            (*to_task).kernel_stack_top
        } else {
            kernel_stack_top() as u64
        };
        platform::gdt_set_kernel_rsp0(kernel_rsp);

        if (*to_task).process_id != INVALID_PROCESS_ID {
            if is_user_mode {
                process_vm_sync_kernel_mappings((*to_task).process_id);
            }
            let page_dir = process_vm_get_page_dir((*to_task).process_id);
            if !page_dir.is_null() && (page_dir as u64) < USER_SPACE_TOP {
                klog_info!(
                    "SCHED: task {} has invalid page_dir pointer 0x{:x}",
                    (*to_task).task_id,
                    page_dir as u64
                );
                let _ = crate::task::task_terminate((*to_task).task_id);
                return;
            }
            if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
                (*to_task).context.cr3 = (*page_dir).pml4_phys.as_u64();
            }
        } else {
            let kernel_dir = paging_get_kernel_directory();
            let kd_phys = (*kernel_dir).pml4_phys.as_u64();
            if kd_phys != 0 {
                (*to_task).context.cr3 = kd_phys;
            }
        }

        let old_ctx_ptr = if !from_task.is_null() && (*from_task).context_from_user == 0 {
            &raw mut (*from_task).context
        } else {
            ptr::null_mut()
        };

        if (*to_task).context.cs & 3 == 3 {
            context_switch_user(old_ctx_ptr, &(*to_task).context);
        } else {
            context_switch(old_ctx_ptr, &(*to_task).context);
        }
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

    // Single-winner dispatch claim: only one CPU may run a READY task.
    // If another CPU already claimed it (or state changed), drop this dequeue.
    let next_task_id = unsafe { (*next_task).task_id };
    if task_set_state(next_task_id, TaskStatus::Running) != 0 {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    execute_task(cpu_id, idle_task, next_task);

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    set_scheduler_current_task(cpu_id, idle_task);

    switch_to_kernel_address_space(idle_task);

    unsafe {
        if !task_is_terminated(next_task)
            && task_is_running(next_task)
            && task_set_state((*next_task).task_id, TaskStatus::Ready) == 0
        {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_local(next_task);
            });
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });

    true
}

/// Validate that a user-mode task's context has sane values before iretq.
/// Catches context corruption early with a clear panic message rather than
/// a mysterious Invalid Opcode at a garbage address.
#[inline]
fn validate_user_context(ctx: &TaskContext, task: *const Task) {
    let cs = ctx.cs;
    let ss = ctx.ss;
    let rip = ctx.rip;
    let rsp = ctx.rsp;

    // CS and SS must have user RPL (ring 3, bits 0:1 == 3)
    let cs_ok = (cs & 3) == 3;
    let ss_ok = (ss & 3) == 3;
    // RIP must be in user VA range (below kernel half)
    let rip_ok = rip < USER_SPACE_TOP;
    // RSP must be in user VA range
    let rsp_ok = rsp < USER_SPACE_TOP;

    if cs_ok && ss_ok && rip_ok && rsp_ok {
        return;
    }

    let task_id = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };
    let cfu = if task.is_null() {
        0
    } else {
        unsafe { (*task).context_from_user }
    };

    let cr3 = ctx.cr3;
    klog_info!(
        "validate_user_context: INVALID context for task {} (cfu={}): \
         cs=0x{:x} (ok={}) ss=0x{:x} (ok={}) rip=0x{:x} (ok={}) rsp=0x{:x} (ok={}) cr3=0x{:x}",
        task_id,
        cfu,
        cs,
        cs_ok,
        ss,
        ss_ok,
        rip,
        rip_ok,
        rsp,
        rsp_ok,
        cr3
    );
    panic!(
        "validate_user_context: corrupt context for task {} (cfu={}): \
         cs=0x{:x} ss=0x{:x} rip=0x{:x} rsp=0x{:x} cr3=0x{:x}",
        task_id, cfu, cs, ss, rip, rsp, cr3
    );
}

fn repair_idle_context(idle_task: *mut Task) -> bool {
    if idle_task.is_null() {
        return false;
    }

    unsafe {
        let ctx = &mut (*idle_task).context;
        *ctx = TaskContext::zero();
        ctx.rflags = 0x202;
        ctx.rip = task_entry_wrapper as *const () as u64;
        ctx.rdi = (*idle_task).entry_point;
        ctx.rsi = (*idle_task).entry_arg as u64;
        ctx.rsp = if (*idle_task).stack_pointer != 0 {
            (*idle_task).stack_pointer
        } else {
            (*idle_task)
                .stack_base
                .saturating_add((*idle_task).stack_size)
                .saturating_sub(8)
        };
        ctx.cs = 0x08;
        ctx.ds = 0x10;
        ctx.es = 0x10;
        ctx.fs = 0;
        ctx.gs = 0;
        ctx.ss = 0x10;
        (*idle_task).context_from_user = 0;

        let kernel_dir = paging_get_kernel_directory();
        if !(*kernel_dir).pml4_phys.is_null() {
            ctx.cr3 = (*kernel_dir).pml4_phys.as_u64();
        }
    }

    true
}

fn ensure_idle_context_valid(idle_task: *mut Task) -> bool {
    if idle_task.is_null() {
        return false;
    }

    unsafe {
        if validate_kernel_context(&(*idle_task).context, idle_task) {
            return true;
        }

        let ctx_cs = core::ptr::read_unaligned(core::ptr::addr_of!((*idle_task).context.cs));
        let ctx_rip = core::ptr::read_unaligned(core::ptr::addr_of!((*idle_task).context.rip));
        let ctx_rsp = core::ptr::read_unaligned(core::ptr::addr_of!((*idle_task).context.rsp));
        let ctx_cr3 = core::ptr::read_unaligned(core::ptr::addr_of!((*idle_task).context.cr3));

        klog_info!(
            "SCHED: repairing idle task {} corrupt context: cs=0x{:x} rip=0x{:x} rsp=0x{:x} cr3=0x{:x}",
            (*idle_task).task_id,
            ctx_cs,
            ctx_rip,
            ctx_rsp,
            ctx_cr3
        );

        if !repair_idle_context(idle_task) {
            return false;
        }

        validate_kernel_context(&(*idle_task).context, idle_task)
    }
}

/// Validate that a kernel-mode resume context has sane values before retq.
///
/// Returns `false` for obviously-corrupted contexts so callers can terminate
/// the offending task instead of jumping into invalid RIP/RSP.
#[inline]
fn validate_kernel_context(ctx: &TaskContext, task: *const Task) -> bool {
    let cs = ctx.cs;
    let rip = ctx.rip;
    let rsp = ctx.rsp;
    let cr3 = ctx.cr3;
    let (text_start, text_end) = kernel_text_range();

    let cs_ok = (cs & 3) == 0;
    let rip_ok = rip >= text_start && rip < text_end;
    let rsp_ok = rsp >= USER_SPACE_TOP;
    let cr3_ok = cr3 != 0;

    if cs_ok && rip_ok && rsp_ok && cr3_ok {
        return true;
    }

    let task_id = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };

    klog_info!(
        "validate_kernel_context: INVALID context for task {}: cs=0x{:x} (ok={}) rip=0x{:x} (ok={}) rsp=0x{:x} (ok={}) cr3=0x{:x} (ok={})",
        task_id,
        cs,
        cs_ok,
        rip,
        rip_ok,
        rsp,
        rsp_ok,
        cr3,
        cr3_ok
    );
    false
}

fn schedule_internal(allow_user_frame_resume: bool) {
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

    requeue_running_task(cpu_id, current);
    switch_from_current_to_idle(cpu_id, current, idle_task, allow_user_frame_resume);
    cpu::restore_flags(irq_flags);
}

pub(crate) fn schedule_from_trap_exit() {
    schedule_internal(true);
}

pub fn schedule() {
    schedule_internal(false);
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

    // Only block if still in WillBlock (set by prepare_to_wait).
    // If Running, the wakeup already arrived — do not re-block.
    if !task_is_will_block(current) {
        return;
    }

    let task_id = unsafe { (*current).task_id };
    if task_set_state(task_id, TaskStatus::Blocked) != 0 {
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

    if task_is_will_block(task) {
        let task_id = unsafe { (*task).task_id };
        let _ = task_set_state(task_id, TaskStatus::Running);
        return 0;
    }

    if task_is_blocked(task) {
        if task_set_state(unsafe { (*task).task_id }, TaskStatus::Ready) != 0 {
            if task_is_terminated(task) || task_is_invalid(task) {
                return -1;
            }
            return 0;
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        return schedule_task(task);
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
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let (current, idle_task) = scheduler_tasks_for_cpu(cpu_id);

    let preempt_active = PreemptGuard::is_active();
    let running_idle = !current.is_null() && current == idle_task;

    if preempt_active && !running_idle {
        scheduler_request_reschedule(RescheduleReason::TimerTick);
        return;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
        sched.increment_ticks();
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
