use core::ffi::{c_char, CStr};

use slopos_drivers::serial;
use slopos_lib::cpu;
use slopos_lib::klog_info;

use crate::shutdown::kernel_shutdown;

unsafe extern "C" {
    fn is_memory_system_initialized() -> i32;
    fn execute_kernel();
}

fn panic_output_str(s: &str) {
    serial::write_line(s);
}

fn read_rip() -> u64 {
    let rip: u64;
    unsafe {
        core::arch::asm!("lea {0}, [rip]", out(reg) rip, options(nomem, nostack, preserves_flags));
    }
    rip
}

fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

fn read_cr(reg: &str) -> u64 {
    let value: u64;
    unsafe {
        match reg {
            "cr0" => {
                core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags))
            }
            "cr3" => {
                core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags))
            }
            _ => {
                core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags))
            }
        }
    }
    value
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_panic(message: *const c_char) {
    cpu::disable_interrupts();

    panic_output_str("\n\n=== KERNEL PANIC ===");

    if !message.is_null() {
        let msg_str = unsafe { CStr::from_ptr(message) }
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_info!("PANIC: {}", msg_str);
    } else {
        panic_output_str("PANIC: No message provided");
    }

    let rip = read_rip();
    let rsp = read_rsp();

    panic_output_str("Register snapshot:");
    klog_info!("RIP: 0x{:x}", rip);
    klog_info!("RSP: 0x{:x}", rsp);
    klog_info!("CR0: 0x{:x}", read_cr("cr0"));
    klog_info!("CR3: 0x{:x}", read_cr("cr3"));
    klog_info!("CR4: 0x{:x}", read_cr("cr4"));

    panic_output_str("===================");
    panic_output_str("Skill issue lol");
    panic_output_str("System halted.");

    unsafe {
        if is_memory_system_initialized() != 0 {
            execute_kernel();
        } else {
            panic_output_str("Memory system unavailable; skipping paint ritual");
        }
    }

    let reason = if message.is_null() {
        b"panic\0".as_ptr() as *const c_char
    } else {
        message
    };
    kernel_shutdown(reason);
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_panic_with_context(
    message: *const c_char,
    function: *const c_char,
    file: *const c_char,
    line: i32,
) {
    cpu::disable_interrupts();

    panic_output_str("\n\n=== KERNEL PANIC ===");

    if !message.is_null() {
        let msg_str = unsafe { CStr::from_ptr(message) }
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_info!("PANIC: {}", msg_str);
    }

    unsafe {
        if !function.is_null() {
            let function_str = CStr::from_ptr(function)
                .to_str()
                .unwrap_or("<invalid utf-8>");
            klog_info!("Function: {}", function_str);
        }
        if !file.is_null() {
            let file_str =
                CStr::from_ptr(file).to_str().unwrap_or("<invalid utf-8>");
            klog_info!("File: {}:{}", file_str, line);
        }
    }

    let rip = read_rip();
    let rsp = read_rsp();

    klog_info!("RIP: 0x{:x}", rip);
    klog_info!("RSP: 0x{:x}", rsp);

    panic_output_str("===================");
    panic_output_str("Skill issue lol");
    panic_output_str("System halted.");

    unsafe {
        if is_memory_system_initialized() != 0 {
            execute_kernel();
        } else {
            panic_output_str("Memory system unavailable; skipping paint ritual");
        }
    }

    let reason = if message.is_null() {
        b"panic\0".as_ptr() as *const c_char
    } else {
        message
    };
    kernel_shutdown(reason);
}

#[unsafe(no_mangle)]
pub extern "C" fn kernel_assert(condition: i32, message: *const c_char) {
    if condition == 0 {
        let msg = if message.is_null() {
            b"Assertion failed\0".as_ptr() as *const c_char
        } else {
            message
        };
        kernel_panic(msg);
    }
}
