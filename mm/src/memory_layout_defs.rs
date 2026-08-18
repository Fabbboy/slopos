//! Memory layout constants for x86_64.

use crate::paging_defs::PAGE_SIZE_4KB;

/// Boot stack size (16 KB).
pub const BOOT_STACK_SIZE: u64 = 0x4000;

pub const BOOT_STACK_PHYS_ADDR: u64 = 0x20000;

pub const EARLY_PML4_PHYS_ADDR: u64 = 0x30000;

pub const EARLY_PDPT_PHYS_ADDR: u64 = 0x31000;

pub const EARLY_PD_PHYS_ADDR: u64 = 0x32000;

/// The kernel is mapped in the highest 2GB of 64-bit address space.
pub const KERNEL_VIRTUAL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Higher Half Direct Map base; physical memory is mapped starting here.
pub const HHDM_VIRT_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Device MMIO maps here, separate from HHDM because Limine v8+ maps only RAM
/// in HHDM.
pub const MMIO_VIRT_BASE: u64 = 0xFFFF_8100_0000_0000;

/// MMIO virtual address space size (16 GB).
pub const MMIO_VIRT_SIZE: u64 = 0x0000_0004_0000_0000;

/// Sentinel kernel-half address for `mm/src/user_copy.rs::check_kernel_guard`.
/// Any stable higher-half address suffices; it has no memory-layout role.
pub const KERNEL_HALF_PROBE_VA: u64 = 0xFFFF_FFFF_9000_0000;

// Kernel task stacks: frames are mapped on demand into this region, each stack
// with an unmapped guard page below it to catch overflow by page fault. The
// region is independent of the kernel image, so growing kernel code costs no
// task-stack capacity.

pub const KSTACK_VA_BASE: u64 = 0xFFFF_FFFF_A000_0000;

/// End of the kernel-stack virtual region (exclusive).
pub const KSTACK_VA_END: u64 = 0xFFFF_FFFF_C000_0000;

/// Stride per slot: 1 guard page + up to 60 KB usable, rounded to 64 KB.
pub const KSTACK_STRIDE: u64 = 0x10000;

pub const KSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

pub const KSTACK_MAX_SLOTS: usize = ((KSTACK_VA_END - KSTACK_VA_BASE) / KSTACK_STRIDE) as usize;

// SafeStack data stacks, one per task that owns a kernel stack: address-taken
// locals and dynamic allocas live here while return addresses and register
// spills stay on the kernel stack, which is what defeats ROP. The slot count
// matches KSTACK_MAX_SLOTS so every live task can own one of each.

pub const USTACK_VA_BASE: u64 = 0xFFFF_FFFF_D000_0000;

/// End of the data-stack virtual region (exclusive).
pub const USTACK_VA_END: u64 = 0xFFFF_FFFF_F000_0000;

/// Stride per data-stack slot (64 KB, matches KSTACK_STRIDE).
pub const USTACK_STRIDE: u64 = 0x10000;

pub const USTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

pub const USTACK_MAX_SLOTS: usize = ((USTACK_VA_END - USTACK_VA_BASE) / USTACK_STRIDE) as usize;

pub const USER_SPACE_START_VA: u64 = 0x0000_0000_0000_0000;

/// User space end virtual address (up to canonical hole).
pub const USER_SPACE_END_VA: u64 = 0x0000_8000_0000_0000;

/// Start of the high-canonical kernel half; between `USER_SPACE_END_VA` and
/// this address lies the non-canonical hole, which faults on access.
///
/// Use this to reject user-supplied addresses; the kernel's own load address is
/// `KERNEL_VIRTUAL_BASE`.
pub const KERNEL_SPACE_START_VA: u64 = 0xFFFF_8000_0000_0000;

pub const PROCESS_CODE_START_VA: u64 = 0x0000_0000_0040_0000;

pub const PROCESS_DATA_START_VA: u64 = 0x0000_0000_0080_0000;

pub const PROCESS_TLS_BASE_VA: u64 = 0x0000_0000_00C0_0000;

