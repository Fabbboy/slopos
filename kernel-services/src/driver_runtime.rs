use core::ffi::c_int;

use slopos_arch::InterruptFrame;

pub const LEGACY_IRQ_TIMER: u8 = 0;
pub const LEGACY_IRQ_KEYBOARD: u8 = 1;
pub const LEGACY_IRQ_COM1: u8 = 4;
pub const LEGACY_IRQ_MOUSE: u8 = 12;
pub const IRQ_LINES: usize = 16;

slopos_service_core::define_service! {
    driver_runtime => DriverRuntimeServices {
        save_preempt_context(frame: *mut InterruptFrame);
        scheduler_timer_tick();
        scheduler_handle_timer_interrupt(frame: *mut InterruptFrame);
        request_reschedule_from_interrupt();
        scheduler_is_enabled() -> c_int;
        current_task_id() -> u32;
        current_task_handle() -> u32;
        current_task_pgid() -> u32;
        current_task_sid() -> u32;
        current_task_controlling_tty() -> Option<slopos_abi::syscall::TtyIndex>;
        set_current_task_controlling_tty(tty: Option<slopos_abi::syscall::TtyIndex>) -> bool;
        clear_session_controlling_tty(session_id: u32, tty: slopos_abi::syscall::TtyIndex) -> usize;
        block_current_task_with_timeout(timeout_ms: u32);
        sleep_current_task_ms(ms: u32) -> c_int;
        mark_current_blocked() -> bool;
        yield_blocked_task();
        yield_blocked_task_with_timeout(timeout_ms: u32);
        set_current_runnable();
        unblock_task(task_id: u32) -> c_int;
        swap_parked_wait_queue(queue: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
        register_idle_wakeup_callback(callback: Option<fn() -> c_int>);
        signal_process_group(pgid: u32, signum: u8) -> bool;
        signal_session(sid: u32, signum: u8) -> bool;
        pgrp_handle(pgid: u32) -> Option<slopos_ostd::KWeak<slopos_ostd::task::ProcessGroup>>;
        session_handle(sid: u32) -> Option<slopos_ostd::KWeak<slopos_ostd::task::Session>>;
        current_task_pgrp_handle() -> Option<slopos_ostd::KWeak<slopos_ostd::task::ProcessGroup>>;
        pgrp_exists_in_session(pgid: u32, sid: u32) -> bool;
        is_current_signal_blocked_or_ignored(signum: u8) -> bool;
        is_pgrp_orphaned(pgid: u32, sid: u32) -> bool;
        has_pending_signal() -> bool;
        debug_dump_tasks();

        irq_init();
        irq_set_route(irq_line: u8, gsi: u32);
        irq_is_masked(irq_line: u8) -> bool;
        irq_enable_line(irq_line: u8);
        irq_disable_line(irq_line: u8);
        irq_get_timer_ticks() -> u64;
        irq_increment_timer_ticks();
        irq_increment_keyboard_events();
    }
}
