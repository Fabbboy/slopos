use core::arch::asm;
use core::ffi::c_char;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::ports::{
    ACPI_PM1A_CNT, ACPI_PM1A_CNT_BOCHS, ACPI_PM1A_CNT_VBOX, COM1, PS2_COMMAND,
};
use slopos_lib::string::cstr_to_str;
use slopos_lib::{cpu, klog_info};

static SHUTDOWN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static INTERRUPTS_QUIESCED: AtomicBool = AtomicBool::new(false);
static SERIAL_DRAINED: AtomicBool = AtomicBool::new(false);

use slopos_core::{scheduler_shutdown, task_shutdown_all};
use slopos_drivers::apic::apic_is_available;
use slopos_drivers::apic::{apic_disable, apic_send_eoi, apic_send_ipi_halt_all, apic_timer_stop};
use slopos_drivers::pit::pit_poll_delay_ms;
use slopos_mm::page_alloc::page_allocator_paint_all;
use slopos_mm::paging::{paging_get_kernel_directory, switch_page_directory};

fn serial_flush() {
    let lsr_port = COM1.offset(5);
    for _ in 0..1024 {
        let lsr = unsafe { lsr_port.read() };
        if (lsr & 0x40) != 0 {
            break;
        }
        cpu::pause();
    }
}
fn ensure_kernel_page_dir() {
    // Ensure LAPIC/IOAPIC MMIO is mapped when shutting down from user context.
    let kernel_dir = paging_get_kernel_directory();
    if !kernel_dir.is_null() {
        let _ = switch_page_directory(kernel_dir);
    }
}
fn poweroff_hardware() {
    unsafe {
        ACPI_PM1A_CNT.write(0x2000);
        ACPI_PM1A_CNT_BOCHS.write(0x2000);
        ACPI_PM1A_CNT_VBOX.write(0x3400);
    }
}
pub fn kernel_quiesce_interrupts() {
    ensure_kernel_page_dir();
    cpu::disable_interrupts();
    if INTERRUPTS_QUIESCED.swap(true, Ordering::SeqCst) {
        return;
    }

    klog_info!("Kernel shutdown: quiescing interrupt controllers");

    if apic_is_available() != 0 {
        // Send shutdown IPIs to all processors before disabling APIC
        apic_send_ipi_halt_all();
        // Small delay to allow IPIs to be delivered
        for _ in 0..100 {
            cpu::pause();
        }
        apic_send_eoi();
        apic_timer_stop();
        apic_disable();
    }
}
pub fn kernel_drain_serial_output() {
    if SERIAL_DRAINED.swap(true, Ordering::SeqCst) {
        return;
    }
    klog_info!("Kernel shutdown: draining serial output");
    serial_flush();
}
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    ensure_kernel_page_dir();
    cpu::disable_interrupts();

    if SHUTDOWN_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        halt();
    }

    klog_info!("=== Kernel Shutdown Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", unsafe { cstr_to_str(reason) });
    }

    scheduler_shutdown();

    if task_shutdown_all() != 0 {
        klog_info!("Warning: Failed to terminate one or more tasks");
    }
    // scheduler_set_current_task removed - no longer needed

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Kernel shutdown complete. Coordinating APIC shutdown and halting processors.");

    poweroff_hardware();
    halt();
}

fn halt() -> ! {
    // Contact APIC for proper shutdown coordination
    if apic_is_available() != 0 {
        apic_send_ipi_halt_all();
        // Small delay to allow IPIs to be delivered
        for _ in 0..100 {
            cpu::pause();
        }
    }

    poweroff_hardware();

    loop {
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}
pub fn kernel_reboot(reason: *const c_char) -> ! {
    ensure_kernel_page_dir();
    cpu::disable_interrupts();

    klog_info!("=== Kernel Reboot Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", unsafe { cstr_to_str(reason) });
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Rebooting via keyboard controller...");

    pit_poll_delay_ms(50);
    unsafe { PS2_COMMAND.write(0xFE) };

    klog_info!("Keyboard reset failed, attempting triple fault...");

    #[repr(C, packed)]
    struct InvalidIdt {
        limit: u16,
        base: u64,
    }

    let invalid_idt = InvalidIdt { limit: 0, base: 0 };
    unsafe {
        asm!("lidt [{}]", in(reg) &invalid_idt, options(nostack, preserves_flags));
        asm!("int3", options(nostack, preserves_flags));
    }

    halt();
}
pub fn execute_kernel() {
    klog_info!("=== EXECUTING KERNEL PURIFICATION RITUAL ===");
    klog_info!("Painting memory with the essence of slop (0x69)...");
    page_allocator_paint_all(0x69);
    klog_info!("Memory purification complete. The slop has been painted eternal.");
}
