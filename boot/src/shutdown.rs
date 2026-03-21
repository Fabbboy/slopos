use core::arch::asm;
use core::ffi::c_char;

use slopos_arch::cpu;
use slopos_sync::StateFlag;
use slopos_utils::klog_info;
use slopos_utils::ports::{
    ACPI_PM1A_CNT, ACPI_PM1A_CNT_BOCHS, ACPI_PM1A_CNT_VBOX, COM1, PS2_COMMAND,
};
use slopos_utils::string::cstr_to_str;

static SHUTDOWN_IN_PROGRESS: StateFlag = StateFlag::new();
static INTERRUPTS_QUIESCED: StateFlag = StateFlag::new();
static SERIAL_DRAINED: StateFlag = StateFlag::new();

use slopos_core::sched::scheduler_shutdown;
use slopos_core::task::task_shutdown_all;
use slopos_drivers::apic;
use slopos_drivers::hpet;
use slopos_mm::page_alloc::{page_allocator_paint_all, pcp_drain_all};
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
    if !INTERRUPTS_QUIESCED.enter() {
        return;
    }

    klog_info!("Kernel shutdown: quiescing interrupt controllers");

    if apic::is_available() {
        // Send shutdown IPIs to all processors before disabling APIC
        apic::send_ipi_halt_all();
        // Small delay to allow IPIs to be delivered
        for _ in 0..100 {
            cpu::pause();
        }
        apic::send_eoi();
        apic::timer_stop();
        apic::disable();
    }
}
pub fn kernel_drain_serial_output() {
    if !SERIAL_DRAINED.enter() {
        return;
    }
    klog_info!("Kernel shutdown: draining serial output");
    serial_flush();
}
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    ensure_kernel_page_dir();
    cpu::disable_interrupts();

    if !SHUTDOWN_IN_PROGRESS.enter() {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        halt();
    }

    klog_info!("=== Kernel Shutdown Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", unsafe { cstr_to_str(reason) });
    }

    pcp_drain_all();

    scheduler_shutdown();

    if task_shutdown_all() != 0 {
        klog_info!("Warning: Failed to terminate one or more tasks");
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Kernel shutdown complete.");

    halt();
}

/// Terminal halt: attempt ACPI power-off, then spin forever.
///
/// All quiescing (IPI broadcast, APIC teardown, serial drain) must be
/// performed *before* calling this function — it exists solely to cut
/// the power and park the BSP.  Callers are `kernel_shutdown` and
/// `kernel_reboot`, both of which route through `kernel_quiesce_interrupts`
/// first.
fn halt() -> ! {
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

    hpet::delay_ms(50);
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
