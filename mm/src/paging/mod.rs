pub mod page_table_defs;
mod tables;
pub mod walker;

pub use crate::error::{MmError, MmResult};
pub use page_table_defs::{PAGE_TABLE_ENTRIES, PageTable, PageTableEntry, PageTableLevel};
pub use walker::{HhdmMapping, PageTableFrameMapping, PageTableWalker, WalkAction, WalkResult};

pub use tables::{
    EARLY_PD, EARLY_PDPT, EARLY_PML4, ProcessPageDir, get_memory_layout_info, get_page_size,
    init_paging, is_mapped, map_page_2mb, map_page_4kb, map_page_4kb_in_dir,
    paging_bump_kernel_mapping_gen, paging_copy_kernel_mappings, paging_free_user_space,
    paging_get_kernel_directory, paging_get_pte_flags, paging_is_cow, paging_is_user_accessible,
    paging_map_shared_kernel_page, paging_mark_cow, paging_mark_range_user, paging_resolve_cow,
    paging_sync_kernel_mappings, paging_update_range_protection, switch_page_directory, unmap_page,
    unmap_page_in_dir, virt_to_phys, virt_to_phys_in_dir, virt_to_phys_process,
};
