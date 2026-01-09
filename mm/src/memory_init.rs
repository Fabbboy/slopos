use crate::kernel_heap::init_kernel_heap;
use crate::memory_layout::{get_kernel_memory_layout, init_kernel_memory_layout};
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT, MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
    MM_RESERVATION_FLAG_MMIO, MmRegion, MmRegionKind, MmReservationType, mm_region_add_usable,
    mm_region_count, mm_region_get, mm_region_highest_usable_frame, mm_region_map_configure,
    mm_region_map_reset, mm_region_reserve, mm_region_total_bytes, mm_reservation_type_name,
    mm_reservations_capacity, mm_reservations_count, mm_reservations_get,
    mm_reservations_overflow_count, mm_reservations_total_bytes,
};
use crate::mm_constants::{
    BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE, EARLY_PD_PHYS_ADDR, EARLY_PDPT_PHYS_ADDR,
    EARLY_PML4_PHYS_ADDR, HHDM_VIRT_BASE, KERNEL_VIRTUAL_BASE, PAGE_SIZE_4KB,
};
use crate::page_alloc::{
    finalize_page_allocator, init_page_allocator, page_allocator_descriptor_size,
};
use crate::paging::init_paging;
use crate::process_vm::init_process_vm;
use core::ffi::{CStr, c_char, c_int};

use slopos_lib::{FramebufferInfo, align_down_u64, align_up_u64, cpu, klog_debug, klog_info};

const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;
const MSR_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ADDR_MASK: u64 = 0xFFFFF000;

// Limine structures (subset needed for this module)
#[repr(C)]
pub struct LimineMemmapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *mut *mut LimineMemmapEntry,
}

#[repr(C)]
pub struct LimineMemmapEntry {
    pub base: u64,
    pub length: u64,
    pub type_: u64,
    pub unused: u64,
}

const LIMINE_MEMMAP_USABLE: u64 = 0;
const LIMINE_MEMMAP_ACPI_RECLAIMABLE: u64 = 2;
const LIMINE_MEMMAP_ACPI_NVS: u64 = 3;
const LIMINE_MEMMAP_FRAMEBUFFER: u64 = 7;

