use core::ffi::{c_char, c_int, c_void};

use crate::InterruptFrame;

pub type DriverTaskHandle = *mut c_void;
pub type DriverIrqHandler = extern "C" fn(u8, *mut InterruptFrame, *mut c_void);

pub const LEGACY_IRQ_TIMER: u8 = 0;
pub const LEGACY_IRQ_KEYBOARD: u8 = 1;
pub const LEGACY_IRQ_COM1: u8 = 4;
pub const LEGACY_IRQ_MOUSE: u8 = 12;
pub const IRQ_LINES: usize = 16;

crate::define_service! {
    driver_runtime => DriverRuntimeServices {
        save_preempt_context(frame: *mut InterruptFrame);
        scheduler_timer_tick();
        scheduler_handle_timer_interrupt(frame: *mut InterruptFrame);
        request_reschedule_from_interrupt();
        scheduler_is_enabled() -> c_int;
        current_task() -> DriverTaskHandle;
        current_task_id() -> u32;
        current_task_pgid() -> u32;
        current_task_sid() -> u32;
        current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex>;
        set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool;
        clear_session_controlling_tty(session_id: u32, tty: slopos_abi::syscall::TtyIndex) -> usize;
        block_current_task();
        unblock_task(task: DriverTaskHandle) -> c_int;
        register_idle_wakeup_callback(callback: Option<fn() -> c_int>);
        signal_process_group(pgid: u32, signum: u8) -> bool;
        signal_session(sid: u32, signum: u8) -> bool;
        pgrp_exists_in_session(pgid: u32, sid: u32) -> bool;
        is_current_signal_blocked_or_ignored(signum: u8) -> bool;
        is_pgrp_orphaned(pgid: u32, sid: u32) -> bool;

        irq_init();
        irq_set_route(irq_line: u8, gsi: u32);
        irq_is_masked(irq_line: u8) -> bool;
        @no_wrapper irq_register_handler(irq_line: u8, handler: Option<DriverIrqHandler>, context: *mut c_void, name: *const c_char) -> i32;
        irq_enable_line(irq_line: u8);
        irq_disable_line(irq_line: u8);
        irq_get_timer_ticks() -> u64;
        irq_increment_timer_ticks();
        irq_increment_keyboard_events();
    }
}

/// Manual wrapper for the `@no_wrapper` service method.
#[inline(always)]
pub fn irq_register_handler(
    irq_line: u8,
    handler: Option<DriverIrqHandler>,
    context: *mut c_void,
    name: *const c_char,
) -> i32 {
    (driver_runtime_services().irq_register_handler)(irq_line, handler, context, name)
}