pub const PROCESS_HEAP_START_VA: u64 = 0x0000_0000_0100_0000;

pub const PROCESS_HEAP_MAX_VA: u64 = 0x0000_0000_4000_0000;

pub const PROCESS_STACK_TOP_VA: u64 = 0x0000_7FFF_FF00_0000;

/// Process stack size in bytes (1 MB).
pub const PROCESS_STACK_SIZE_BYTES: u64 = 0x0000_0000_0010_0000;

pub const PROCESS_MMAP_START_VA: u64 = 0x0000_0000_4000_0000;

pub const PROCESS_MMAP_END_VA: u64 = 0x0000_7FFF_FE00_0000;

pub const EXCEPTION_STACK_REGION_BASE: u64 = 0xFFFF_FFFF_C000_0000;

/// Exclusive end of the IST **safe**-stack region, where `USTACK_VA_BASE`
/// begins. `__safestack_pointer_address` tests the running `RSP` against this
/// range to tell IST context from task/kernel/boot context; only IST safe
/// stacks live in it.
pub const EXCEPTION_STACK_REGION_END: u64 = USTACK_VA_BASE;

/// Stride between exception stacks (64 KB).
pub const EXCEPTION_STACK_REGION_STRIDE: u64 = 0x0001_0000;

pub const EXCEPTION_STACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

pub const EXCEPTION_STACK_PAGES: u64 = 8;

/// Exception stack usable size (32 KB).
pub const EXCEPTION_STACK_SIZE: u64 = EXCEPTION_STACK_PAGES * PAGE_SIZE_4KB;

// Per-CPU data-stack analogue of the IST safe-stack region: without it an
// exception handler's address-taken locals land on whichever task happened to
// be interrupted. One guard-paged stack per CPU suffices because interrupts are
// masked inside exception handlers, so nesting is strictly LIFO.

pub const EXC_DSTACK_REGION_BASE: u64 = USTACK_VA_END;

/// Stride per CPU slot (128 KB: 4 KB guard + 124 KB usable), sized so the
/// deepest exception → diagnostic-dump → `panic!` → `core::fmt` chain, plus an
/// NMI nested on top, cannot exhaust it.
pub const EXC_DSTACK_REGION_STRIDE: u64 = 0x0002_0000;

/// Unmapped guard page at the slot base; the stack grows down into it.
pub const EXC_DSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's exception data stack (124 KB).
pub const EXC_DSTACK_USABLE_SIZE: u64 = EXC_DSTACK_REGION_STRIDE - EXC_DSTACK_GUARD_SIZE;

pub const EXC_DSTACK_PAGES: u64 = EXC_DSTACK_USABLE_SIZE / PAGE_SIZE_4KB;

// `slopos-ostd` sits below `mm` in the crate graph and cannot import this
// module, so `__safestack_pointer_address` carries its own copy of these
// bounds; drift would route an exception handler's locals onto the wrong data
// stack.
const _: () = {
    assert!(
        EXCEPTION_STACK_REGION_BASE == slopos_arch::pcr::SAFESTACK_IST_REGION_BASE,
        "SafeStack IST-region base drifted from EXCEPTION_STACK_REGION_BASE",
    );
    assert!(
        EXCEPTION_STACK_REGION_END - EXCEPTION_STACK_REGION_BASE
            == slopos_arch::pcr::SAFESTACK_IST_REGION_SPAN,
        "SafeStack IST-region span drifted from EXCEPTION_STACK_REGION span",
    );
};

// The fatal-fault/panic path switches both the safe (RSP) and SafeStack data
// stacks to these per-CPU emergency stacks before any `core::fmt` runs, so
// panic formatting cannot overflow a near-full task stack into its guard page.
// They are separate from the IST/EXC_DSTACK stacks so an NMI nested on top of
// the panic cannot collide with the report in progress.

pub const EMERGENCY_DSTACK_REGION_BASE: u64 =
    EXC_DSTACK_REGION_BASE + (slopos_arch::pcr::MAX_CPUS as u64) * EXC_DSTACK_REGION_STRIDE;

