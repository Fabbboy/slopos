use core::ffi::c_char;

use slopos_arch::cpu;
use slopos_ostd::io::power as ostd_power;
use slopos_ostd::klog_info;
use slopos_ostd::string::cstr_to_str_lossy;
use slopos_ostd::sync::StateFlag;

static SHUTDOWN_IN_PROGRESS: StateFlag = StateFlag::new();
static INTERRUPTS_QUIESCED: StateFlag = StateFlag::new();
static SERIAL_DRAINED: StateFlag = StateFlag::new();

use slopos_core::sched::scheduler_shutdown;
use slopos_core::task::task_shutdown_all;
use slopos_drivers::apic;
use slopos_drivers::hpet;
use slopos_kernel_services::kernel_vm_space::activate_post_user_fault;
use slopos_mm::page_alloc::{page_allocator_paint_all, pcp_drain_all};
use slopos_mm::stack_region::KstackRegion;
use slopos_mm::stack_va::pcp_drain_all as stack_pcp_drain_all;

fn serial_flush() {
    ostd_power::drain_serial_tx(|| cpu::pause(), 1024);
}
fn ensure_kernel_page_dir() {
    // Ensure LAPIC/IOAPIC MMIO is mapped when shutting down from user context.
    activate_post_user_fault();
}
fn poweroff_hardware() {
    ostd_power::acpi_poweroff_broadcast();
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
        klog_info!("Reason: {}", cstr_to_str_lossy(reason));
    }

    pcp_drain_all();
    stack_pcp_drain_all::<KstackRegion>();

    // Terminate all tasks while the scheduler is still enabled so that APs
    // whose current task is destroyed can schedule() to idle normally.
    if task_shutdown_all() != 0 {
        klog_info!("Warning: Failed to terminate one or more tasks");
    }

    scheduler_shutdown();

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

    slopos_ostd::cpu::x86_64::core::halt_loop();
}
pub fn kernel_reboot(reason: *const c_char) -> ! {
    ensure_kernel_page_dir();
    cpu::disable_interrupts();

    klog_info!("=== Kernel Reboot Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", cstr_to_str_lossy(reason));
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();

    klog_info!("Rebooting via keyboard controller...");

    hpet::delay_ms(50);
    ostd_power::ps2_reset_pulse();

    klog_info!("Keyboard reset failed, attempting triple fault...");

    slopos_ostd::cpu::x86_64::core::trigger_triple_fault();
}
pub fn execute_kernel() {
    klog_info!("=== EXECUTING KERNEL PURIFICATION RITUAL ===");
    klog_info!("Painting memory with the essence of slop (0x69)...");
    page_allocator_paint_all(0x69);
    klog_info!("Memory purification complete. The slop has been painted eternal.");
}