const BOOT_REGION_STATIC_CAP: usize = 4096;
const DESC_ALIGN_BYTES: u64 = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MemoryInitStats {
    total_memory_bytes: u64,
    available_memory_bytes: u64,
    reserved_device_bytes: u64,
    memory_regions_count: u32,
    reserved_region_count: u32,
    hhdm_offset: u64,
    tracked_page_frames: u32,
    allocator_metadata_bytes: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct AllocatorPlan {
    buffer: *mut u8,
    phys_base: u64,
    bytes: u64,
    capacity_frames: u32,
}

static mut REGION_BOOT_BUFFER: [MmRegion; BOOT_REGION_STATIC_CAP] =
    [MmRegion::zeroed(); BOOT_REGION_STATIC_CAP];
static mut INIT_STATS: MemoryInitStats = MemoryInitStats {
    total_memory_bytes: 0,
    available_memory_bytes: 0,
    reserved_device_bytes: 0,
    memory_regions_count: 0,
    reserved_region_count: 0,
    hhdm_offset: 0,
    tracked_page_frames: 0,
    allocator_metadata_bytes: 0,
};
static mut EARLY_PAGING_OK: bool = false;
static mut MEMORY_SYSTEM_INITIALIZED: bool = false;
static mut FRAMEBUFFER_BOOT_INFO: Option<FramebufferInfo> = None;

fn mm_panic(msg: &str) -> ! {
    panic!("{msg}");
}

fn framebuffer_boot_info() -> Option<FramebufferInfo> {
    unsafe { FRAMEBUFFER_BOOT_INFO }
}

fn configure_region_store(memmap: *const LimineMemmapResponse) {
    let mut needed: u32 = 64;
    unsafe {
        if !memmap.is_null() {
            let response = &*memmap;
            if response.entry_count < u32::MAX as u64 {
                let mut estimate = 4u64.saturating_mul(response.entry_count) + 64;
                if estimate > u32::MAX as u64 {
                    estimate = u32::MAX as u64;
                }
                needed = estimate as u32;
            }
        }

        if needed as usize > BOOT_REGION_STATIC_CAP {
            klog_info!(
                "MM: region map estimate {} exceeds capacity {}, clamping",
                needed,
                BOOT_REGION_STATIC_CAP
            );
            needed = BOOT_REGION_STATIC_CAP as u32;
        }

        let capacity = if (needed as usize) < BOOT_REGION_STATIC_CAP {
            needed
        } else {
            BOOT_REGION_STATIC_CAP as u32
        };

        mm_region_map_configure(REGION_BOOT_BUFFER.as_mut_ptr(), capacity);
        mm_region_map_reset();
    }
}

fn add_reservation_or_panic(
    base: u64,
    length: u64,
    type_: MmReservationType,
    flags: u32,
    label: *const c_char,
) {
    if mm_region_reserve(base, length, type_, flags, label) != 0 {
        mm_panic("MM: Failed to record reserved region");
    }
}

fn add_usable_or_panic(base: u64, length: u64, label: *const c_char) {
    if mm_region_add_usable(base, length, label) != 0 {
        mm_panic("MM: Failed to record usable region");
    }
}

fn virt_to_phys_kernel(virt: u64) -> u64 {
    if virt >= KERNEL_VIRTUAL_BASE {
        return virt - KERNEL_VIRTUAL_BASE;
    }
    if crate::hhdm::is_available() {
        let hhdm_base = crate::hhdm::offset();
        if virt >= hhdm_base {
            return virt - hhdm_base;
        }
    }
    virt
}

fn record_memmap_usable(memmap: *const LimineMemmapResponse) {
    if memmap.is_null() {
        mm_panic("MM: Missing Limine memmap for usable regions");
    }
    unsafe {
        let response = &*memmap;
        if response.entry_count == 0 || response.entries.is_null() {
            mm_panic("MM: Missing Limine memmap for usable regions");
        }
        INIT_STATS.total_memory_bytes = 0;
        for i in 0..response.entry_count {
            let entry_ptr = *response.entries.add(i as usize);
            if entry_ptr.is_null() {
                continue;
            }
            let entry = &*entry_ptr;
            if entry.length == 0 {
                continue;
            }
            INIT_STATS.total_memory_bytes =
                INIT_STATS.total_memory_bytes.saturating_add(entry.length);
            if entry.type_ != LIMINE_MEMMAP_USABLE {
                continue;
            }
            let base = align_up_u64(entry.base, PAGE_SIZE_4KB);
            let end = align_down_u64(entry.base + entry.length, PAGE_SIZE_4KB);
            if end <= base {
                continue;
            }
            add_usable_or_panic(base, end - base, b"usable\0".as_ptr() as *const c_char);
        }
    }
}

fn compute_memory_stats(memmap: *const LimineMemmapResponse, hhdm_offset: u64) {
    let _ = memmap;
    unsafe {
        INIT_STATS.hhdm_offset = hhdm_offset;
        INIT_STATS.memory_regions_count = mm_region_count();
        INIT_STATS.available_memory_bytes = mm_region_total_bytes(MmRegionKind::Usable);
        if INIT_STATS.available_memory_bytes == 0 {
            INIT_STATS.tracked_page_frames = 0;
        } else {
            let highest_frame = mm_region_highest_usable_frame();
            INIT_STATS.tracked_page_frames = if highest_frame >= u32::MAX as u64 {
                0
            } else {
                (highest_frame + 1) as u32
            };
        }
        if INIT_STATS.tracked_page_frames == 0 && INIT_STATS.available_memory_bytes > 0 {
            mm_panic("MM: Usable memory exceeds supported frame range");
        }
        INIT_STATS.reserved_region_count = mm_reservations_count();
        INIT_STATS.reserved_device_bytes =
            mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);
    }
}

