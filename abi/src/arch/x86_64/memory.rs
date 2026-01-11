//! Memory layout constants for x86_64.
//!
//! This module defines the virtual and physical address space layout used
//! by SlopOS, including kernel space, user space, and special regions.

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

/// PML4 index for kernel space (entry 511).
pub const KERNEL_PML4_INDEX: usize = 511;

/// PDPT index for kernel space (entry 510).
pub const KERNEL_PDPT_INDEX: usize = 510;

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

// =============================================================================
// User Virtual Address Space
// =============================================================================

/// User space start virtual address.
pub const USER_SPACE_START_VA: u64 = 0x0000_0000_0000_0000;

/// User space end virtual address (up to canonical hole).
pub const USER_SPACE_END_VA: u64 = 0x0000_8000_0000_0000;

/// Process code segment start virtual address.
pub const PROCESS_CODE_START_VA: u64 = 0x0000_0000_0040_0000;

/// Process data segment start virtual address.
pub const PROCESS_DATA_START_VA: u64 = 0x0000_0000_0080_0000;

/// Process heap start virtual address.
pub const PROCESS_HEAP_START_VA: u64 = 0x0000_0000_0100_0000;

/// Process heap maximum virtual address.
pub const PROCESS_HEAP_MAX_VA: u64 = 0x0000_0000_4000_0000;

/// Process stack top virtual address.
pub const PROCESS_STACK_TOP_VA: u64 = 0x0000_7FFF_FF00_0000;

/// Process stack size in bytes (1 MB).
pub const PROCESS_STACK_SIZE_BYTES: u64 = 0x0000_0000_0010_0000;

// =============================================================================
// Exception Stack Region
// =============================================================================

/// Exception stack region base virtual address.
pub const EXCEPTION_STACK_REGION_BASE: u64 = 0xFFFF_FFFF_B000_0000;

/// Stride between exception stacks (64 KB).
pub const EXCEPTION_STACK_REGION_STRIDE: u64 = 0x0001_0000;

/// Guard page size for exception stacks (4 KB).
pub const EXCEPTION_STACK_GUARD_SIZE: u64 = 0x1000;

/// Number of pages per exception stack (8 pages = 32 KB).
pub const EXCEPTION_STACK_PAGES: u64 = 8;

/// Exception stack usable size (32 KB).
pub const EXCEPTION_STACK_SIZE: u64 = EXCEPTION_STACK_PAGES * 0x1000;

/// Total exception stack size including guard page (36 KB).
pub const EXCEPTION_STACK_TOTAL_SIZE: u64 = EXCEPTION_STACK_GUARD_SIZE + EXCEPTION_STACK_SIZE;

// =============================================================================
// Process Limits
// =============================================================================

/// Maximum number of memory regions.
pub const MAX_MEMORY_REGIONS: usize = 64;

/// Maximum number of processes.
pub const MAX_PROCESSES: usize = 256;

// Note: INVALID_PROCESS_ID is defined in abi/src/task.rs as the canonical location.
// Use `slopos_abi::task::INVALID_PROCESS_ID` or import via mm_constants.
