//! UEFI Runtime Services region mapping.
//!
//! Firmware Runtime Services (`ResetSystem`, see [`slopos_ostd::uefi`]) stay
//! callable after `ExitBootServices` only while their memory is mapped.
//! Since the kernel uses its own page tables, this maps every UEFI runtime
//! region into the kernel page table early in boot.
//!
//! Each region is mapped at BOTH its identity (`virt == phys`) and HHDM
//! (`virt == phys + offset`) address: the firmware's internal pointers may
//! be physical (identity alias) while Limine reports the system table at
//! its HHDM alias, so mapping both keeps `ResetSystem` reachable either way.

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

/// `EFI_MEMORY_RUNTIME` — the region must remain mapped for runtime calls.
const EFI_MEMORY_RUNTIME: u64 = 0x8000_0000_0000_0000;

// EFI memory-mapped device I/O (mapped uncached, non-executable).
const EFI_MEMORY_MAPPED_IO: u32 = 11;
// EFI I/O *port* space: accessed via `in`/`out`, not memory — its
// `PhysicalStart` is a port range, so it must not be page-mapped.
const EFI_MEMORY_MAPPED_IO_PORT_SPACE: u32 = 12;

/// Per-descriptor page cap. Runtime code/data regions are a few MiB at
/// most; this bounds a pathological / corrupt descriptor without aborting
/// the whole map. Truncation is logged rather than silently swallowed.
const MAX_PAGES_PER_DESC: u64 = 0x10000; // 256 MiB

/// Install one alias of a runtime-services page, unless the address
/// already resolves.
///
/// Firmware memory is not allocator memory — the buddy has never owned
/// these pages and must never be handed them — so the mapping owns no
/// frame reference. A VA that already translates is left alone: the
/// cursor refuses to overwrite a present leaf, and both aliases of a
/// region the HHDM already covers are exactly that case.
fn map_alias_if_absent(virt: VirtAddr, phys: PhysAddr, flags: u64) {
    if kernel_is_mapped(virt) {
        return;
    }
    let _ = kernel_map_io_4kb(virt, phys, flags);
}

/// Map every UEFI runtime region into the kernel page table (identity +
/// HHDM aliases). Runs after paging/HHDM are up and while the EFI memory
/// map is still mapped. No-op on a non-UEFI boot.
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
        // I/O port space is reached via `in`/`out`, not page mappings.
        if typ == EFI_MEMORY_MAPPED_IO_PORT_SPACE {
            continue;
        }

        // Device MMIO must be uncached + NX; code/data is mapped RWX
        // (KERNEL_RW leaves the NX bit clear) so runtime code can execute.
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
