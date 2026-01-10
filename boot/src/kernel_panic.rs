use core::ffi::c_char;

use slopos_drivers::keyboard::keyboard_poll_wait_enter;
use slopos_drivers::serial;
use slopos_lib::cpu;
use slopos_lib::klog_info;
use slopos_lib::string::cstr_to_str;
use slopos_video::panic_screen;

use crate::shutdown::kernel_shutdown;

use crate::shutdown::execute_kernel;
use slopos_mm::memory_init::is_memory_system_initialized;

fn panic_output_str(s: &str) {
    serial::write_line(s);
}

fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

#[derive(Clone, Copy)]
enum ControlRegister {
    Cr0,
    Cr3,
    Cr4,
}

fn read_cr(reg: ControlRegister) -> u64 {
    let value: u64;
    unsafe {
        match reg {
            ControlRegister::Cr0 => {
                core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags))
            }
            ControlRegister::Cr3 => {
                core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags))
            }
            ControlRegister::Cr4 => {
                core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags))
            }
        }
    }
    value
}

fn log_register_snapshot(rip: Option<u64>, rsp: Option<u64>) {
    panic_output_str("Register snapshot:");
    if let Some(rip) = rip {
        klog_info!("RIP: 0x{:x}", rip);
    }
    if let Some(rsp) = rsp {
        klog_info!("RSP: 0x{:x}", rsp);
    }
    klog_info!("CR0: 0x{:x}", read_cr(ControlRegister::Cr0));
    klog_info!("CR3: 0x{:x}", read_cr(ControlRegister::Cr3));
    klog_info!("CR4: 0x{:x}", read_cr(ControlRegister::Cr4));
}
pub fn kernel_panic(message: *const c_char) -> ! {
    cpu::disable_interrupts();

    panic_output_str("\n\n=== KERNEL PANIC ===");

    let msg_str = if !message.is_null() {
        let s = unsafe { cstr_to_str(message) };
        klog_info!("PANIC: {}", s);
        Some(s)
    } else {
        panic_output_str("PANIC: No message provided");
        None
    };

    let rsp = read_rsp();
    let cr0 = read_cr(ControlRegister::Cr0);
    let cr3 = read_cr(ControlRegister::Cr3);
    let cr4 = read_cr(ControlRegister::Cr4);
    log_register_snapshot(None, Some(rsp));

    panic_output_str("===================");
    panic_output_str("Kernel panic: unrecoverable error");

    // Display graphical panic screen if framebuffer is available
    if panic_screen::display_panic_screen(msg_str, None, Some(rsp), cr0, cr3, cr4) {
        panic_output_str("Press ENTER to shutdown...");
        keyboard_poll_wait_enter();
    } else {
        panic_output_str("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_output_str("Memory system unavailable; skipping paint ritual");
    }

    let reason = if message.is_null() {
        b"panic\0".as_ptr() as *const c_char
    } else {
        message
    };
    kernel_shutdown(reason);
}
pub fn kernel_panic_with_context(
    message: *const c_char,
    function: *const c_char,
    file: *const c_char,
    line: i32,
) {
    cpu::disable_interrupts();

    panic_output_str("\n\n=== KERNEL PANIC ===");

    let msg_str = if !message.is_null() {
        let s = unsafe { cstr_to_str(message) };
        klog_info!("PANIC: {}", s);
        Some(s)
    } else {
        None
    };

    unsafe {
        if !function.is_null() {
            klog_info!("Function: {}", cstr_to_str(function));
        }
        if !file.is_null() {
            klog_info!("File: {}:{}", cstr_to_str(file), line);
        }
    }

    let rsp = read_rsp();
    let cr0 = read_cr(ControlRegister::Cr0);
    let cr3 = read_cr(ControlRegister::Cr3);
    let cr4 = read_cr(ControlRegister::Cr4);
    log_register_snapshot(None, Some(rsp));

    panic_output_str("===================");
    panic_output_str("Kernel panic: unrecoverable error");

    // Display graphical panic screen if framebuffer is available
    if panic_screen::display_panic_screen(msg_str, None, Some(rsp), cr0, cr3, cr4) {
        panic_output_str("Press ENTER to shutdown...");
        keyboard_poll_wait_enter();
    } else {
        panic_output_str("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_output_str("Memory system unavailable; skipping paint ritual");
    }

    let reason = if message.is_null() {
        b"panic\0".as_ptr() as *const c_char
    } else {
        message
    };
    kernel_shutdown(reason);
}

pub fn kernel_panic_with_state(message: *const c_char, rip: u64, rsp: u64) {
    cpu::disable_interrupts();

    panic_output_str("\n\n=== KERNEL PANIC ===");

    let msg_str = if !message.is_null() {
        let s = unsafe { cstr_to_str(message) };
        klog_info!("PANIC: {}", s);
        Some(s)
    } else {
        panic_output_str("PANIC: No message provided");
        None
    };

    let cr0 = read_cr(ControlRegister::Cr0);
    let cr3 = read_cr(ControlRegister::Cr3);
    let cr4 = read_cr(ControlRegister::Cr4);
    log_register_snapshot(Some(rip), Some(rsp));

    panic_output_str("===================");
    panic_output_str("Kernel panic: unrecoverable error");

    // Display graphical panic screen if framebuffer is available
    if panic_screen::display_panic_screen(msg_str, Some(rip), Some(rsp), cr0, cr3, cr4) {
        panic_output_str("Press ENTER to shutdown...");
        keyboard_poll_wait_enter();
    } else {
        panic_output_str("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_output_str("Memory system unavailable; skipping paint ritual");
    }

    let reason = if message.is_null() {
        b"panic\0".as_ptr() as *const c_char
    } else {
        message
    };
    kernel_shutdown(reason);
}
pub fn kernel_assert(condition: i32, message: *const c_char) {
    if condition == 0 {
        let msg = if message.is_null() {
            b"Assertion failed\0".as_ptr() as *const c_char
        } else {
            message
        };
        kernel_panic(msg);
    }
}