/// Stride per CPU slot (128 KB: 4 KB guard + 124 KB usable).
pub const EMERGENCY_DSTACK_REGION_STRIDE: u64 = 0x0002_0000;

pub const EMERGENCY_DSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's emergency data stack (124 KB).
pub const EMERGENCY_DSTACK_USABLE_SIZE: u64 =
    EMERGENCY_DSTACK_REGION_STRIDE - EMERGENCY_DSTACK_GUARD_SIZE;

pub const EMERGENCY_DSTACK_PAGES: u64 = EMERGENCY_DSTACK_USABLE_SIZE / PAGE_SIZE_4KB;

pub const EMERGENCY_SAFE_STACK_REGION_BASE: u64 = EMERGENCY_DSTACK_REGION_BASE
    + (slopos_arch::pcr::MAX_CPUS as u64) * EMERGENCY_DSTACK_REGION_STRIDE;

/// Stride per CPU slot (64 KB: 4 KB guard + 60 KB usable).
pub const EMERGENCY_SAFE_STACK_REGION_STRIDE: u64 = 0x0001_0000;

pub const EMERGENCY_SAFE_STACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's emergency safe stack (60 KB).
pub const EMERGENCY_SAFE_STACK_USABLE_SIZE: u64 =
    EMERGENCY_SAFE_STACK_REGION_STRIDE - EMERGENCY_SAFE_STACK_GUARD_SIZE;

pub const EMERGENCY_SAFE_STACK_PAGES: u64 = EMERGENCY_SAFE_STACK_USABLE_SIZE / PAGE_SIZE_4KB;

/// Guards the emergency-stack reservation against running off the end of the
/// kernel higher half as MAX_CPUS grows.
const _: () = {
    let top = EMERGENCY_SAFE_STACK_REGION_BASE
        + (slopos_arch::pcr::MAX_CPUS as u64) * EMERGENCY_SAFE_STACK_REGION_STRIDE;
    assert!(
        top > EMERGENCY_DSTACK_REGION_BASE && top <= 0xFFFF_FFFF_FF00_0000,
        "emergency stack regions overflow the kernel higher half",
    );
};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessMemoryLayout {
    pub code_start: u64,
    pub data_start: u64,
    pub heap_start: u64,
    pub heap_max: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    pub user_space_start: u64,
    pub user_space_end: u64,
}

/// Default (non-randomized) process memory layout, used as the base for ASLR
/// randomization and heap limit checks; ASLR produces a modified copy at
/// process creation time.
pub const DEFAULT_PROCESS_LAYOUT: ProcessMemoryLayout = ProcessMemoryLayout {
    code_start: PROCESS_CODE_START_VA,
    data_start: PROCESS_DATA_START_VA,
    heap_start: PROCESS_HEAP_START_VA,
    heap_max: PROCESS_HEAP_MAX_VA,
    stack_top: PROCESS_STACK_TOP_VA,
    stack_size: PROCESS_STACK_SIZE_BYTES,
    user_space_start: USER_SPACE_START_VA,
    user_space_end: USER_SPACE_END_VA,
};

/// Re-exported from `abi` rather than declared here: the process registry, this
/// crate's address-space table and the descriptor tables key on each other's
/// slot indices, so the bound must be one number all three can see.
pub use slopos_abi::task::MAX_PROCESSES;

/// Highest process id the allocator ever hands out; ids start at 1, so the id
/// space is `1..=MAX_PROCESS_ID`. It is as wide as the slot space because
/// nothing indexes an array by process id.
pub const MAX_PROCESS_ID: u32 = MAX_PROCESSES as u32;

// The free-id ring is `MAX_PROCESSES` entries of `u16`, and every issued
// id can be free at once.
const _: () = assert!(MAX_PROCESS_ID as usize <= MAX_PROCESSES);
const _: () = assert!(MAX_PROCESS_ID <= u16::MAX as u32);
const _: () = assert!(MAX_PROCESS_ID > 0);
