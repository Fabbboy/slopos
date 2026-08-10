//! Memory layout constants for x86_64.
//!
//! This module defines the virtual and physical address space layout used
//! by SlopOS, including kernel space, user space, and special regions.

use crate::paging_defs::PAGE_SIZE_4KB;

// =============================================================================
// Boot-Time Memory
// =============================================================================

/// Boot stack size (16 KB).
pub const BOOT_STACK_SIZE: u64 = 0x4000;

/// Boot stack physical address.
pub const BOOT_STACK_PHYS_ADDR: u64 = 0x20000;

/// Early PML4 table physical address.
pub const EARLY_PML4_PHYS_ADDR: u64 = 0x30000;

/// Early PDPT table physical address.
pub const EARLY_PDPT_PHYS_ADDR: u64 = 0x31000;

/// Early PD table physical address.
pub const EARLY_PD_PHYS_ADDR: u64 = 0x32000;

// =============================================================================
// Kernel Virtual Address Space
// =============================================================================

/// Kernel virtual base address.
/// The kernel is mapped in the highest 2GB of 64-bit address space.
pub const KERNEL_VIRTUAL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Higher Half Direct Map base address.
/// Physical memory is identity-mapped starting at this virtual address.
pub const HHDM_VIRT_BASE: u64 = 0xFFFF_8000_0000_0000;

/// MMIO virtual address space base.
/// Device MMIO regions are mapped starting at this virtual address.
/// This is separate from HHDM because Limine v8+ only maps RAM in HHDM.
pub const MMIO_VIRT_BASE: u64 = 0xFFFF_8100_0000_0000;

/// MMIO virtual address space size (16 GB should be more than enough).
pub const MMIO_VIRT_SIZE: u64 = 0x0000_0004_0000_0000;

/// Sentinel kernel-half virtual address for the user-VA-predicate
/// guard in `mm/src/user_copy.rs::check_kernel_guard`. Any reliably-
/// mapped higher-half address suffices — the guard merely asserts
/// that the predicate rejects a known-kernel VA. The constant has no
/// memory-layout role beyond being kernel-half and stable across
/// boots.
pub const KERNEL_HALF_PROBE_VA: u64 = 0xFFFF_FFFF_9000_0000;

// =============================================================================
// Kernel Stack Virtual Region (dynamic task stacks)
// =============================================================================
//
// A dedicated kernel virtual address region backs `KernelStack` allocations.
// Physical frames are requested on demand from the page allocator and mapped
// into this region; each stack has an unmapped guard page below it to catch
// overflow via page fault.
//
// This region is **independent of the kernel image**, so growing kernel code
// (adjusting `_kernel_end`) does not reduce task-stack capacity — unlike the
// previous scheme that allocated stacks from the kernel heap, whose free
// pages compete with the reserved kernel-image region.
//
// Layout (between the heap and the IST/exception-stack region):
//   KERNEL_HEAP_VEND      = 0xFFFF_FFFF_A000_0000  (heap ends)
//   KSTACK_VA_BASE        = 0xFFFF_FFFF_A000_0000  (new region starts)
//   KSTACK_VA_END         = 0xFFFF_FFFF_C000_0000  (new region ends)
//   EXCEPTION_STACK_BASE  = 0xFFFF_FFFF_C000_0000  (IST region begins)
//
// 512 MB / 64 KB stride = 8192 slots.

/// Base of the kernel-stack virtual region.
pub const KSTACK_VA_BASE: u64 = 0xFFFF_FFFF_A000_0000;

/// End of the kernel-stack virtual region (exclusive).
pub const KSTACK_VA_END: u64 = 0xFFFF_FFFF_C000_0000;

/// Stride per slot: 1 guard page + up to 60 KB usable, rounded to 64 KB.
pub const KSTACK_STRIDE: u64 = 0x10000;

/// Guard page size (one unmapped 4 KB page per slot).
pub const KSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Maximum number of concurrently allocated kernel stacks.
pub const KSTACK_MAX_SLOTS: usize = ((KSTACK_VA_END - KSTACK_VA_BASE) / KSTACK_STRIDE) as usize;

