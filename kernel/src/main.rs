#![no_std]
#![no_main]
#![forbid(unsafe_op_in_unsafe_fn)]
#![feature(alloc_error_handler)]
#![allow(bad_asm_style)]

extern crate alloc;

use core::alloc::Layout;
use core::arch::global_asm;
use core::panic::PanicInfo;

use slopos_core as sched;
use slopos_drivers::serial;
use slopos_fs as fs;
use slopos_lib::{cpu, klog_error};
use slopos_mm::BumpAllocator;
use slopos_userland as userland;
mod ffi;
use slopos_video as video;

#[global_allocator]
static GLOBAL_ALLOCATOR: BumpAllocator = BumpAllocator::new();

// Include the Limine assembly trampoline that sets up stack + serial and jumps into kernel_main.
global_asm!(include_str!("../../boot/limine_entry.s"));

// Ensure the boot crate is linked so kernel_main is available for the assembly entry.
#[used]
static BOOT_LINK_GUARD: ffi::BootEntry = ffi::BOOT_ENTRY;

// Force-link userland so its boot init steps (roulette/shell) stay in the image.
#[used]
static USERLAND_LINK_GUARD: fn() = userland::init;

// Pull in other subsystems that the boot crate expects to call by making a volatile reference to them.
fn __link_boot_deps() {
    unsafe {
        core::ptr::read_volatile(&((sched::scheduler_shutdown as *const ()) as usize));
        core::ptr::read_volatile(&((sched::task_shutdown_all as *const ()) as usize));
        core::ptr::read_volatile(&((sched::task_set_current as *const ()) as usize));
        core::ptr::read_volatile(&((sched::boot_step_idle_task as *const ()) as usize));
        core::ptr::read_volatile(&((video::framebuffer::get_display_info as *const ()) as usize));
        core::ptr::read_volatile(
            &((video::framebuffer::framebuffer_is_initialized as *const ()) as usize),
        );
        core::ptr::read_volatile(&((sched::boot_step_scheduler_init as *const ()) as usize));
        core::ptr::read_volatile(&((sched::boot_step_task_manager_init as *const ()) as usize));
        core::ptr::read_volatile(&((sched::scheduler_get_current_task as *const ()) as usize));
        core::ptr::read_volatile(&((sched::task_terminate as *const ()) as usize));
        core::ptr::read_volatile(
            &((sched::scheduler_request_reschedule_from_interrupt as *const ()) as usize),
        );
        core::ptr::read_volatile(&((sched::start_scheduler as *const ()) as usize));
        core::ptr::read_volatile(&((sched::scheduler_timer_tick as *const ()) as usize));
        core::ptr::read_volatile(&((fs::fileio_create_table_for_process as *const ()) as usize));
        core::ptr::read_volatile(&((fs::fileio_destroy_table_for_process as *const ()) as usize));
    }
}

#[used]
static FORCE_LINK_BOOT_DEPS: fn() = __link_boot_deps;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    serial::init();
    klog_error!("Allocation failure: {:?}", layout);
    cpu::halt_loop();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    #[cfg(feature = "builtin-tests")]
    {
        if !slopos_lib::panic_recovery::recovery_is_active() {
            slopos_tests::tests_mark_panic();
        }
    }
    slopos_boot::panic_handler_impl(info)
}
