use core::arch::naked_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use limine::mp::{MP_FLAG_X2APIC, MpInfo};

use slopos_arch::{cpu, is_cpu_online, pcr};
use slopos_core::sched::{enter_scheduler, init_scheduler_for_ap};
use slopos_core::scheduler::safestack_rt;
use slopos_drivers::apic;
use slopos_mm::tlb;
use slopos_utils::klog_info;

use crate::gdt::syscall_msr_init;
use crate::idt::idt_load;
use crate::ist_stacks;
use crate::limine_protocol;

const AP_STARTED_MAGIC: u64 = 0x4150_5354_4152_5444;
const MAX_CPUS: usize = 256;

/// Naked AP entry trampoline — installs GS_BASE before any instrumented
/// Rust code runs on this AP.
///
/// Limine's MP boot flow jumps each AP directly to this function with
/// `rdi` pointing at the `Cpu` / `MpInfo` struct whose `extra` field
/// (offset 24) we set to the 1-based AP slot in [`smp_init`].  The
/// BSP pre-populated [`pcr::AP_PCR_PTRS`]`[slot - 1]` and primed
/// `AP_PCRS[slot - 1]`'s `self_ref` + `unsafe_sp` before triggering
/// the bootstrap, so all this trampoline has to do is:
///
///   1. Load the PCR pointer from `AP_PCR_PTRS[slot - 1]`.
///   2. WRMSR IA32_GS_BASE with that pointer.
///   3. Jump to [`ap_entry_rust`], preserving `rdi` (the MpInfo ptr).
///
/// Naked because the first instruction of any non-naked fn compiled
/// with `-Zsanitizer=safestack` fetches `gs:[0]` — which would read
/// garbage on an AP with GS_BASE still at 0.
#[unsafe(naked)]
#[unsafe(no_mangle)]
unsafe extern "C" fn ap_entry(_cpu_info: &MpInfo) -> ! {
    naked_asm!(
        // rdi = &MpInfo, preserved across this trampoline.
        //
        // Read slot from MpInfo.extra @ offset 24 (1-based).  This
        // is the index the BSP assigned in `smp_init` below; each
        // AP's PCR, bootstrap Task, and bootstrap unsafe stack are
        // all indexed by `slot - 1` on the Rust side.  All we do here
        // is install GS_BASE — the Rust `init_bootstrap_tasks` call
        // already stamped this AP's bootstrap Task's
        // `unsafe_stack_sp` field, and `init_ap_pcr_lookup` already
        // stamped this AP's PCR `self_ref` + `current_task`.
        "mov rax, [rdi + {extra_offset}]",
        "dec rax",                              // zero-based slot index
        // rax = AP_PCR_PTRS[rax]
        "lea rcx, [rip + {ap_pcr_ptrs}]",
        "mov rax, [rcx + rax*8]",               // rax = &AP_PCRS[slot-1]
        // WRMSR IA32_GS_BASE = rax
        "mov rdx, rax",
        "shr rdx, 32",
        // eax already holds low 32 of rax
        "mov ecx, {ia32_gs_base}",              // IA32_GS_BASE MSR number
        "wrmsr",
        // Tail-call into the real Rust AP entry.  It is compiled with
        // -Zsanitizer=safestack and its prologue will now see a valid
        // `gs:[CURRENT_TASK]` (→ AP's bootstrap Task with primed
        // unsafe_stack_sp).
        "jmp {ap_entry_rust}",
        extra_offset = const 24,
        ia32_gs_base = const 0xC000_0101_u32,
        ap_pcr_ptrs = sym pcr::AP_PCR_PTRS,
        ap_entry_rust = sym ap_entry_rust,
    )
}

// The asm trampoline's `mov rax, [rdi + 24]` loads MpInfo.extra_argument.
// Limine 0.6's MpInfo layout documents this offset as fixed (processor_id
// u32 + lapic_id u32 + _resvd0 u64 + goto_addr AtomicPtr<()> = 24 bytes).
// The field is `pub(crate)` so a `const { offset_of!(...) }` probe can't
// see it; pinning the Cargo.lock to 0.6.3 and the explicit 24 here is the
// compile-time contract.  A mismatched limine bump would show up as an
// immediate triple-fault on AP bringup.

/// Per-CPU completion signals.  The BSP passes each AP its index into this
/// array via `MpInfo::bootstrap(ap_entry, slot)`.  The AP stores
/// `AP_STARTED_MAGIC` here once it is fully initialised so the BSP can
/// spin-wait for it—replacing the now-private `MpInfo::extra_argument` field
/// that limine 0.6 no longer exposes for writing.
static AP_SIGNALS: [AtomicU64; MAX_CPUS] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_CPUS]
};

unsafe extern "C" fn ap_entry_rust(cpu_info: &MpInfo) -> ! {
    cpu::disable_interrupts();

    cpu::enable_sse();

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
    // Use the 1-based slot the BSP encoded in `cpu.extra` — the naked
    // trampoline already selected `AP_PCRS[extra - 1]` and installed
    // GS_BASE to it, so `init_ap_pcr` below MUST use the same index
    // or it will point the AP at a different PCR, silently swapping
    // the SafeStack unsafe-SP slot mid-boot.  The old
    // `NEXT_CPU_ID.fetch_add(1)` scheme raced with the trampoline's
    // fixed indexing whenever APs came up out of order.
    let cpu_idx = cpu_info.extra_argument() as usize;

    tlb::notify_cpu_online_id(cpu_idx);

    unsafe {
        let ap_pcr = pcr::init_ap_pcr(cpu_idx, apic_id);
        (*ap_pcr).init_gdt();
        (*ap_pcr).install();
    }

    // APs have per-CPU TSS structures; re-bind IST pointers after installing
    // the AP GDT/TSS so exceptions (notably #PF) do not enter with IST=0.
    let mut ap_boot_ctx = slopos_hermetic::take_for_ap(cpu_idx);
    ist_stacks::ist_bind_current_cpu(&mut ap_boot_ctx);

    idt_load();
    syscall_msr_init();
    slopos_hermetic::return_after_ap(cpu_idx, ap_boot_ctx);

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

    // Seed each AP PCR's self_ref + current_task so the naked
    // `ap_entry` trampoline can install GS_BASE and have
    // `__safestack_pointer_address` find a valid bootstrap Task
    // on the very first instrumented call.  Limited to MAX_STATIC_APS
    // — if the platform reports more APs we only start the first N.
    const MAX_STATIC_APS: usize = safestack_rt::MAX_STATIC_APS;
    unsafe {
        safestack_rt::init_bootstrap_tasks();
        let ap_task_ptrs = safestack_rt::ap_bootstrap_task_ptrs();
        pcr::init_ap_pcr_lookup(&ap_task_ptrs);
    }

    let mut ap_count = 0usize;
    let mut ap_slot = 0u64;
    for (i, cpu) in cpus.iter().enumerate() {
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

        AP_SIGNALS[i].store(0, Ordering::Release);
        cpu.bootstrap(ap_entry, ap_slot);
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
