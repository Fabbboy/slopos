#![no_std]
#![no_main]

use slopos_userland::apps::sysinfo::sysinfo_main;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    sysinfo_main(core::ptr::null_mut());
    loop {}
}
