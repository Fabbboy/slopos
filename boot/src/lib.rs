#![no_std]

pub mod boot_drivers;
pub mod boot_impl;
pub mod boot_memory;
pub mod boot_services;
pub mod cpu_verify;
pub mod early_init;
pub mod ffi_boundary;
pub mod gdt;
pub use gdt::gdt_set_kernel_rsp0;
pub mod idt;
pub mod limine_protocol;
pub mod panic;
pub mod safe_stack;
pub mod shutdown;

pub use early_init::{
    boot_get_cmdline, boot_get_hhdm_offset, boot_get_memmap, boot_init_run_all,
    boot_init_run_phase, boot_mark_initialized, get_initialization_progress, is_kernel_initialized,
    kernel_main_no_multiboot, report_kernel_status,
};
pub use ffi_boundary::kernel_main;
pub use limine_protocol::{
    BootFramebuffer, BootInfo, MemmapEntry, MemoryRegion, MemoryRegionKind, boot_info,
    ensure_base_revision, memmap_entry_count, memory_regions,
};
pub use panic::{panic_handler_impl, set_panic_cpu_state};
pub use shutdown::{
    execute_kernel, kernel_drain_serial_output, kernel_quiesce_interrupts, kernel_reboot,
    kernel_shutdown,
};
