use core::ffi::c_char;

use slopos_arch::cpu;
use slopos_ostd::io::power as ostd_power;
use slopos_ostd::klog_info;
use slopos_ostd::string::cstr_to_str_lossy;
use slopos_ostd::sync::StateFlag;

static SHUTDOWN_IN_PROGRESS: StateFlag = StateFlag::new();
static INTERRUPTS_QUIESCED: StateFlag = StateFlag::new();
static SERIAL_DRAINED: StateFlag = StateFlag::new();
static FS_SYNCED: StateFlag = StateFlag::new();

use slopos_acpi::fadt::PowerConfig;
use slopos_acpi::tables::AcpiTables;
use slopos_ostd::uefi::{self, EfiResetType};

use slopos_drivers::apic;
use slopos_drivers::hpet;
use slopos_kernel_services::kernel_vm_space::activate_post_user_fault;
use slopos_kernel_services::platform;
use slopos_mm::page_alloc::{page_allocator_paint_all, pcp_drain_all};
use slopos_mm::stack_region::KstackRegion;
use slopos_mm::stack_va::pcp_drain_all as stack_pcp_drain_all;
use slopos_sched::scheduler::scheduler_shutdown;
use slopos_sched::task::task_shutdown_all;

fn serial_flush() {
    ostd_power::drain_serial_tx(|| cpu::pause(), 1024);
}
fn flush_filesystems_for_shutdown() {
    if !FS_SYNCED.enter() {
        return;
    }
    klog_info!("Kernel shutdown: flushing filesystem caches");
    slopos_fs::ext2_vfs_shutdown_sync();
}
/// Map LAPIC/IOAPIC MMIO before a shutdown path touches it — the
/// caller may arrive from user context, where it is not mapped.
fn ensure_shutdown_mmio_mapped() {
    activate_post_user_fault();
}
/// Parse the FADT (+ DSDT `\_S5`) for the platform's power-management
/// registers. Parsed on demand (pure HHDM reads) rather than cached, since
/// the firmware tables live for the kernel's lifetime and shutdown is rare.
fn acpi_power_config() -> Option<PowerConfig> {
    if !platform::is_rsdp_available() {
        return None;
    }
    let tables = AcpiTables::from_phys(platform::get_rsdp_phys())?;
    PowerConfig::from_tables(&tables)
}

/// Request firmware power-off / reset via UEFI Runtime Services
/// `ResetSystem` — the most reliable path on modern UEFI hardware, where
/// the firmware performs the chipset-correct sequence internally. No-op on
/// a BIOS boot (no system table). Returns if the firmware ignored it.
fn try_uefi_reset(reset_type: EfiResetType) {
    let system_table = crate::limine_protocol::efi_system_table_addr();
    if system_table != 0 {
        klog_info!(
            "UEFI ResetSystem: type={:?} system_table={:#x}",
            reset_type,
            system_table
        );
        uefi::reset_system(system_table, reset_type);
    }
}

fn reboot_via_uefi() {
    try_uefi_reset(EfiResetType::Cold);
}