fn record_kernel_core_reservations() {
    let layout_ptr = get_kernel_memory_layout();
    if layout_ptr.is_null() {
        klog_info!("MM: kernel layout unavailable; cannot reserve kernel image");
        return;
    }
    let layout = unsafe { &*layout_ptr };

    let kernel_phys = virt_to_phys_kernel(layout.kernel_start_phys);
    let kernel_end_phys = virt_to_phys_kernel(layout.kernel_end_phys);
    let kernel_size = if kernel_end_phys > kernel_phys {
        kernel_end_phys - kernel_phys
    } else {
        0
    };

    if kernel_size > 0 {
        add_reservation_or_panic(
            kernel_phys,
            kernel_size,
            MmReservationType::FirmwareOther,
            MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS | MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
            b"Kernel image\0".as_ptr() as *const c_char,
        );
    }

    add_reservation_or_panic(
        BOOT_STACK_PHYS_ADDR,
        BOOT_STACK_SIZE,
        MmReservationType::FirmwareOther,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
        b"Boot stack\0".as_ptr() as *const c_char,
    );

    add_reservation_or_panic(
        EARLY_PML4_PHYS_ADDR,
        PAGE_SIZE_4KB,
        MmReservationType::FirmwareOther,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
        b"Early PML4\0".as_ptr() as *const c_char,
    );

    add_reservation_or_panic(
        EARLY_PDPT_PHYS_ADDR,
        PAGE_SIZE_4KB,
        MmReservationType::FirmwareOther,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
        b"Early PDPT\0".as_ptr() as *const c_char,
    );

    add_reservation_or_panic(
        EARLY_PD_PHYS_ADDR,
        PAGE_SIZE_4KB,
        MmReservationType::FirmwareOther,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
        b"Early PD\0".as_ptr() as *const c_char,
    );
}

fn record_memmap_reservations(memmap: *const LimineMemmapResponse) {
    if memmap.is_null() {
        return;
    }
    unsafe {
        let response = &*memmap;
        if response.entry_count == 0 || response.entries.is_null() {
            return;
        }
        for i in 0..response.entry_count {
            let entry_ptr = *response.entries.add(i as usize);
            if entry_ptr.is_null() {
                continue;
            }
            let entry = &*entry_ptr;
            if entry.length == 0 {
                continue;
            }
            match entry.type_ {
                LIMINE_MEMMAP_ACPI_RECLAIMABLE => add_reservation_or_panic(
                    entry.base,
                    entry.length,
                    MmReservationType::AcpiReclaimable,
                    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                    b"ACPI reclaimable\0".as_ptr() as *const c_char,
                ),
                LIMINE_MEMMAP_ACPI_NVS => add_reservation_or_panic(
                    entry.base,
                    entry.length,
                    MmReservationType::AcpiNvs,
                    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
                    b"ACPI NVS\0".as_ptr() as *const c_char,
                ),
                LIMINE_MEMMAP_FRAMEBUFFER => add_reservation_or_panic(
                    entry.base,
                    entry.length,
                    MmReservationType::Framebuffer,
                    MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS
                        | MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT
                        | MM_RESERVATION_FLAG_MMIO,
                    b"Framebuffer\0".as_ptr() as *const c_char,
                ),
                _ => {}
            }
        }
    }
}

fn record_framebuffer_reservation() {
    let Some(fb) = framebuffer_boot_info() else {
        return;
    };

    let mut phys_base = fb.address as u64;
    if crate::hhdm::is_available() {
        let offset = crate::hhdm::offset();
        if phys_base >= offset {
            phys_base -= offset;
        }
    }
    if phys_base == 0 || fb.pitch == 0 || fb.height == 0 {
        return;
    }
    let length = fb.pitch.saturating_mul(fb.height);
    if length == 0 {
        return;
    }
    add_reservation_or_panic(
        phys_base,
        length,
        MmReservationType::Framebuffer,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS
            | MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT
            | MM_RESERVATION_FLAG_MMIO,
        b"Framebuffer\0".as_ptr() as *const c_char,
    );
}

fn record_apic_reservation() {
    let (_a, _b, _c, d) = cpu::cpuid(1);
    if (d & CPUID_FEAT_EDX_APIC) == 0 {
        return;
    }
    let apic_base_msr = cpu::read_msr(MSR_APIC_BASE);
    let apic_phys = apic_base_msr & APIC_BASE_ADDR_MASK;
    if apic_phys == 0 {
        return;
    }
    add_reservation_or_panic(
        apic_phys,
        0x1000,
        MmReservationType::Apic,
        MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS | MM_RESERVATION_FLAG_MMIO,
        b"Local APIC\0".as_ptr() as *const c_char,
    );
}

