use core::ffi::c_void;

use slopos_lib::{klog_printf, KlogLevel};

use crate::early_init::{boot_init_optional_step, boot_init_priority, boot_init_step_with_flags};

#[repr(C)]
struct FramebufferInfo {
    physical_addr: u64,
    virtual_addr: *mut c_void,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
    pixel_format: u8,
    buffer_size: u32,
    initialized: u8,
}

extern "C" {
    fn boot_mark_initialized();
    fn framebuffer_get_info() -> *mut FramebufferInfo;
    fn framebuffer_is_initialized() -> i32;
}

fn log(level: KlogLevel, msg: &[u8]) {
    unsafe { klog_printf(level, msg.as_ptr() as *const c_char) };
}

fn log_info(msg: &[u8]) {
    log(KlogLevel::Info, msg);
}

fn log_debug(msg: &[u8]) {
    log(KlogLevel::Debug, msg);
}

extern "C" fn boot_step_mark_kernel_ready() -> i32 {
    unsafe { boot_mark_initialized() };
    log_info(b"Kernel core services initialized.\0");
    0
}

extern "C" fn boot_step_framebuffer_demo() -> i32 {
    let fb_info = unsafe { framebuffer_get_info() };
    if fb_info.is_null() || unsafe { framebuffer_is_initialized() } == 0 {
        log_info(b"Graphics demo: framebuffer not initialized, skipping\0");
        return 0;
    }

    let fb = unsafe { &*fb_info };
    if !fb.virtual_addr.is_null() && (fb.virtual_addr as u64) != fb.physical_addr {
        unsafe {
            klog_printf(
                KlogLevel::Debug,
                b"Graphics: Framebuffer using translated virtual address 0x%lx (translation verified)\n\0"
                    .as_ptr() as *const core::ffi::c_char,
                fb.virtual_addr as u64,
            );
        }
    }

    log_debug(b"Graphics demo: framebuffer validation complete\0");
    0
}

boot_init_step_with_flags!(
    services,
    b"mark ready\0",
    boot_step_mark_kernel_ready,
    boot_init_priority(60)
);
boot_init_optional_step!(optional, b"framebuffer demo\0", boot_step_framebuffer_demo);
