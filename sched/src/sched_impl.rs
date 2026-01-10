use core::ffi::c_int;
use slopos_abi::sched_traits::{FateResult, OpaqueTask, SchedulerServices, TaskRef};

use crate::fate_api;
use crate::scheduler;
use crate::task;

pub struct SchedImpl;

pub static SCHED_IMPL: SchedImpl = SchedImpl;

unsafe impl Send for SchedImpl {}
unsafe impl Sync for SchedImpl {}

impl SchedulerServices for SchedImpl {
    fn timer_tick(&self) {
        scheduler::scheduler_timer_tick();
    }

    fn handle_post_irq(&self) {
        scheduler::scheduler_handle_post_irq();
    }

    fn request_reschedule_from_interrupt(&self) {
        scheduler::scheduler_request_reschedule_from_interrupt();
    }

    fn get_current_task(&self) -> TaskRef {
        TaskRef::from_raw(scheduler::scheduler_get_current_task() as *mut _)
    }

    fn yield_cpu(&self) {
        scheduler::yield_();
    }

    fn schedule(&self) {
        scheduler::schedule();
    }

    fn task_terminate(&self, task_id: u32) -> c_int {
        task::task_terminate(task_id)
    }

    fn block_current_task(&self) {
        scheduler::block_current_task();
    }

    fn task_is_blocked(&self, task: TaskRef) -> bool {
        task::task_is_blocked(task.as_raw() as *const task::Task)
    }

    fn unblock_task(&self, task: TaskRef) -> c_int {
        scheduler::unblock_task(task.as_raw() as *mut task::Task)
    }

    fn is_enabled(&self) -> c_int {
        scheduler::scheduler_is_enabled()
    }

    fn is_preemption_enabled(&self) -> c_int {
        scheduler::scheduler_is_preemption_enabled()
    }

    fn get_task_stats(&self) -> (u32, u32, u64) {
        let mut total = 0u32;
        let mut active = 0u32;
        let mut switches = 0u64;
        task::get_task_stats(&mut total, &mut active, &mut switches);
        (total, active, switches)
    }

    fn get_scheduler_stats(&self) -> (u64, u64, u32, u32) {
        let mut switches = 0u64;
        let mut yields = 0u64;
        let mut ready = 0u32;
        let mut calls = 0u32;
        scheduler::get_scheduler_stats(&mut switches, &mut yields, &mut ready, &mut calls);
        (switches, yields, ready, calls)
    }

    fn register_idle_wakeup_callback(&self, cb: Option<fn() -> c_int>) {
        scheduler::scheduler_register_idle_wakeup_callback(cb);
    }

    fn fate_spin(&self) -> FateResult {
        fate_api::fate_spin()
    }

    fn fate_set_pending(&self, res: FateResult, task_id: u32) -> c_int {
        fate_api::fate_set_pending(res, task_id)
    }

    fn fate_take_pending(&self, task_id: u32) -> Option<FateResult> {
        let mut out = FateResult::default();
        if fate_api::fate_take_pending(task_id, &mut out) == 0 {
            Some(out)
        } else {
            None
        }
    }

    fn fate_apply_outcome(&self, res: &FateResult, resolution: u32, award: bool) {
        fate_api::fate_apply_outcome(res as *const FateResult, resolution, award);
    }

    fn get_current_task_opaque(&self) -> *mut OpaqueTask {
        scheduler::scheduler_get_current_task() as *mut OpaqueTask
    }
}

pub fn register_with_bridge() {
    slopos_drivers::sched_bridge::init_scheduler(&SCHED_IMPL);
}
