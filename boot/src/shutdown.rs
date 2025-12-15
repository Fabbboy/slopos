use core::arch::asm;
use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_lib::{cpu, io, klog_info};

static SHUTDOWN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static INTERRUPTS_QUIESCED: AtomicBool = AtomicBool::new(false);
static SERIAL_DRAINED: AtomicBool = AtomicBool::new(false);

use slopos_drivers::apic::apic_is_available;
use slopos_drivers::apic::{apic_disable, apic_send_eoi, apic_send_ipi_halt_all, apic_timer_stop};
use slopos_drivers::pit::pit_poll_delay_ms;
use slopos_mm::page_alloc::page_allocator_paint_all;
use slopos_sched::{scheduler_shutdown, task_shutdown_all};

fn serial_flush() {
    // Best-effort drain by waiting for line status transmit empty bit.
    const LINE_STATUS_PORT_OFFSET: u16 = 5;
    const COM1_BASE: u16 = 0x3F8;
    for _ in 0..1024 {
        let lsr = unsafe { io::inb(COM1_BASE + LINE_STATUS_PORT_OFFSET) };
        if (lsr & 0x40) != 0 {
            break;
        }
        cpu::pause();
    }
}
pub fn kernel_quiesce_interrupts() {
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
pub fn kernel_shutdown(reason: *const c_char) {
    cpu::disable_interrupts();

    if SHUTDOWN_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        halt();
    }

    klog_info!("=== Kernel Shutdown Requested ===");
    if !reason.is_null() {
        let reason_str = unsafe { CStr::from_ptr(reason) }
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_info!("Reason: {}", reason_str);
    }

    scheduler_shutdown();

    if task_shutdown_all() != 0 {
        klog_info!("Warning: Failed to terminate one or more tasks");
    }
    // scheduler_set_current_task removed - no longer needed

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Kernel shutdown complete. Coordinating APIC shutdown and halting processors.");

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

    loop {
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}
pub fn kernel_reboot(reason: *const c_char) {
    cpu::disable_interrupts();

    klog_info!("=== Kernel Reboot Requested ===");
    if !reason.is_null() {
        let reason_str = unsafe { CStr::from_ptr(reason) }
            .to_str()
            .unwrap_or("<invalid utf-8>");
        klog_info!("Reason: {}", reason_str);
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Rebooting via keyboard controller...");

    unsafe {
        pit_poll_delay_ms(50);
        io::outb(0x64, 0xFE);
    }

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