// =============================================================================
// Data-Stack Virtual Region (SafeStack dual-stack data stacks)
// =============================================================================
//
// Mirror of the KSTACK VA region, dedicated to the SafeStack-sanitizer
// data stacks.  Each task that owns a kernel stack also owns one data
// stack in this region.  Address-taken locals and dynamic allocas live on
// the data stack; return addresses and register spills remain on the
// kernel stack — isolating the two is what defeats ROP by design.
//
// Layout (above the exception-stack region):
//   EXCEPTION_STACK_REGION_BASE  = 0xFFFF_FFFF_C000_0000
//   ... IST slots (MAX_CPUS * 7 * 64 KB) ...
//   USTACK_VA_BASE               = 0xFFFF_FFFF_D000_0000  (data-stack region)
//   USTACK_VA_END                = 0xFFFF_FFFF_F000_0000
//
// 512 MB / 64 KB stride = 8192 slots — matches the KSTACK cap so every live
// task can own one of each.

/// Base of the data-stack virtual region.
pub const USTACK_VA_BASE: u64 = 0xFFFF_FFFF_D000_0000;

/// End of the data-stack virtual region (exclusive).
pub const USTACK_VA_END: u64 = 0xFFFF_FFFF_F000_0000;

/// Stride per data-stack slot (64 KB, matches KSTACK_STRIDE).
pub const USTACK_STRIDE: u64 = 0x10000;

/// Guard page size for the data stack (one 4 KB page, unmapped).
pub const USTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Maximum concurrent data stacks (matches KSTACK_MAX_SLOTS).
pub const USTACK_MAX_SLOTS: usize = ((USTACK_VA_END - USTACK_VA_BASE) / USTACK_STRIDE) as usize;

// =============================================================================
// User Virtual Address Space
// =============================================================================

/// User space start virtual address.
pub const USER_SPACE_START_VA: u64 = 0x0000_0000_0000_0000;

/// User space end virtual address (up to canonical hole).
pub const USER_SPACE_END_VA: u64 = 0x0000_8000_0000_0000;

/// Start of kernel (high-canonical) virtual address space.
///
/// On x86-64, addresses between `USER_SPACE_END_VA` and this value fall in
/// the non-canonical hole and fault on access.  Anything at or above this
/// address is in the high-canonical half reserved for the kernel.
///
/// Use this to reject user-supplied addresses that would land in kernel space
/// (e.g. ELF segment validation).  For the kernel's own load address, use
/// `KERNEL_VIRTUAL_BASE` instead.
pub const KERNEL_SPACE_START_VA: u64 = 0xFFFF_8000_0000_0000;

/// Process code segment start virtual address.
pub const PROCESS_CODE_START_VA: u64 = 0x0000_0000_0040_0000;

/// Process data segment start virtual address.
pub const PROCESS_DATA_START_VA: u64 = 0x0000_0000_0080_0000;

/// Process static TLS block base virtual address.
pub const PROCESS_TLS_BASE_VA: u64 = 0x0000_0000_00C0_0000;

/// Process heap start virtual address.
pub const PROCESS_HEAP_START_VA: u64 = 0x0000_0000_0100_0000;

/// Process heap maximum virtual address.
pub const PROCESS_HEAP_MAX_VA: u64 = 0x0000_0000_4000_0000;

/// Process stack top virtual address.
pub const PROCESS_STACK_TOP_VA: u64 = 0x0000_7FFF_FF00_0000;

/// Process stack size in bytes (1 MB).
pub const PROCESS_STACK_SIZE_BYTES: u64 = 0x0000_0000_0010_0000;

/// mmap region start virtual address (above heap max).
pub const PROCESS_MMAP_START_VA: u64 = 0x0000_0000_4000_0000;

/// mmap region end virtual address (below stack).
pub const PROCESS_MMAP_END_VA: u64 = 0x0000_7FFF_FE00_0000;

// =============================================================================
// Exception Stack Region
// =============================================================================

/// Exception stack region base virtual address.
pub const EXCEPTION_STACK_REGION_BASE: u64 = 0xFFFF_FFFF_C000_0000;

/// Exception (IST) **safe**-stack region end (exclusive) — the data-stack
/// region (`USTACK_VA_BASE`) begins here.  Used by
/// `__safestack_pointer_address` to decide, purely from the running `RSP`,
/// whether instrumented code is executing on an IST/exception stack (and
/// must therefore walk the per-CPU exception data stack) or on a task /
/// kernel / boot stack (walk the per-task data stack).  Only IST safe
/// stacks live in `[EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_END)`;
/// PCRs are `.bss` statics in the kernel-image region and task stacks are
/// in `KSTACK`/`USTACK`, so this range uniquely identifies IST context.
pub const EXCEPTION_STACK_REGION_END: u64 = USTACK_VA_BASE;

/// Stride between exception stacks (64 KB).
pub const EXCEPTION_STACK_REGION_STRIDE: u64 = 0x0001_0000;

