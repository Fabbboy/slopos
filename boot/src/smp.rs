use core::sync::atomic::{AtomicUsize, Ordering};

use limine::mp::{Cpu as MpCpu, ResponseFlags as MpResponseFlags};

use slopos_core::wl_currency::{award_loss, award_win};
use slopos_core::{init_scheduler_for_ap, scheduler_run_ap};
use slopos_drivers::apic;
use slopos_lib::{cpu, init_bsp, init_percpu_for_cpu, is_cpu_online, klog_info};
use slopos_mm::tlb;

use crate::gdt::{gdt_init_for_cpu, syscall_msr_init};
use crate::idt::idt_load;
use crate::limine_protocol;

static NEXT_CPU_ID: AtomicUsize = AtomicUsize::new(1);

const AP_STARTED_MAGIC: u64 = 0x4150_5354_4152_5444;

unsafe extern "C" fn ap_entry(cpu_info: &MpCpu) -> ! {
    cpu::disable_interrupts();

    cpu::enable_sse();

    apic::enable();

    let apic_id = apic::get_id();
    let cpu_idx = NEXT_CPU_ID.fetch_add(1, Ordering::AcqRel);

    init_percpu_for_cpu(cpu_idx, apic_id);
    tlb::register_cpu(apic_id);

    gdt_init_for_cpu(cpu_idx);
    idt_load();
    syscall_msr_init();
    cpu::enable_interrupts();

    cpu_info.extra.store(AP_STARTED_MAGIC, Ordering::Release);
    klog_info!(
        "MP: CPU online (idx {}, apic 0x{:x}, acpi {})",
        cpu_idx,
        apic_id,
        cpu_info.id
    );

    init_scheduler_for_ap(cpu_idx);
    scheduler_run_ap(cpu_idx);
}

pub fn smp_init() {
    let Some(resp) = limine_protocol::mp_response() else {
        klog_info!("MP: Limine MP response unavailable; skipping AP startup");
        award_loss();
        return;
    };

    award_win();

    let cpus = resp.cpus();
    let bsp_lapic = resp.bsp_lapic_id();

    init_bsp(bsp_lapic);
    tlb::set_bsp_apic_id(bsp_lapic);

    let flags = resp.flags();
    let x2apic = if flags.contains(MpResponseFlags::X2APIC) {
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
        klog_info!("MP: CPU {} lapic 0x{:x} ({})", cpu.id, cpu.lapic_id, role);
    }

    let mut ap_count = 0usize;
    for cpu in cpus {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }

        cpu.extra.store(0, Ordering::Release);
        cpu.goto_address.write(ap_entry);
        ap_count += 1;
    }

    if ap_count == 0 {
        klog_info!("MP: no secondary CPUs to start");
        return;
    }

    let mut started_count = 0usize;

    for cpu in cpus {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }

        let mut spins = 2_000_000u32;
        while cpu.extra.load(Ordering::Acquire) != AP_STARTED_MAGIC && spins > 0 {
            cpu::pause();
            spins -= 1;
        }

        if cpu.extra.load(Ordering::Acquire) == AP_STARTED_MAGIC {
            award_win();
            klog_info!("MP: CPU 0x{:x} reported online", cpu.lapic_id);
            started_count += 1;
        } else {
            award_loss();
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
