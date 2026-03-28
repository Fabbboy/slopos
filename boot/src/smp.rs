use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use limine::mp::{MP_FLAG_X2APIC, MpInfo};

use slopos_arch::{cpu, is_cpu_online, pcr};
use slopos_core::sched::{enter_scheduler, init_scheduler_for_ap};
use slopos_drivers::apic;
use slopos_mm::tlb;
use slopos_utils::klog_info;

use crate::gdt::syscall_msr_init;
use crate::idt::idt_load;
use crate::ist_stacks;
use crate::limine_protocol;

static NEXT_CPU_ID: AtomicUsize = AtomicUsize::new(1);

const AP_STARTED_MAGIC: u64 = 0x4150_5354_4152_5444;
const MAX_CPUS: usize = 256;

/// Per-CPU completion signals.  The BSP passes each AP its index into this
/// array via `MpInfo::bootstrap(ap_entry, slot)`.  The AP stores
/// `AP_STARTED_MAGIC` here once it is fully initialised so the BSP can
/// spin-wait for it—replacing the now-private `MpInfo::extra_argument` field
/// that limine 0.6 no longer exposes for writing.
static AP_SIGNALS: [AtomicU64; MAX_CPUS] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_CPUS]
};

unsafe extern "C" fn ap_entry(cpu_info: &MpInfo) -> ! {
    cpu::disable_interrupts();

    cpu::enable_sse();

    // Replicate the BSP's XSAVE configuration (CR4.OSXSAVE + XCR0).
    slopos_arch::cpu::xsave::enable_on_current_cpu();

    // Limine may start APs in x2APIC mode (MSR-based register access).
    // The kernel uses xAPIC MMIO for all LAPIC access, so if x2APIC is
    // active we must transition back: x2APIC → disabled → xAPIC.
    // This must happen before apic::enable() which uses MMIO write_register.
    {
        use slopos_arch::cpu::apic_msr::ApicBaseMsr;
        use slopos_arch::cpu::msr::Msr;
        let msr_val = cpu::read_msr(Msr::APIC_BASE);
        if msr_val & ApicBaseMsr::X2APIC_ENABLE != 0 {
            // Step 1: disable APIC entirely (clear both GLOBAL_ENABLE and X2APIC_ENABLE)
            cpu::write_msr(
                Msr::APIC_BASE,
                msr_val & !(ApicBaseMsr::GLOBAL_ENABLE | ApicBaseMsr::X2APIC_ENABLE),
            );
            // Step 2: re-enable in xAPIC mode (set GLOBAL_ENABLE, leave X2APIC_ENABLE clear)
            cpu::write_msr(
                Msr::APIC_BASE,
                (msr_val & !ApicBaseMsr::X2APIC_ENABLE) | ApicBaseMsr::GLOBAL_ENABLE,
            );
        }
    }

    apic::enable();

    let apic_id = apic::get_id();
    let cpu_idx = NEXT_CPU_ID.fetch_add(1, Ordering::AcqRel);

    tlb::notify_cpu_online_id(cpu_idx);

    unsafe {
        let ap_pcr = pcr::init_ap_pcr(cpu_idx, apic_id);
        (*ap_pcr).init_gdt();
        (*ap_pcr).install();
    }

    // APs have per-CPU TSS structures; re-bind IST pointers after installing
    // the AP GDT/TSS so exceptions (notably #PF) do not enter with IST=0.
    ist_stacks::ist_bind_current_cpu();

    idt_load();
    syscall_msr_init();

    // Initialize the per-CPU scheduler and create the idle task BEFORE
    // enabling interrupts.  The previous order (enable_interrupts → init)
    // opened a race window where timer IPIs, TLB shootdowns, or reschedule
    // IPIs could arrive and touch uninitialised per-CPU scheduler state.
    init_scheduler_for_ap(cpu_idx);

    // Signal the BSP that this AP is fully initialised.
    let signal_slot = cpu_info.extra_argument() as usize;
    AP_SIGNALS[signal_slot].store(AP_STARTED_MAGIC, Ordering::Release);

    klog_info!(
        "MP: CPU online (idx {}, apic 0x{:x}, acpi {})",
        cpu_idx,
        apic_id,
        cpu_info.processor_id
    );

    // AP LAPIC timer is started later by deferred_start_ap_timer() in the
    // scheduler loop, after the BSP completes HPET init + LAPIC calibration.
    // Interrupts are enabled here only after all per-CPU state is ready.
    cpu::enable_interrupts();

    enter_scheduler(cpu_idx);
}

pub fn smp_init() {
    let Some(resp) = limine_protocol::mp_response() else {
        klog_info!("MP: Limine MP response unavailable; skipping AP startup");
        return;
    };

    let cpus = resp.cpus();
    let bsp_lapic = resp.bsp_lapic_id;

    // BSP PCR already initialized in early_init; nothing more needed here.

    let x2apic = if resp.flags as u64 & MP_FLAG_X2APIC != 0 {
        "on"
    } else {
        "off"
    };

    klog_info!(
        "MP: discovered {} CPUs, BSP LAPIC 0x{:x}, x2apic {}",
        cpus.len(),
        bsp_lapic,
        x2apic
    );
    klog_info!("APIC: Local APIC base 0x{:x}", apic::get_base_address());

    for cpu in cpus {
        let role = if cpu.lapic_id == bsp_lapic {
            "bsp"
        } else {
            "ap"
        };
        klog_info!(
            "MP: CPU {} lapic 0x{:x} ({})",
            cpu.processor_id,
            cpu.lapic_id,
            role
        );
    }

    let mut ap_count = 0usize;
    for (i, cpu) in cpus.iter().enumerate() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }

        AP_SIGNALS[i].store(0, Ordering::Release);
        cpu.bootstrap(ap_entry, i as u64);
        ap_count += 1;
    }

    if ap_count == 0 {
        klog_info!("MP: no secondary CPUs to start");
        return;
    }

    let mut started_count = 0usize;

    for (i, cpu) in cpus.iter().enumerate() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }

        let mut spins = 2_000_000u32;
        while AP_SIGNALS[i].load(Ordering::Acquire) != AP_STARTED_MAGIC && spins > 0 {
            cpu::pause();
            spins -= 1;
        }

        if AP_SIGNALS[i].load(Ordering::Acquire) == AP_STARTED_MAGIC {
            klog_info!("MP: CPU 0x{:x} reported online", cpu.lapic_id);
            started_count += 1;
        } else {
            klog_info!("MP: CPU 0x{:x} did not respond", cpu.lapic_id);
        }
    }

    for cpu_idx in 1..=started_count {
        let mut spins = 5_000_000u32;
        while !is_cpu_online(cpu_idx) && spins > 0 {
            cpu::pause();
            spins -= 1;
        }
        if !is_cpu_online(cpu_idx) {
            klog_info!("MP: Warning - CPU {} scheduler not fully online", cpu_idx);
        }
    }
}