/// Guard page size for exception stacks (one 4 KB page).
pub const EXCEPTION_STACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Number of pages per exception stack (8 pages = 32 KB).
pub const EXCEPTION_STACK_PAGES: u64 = 8;

/// Exception stack usable size (32 KB).
pub const EXCEPTION_STACK_SIZE: u64 = EXCEPTION_STACK_PAGES * PAGE_SIZE_4KB;

// =============================================================================
// Exception / IST SafeStack DATA-Stack Region (per-CPU)
// =============================================================================
//
// The data-stack analogue of the IST safe-stack region above.  While the
// IST mechanism gives each exception/IRQ vector a dedicated *safe* stack
// (RSP), the SafeStack sanitizer needs a matching *data* stack for the
// handler's address-taken locals (the `[core::fmt::Argument; N]` array a
// `klog!`/`panic!` builds).  Without it, an exception handler's
// instrumented code writes those locals onto whichever task happened to be
// interrupted — the root cause of the recursive-#PF-in-panic crash.
//
// One mapped, guard-paged data stack per CPU, shared LIFO across all
// exception / NMI / MCE / fault-nesting on that CPU (interrupts are masked
// inside exception handlers, so usage is strictly last-in-first-out and a
// single per-CPU stack suffices).  `__safestack_pointer_address` selects
// it (via `gs:[ProcessorControlRegion::ist_unsafe_sp]`) whenever the
// running `RSP` lies in `EXCEPTION_STACK_REGION`.
//
// Layout (above the data-stack/USTACK region, below the top of the
// high-canonical half):
//   USTACK_VA_END           = 0xFFFF_FFFF_F000_0000  (per-task data stacks end)
//   EXC_DSTACK_REGION_BASE   = 0xFFFF_FFFF_F000_0000  (this region starts)
//   ... MAX_CPUS slots, 128 KB stride ...

/// Base of the per-CPU exception/IST data-stack region.
pub const EXC_DSTACK_REGION_BASE: u64 = USTACK_VA_END;

/// Stride per CPU slot (128 KB: 4 KB guard + 124 KB usable).  Generous
/// versus the 64 KB bootstrap data stack so the deepest exception →
/// diagnostic-dump → `panic!` → `core::fmt` chain (plus an NMI nested on
/// top) cannot exhaust it; the guard page + CI budget gate are the
/// backstops if that assumption is ever violated.
pub const EXC_DSTACK_REGION_STRIDE: u64 = 0x0002_0000;

/// Guard page size for the exception data stack (one unmapped 4 KB page at
/// the slot base; the stack grows down into it on overflow).
pub const EXC_DSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's exception data stack (124 KB).
pub const EXC_DSTACK_USABLE_SIZE: u64 = EXC_DSTACK_REGION_STRIDE - EXC_DSTACK_GUARD_SIZE;

/// Usable pages of one CPU's exception data stack.
pub const EXC_DSTACK_PAGES: u64 = EXC_DSTACK_USABLE_SIZE / PAGE_SIZE_4KB;

// Bind the SafeStack resolver's IST-region bounds — which live in
// `slopos-ostd` (below `mm` in the crate graph, so it cannot import this
// module) and are consumed as naked-asm `const` operands by
// `__safestack_pointer_address` — to this canonical layout.  A drift here
// would silently route an exception handler's address-taken locals onto the
// wrong data stack, so we fail the build instead.
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

// =============================================================================
// Reliable Abort Core — per-CPU emergency fault stacks
// =============================================================================
//
// The fatal-fault / panic path switches BOTH the SAFE stack (RSP) and the
// SafeStack DATA stack to dedicated per-CPU emergency stacks before any
// `core::fmt` runs, so panic formatting cannot overflow a near-full task stack
// into its guard page (the recursive-#PF that hides the original fault). These
// are SEPARATE from the IST/EXC_DSTACK stacks so a fault nested on top of the
// panic (NMI only — IRQs are off) cannot collide with the report in progress.
//
// Layout (above the EXC_DSTACK region, still in the kernel higher half):
//   EXC_DSTACK_REGION_BASE          = 0xFFFF_FFFF_F000_0000
//   ... MAX_CPUS slots, 128 KB stride ...
//   EMERGENCY_DSTACK_REGION_BASE    = 0xFFFF_FFFF_F200_0000  (data stacks)
//   ... MAX_CPUS slots, 128 KB stride ...
//   EMERGENCY_SAFE_STACK_REGION_BASE= 0xFFFF_FFFF_F400_0000  (safe/RSP stacks)
//   ... MAX_CPUS slots, 64 KB stride ...

