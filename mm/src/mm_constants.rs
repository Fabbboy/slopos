//! SlopOS Memory and Paging Constants.
//!
//! This module re-exports memory and paging constants from `slopos_abi::arch::x86_64`.
//! All code should use the type-safe `PageFlags` bitflags for page table operations.

// Re-export memory layout constants from abi
pub use slopos_abi::arch::x86_64::memory::{
    BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE, EARLY_PDPT_PHYS_ADDR, EARLY_PD_PHYS_ADDR,
    EARLY_PML4_PHYS_ADDR, EXCEPTION_STACK_GUARD_SIZE, EXCEPTION_STACK_PAGES,
    EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE, EXCEPTION_STACK_SIZE,
    EXCEPTION_STACK_TOTAL_SIZE, HHDM_VIRT_BASE, KERNEL_HEAP_SIZE, KERNEL_HEAP_VBASE,
    KERNEL_PDPT_INDEX, KERNEL_PML4_INDEX, KERNEL_VIRTUAL_BASE, MAX_MEMORY_REGIONS, MAX_PROCESSES,
    MMIO_VIRT_BASE, MMIO_VIRT_SIZE, PROCESS_CODE_START_VA, PROCESS_DATA_START_VA,
    PROCESS_HEAP_MAX_VA, PROCESS_HEAP_START_VA, PROCESS_STACK_SIZE_BYTES, PROCESS_STACK_TOP_VA,
    USER_SPACE_END_VA, USER_SPACE_START_VA,
};

// INVALID_PROCESS_ID is canonical in task module
pub use slopos_abi::task::INVALID_PROCESS_ID;

// Re-export paging constants from abi
pub use slopos_abi::arch::x86_64::paging::{
    EFI_CONVENTIONAL_MEMORY, EFI_PAGE_SIZE, ENTRIES_PER_PAGE_TABLE, PAGE_ALIGN, PAGE_SIZE_1GB,
    PAGE_SIZE_2MB, PAGE_SIZE_4KB, PAGE_SIZE_4KB_USIZE, STACK_ALIGN,
};

// Re-export PageFlags for type-safe flag manipulation
pub use slopos_abi::arch::x86_64::paging::PageFlags;
