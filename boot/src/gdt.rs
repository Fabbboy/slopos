use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::arch::gdt::IstSlot;
use slopos_arch::pcr::{MAX_CPUS, get_current_cpu};
use slopos_hermetic::KernelStackTop;
use slopos_ostd::arch::x86_64::msr::{Msr, install_syscall_msrs, star_from_selectors, write_msr};
use slopos_ostd::arch::x86_64::per_cpu_gdt;
use slopos_ostd::klog_debug;

// `SYSCALL_CPU_DATA_PTR` keeps the same C-ABI symbol the asm
// trampoline reads via `gs:[16]` on CPU 0 before PCR is live. The
// underlying per-CPU storage lives in
// `slopos_ostd::arch::x86_64::per_cpu_gdt`; this atomic caches the
// CPU-0 pointer for the syscall asm to load on entry. `AtomicU64`
// has the same memory layout as a bare `u64`, so the asm side's
// `gs:[16]` load sees the value verbatim.
slopos_ostd::no_mangle_static! {
    static SYSCALL_CPU_DATA_PTR: AtomicU64 = AtomicU64::new(0);
}

pub fn gdt_init() {
    gdt_init_for_cpu(0);
}

pub fn gdt_init_for_cpu(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }

    if slopos_arch::pcr::is_pcr_initialized() {
        klog_debug!("GDT: Skipped - using PCR-based GDT for CPU {}", cpu_id);
        return;
    }

    klog_debug!("GDT: Initializing descriptor tables for CPU {}", cpu_id);

    if cpu_id == 0 {
        per_cpu_gdt::set_kernel_rsp0(
            cpu_id,
            slopos_ostd::arch::x86_64::linker::kernel_stack_top() as u64,
        );
    }
    per_cpu_gdt::init_and_install(cpu_id);

    klog_debug!("GDT: Initialized with TSS loaded for CPU {}", cpu_id);
}
pub fn gdt_set_kernel_rsp0(rsp0: u64) {
    let cpu_id = get_current_cpu();
    gdt_set_kernel_rsp0_for_cpu(cpu_id, rsp0);
}

pub fn gdt_set_kernel_rsp0_for_cpu(cpu_id: usize, rsp0: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    per_cpu_gdt::set_kernel_rsp0(cpu_id, rsp0);
    if let Some(pcr) = slopos_arch::pcr::get_pcr_mut_via_token(cpu_id) {
        pcr.kernel_rsp = rsp0;
        pcr.sync_tss_rsp0();
    }
}

/// Bind an IST slot on the current CPU to a real kernel stack.
///
/// `slot` is the typed IST slot enum (1..7); `stack_top` is a borrowed
/// `KernelStackTop` whose lifetime is tied to the backing allocation.
/// Both replace earlier `u8` / `u64` parameters whose runtime checks
/// (zero index, overflow index, fake address) become compile-time
/// errors.
///
/// `&mut BootCtx<'_, K: CpuInitKind>` gates the call: BSP-init,
/// AP-init, and hermetic test scopes all need to bind IST slots, so
/// the surface is kind-polymorphic over [`slopos_hermetic::CpuInitKind`].
pub fn gdt_set_ist<'b, K: slopos_hermetic::CpuInitKind>(
    _ctx: &mut slopos_hermetic::BootCtx<'b, K>,
    slot: IstSlot,
    stack_top: KernelStackTop<'_>,
) {
    let cpu_id = get_current_cpu();
    gdt_set_ist_for_cpu(_ctx, cpu_id, slot, stack_top);
}

pub fn gdt_set_ist_for_cpu<'b, K: slopos_hermetic::CpuInitKind>(
    _ctx: &mut slopos_hermetic::BootCtx<'b, K>,
    cpu_id: usize,
    slot: IstSlot,
    stack_top: KernelStackTop<'_>,
) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let addr = stack_top.as_u64();
    let offset = slot.as_tss_offset();
    per_cpu_gdt::set_ist(cpu_id, offset, addr);
    if let Some(pcr) = slopos_arch::pcr::get_pcr_mut_via_token(cpu_id) {
        pcr.set_ist(slot.as_index(), addr);
    }
}

