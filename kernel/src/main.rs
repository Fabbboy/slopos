#![no_std]
#![no_main]
#![forbid(unsafe_op_in_unsafe_fn)]
#![feature(alloc_error_handler)]
#![allow(bad_asm_style)]

extern crate alloc;

use core::alloc::Layout;
use core::arch::global_asm;
use core::panic::PanicInfo;

use slopos_drivers::serial;
use slopos_ostd::mm::heap::KernelHeap;
mod ffi;

#[global_allocator]
static GLOBAL_ALLOCATOR: KernelHeap = KernelHeap;

global_asm!(include_str!("../../boot/limine_entry.s"));

// Ensure the boot crate is linked so kernel_main is available for the assembly entry.
#[used]
static BOOT_LINK_GUARD: ffi::BootEntry = ffi::BOOT_ENTRY;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    serial::init();
    panic!(
        "OOM: allocation failed for size={} align={}",
        layout.size(),
        layout.align()
    );
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    #[cfg(feature = "tests")]
    {
        if !slopos_ostd::panic_recovery::recovery_is_active() {
            slopos_testing::tests_mark_panic();
        }
    }
    slopos_boot::panic_handler_impl(info)
}
