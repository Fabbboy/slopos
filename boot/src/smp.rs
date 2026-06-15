use core::sync::atomic::{AtomicU64, Ordering};

use limine::mp::{MP_FLAG_X2APIC, MpGotoFunction};

use slopos_arch::{cpu, is_cpu_online, pcr};
use slopos_drivers::apic;
use slopos_mm::tlb;
use slopos_ostd::arch::x86_64::safestack::{install_ap_trampoline_as, install_safestack_runtime};
use slopos_ostd::boot::smp::register_ap_late_entry;
use slopos_ostd::klog_info;
use slopos_sched::safestack_rt;
use slopos_sched::scheduler::{enter_scheduler, init_scheduler_for_ap};

use crate::gdt::syscall_msr_init;
use crate::idt::idt_load;
use crate::ist_stacks;
use crate::limine_protocol;

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

/// Kernel-side AP late entry. Registered with OSTD's
/// `slopos_ostd::boot::smp::AP_LATE_ENTRY` before any AP is started;
/// the OSTD `ap_early_entry` tail-calls this once the AP's GS_BASE is
/// installed and `MpInfo.extra` has been decoded.
///
/// `cpu_idx` is the 1-based slot the BSP encoded in `cpu.extra`. The
/// naked trampoline already selected `AP_PCRS[cpu_idx - 1]` and
/// installed GS_BASE to it; this function MUST use the same index
/// when re-installing the per-CPU PCR via `ApPcrHandle::init` or the
/// AP would point at a different PCR mid-boot, silently swapping the
/// SafeStack data-SP slot.
fn ap_late_entry(cpu_idx: usize) -> ! {
    cpu::disable_interrupts();
    cpu::enable_sse();

    // Enter the per-AP init scope: mint an `ApToken<'brand>` whose
    // brand is invariant in the HRTB closure, and thread it through
    // every per-CPU init call below. `enter_scheduler` stays OUTSIDE
    // the closure (its `-> !` divergence keeps the closure-return
    // path unreachable in practice).
    slopos_ostd::sync::run_ap_init(cpu_idx, |ap_token| {
        // Replicate the BSP's XSAVE configuration (CR4.OSXSAVE + XCR0).
        slopos_arch::cpu::xsave::enable_on_current_cpu();

        // Match the BSP supervisor-mode feature mask (CR4.PGE + SMEP + SMAP).
        // Must happen before this AP's first CR3 reload so global kernel
        // mappings are tagged consistently with the BSP.
        slopos_arch::cpu::security::enable_supervisor_features();

        // Enable CR4.PCIDE on this AP if the BSP decided PCID is live.
        // Must run before any CR3 load that embeds a non-zero PCID.
        slopos_mm::mmu::init_ap();

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

        pcr::ApPcrHandle::init(ap_token, apic_id).init_gdt_and_install();

        // APs have per-CPU TSS structures; re-bind IST pointers after installing
        // the AP GDT/TSS so exceptions (notably #PF) do not enter with IST=0.
        let mut ap_boot_ctx = slopos_hermetic::take_for_ap(ap_token);
        ist_stacks::ist_bind_current_cpu(&mut ap_boot_ctx);

        idt_load(ap_token);
        syscall_msr_init(ap_token);
        slopos_hermetic::return_after_ap(cpu_idx, ap_boot_ctx);

        // Initialize the per-CPU scheduler and create the idle task BEFORE
        // enabling interrupts.  The previous order (enable_interrupts → init)
        // opened a race window where timer IPIs, TLB shootdowns, or reschedule
        // IPIs could arrive and touch uninitialised per-CPU scheduler state.
        init_scheduler_for_ap(cpu_idx);

        // AP LAPIC timer is started later by deferred_start_ap_timer() in the
        // scheduler loop, after the BSP completes HPET init + LAPIC calibration.
        // Interrupts are enabled here only after all per-CPU state is ready.
        cpu::enable_interrupts();

        // An AP must be able to *service* a TLB shootdown IPI (interrupts
        // enabled, IDT loaded) before any initiator may *target* it.
        // Joining the shootdown set before enabling interrupts leaves a
        // window where it is a target that can never ack — and a stalled
        // AP would stay that way, wedging the initiator forever. Discard
        // any stale kernel translation, then join the set.
        tlb::flush_local_all();
        tlb::notify_cpu_online_id(cpu_idx);

        // Signal the BSP that this AP is a live, ack-capable shootdown
        // target. The BSP's bounded wait must not release until this
        // point, so it never proceeds with a half-joined AP in the set.
        AP_SIGNALS[cpu_idx].store(AP_STARTED_MAGIC, Ordering::Release);

        klog_info!("MP: CPU online (idx {}, apic 0x{:x})", cpu_idx, apic_id);
    });

    enter_scheduler(cpu_idx);
}

