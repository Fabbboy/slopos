use core::ffi::{c_char, c_int, c_void};

use crate::{early_init, gdt, idt, limine_protocol, shutdown};
use slopos_drivers::platform_init;

fn kernel_shutdown_fn(reason: *const c_char) -> ! {
    shutdown::kernel_shutdown(reason)
}

fn kernel_reboot_fn(reason: *const c_char) -> ! {
    shutdown::kernel_reboot(reason)
}

fn is_rsdp_available_fn() -> bool {
    limine_protocol::is_rsdp_available() != 0
}

fn get_rsdp_address_fn() -> *const c_void {
    limine_protocol::get_rsdp_address()
}

fn is_kernel_initialized_fn() -> bool {
    early_init::is_kernel_initialized() != 0
}

fn idt_get_gate_fn(vector: u8, entry: *mut c_void) -> c_int {
    idt::idt_get_gate_opaque(vector, entry)
}

pub fn register_boot_services() {
    platform_init::register_gdt_rsp0_callback(gdt::gdt_set_kernel_rsp0);
    platform_init::register_kernel_shutdown_callback(kernel_shutdown_fn);
    platform_init::register_kernel_reboot_callback(kernel_reboot_fn);
    platform_init::register_rsdp_callbacks(is_rsdp_available_fn, get_rsdp_address_fn);
    platform_init::register_kernel_initialized_callback(is_kernel_initialized_fn);
    platform_init::register_idt_get_gate_callback(idt_get_gate_fn);
}