fn select_allocator_window(reserved_bytes: u64) -> u64 {
    unsafe {
        for i in (0..mm_region_count()).rev() {
            let region = mm_region_get(i);
            if region.is_null() {
                continue;
            }
            let region_ref = &*region;
            if region_ref.kind != MmRegionKind::Usable || region_ref.length < reserved_bytes {
                continue;
            }
            let region_end = region_ref.phys_base + region_ref.length;
            let mut candidate = align_down_u64(region_end - reserved_bytes, PAGE_SIZE_4KB);
            if candidate < region_ref.phys_base {
                candidate = region_ref.phys_base;
            }
            return candidate;
        }
    }
    0
}

fn plan_allocator_metadata(
    _memmap: *const LimineMemmapResponse,
    hhdm_offset: u64,
) -> AllocatorPlan {
    unsafe {
        if INIT_STATS.tracked_page_frames == 0 {
            mm_panic("MM: No tracked frames available for allocator sizing");
        }
        let desc_bytes =
            (INIT_STATS.tracked_page_frames as u64) * page_allocator_descriptor_size() as u64;
        let mut aligned_bytes = align_up_u64(desc_bytes, DESC_ALIGN_BYTES);
        aligned_bytes = align_up_u64(aligned_bytes, PAGE_SIZE_4KB);
        INIT_STATS.allocator_metadata_bytes = desc_bytes;

        let phys_base = select_allocator_window(aligned_bytes);
        if phys_base == 0 {
            mm_panic("MM: Failed to find window for allocator metadata");
        }
        add_reservation_or_panic(
            phys_base,
            aligned_bytes,
            MmReservationType::AllocatorMetadata,
            MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS | MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT,
            b"Allocator metadata\0".as_ptr() as *const c_char,
        );
        AllocatorPlan {
            buffer: (phys_base + hhdm_offset) as *mut u8,
            phys_base,
            bytes: aligned_bytes,
            capacity_frames: INIT_STATS.tracked_page_frames,
        }
    }
}

fn finalize_reserved_regions() {
    unsafe {
        INIT_STATS.reserved_region_count = mm_reservations_count();
        INIT_STATS.reserved_device_bytes =
            mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);

        log_reserved_regions();

        if mm_reservations_overflow_count() > 0 {
            mm_panic("MM: Reserved region capacity exceeded");
        }
    }
}

fn log_reserved_regions() {
    unsafe {
        let count = mm_reservations_count();
        if count == 0 {
            klog_info!("MM: No device memory reservations detected");
            return;
        }
        let total_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);
        klog_info!("MM: Reserved device regions ({})", count);
        for i in 0..count {
            let region = mm_reservations_get(i);
            if region.is_null() {
                continue;
            }
            let region_ref = &*region;
            let label_ptr = if region_ref.label[0] != 0 {
                region_ref.label.as_ptr()
            } else {
                mm_reservation_type_name(region_ref.type_) as *const u8
            };
            let region_end = region_ref.phys_base + region_ref.length;
            let label_str = CStr::from_ptr(label_ptr as *const c_char)
                .to_str()
                .unwrap_or("<invalid utf-8>");
            klog_info!(
                "  {}: 0x{:x} - 0x{:x} ({} KB)",
                label_str,
                region_ref.phys_base,
                region_end - 1,
                region_ref.length / 1024
            );
        }
        if total_bytes > 0 {
            klog_info!("  Total reserved:      {} KB", total_bytes / 1024);
        }
        if mm_reservations_overflow_count() > 0 {
            klog_info!(
                "  Reservation drops:   {} (capacity {})",
                mm_reservations_overflow_count(),
                mm_reservations_capacity()
            );
        }
    }
}