pub fn smp_init<'b>(ctx: &mut slopos_hermetic::BootCtx<'b, slopos_hermetic::BspInit>) {
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

    // Seed each AP PCR's self_ref + current_task so the OSTD-side
    // `ap_entry` naked trampoline can install GS_BASE and have
    // `__safestack_pointer_address` find a valid bootstrap Task on the
    // very first instrumented call.  Limited to MAX_STATIC_APS — if
    // the platform reports more APs we only start the first N.
    const MAX_STATIC_APS: usize = safestack_rt::MAX_STATIC_APS;
    safestack_rt::init_bootstrap_tasks();

    // Register the kernel-side AP late entry with OSTD before any AP
    // is fired. The OSTD trampoline waits on this `OnceLock` after
    // installing `IA32_GS_BASE`; firing an AP before registration
    // would spin forever in `OnceLock::wait`.
    register_ap_late_entry(&ctx.bsp_token(), ap_late_entry);

    // Route the SafeStack-runtime hook + AP boot trampoline through
    // OSTD's safe wrappers under the outer `run_bsp_init` scope opened
    // in `kernel_main_impl`. `install_ap_trampoline_as::<MpGotoFunction>`
    // returns the OSTD `ApTrampolineFn` already reinterpreted as
    // limine's `MpGotoFunction`; both are `extern "C" fn(<single
    // pointer>) -> !`, so the transmute is centralised inside OSTD and
    // boot stays unsafe-free.
    let ap_trampoline: MpGotoFunction = {
        let bsp = ctx.bsp_token();
        install_safestack_runtime(&bsp);
        install_ap_trampoline_as::<MpGotoFunction>(&bsp)
    };

    let ap_task_ptrs = safestack_rt::ap_bootstrap_task_ptrs();
    // `init_ap_pcr_lookup` must run exactly once before any AP boots;
    // `smp_init` is the single caller and runs on the BSP only — the
    // `&BspToken` witness from `ctx.bsp_token()` carries that proof.
    pcr::init_ap_pcr_lookup(&ctx.bsp_token(), &ap_task_ptrs);

    // AP_SIGNALS is indexed uniformly by `ap_slot` (the 1-based
    // non-BSP-CPU counter that we also thread through
    // `cpu.bootstrap(..., ap_slot)`). The AP-side write at
    // `boot/src/smp.rs:116` uses `cpu_idx = ap_slot`; this BSP-side
    // zero/wait must match. Using the limine `enumerate` index here
    // would mis-align whenever the BSP isn't `cpus[0]`.
    let mut ap_count = 0usize;
    let mut ap_slot = 0u64;
    for cpu in cpus.iter() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }
        ap_slot += 1;
        if (ap_slot as usize) > MAX_STATIC_APS {
            klog_info!(
                "MP: skipping CPU 0x{:x} (lapic_id {}): exceeds MAX_STATIC_APS={}",
                cpu.lapic_id,
                cpu.lapic_id,
                MAX_STATIC_APS
            );
            break;
        }

        AP_SIGNALS[ap_slot as usize].store(0, Ordering::Release);
        cpu.bootstrap(ap_trampoline, ap_slot);
        ap_count += 1;
    }

    if ap_count == 0 {
        klog_info!("MP: no secondary CPUs to start");
        return;
    }

    let mut started_count = 0usize;

    // Re-walk under the same ap_slot mapping the spawn loop above used,
    // so the wait reads the same AP_SIGNALS slot the AP wrote to.
    let mut ap_slot = 0u64;
    for cpu in cpus.iter() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }
        ap_slot += 1;
        if (ap_slot as usize) > MAX_STATIC_APS {
            break;
        }

        let mut spins = 2_000_000u32;
        while AP_SIGNALS[ap_slot as usize].load(Ordering::Acquire) != AP_STARTED_MAGIC && spins > 0
        {
            cpu::pause();
            spins -= 1;
        }

        if AP_SIGNALS[ap_slot as usize].load(Ordering::Acquire) == AP_STARTED_MAGIC {
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
