#![no_std]
#![feature(sync_unsafe_cell)]
#![forbid(unsafe_code)]

pub mod aslr;
pub mod cow;
pub mod demand;
pub mod dual_paging;
pub mod elf;
pub mod error;
pub mod hhdm;
pub mod io_mem_mapper_shim;
pub mod kernel_mappings;
pub mod kernel_meta;
pub mod memfd;
pub mod memory_init;
pub mod memory_layout;
pub mod memory_layout_defs;
pub mod memory_reservations;
pub mod mmio;
pub mod mmu;
pub mod page_alloc;
pub mod page_fault;
pub mod paging;
pub mod paging_defs;
pub mod pat;
pub mod process_vm;
pub mod slab;
pub mod stack_region;
pub mod stack_va;
pub mod symbols;
#[cfg(feature = "test-hooks")]
pub mod tests;
pub mod tlb;
pub mod user_copy;
pub mod user_io_buf;
pub mod user_ptr;
pub mod vma_region;

use slopos_ostd::mm::heap::register_kernel_slab_handle;
use slopos_ostd::sync::BspToken;

/// Promote the global allocator from OSTD's bootstrap bump pool to
/// the `mm` crate's BSS-resident `slab::KERNEL_SLAB`. Must run after
/// the buddy allocator + HHDM are live (Memory phase priority ≥ 10)
/// and after [`slab::init_kernel_slab`] has transitioned the slab
/// state machine from `Uninit` to `Live`.
///
/// The `&BspToken<'brand>` witness binds the call to the BSP-init
/// scope opened by `slopos_ostd::sync::run_bsp_init`; it is forwarded
/// to OSTD's [`register_kernel_slab_handle`], which is one-shot.
pub fn global_allocator_use_kernel_slab<'brand>(token: &BspToken<'brand>) {
    register_kernel_slab_handle(token, crate::slab::slab_handle());
}
