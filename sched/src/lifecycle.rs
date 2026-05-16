use core::sync::atomic::Ordering;

use slopos_ostd::klog_info;

use super::per_cpu;
use super::runtime::{create_idle_task, create_idle_task_for_cpu};
use super::scheduler::{init_scheduler, install_reschedule_callback, set_scheduler_enabled};
use super::sleep::reset_sleep_queue;

pub fn stop_scheduler() {
    set_scheduler_enabled(false);
}

pub fn scheduler_enable() {
    set_scheduler_enabled(true);
}

pub fn scheduler_shutdown() {
    set_scheduler_enabled(false);
    reset_sleep_queue();
}

pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    use slopos_ostd::util::ptr_buf::nullable_write;
    nullable_write(context_switches, per_cpu::get_total_switches());
    nullable_write(yields, per_cpu::get_total_yields());
    nullable_write(schedule_calls, per_cpu::get_total_schedule_calls());
    nullable_write(ready_tasks, per_cpu::get_total_ready_tasks());
}

pub fn boot_step_task_manager_init(
    _ctx: &mut slopos_hermetic::BootCtx<'_, slopos_hermetic::BspInit>,
) -> i32 {
    crate::task::init_task_manager()
}

pub fn boot_step_scheduler_init(
    ctx: &mut slopos_hermetic::BootCtx<'_, slopos_hermetic::BspInit>,
) -> i32 {
    let rc = init_scheduler();
    if rc != 0 {
        return rc;
    }
    install_reschedule_callback(&ctx.bsp_token());
    0
}

pub fn boot_step_idle_task(
    _ctx: &mut slopos_hermetic::BootCtx<'_, slopos_hermetic::BspInit>,
) -> i32 {
    create_idle_task()
}

pub fn init_scheduler_for_ap(cpu_id: usize) {
    per_cpu::init_percpu_scheduler(cpu_id);

    if create_idle_task_for_cpu(cpu_id) != 0 {
        klog_info!(
            "SCHED: Warning - failed to create idle task for CPU {}",
            cpu_id
        );
    }
}

pub fn get_percpu_scheduler_stats(
    cpu_id: usize,
    switches: *mut u64,
    preemptions: *mut u64,
    ready_tasks: *mut u32,
) {
    use slopos_ostd::util::ptr_buf::nullable_write;
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        nullable_write(switches, sched.total_switches.load(Ordering::Relaxed));
        nullable_write(preemptions, sched.total_preemptions.load(Ordering::Relaxed));
        nullable_write(ready_tasks, sched.total_ready_count());
    });
}

pub fn get_total_ready_tasks_all_cpus() -> u32 {
    per_cpu::get_total_ready_tasks()
}

pub fn send_reschedule_ipi(target_cpu: usize) {
    use slopos_arch::arch::idt::RESCHEDULE_IPI_VECTOR;

    let current_cpu = slopos_arch::pcr::get_current_cpu();
    if target_cpu == current_cpu {
        return;
    }

    if let Some(apic_id) = slopos_arch::pcr::apic_id_from_cpu_index(target_cpu) {
        slopos_arch::pcr::send_ipi_to_cpu(apic_id, RESCHEDULE_IPI_VECTOR);
    }
}
