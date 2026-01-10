//! SlopOS Memory and Paging Constants.
//!
//! This module re-exports memory and paging constants from `slopos_abi::arch::x86_64`
//! for backward compatibility. New code should import directly from abi.

// Re-export memory layout constants from abi
pub use slopos_abi::arch::x86_64::memory::{
    BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE, EARLY_PD_PHYS_ADDR, EARLY_PDPT_PHYS_ADDR,
    EARLY_PML4_PHYS_ADDR, EXCEPTION_STACK_GUARD_SIZE, EXCEPTION_STACK_PAGES,
    EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE, EXCEPTION_STACK_SIZE,
    EXCEPTION_STACK_TOTAL_SIZE, HHDM_VIRT_BASE, KERNEL_HEAP_SIZE, KERNEL_HEAP_VBASE,
    KERNEL_PDPT_INDEX, KERNEL_PML4_INDEX, KERNEL_VIRTUAL_BASE, MAX_MEMORY_REGIONS, MAX_PROCESSES,
    PROCESS_CODE_START_VA, PROCESS_DATA_START_VA, PROCESS_HEAP_MAX_VA, PROCESS_HEAP_START_VA,
    PROCESS_STACK_SIZE_BYTES, PROCESS_STACK_TOP_VA, USER_SPACE_END_VA, USER_SPACE_START_VA,
};

// INVALID_PROCESS_ID is canonical in task module
pub use slopos_abi::task::INVALID_PROCESS_ID;

// Re-export paging constants from abi
pub use slopos_abi::arch::x86_64::paging::{
    EFI_CONVENTIONAL_MEMORY, EFI_PAGE_SIZE, ENTRIES_PER_PAGE_TABLE, PAGE_ALIGN, PAGE_SIZE_1GB,
    PAGE_SIZE_2MB, PAGE_SIZE_4KB, STACK_ALIGN,
};

// Re-export PageFlags for type-safe flag manipulation
pub use slopos_abi::arch::x86_64::paging::PageFlags;

// Backward compatibility: raw page flag constants
// These match the bitflags values but as raw u64 for existing code
pub const PAGE_PRESENT: u64 = 0x001;
pub const PAGE_WRITABLE: u64 = 0x002;
pub const PAGE_USER: u64 = 0x004;
pub const PAGE_WRITE_THROUGH: u64 = 0x008;
pub const PAGE_CACHE_DISABLE: u64 = 0x010;
pub const PAGE_ACCESSED: u64 = 0x020;
pub const PAGE_DIRTY: u64 = 0x040;
pub const PAGE_SIZE_FLAG: u64 = 0x080;
/// Preserve legacy name from C header; used as the large-page flag.
pub const PAGE_SIZE_FLAG_COMPAT: u64 = PAGE_SIZE_FLAG;
pub const PAGE_GLOBAL: u64 = 0x100;

// Composite flags (backward compatibility)
pub const PAGE_KERNEL_RW: u64 = PAGE_PRESENT | PAGE_WRITABLE;
pub const PAGE_KERNEL_RO: u64 = PAGE_PRESENT;
pub const PAGE_USER_RW: u64 = PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;
pub const PAGE_USER_RO: u64 = PAGE_PRESENT | PAGE_USER;
pub const PAGE_LARGE_KERNEL_RW: u64 = PAGE_PRESENT | PAGE_WRITABLE | PAGE_SIZE_FLAG;
