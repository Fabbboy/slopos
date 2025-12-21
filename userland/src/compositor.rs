use core::ffi::c_void;

use crate::syscall::{sys_compositor_present, sys_sleep_ms, sys_yield};

#[unsafe(link_section = ".user_text")]
pub fn compositor_user_main(_arg: *mut c_void) {
    loop {
        let _ = sys_compositor_present();
        sys_sleep_ms(16);
        sys_yield();
    }
}
