//! Kernel-half address translation: a lock-free, allocation-free page-table
//! walker plus the translation queries built on it.
//!
//! Every write goes through `slopos_mm::kernel_mappings` under the
//! `KERNEL_VM_SPACE` lock — the single writer of the kernel half, in every
//! address space, after boot priority memory/7.

pub mod page_table_defs;
pub(crate) mod tables;
pub mod walker;

pub use crate::error::{MmError, MmResult};
pub use page_table_defs::{PAGE_TABLE_ENTRIES, PageTableEntry, PageTableLevel};
pub use walker::{WalkResult, walk_phys};

pub use tables::{
    get_page_size, init_paging, is_mapped, kernel_pml4_phys, paging_mark_kernel_global,
    virt_to_phys,
};