fn display_memory_summary() {
    unsafe {
        klog_info!("\n========== SlopOS Memory System Initialized ==========");
        let early_paging_str = if EARLY_PAGING_OK { "OK" } else { "SKIPPED" };
        klog_info!("Early Paging:          {}", early_paging_str);
        klog_info!(
            "Reserved Regions:      {}",
            INIT_STATS.reserved_region_count
        );
        klog_info!("Tracked Frames:        {}", INIT_STATS.tracked_page_frames);
        klog_info!(
            "Allocator Metadata:    {} KB",
            INIT_STATS.allocator_metadata_bytes / 1024
        );
        klog_info!(
            "Reserved Device Mem:   {} KB",
            INIT_STATS.reserved_device_bytes / 1024
        );
        klog_info!(
            "Total Memory:          {} MB",
            INIT_STATS.total_memory_bytes / (1024 * 1024)
        );
        klog_info!(
            "Available Memory:      {} MB",
            INIT_STATS.available_memory_bytes / (1024 * 1024)
        );
        klog_info!("Memory Regions:        {}", INIT_STATS.memory_regions_count);
        klog_info!("HHDM Offset:           0x{:x}", INIT_STATS.hhdm_offset);
        klog_info!("=====================================================\n");
    }
}
pub fn init_memory_system(
    memmap: *const LimineMemmapResponse,
    hhdm_offset: u64,
    hhdm_available: bool,
    framebuffer: Option<FramebufferInfo>,
) -> c_int {
    unsafe {
        klog_debug!("========== SlopOS Memory System Initialization ==========");
        klog_debug!("Initializing complete memory management system...");

        FRAMEBUFFER_BOOT_INFO = framebuffer;

        // Initialize the unified HHDM module (single source of truth)
        if hhdm_available {
            crate::hhdm::init(hhdm_offset);
            if hhdm_offset != HHDM_VIRT_BASE {
                klog_info!(
                    "MM: WARNING - HHDM base 0x{:x} differs from expected 0x{:x}",
                    hhdm_offset,
                    HHDM_VIRT_BASE
                );
            }
        }

        if memmap.is_null() {
            mm_panic("MM: Missing Limine memory map");
        }

        init_kernel_memory_layout();
        if !crate::hhdm::is_available() {
            mm_panic("MM: HHDM unavailable; cannot translate physical addresses");
        }

        configure_region_store(memmap);
        record_memmap_usable(memmap);
        record_kernel_core_reservations();
        record_memmap_reservations(memmap);
        record_framebuffer_reservation();
        record_apic_reservation();

        compute_memory_stats(memmap, hhdm_offset);
        let allocator_plan = plan_allocator_metadata(memmap, hhdm_offset);

        compute_memory_stats(memmap, hhdm_offset);
        finalize_reserved_regions();

        EARLY_PAGING_OK = true;

        if init_page_allocator(
            allocator_plan.buffer as *mut _,
            allocator_plan.capacity_frames,
        ) != 0
        {
            mm_panic("MM: Page allocator initialization failed");
        }
        if finalize_page_allocator() != 0 {
            klog_info!("MM: WARNING - page allocator finalization reported issues");
        }

        init_paging();

        if init_kernel_heap() != 0 {
            mm_panic("MM: Kernel heap initialization failed");
        }

        if init_process_vm() != 0 {
            mm_panic("MM: Process VM initialization failed");
        }

        MEMORY_SYSTEM_INITIALIZED = true;
        display_memory_summary();

        klog_info!("MM: Complete memory system initialization successful!");
        klog_debug!("MM: Ready for scheduler and video subsystem initialization\n");
    }
    0
}
pub fn is_memory_system_initialized() -> c_int {
    unsafe { MEMORY_SYSTEM_INITIALIZED as c_int }
}
pub fn get_memory_statistics(
    total_memory_out: *mut u64,
    available_memory_out: *mut u64,
    regions_count_out: *mut u32,
) {
    unsafe {
        if !total_memory_out.is_null() {
            *total_memory_out = INIT_STATS.total_memory_bytes;
        }
        if !available_memory_out.is_null() {
            *available_memory_out = INIT_STATS.available_memory_bytes;
        }
        if !regions_count_out.is_null() {
            *regions_count_out = INIT_STATS.memory_regions_count;
        }
    }
}