/// Program the SYSCALL/SYSRET MSRs (`STAR`, `LSTAR`, `SFMASK`) and
/// initialise the per-CPU GS_BASE syscall-scratch wiring.
///
/// Both BSP-init and per-AP bringup paths call this, so it accepts
/// any `CpuInitWitness` (`BspToken` or `ApToken`); the witness simply
/// gates the call to a boot-init scope.
pub fn syscall_msr_init<W: slopos_ostd::sync::CpuInitWitness>(witness: &W) {
    use slopos_arch::arch::gdt::SegmentSelector;

    klog_debug!("SYSCALL: Initializing MSRs for fast syscall path");

    let star_value = star_from_selectors(SegmentSelector::KERNEL_CODE, SegmentSelector::USER_DATA);
    let lstar_value = slopos_ostd::user::mode::user_return_trampoline_addr();
    let sfmask_value: u64 = 0x0000_0000_0004_7700;

    // Inv. 2: __ostd_user_return is the LSTAR target — see
    // `slopos_ostd::user::asm::user_return.s`.  The STAR selectors
    // match the GDT layout already loaded by gdt_init_for_cpu /
    // PCR::install.  The trampoline expects `pcr.user_ctx_ptr` and
    // `pcr.kernel_return_ctx` to have been populated by the OSTD
    // user-mode backend (registered inline from the boot path in
    // `kernel_main_impl`) before any user-mode SYSCALL fires; per-task
    // entry rides through `user_task_first_run` / `user_task_loop`
    // (see `core::syscall::user_loop`). `install_syscall_msrs` itself
    // is safe — it takes the CpuInitWitness as its soundness gate.
    install_syscall_msrs(witness, star_value, lstar_value, sfmask_value);

    klog_debug!(
        "SYSCALL: STAR=0x{:016x} LSTAR=0x{:016x} SFMASK=0x{:016x}",
        star_value,
        lstar_value,
        sfmask_value
    );

    syscall_gs_base_init();
}

fn syscall_gs_base_init() {
    if slopos_arch::pcr::is_pcr_initialized() {
        klog_debug!("SYSCALL: Skipped GS_BASE init - using PCR");
        return;
    }
    let cpu_id = get_current_cpu();
    syscall_gs_base_init_for_cpu(cpu_id);
}

fn syscall_gs_base_init_for_cpu(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    let rsp0 = per_cpu_gdt::rsp0(cpu_id);
    per_cpu_gdt::set_syscall_kernel_rsp(cpu_id, rsp0);
    let cpu_data_ptr = per_cpu_gdt::syscall_data_ptr(cpu_id);
    if cpu_id == 0 {
        store_syscall_cpu_data_ptr(cpu_data_ptr);
    }
    write_msr(Msr::KERNEL_GS_BASE, cpu_data_ptr);
    klog_debug!(
        "SYSCALL: CPU {} KERNEL_GS_BASE=0x{:016x}",
        cpu_id,
        cpu_data_ptr
    );
}

/// Store `cpu_data_ptr` into the `SYSCALL_CPU_DATA_PTR` cell. The
/// asm syscall trampoline reads this address before PCR-based
/// `gs:[…]` accessors come online.
fn store_syscall_cpu_data_ptr(cpu_data_ptr: u64) {
    SYSCALL_CPU_DATA_PTR.store(cpu_data_ptr, Ordering::Release);
}

pub fn syscall_update_kernel_rsp(rsp: u64) {
    let cpu_id = get_current_cpu();
    syscall_update_kernel_rsp_for_cpu(cpu_id, rsp);
}

pub fn syscall_update_kernel_rsp_for_cpu(cpu_id: usize, rsp: u64) {
    per_cpu_gdt::set_syscall_kernel_rsp(cpu_id, rsp);
}