/// Base of the per-CPU emergency DATA-stack region (right above EXC_DSTACK).
pub const EMERGENCY_DSTACK_REGION_BASE: u64 =
    EXC_DSTACK_REGION_BASE + (slopos_arch::pcr::MAX_CPUS as u64) * EXC_DSTACK_REGION_STRIDE;

/// Stride per CPU slot (128 KB: 4 KB guard + 124 KB usable), matching EXC_DSTACK.
pub const EMERGENCY_DSTACK_REGION_STRIDE: u64 = 0x0002_0000;

/// Guard page size for the emergency data stack (one unmapped 4 KB page).
pub const EMERGENCY_DSTACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's emergency data stack (124 KB).
pub const EMERGENCY_DSTACK_USABLE_SIZE: u64 =
    EMERGENCY_DSTACK_REGION_STRIDE - EMERGENCY_DSTACK_GUARD_SIZE;

/// Usable pages of one CPU's emergency data stack.
pub const EMERGENCY_DSTACK_PAGES: u64 = EMERGENCY_DSTACK_USABLE_SIZE / PAGE_SIZE_4KB;

/// Base of the per-CPU emergency SAFE-stack region (right above emergency data).
pub const EMERGENCY_SAFE_STACK_REGION_BASE: u64 = EMERGENCY_DSTACK_REGION_BASE
    + (slopos_arch::pcr::MAX_CPUS as u64) * EMERGENCY_DSTACK_REGION_STRIDE;

/// Stride per CPU slot (64 KB: 4 KB guard + 60 KB usable).
pub const EMERGENCY_SAFE_STACK_REGION_STRIDE: u64 = 0x0001_0000;

/// Guard page size for the emergency safe stack (one unmapped 4 KB page).
pub const EMERGENCY_SAFE_STACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Usable bytes of one CPU's emergency safe stack (60 KB).
pub const EMERGENCY_SAFE_STACK_USABLE_SIZE: u64 =
    EMERGENCY_SAFE_STACK_REGION_STRIDE - EMERGENCY_SAFE_STACK_GUARD_SIZE;

/// Usable pages of one CPU's emergency safe stack.
pub const EMERGENCY_SAFE_STACK_PAGES: u64 = EMERGENCY_SAFE_STACK_USABLE_SIZE / PAGE_SIZE_4KB;

/// Exclusive top of the whole emergency-stack reservation; must stay within the
/// kernel higher half. A compile-time guard against the regions running off the
/// end of the canonical address space as MAX_CPUS grows.
const _: () = {
    let top = EMERGENCY_SAFE_STACK_REGION_BASE
        + (slopos_arch::pcr::MAX_CPUS as u64) * EMERGENCY_SAFE_STACK_REGION_STRIDE;
    assert!(
        top > EMERGENCY_DSTACK_REGION_BASE && top <= 0xFFFF_FFFF_FF00_0000,
        "emergency stack regions overflow the kernel higher half",
    );
};

// =============================================================================
// Default Process Memory Layout
// =============================================================================

/// Default (non-randomized) process memory layout.
///
/// Used as the base layout for ASLR randomization and heap limit checks.
/// All fields are compile-time constants; ASLR produces a modified copy at
/// process creation time.
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

// =============================================================================
// Process Limits
// =============================================================================

/// Maximum number of processes.
///
/// Re-exported from `abi` rather than declared here: the process registry,
/// this crate's address-space table and the descriptor tables key on each
/// other's slot indices, so the bound is one number in the one crate all
/// three can see.
pub use slopos_abi::task::MAX_PROCESSES;

/// Highest process id the allocator ever hands out.
///
/// Ids start at 1, so the id space is `1..=MAX_PROCESS_ID`. It is as wide
/// as the slot space because nothing indexes an array by process id: the
/// per-process shootdown table is keyed by address-space slot, and every
/// other per-process table resolves its slot by lookup.
pub const MAX_PROCESS_ID: u32 = MAX_PROCESSES as u32;

// The free-id ring is `MAX_PROCESSES` entries of `u16`, and every issued
// id can be free at once.
const _: () = assert!(MAX_PROCESS_ID as usize <= MAX_PROCESSES);
const _: () = assert!(MAX_PROCESS_ID <= u16::MAX as u32);
const _: () = assert!(MAX_PROCESS_ID > 0);

// Note: INVALID_PROCESS_ID is defined in abi/src/task.rs as the canonical location.
// Use `slopos_abi::task::INVALID_PROCESS_ID` directly.
