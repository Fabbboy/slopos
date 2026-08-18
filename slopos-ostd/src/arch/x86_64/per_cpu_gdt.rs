//! Per-CPU GDT/TSS/syscall-data storage.
//!
//! # Soundness
//!
//! Each slot is touched by exactly one CPU — CPU 0 during BSP-init, `cpu_id`
//! during that AP's bringup — so the per-slot writes are race-free (Inv. 8).
//!
//! Only reached before PCR is live. The cells are retained anyway so
//! early-boot diagnostics and hermetic fixtures can read a per-CPU GDT
//! footprint without depending on PCR bringup ordering.

use core::cell::SyncUnsafeCell;

use crate::arch::x86_64::gdt::{GDT_STANDARD_ENTRIES, GdtLayout, SegmentSelector, Tss64};

/// Maximum CPU count tracked by the per-CPU GDT slots.
///
/// Mirrors `slopos_arch::pcr::MAX_CPUS`, kept independent so callers need not
/// pull in `slopos_arch` for the constant.
pub const MAX_CPUS: usize = 256;

/// Per-CPU syscall scratch: low quad is the SWAPGS user-RSP scratch, high quad
/// the kernel RSP loaded on syscall entry. The asm trampoline reads them as
/// `gs:[0]` / `gs:[8]`, so the layout must stay `#[repr(C)]` and 16 bytes.
#[repr(C)]
pub struct PerCpuSyscallData {
    pub user_rsp_scratch: u64,
    pub kernel_rsp: u64,
}

impl PerCpuSyscallData {
    pub const fn new() -> Self {
        Self {
            user_rsp_scratch: 0,
            kernel_rsp: 0,
        }
    }
}

static PER_CPU_GDT: SyncUnsafeCell<[GdtLayout; MAX_CPUS]> =
    SyncUnsafeCell::new([GdtLayout::new(); MAX_CPUS]);

static PER_CPU_TSS: SyncUnsafeCell<[Tss64; MAX_CPUS]> =
    SyncUnsafeCell::new([Tss64::new(); MAX_CPUS]);

static PER_CPU_SYSCALL_DATA: SyncUnsafeCell<[PerCpuSyscallData; MAX_CPUS]> = SyncUnsafeCell::new({
    const EMPTY: PerCpuSyscallData = PerCpuSyscallData::new();
    [EMPTY; MAX_CPUS]
});

/// # Safety
/// Per-slot writes are race-free under Inv. 8 (single-CPU task
/// ownership / each CPU owns its `cpu_id` slot). Callers should
/// route through the safe wrappers below rather than calling this
/// directly.
unsafe fn gdt_array_mut() -> &'static mut [GdtLayout; MAX_CPUS] {
    // SAFETY: see fn-level docs.
    unsafe { &mut *PER_CPU_GDT.get() }
}

unsafe fn tss_array_mut() -> &'static mut [Tss64; MAX_CPUS] {
    // SAFETY: see `gdt_array_mut`.
    unsafe { &mut *PER_CPU_TSS.get() }
}

unsafe fn syscall_data_array_mut() -> &'static mut [PerCpuSyscallData; MAX_CPUS] {
    // SAFETY: see `gdt_array_mut`.
    unsafe { &mut *PER_CPU_SYSCALL_DATA.get() }
}

/// Initialise the per-CPU GDT/TSS pair for `cpu_id` and load it on the current
/// CPU. Caller must have populated the TSS (e.g. `rsp0`) before this call.
pub fn init_and_install(cpu_id: usize) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    // SAFETY: per-CPU slot write — see module-level Inv. 8 note.
    unsafe {
        let gdts = gdt_array_mut();
        let tsses = tss_array_mut();
        gdts[cpu_id].entries = GDT_STANDARD_ENTRIES;
        gdts[cpu_id].load_tss(&tsses[cpu_id]);
        tsses[cpu_id].iomap_base = core::mem::size_of::<Tss64>() as u16;
    }
    // SAFETY: `install` reads the layout just populated; both live in
    // `'static` per-CPU cells, so the borrow stays valid while the CPU runs.
    // Inv. 2.
    unsafe {
        super::gdt::install(&(*PER_CPU_GDT.get())[cpu_id], SegmentSelector::TSS);
    }
}

/// Bind a TSS IST slot on `cpu_id` to a kernel-stack-top address. `offset` is
/// the index into `Tss64.ist[]` (0-based, 0..=6 valid).
pub fn set_ist(cpu_id: usize, offset: usize, stack_top: u64) {
    if cpu_id >= MAX_CPUS || offset >= 7 {
        return;
    }
    // SAFETY: per-CPU TSS slot write under Inv. 8.
    unsafe {
        tss_array_mut()[cpu_id].ist[offset] = stack_top;
    }
}

/// Set `rsp0` on `cpu_id`'s TSS *and* the matching syscall-data `kernel_rsp`.
pub fn set_kernel_rsp0(cpu_id: usize, rsp0: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    // SAFETY: per-CPU TSS + syscall-data slot writes under Inv. 8.
    unsafe {
        tss_array_mut()[cpu_id].rsp0 = rsp0;
        syscall_data_array_mut()[cpu_id].kernel_rsp = rsp0;
    }
}

/// Read back `rsp0` from `cpu_id`'s TSS; used by the pre-PCR GS_BASE seeding
/// path.
pub fn rsp0(cpu_id: usize) -> u64 {
    if cpu_id >= MAX_CPUS {
        return 0;
    }
    // SAFETY: read-only access to a per-CPU slot is sound (Inv. 8).
    unsafe { (*PER_CPU_TSS.get())[cpu_id].rsp0 }
}

/// Update only the syscall-data `kernel_rsp` half, for the per-task
/// kernel-stack swap on `task_first_run`.
pub fn set_syscall_kernel_rsp(cpu_id: usize, rsp: u64) {
    if cpu_id >= MAX_CPUS {
        return;
    }
    // SAFETY: per-CPU syscall-data slot write under Inv. 8.
    unsafe {
        syscall_data_array_mut()[cpu_id].kernel_rsp = rsp;
    }
}

/// Return a raw pointer to `cpu_id`'s syscall-data slot. Boot's pre-PCR
/// GS_BASE wiring writes it into the legacy `SYSCALL_CPU_DATA_PTR` cell and
/// the `KERNEL_GS_BASE` MSR.
pub fn syscall_data_ptr(cpu_id: usize) -> u64 {
    if cpu_id >= MAX_CPUS {
        return 0;
    }
    // SAFETY: address computation from a `'static` cell, no dereference.
    unsafe { &(*PER_CPU_SYSCALL_DATA.get())[cpu_id] as *const PerCpuSyscallData as u64 }
}