fn poweroff_hardware() {
    // First choice: firmware power-off via UEFI ResetSystem(Shutdown).
    // Falls through on a BIOS boot or if the firmware does not support it.
    try_uefi_reset(EfiResetType::Shutdown);

    // Fallback: ACPI S5 (soft-off) via the FADT PM1 control registers with
    // the DSDT/SSDT `\_S5` sleep type.
    match acpi_power_config() {
        Some(cfg) => {
            klog_info!(
                "ACPI poweroff: pm1a={:#x} pm1b={:#x} slp_a={:?} slp_b={:?}",
                cfg.pm1a_cnt_port,
                cfg.pm1b_cnt_port,
                cfg.slp_typ_a,
                cfg.slp_typ_b
            );
            ostd_power::acpi_enable_if_needed(
                cfg.pm1a_cnt_port,
                cfg.smi_cmd,
                cfg.acpi_enable,
                || hpet::delay_ms(1),
            );
            if let Some(slp_a) = cfg.slp_typ_a {
                let slp_b = cfg.slp_typ_b.unwrap_or(slp_a);
                ostd_power::acpi_s5_poweroff(cfg.pm1a_cnt_port, cfg.pm1b_cnt_port, slp_a, slp_b);
            } else {
                klog_info!("ACPI poweroff: no \\_S5 sleep type found; using fallback ports");
            }
        }
        None => klog_info!("ACPI poweroff: FADT unavailable; using fallback ports"),
    }

    // Fallback: the hypervisor-specific ACPI PM1A_CNT ports (QEMU 0x604,
    // Bochs 0xB004, VirtualBox 0x4004). Harmless no-ops on bare metal.
    ostd_power::acpi_poweroff_broadcast();
}
pub fn kernel_quiesce_interrupts() {
    ensure_shutdown_mmio_mapped();
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
    ensure_shutdown_mmio_mapped();
    // Flush filesystem write-back caches to disk while interrupts are STILL
    // ENABLED — the virtio-blk completion path needs IRQs + the scheduler to
    // post the used-buffer event. Doing this before `disable_interrupts` is
    // load-bearing for durability; once interrupts are off a device flush
    // would only time out. Best-effort, runs once, no-op if ext2 unmounted.
    flush_filesystems_for_shutdown();

    if !SHUTDOWN_IN_PROGRESS.enter() {
        kernel_quiesce_interrupts();
        kernel_drain_serial_output();
        halt();
    }

    klog_info!("=== Kernel Shutdown Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", cstr_to_str_lossy(reason));
    }

    // Terminate all tasks while the scheduler is still enabled so that APs
    // whose current task is destroyed can schedule() to idle normally, and with
    // interrupts still ENABLED: a task's destructor returns its kernel stack,
    // unsafe stack and address space to the buddy allocator, whose reuse path
    // waits on synchronous cross-CPU TLB drains. Running that with interrupts
    // off is the slab/buddy deadlock, and `Task`'s destructor asserts against
    // it rather than hanging.
    if task_shutdown_all() != 0 {
        klog_info!("Warning: Failed to terminate one or more tasks");
    }

    cpu::disable_interrupts();

    // After teardown, so the kernel stacks it just freed are drained too.
    pcp_drain_all();
    stack_pcp_drain_all::<KstackRegion>();

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
fn reboot_via_cf9() {
    ostd_power::cf9_reset_pulse(|| hpet::delay_ms(1));
}

/// Issue the firmware-advertised ACPI reset (FADT `RESET_REG`) when the
/// platform provides one (on Intel PCH this is typically the `0xCF9` port).
/// Returns if no reset register is advertised or the platform ignored it.
fn reboot_via_acpi() {
    if let Some((reg, value)) = acpi_power_config().and_then(|c| c.reset) {
        klog_info!(
            "ACPI reset: space={} addr={:#x} value={:#x}",
            reg.address_space_id,
            reg.address,
            value
        );
        ostd_power::acpi_reset(reg.address_space_id, reg.address, value);
    } else {
        klog_info!("ACPI reset: FADT advertises no RESET_REG");
    }
}

/// Recoverable platform resets, tried in order of decreasing likelihood
/// on modern hardware. Each returns only if it did not reset the machine,
/// so the next is attempted; the terminal triple fault (resets every sane
/// x86 — but notably *not* every UEFI platform) is the final fallback.
const REBOOT_METHODS: &[(&str, fn())] = &[
    ("UEFI ResetSystem", reboot_via_uefi),
    ("ACPI RESET_REG", reboot_via_acpi),
    ("PCH 0xCF9 reset register", reboot_via_cf9),
    ("PS/2 keyboard controller", ostd_power::ps2_reset_pulse),
];

pub fn kernel_reboot(reason: *const c_char) -> ! {
    ensure_shutdown_mmio_mapped();
    cpu::disable_interrupts();

    klog_info!("=== Kernel Reboot Requested ===");
    if !reason.is_null() {
        klog_info!("Reason: {}", cstr_to_str_lossy(reason));
    }

    kernel_quiesce_interrupts();
    kernel_drain_serial_output();
    hpet::delay_ms(50);

    for &(method, reset) in REBOOT_METHODS {
        klog_info!("Rebooting via {}", method);
        reset();
        hpet::delay_ms(50);
    }

    klog_info!("Firmware reset ignored; forcing triple fault");
    slopos_ostd::cpu::x86_64::core::trigger_triple_fault();
}
pub fn execute_kernel() {
    klog_info!("=== EXECUTING KERNEL PURIFICATION RITUAL ===");
    klog_info!("Painting memory with the essence of slop (0x69)...");
    page_allocator_paint_all(0x69);
    klog_info!("Memory purification complete. The slop has been painted eternal.");
}
