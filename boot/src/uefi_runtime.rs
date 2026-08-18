//! UEFI Runtime Services region mapping.
//!
//! Each region is mapped at both its identity and its HHDM address: firmware's
//! own pointers may be physical while Limine reports the system table at its
//! HHDM alias.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_mm::kernel_mappings::{kernel_is_mapped, kernel_map_io_4kb};
use slopos_mm::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use slopos_ostd::klog_info;
use slopos_ostd::util::packed_view::read_packed;

use crate::limine_protocol;

// EFI_MEMORY_DESCRIPTOR field offsets (UEFI 2.x).
const DESC_TYPE: usize = 0;
const DESC_PHYSICAL_START: usize = 8;
const DESC_NUMBER_OF_PAGES: usize = 24;
const DESC_ATTRIBUTE: usize = 32;

/// The region must remain mapped for runtime calls.
const EFI_MEMORY_RUNTIME: u64 = 0x8000_0000_0000_0000;

const EFI_MEMORY_MAPPED_IO: u32 = 11;
// `PhysicalStart` is an I/O port range, not memory, so it must not be page-mapped.
const EFI_MEMORY_MAPPED_IO_PORT_SPACE: u32 = 12;

/// Bounds a corrupt descriptor without aborting the whole map; real runtime
/// regions are a few MiB at most.
const MAX_PAGES_PER_DESC: u64 = 0x10000; // 256 MiB

/// Install one alias of a runtime-services page, unless the address already
/// resolves. Firmware memory is not allocator memory, so the mapping owns no
/// frame reference.
fn map_alias_if_absent(virt: VirtAddr, phys: PhysAddr, flags: u64) {
    if kernel_is_mapped(virt) {
        return;
    }
    let _ = kernel_map_io_4kb(virt, phys, flags);
}

/// Map every UEFI runtime region (identity + HHDM aliases). Runs after paging
/// and the HHDM are up, while the EFI memory map is still mapped.
pub fn map_runtime_regions(hhdm_offset: u64) {
    let Some((memmap, desc_size)) = limine_protocol::efi_memmap() else {
        klog_info!("UEFI: no EFI memory map (BIOS boot?); runtime services unavailable");
        return;
    };
    if desc_size < DESC_ATTRIBUTE + 8 {
        return;
    }

    let mut mapped_pages = 0u64;
    let mut truncated = false;
    let mut off = 0usize;
    while off + desc_size <= memmap.len() {
        let desc = &memmap[off..off + desc_size];
        off += desc_size;

        let (Some(attr), Some(typ), Some(phys_start), Some(num_pages)) = (
            read_packed::<u64>(desc, DESC_ATTRIBUTE),
            read_packed::<u32>(desc, DESC_TYPE),
            read_packed::<u64>(desc, DESC_PHYSICAL_START),
            read_packed::<u64>(desc, DESC_NUMBER_OF_PAGES),
        ) else {
            continue;
        };
        if attr & EFI_MEMORY_RUNTIME == 0 {
            continue;
        }
        if typ == EFI_MEMORY_MAPPED_IO_PORT_SPACE {
            continue;
        }

        // Runtime code must execute, so code/data gets KERNEL_RW (NX clear).
        let flags = if typ == EFI_MEMORY_MAPPED_IO {
            PageFlags::MMIO.bits()
        } else {
            PageFlags::KERNEL_RW.bits()
        };

        let pages = if num_pages > MAX_PAGES_PER_DESC {
            truncated = true;
            MAX_PAGES_PER_DESC
        } else {
            num_pages
        };

        for i in 0..pages {
            let Some(phys) = i
                .checked_mul(PAGE_SIZE_4KB)
                .and_then(|d| phys_start.checked_add(d))
            else {
                break;
            };
            map_alias_if_absent(VirtAddr::new(phys), PhysAddr::new(phys), flags);
            if let Some(hhdm_virt) = phys.checked_add(hhdm_offset) {
                map_alias_if_absent(VirtAddr::new(hhdm_virt), PhysAddr::new(phys), flags);
            }
            mapped_pages += 1;
        }
    }

    if truncated {
        klog_info!(
            "UEFI: mapped {} runtime-services pages (identity + HHDM); some regions capped at {} pages",
            mapped_pages,
            MAX_PAGES_PER_DESC
        );
    } else {
        klog_info!(
            "UEFI: mapped {} runtime-services pages (identity + HHDM)",
            mapped_pages
        );
    }
}
