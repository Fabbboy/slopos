#![no_std]
#![no_main]

use slopos_userland::compositor::compositor_user_main;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    compositor_user_main(core::ptr::null_mut());
    loop {}
}
