#![no_std]

pub mod boot_drivers;
pub mod boot_memory;
pub mod boot_services;
pub mod cpu_verify;
pub mod early_init;
pub mod gdt;
pub mod idt;
pub mod kernel_panic;
pub mod limine_protocol;
pub mod safe_stack;
pub mod shutdown;

pub use early_init::{
    boot_get_cmdline, boot_get_hhdm_offset, boot_get_memmap, boot_init_run_all,
    boot_init_run_phase, boot_mark_initialized, get_initialization_progress, is_kernel_initialized,
    kernel_main, kernel_main_no_multiboot, report_kernel_status,
};
pub use kernel_panic::{kernel_assert, kernel_panic, kernel_panic_with_context};
pub use limine_protocol::{boot_info, ensure_base_revision, BootInfo, FramebufferInfo, MemmapEntry};
pub use shutdown::{
    execute_kernel, kernel_drain_serial_output, kernel_quiesce_interrupts, kernel_reboot,
    kernel_shutdown,
};
