pub mod error;
mod tables;
pub mod walker;

pub use error::{PagingError, PagingResult};
pub use walker::{HhdmMapping, PageTableFrameMapping, PageTableWalker, WalkAction, WalkResult};

pub use slopos_abi::arch::x86_64::page_table::PageTable;

pub use tables::{
    EARLY_PD, EARLY_PDPT, EARLY_PML4, ProcessPageDir, get_current_page_directory,
    get_memory_layout_info, get_page_size, init_paging, is_mapped, map_page_2mb, map_page_4kb,
    map_page_4kb_in_dir, paging_copy_kernel_mappings, paging_free_user_space,
    paging_get_kernel_directory, paging_is_user_accessible, paging_map_shared_kernel_page,
    paging_mark_range_user, paging_set_current_directory, switch_page_directory, unmap_page,
    unmap_page_in_dir, virt_to_phys, virt_to_phys_in_dir, virt_to_phys_process,
};
