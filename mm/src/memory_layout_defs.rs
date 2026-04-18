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

/// Kernel heap virtual base address.
pub const KERNEL_HEAP_VBASE: u64 = 0xFFFF_FFFF_9000_0000;

/// Kernel heap size (256 MB).
pub const KERNEL_HEAP_SIZE: u64 = 256 * 1024 * 1024;

/// Kernel heap end virtual address (derived from base + size).
pub const KERNEL_HEAP_VEND: u64 = KERNEL_HEAP_VBASE + KERNEL_HEAP_SIZE;

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

/// Stride between exception stacks (64 KB).
pub const EXCEPTION_STACK_REGION_STRIDE: u64 = 0x0001_0000;

/// Guard page size for exception stacks (one 4 KB page).
pub const EXCEPTION_STACK_GUARD_SIZE: u64 = PAGE_SIZE_4KB;

/// Number of pages per exception stack (8 pages = 32 KB).
pub const EXCEPTION_STACK_PAGES: u64 = 8;

/// Exception stack usable size (32 KB).
pub const EXCEPTION_STACK_SIZE: u64 = EXCEPTION_STACK_PAGES * PAGE_SIZE_4KB;

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
pub const MAX_PROCESSES: usize = 256;

// Note: INVALID_PROCESS_ID is defined in abi/src/task.rs as the canonical location.
// Use `slopos_abi::task::INVALID_PROCESS_ID` directly.
