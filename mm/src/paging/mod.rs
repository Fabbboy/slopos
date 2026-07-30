//! Legacy kernel-side paging surface.
//!
//! Per-process paging is now handled exclusively through the OSTD
//! `VmSpace` cursor. The functions still exposed here are the
//! kernel-half mapping helpers used by early-boot callers
//! (memory_init, kernel_heap, mmio, stack_va, IST stacks) that run
//! BEFORE `KERNEL_VM_SPACE` is installed at boot priority 55.
//!
//! Post-priority-55 callers should prefer `slopos_mm::kernel_mappings::*`
//! which routes through the OSTD cursor.

pub mod page_table_defs;
pub(crate) mod tables;
pub mod walker;

pub use crate::error::{MmError, MmResult};
pub use page_table_defs::{PAGE_TABLE_ENTRIES, PageTableEntry, PageTableLevel};
pub use walker::{WalkResult, walk_phys};

pub use tables::{
    ProcessPageDir, get_page_size, init_paging, is_mapped, map_page_4kb,
    paging_bump_kernel_mapping_gen, paging_get_kernel_directory, paging_mark_kernel_global,
    unmap_page, virt_to_phys,
};
