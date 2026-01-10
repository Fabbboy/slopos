//! Boot services trait implementation.
//!
//! This module implements the BootServices trait defined in `abi/sched_traits.rs`,
//! providing access to boot-time data and critical kernel operations.

use core::ffi::{c_char, c_int, c_void};
use slopos_abi::sched_traits::BootServices;

use crate::{early_init, gdt, idt, limine_protocol, shutdown};

/// Singleton boot services implementation.
pub struct BootImpl;

/// Static instance for registration with sched_bridge.
pub static BOOT_IMPL: BootImpl = BootImpl;

// SAFETY: BootImpl is a zero-sized type with no mutable state; all methods
// delegate to module-level functions that handle their own synchronization.
unsafe impl Send for BootImpl {}
unsafe impl Sync for BootImpl {}

impl BootServices for BootImpl {
    fn is_rsdp_available(&self) -> bool {
        limine_protocol::is_rsdp_available() != 0
    }

    fn get_rsdp_address(&self) -> *const c_void {
        limine_protocol::get_rsdp_address()
    }

    fn gdt_set_kernel_rsp0(&self, rsp0: u64) {
        gdt::gdt_set_kernel_rsp0(rsp0);
    }

    fn is_kernel_initialized(&self) -> bool {
        early_init::is_kernel_initialized() != 0
    }

    fn kernel_panic(&self, msg: *const c_char) -> ! {
        crate::kernel_panic::kernel_panic(msg)
    }

    fn kernel_shutdown(&self, reason: *const c_char) -> ! {
        shutdown::kernel_shutdown(reason)
    }

    fn kernel_reboot(&self, reason: *const c_char) -> ! {
        shutdown::kernel_reboot(reason)
    }

    fn idt_get_gate(&self, vector: u8, entry: *mut c_void) -> c_int {
        idt::idt_get_gate_opaque(vector, entry)
    }
}

pub fn register_with_bridge() {
    slopos_drivers::sched_bridge::init_boot(&BOOT_IMPL);
}
