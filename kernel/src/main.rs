#![no_std]
#![no_main]
#![forbid(unsafe_op_in_unsafe_fn)]
#![feature(alloc_error_handler)]

extern crate alloc;

use core::alloc::Layout;
use core::panic::PanicInfo;

use slopos_boot as boot;
use slopos_drivers::{
    fate::{detonate, RouletteOutcome, Wheel},
    interrupts,
    serial,
    serial_println,
    wl_currency,
};
use slopos_mm::{self as mm, BumpAllocator};
use slopos_video as video;
use slopos_fs as fs;
use slopos_userland as userland;
use slopos_lib::cpu;

#[global_allocator]
static GLOBAL_ALLOCATOR: BumpAllocator = BumpAllocator::new();

#[no_mangle]
pub static kernel_stack_top: u8 = 0;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    serial::init();
    serial_println!("Allocation failure: {layout:?}");
    wl_currency::award_loss();
    cpu::halt_loop();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::init();
    serial_println!("Kernel panic: {info}");
    wl_currency::award_loss();
    cpu::halt_loop();
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    boot::ensure_base_revision();
    let _ = boot::early_init::boot_init_run_all();
    let boot_info = boot::boot_info();

    serial::init();
    serial_println!("SlopOS (Rust rewrite) has washed ashore on Sloptopia.");
    serial_println!(
        "HHDM offset: 0x{:x} | memmap entries: {}",
        boot_info.hhdm_offset,
        boot_info.memmap_entries
    );

    wl_currency::reset();
    mm::init(boot_info.hhdm_offset);
    wl_currency::award_win();

    let mut wheel = Wheel::new();
    match wheel.spin() {
        RouletteOutcome::Survive => {
            serial_println!("The wizards live to gamble another spin.");
        }
        RouletteOutcome::Panic => {
            serial_println!("L bozzo lol");
            detonate();
        }
    }

    video::init(boot_info.framebuffer);
    fs::ramfs_init();
    userland::init();

    let itests_cfg = interrupts::config_from_cmdline(boot_info.cmdline);
    interrupts::run(&itests_cfg);

    unsafe {
        slopos_sched::init_scheduler();
        slopos_sched::create_idle_task();
        slopos_sched::start_scheduler();
    }
    cpu::halt_loop()
}

