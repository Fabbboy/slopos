use crate::kernel_mappings::{kernel_is_mapped, kernel_map_io_4kb};
use crate::memory_layout::{init_kernel_bounds, kernel_image_bounds};
use crate::memory_layout_defs::{
    BOOT_STACK_PHYS_ADDR, BOOT_STACK_SIZE, EARLY_PD_PHYS_ADDR, EARLY_PDPT_PHYS_ADDR,
    EARLY_PML4_PHYS_ADDR, HHDM_VIRT_BASE, KERNEL_VIRTUAL_BASE,
};
use crate::memory_reservations::{
    MM_RESERVATION_FLAG_ALLOW_MM_PHYS_TO_VIRT, MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS,
    MM_RESERVATION_FLAG_MMIO, MmRegionKind, MmReservationType, mm_region_add_usable,
    mm_region_count, mm_region_get, mm_region_highest_usable_frame, mm_region_map_reset,
    mm_region_reserve, mm_region_total_bytes, mm_reservation_type_name, mm_reservations_capacity,
    mm_reservations_count, mm_reservations_get, mm_reservations_overflow_count,
    mm_reservations_total_bytes,
};
use crate::page_alloc::{BUDDY_ALLOCATOR, page_allocator_descriptor_size};
use crate::paging::init_paging;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::init_process_vm;
use core::ffi::{c_char, c_int};
use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::lock_class;

use slopos_abi::DisplayInfo;
use slopos_arch::cpu;
use slopos_arch::cpu::apic_msr::ApicBaseMsr;
use slopos_arch::cpu::cpuid::{CPUID_FEAT_EDX_APIC, CPUID_LEAF_FEATURES};
use slopos_arch::cpu::msr::Msr;
use slopos_ostd::boot_info::{LimineMemmapResponse, limine_memmap_iter};
use slopos_ostd::sync::{BspToken, InitFlag, LOCK_LEVEL_RESOURCE, OnceLock, SpinLock};
use slopos_ostd::{align_down_u64, align_up_u64, klog_debug, klog_info};

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

static INIT_STATS: SpinLock<MemoryInitStats> = SpinLock::new(
    MemoryInitStats {
        total_memory_bytes: 0,
        available_memory_bytes: 0,
        reserved_device_bytes: 0,
        memory_regions_count: 0,
        reserved_region_count: 0,
        hhdm_offset: 0,
        tracked_page_frames: 0,
        allocator_metadata_bytes: 0,
    },
    lock_class!("INIT_STATS", LOCK_LEVEL_RESOURCE),
);

fn init_stats_snapshot() -> MemoryInitStats {
    *INIT_STATS.lock()
}

static EARLY_PAGING_INIT: InitFlag = InitFlag::new();
static MEMORY_SYSTEM_INIT: InitFlag = InitFlag::new();
#[derive(Clone, Copy)]
struct FramebufferReservation {
    address: u64,
    pitch: u64,
    height: u64,
}

static FRAMEBUFFER_RESERVATION: SpinLock<Option<FramebufferReservation>> = SpinLock::new(
    None,
    lock_class!("FRAMEBUFFER_RESERVATION", LOCK_LEVEL_RESOURCE),
);

/// Plumbing for the pre-typestate / post-typestate boot-step split. The
/// pre step receives `memmap` and `hhdm_offset` from Limine; the post
/// step needs them again for `map_acpi_regions`. Stored as a raw
/// integer so the pointer is `Send + Sync`-clean for static storage —
/// reconstructed via `as *const _` in the post step.
struct MemoryInitCtx {
    memmap_ptr: usize,
    hhdm_offset: u64,
}

static MEMORY_INIT_CTX: OnceLock<MemoryInitCtx> = OnceLock::new();

fn framebuffer_reservation() -> Option<FramebufferReservation> {
    *FRAMEBUFFER_RESERVATION.lock()
}

fn configure_region_store(memmap: *const LimineMemmapResponse) {
    // The reservation store now embeds its 4096-slot array directly inside
    // a SpinLock; capacity is fixed at the type level. Iterating the
    // memmap purely to derive a planning estimate would only be a logging
    // exercise — emit an informational warning if the entry count would
    // have overflowed the legacy estimate, then reset.
    let entry_count = limine_memmap_iter(memmap).count();
    if entry_count > BOOT_REGION_STATIC_CAP {
        klog_info!(
            "MM: region map saw {} entries; clamping to capacity {}",
            entry_count,
            BOOT_REGION_STATIC_CAP
        );
    }
    mm_region_map_reset();
}

fn add_reservation_or_panic(
    base: u64,
    length: u64,
    type_: MmReservationType,
    flags: u32,
    label: *const c_char,
) {
    if mm_region_reserve(base, length, type_, flags, label) != 0 {
        panic!("MM: Failed to record reserved region");
    }
}

fn add_usable_or_panic(base: u64, length: u64, label: *const c_char) {
    if mm_region_add_usable(base, length, label) != 0 {
        panic!("MM: Failed to record usable region");
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
        panic!("MM: Missing Limine memmap for usable regions");
    }
    {
        let mut stats = INIT_STATS.lock();
        stats.total_memory_bytes = 0;
    }
    let mut total: u64 = 0;
    let mut saw_any = false;
    for entry in limine_memmap_iter(memmap) {
        saw_any = true;
        if entry.length == 0 {
            continue;
        }
        total = total.saturating_add(entry.length);
        if entry.typ != LIMINE_MEMMAP_USABLE {
            continue;
        }
        let base = align_up_u64(entry.base, PAGE_SIZE_4KB);
        let end = align_down_u64(entry.base + entry.length, PAGE_SIZE_4KB);
        if end <= base {
            continue;
        }
        add_usable_or_panic(base, end - base, b"usable\0".as_ptr() as *const c_char);
    }
    if !saw_any {
        panic!("MM: Missing Limine memmap for usable regions");
    }
    INIT_STATS.lock().total_memory_bytes = total;
}

fn compute_memory_stats(memmap: *const LimineMemmapResponse, hhdm_offset: u64) {
    let _ = memmap;
    // Gather snapshots from the region store (REGION_STORE lock, same
    // LOCK_LEVEL_RESOURCE as INIT_STATS) BEFORE acquiring INIT_STATS.
    // Holding INIT_STATS while calling mm_region_* would nest two
    // same-level locks, which OSTD's lock-tracking walker treats as
    // an AB-BA deadlock risk.
    let memory_regions_count = mm_region_count();
    let available_memory_bytes = mm_region_total_bytes(MmRegionKind::Usable);
    let tracked_page_frames = if available_memory_bytes == 0 {
        0
    } else {
        let highest_frame = mm_region_highest_usable_frame();
        if highest_frame >= u32::MAX as u64 {
            0
        } else {
            (highest_frame + 1) as u32
        }
    };
    if tracked_page_frames == 0 && available_memory_bytes > 0 {
        panic!("MM: Usable memory exceeds supported frame range");
    }
    let reserved_region_count = mm_reservations_count();
    let reserved_device_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);

    let mut stats = INIT_STATS.lock();
    stats.hhdm_offset = hhdm_offset;
    stats.memory_regions_count = memory_regions_count;
    stats.available_memory_bytes = available_memory_bytes;
    stats.tracked_page_frames = tracked_page_frames;
    stats.reserved_region_count = reserved_region_count;
    stats.reserved_device_bytes = reserved_device_bytes;
}

fn record_kernel_core_reservations() {
    let (kstart, kend) = kernel_image_bounds();
    if kstart == 0 && kend == 0 {
        klog_info!("MM: kernel bounds unavailable; cannot reserve kernel image");
        return;
    }

    let kstart_phys = virt_to_phys_kernel(kstart);
    let kend_phys = virt_to_phys_kernel(kend);
    let kernel_size = kend_phys.saturating_sub(kstart_phys);

    if kernel_size > 0 {
        add_reservation_or_panic(
            kstart_phys,
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

/// Make every ACPI-reclaimable region reachable through the HHDM, so
/// the table parsers can read them. Limine revision 3 no longer maps
/// these regions itself.
///
/// The pages are firmware memory, not allocator memory: the reservation
/// store keeps them out of the buddy, and a leaf that owned one would
/// hand ACPI tables back to the allocator the day something unmapped
/// it. They go in through the no-ownership entry point for that reason.
///
/// A VA that already resolves is left exactly as it is. Re-installing
/// an identical translation would mean demoting whatever huge leaf
/// currently covers it, which costs page tables to change nothing, and
/// the cursor refuses to overwrite a present leaf in any case.
fn map_acpi_regions(memmap: *const LimineMemmapResponse, hhdm_offset: u64) {
    if memmap.is_null() {
        return;
    }
    let flags = PageFlags::KERNEL_RW.bits();
    let mut mapped_count = 0u32;
    let mut present_count = 0u32;
    for entry in limine_memmap_iter(memmap) {
        if entry.length == 0 || entry.typ != LIMINE_MEMMAP_ACPI_RECLAIMABLE {
            continue;
        }
        let aligned_base = align_down_u64(entry.base, PAGE_SIZE_4KB);
        let aligned_end = align_up_u64(entry.base + entry.length, PAGE_SIZE_4KB);
        let mut phys = aligned_base;
        while phys < aligned_end {
            let virt = VirtAddr::new(phys + hhdm_offset);
            if kernel_is_mapped(virt) {
                present_count += 1;
            } else if kernel_map_io_4kb(virt, PhysAddr::new(phys), flags) == 0 {
                mapped_count += 1;
            }
            phys += PAGE_SIZE_4KB;
        }
    }
    if mapped_count > 0 || present_count > 0 {
        klog_debug!(
            "MM: Mapped {} ACPI reclaimable pages to HHDM ({} already present)",
            mapped_count,
            present_count
        );
    }
}

fn record_memmap_reservations(memmap: *const LimineMemmapResponse) {
    if memmap.is_null() {
        return;
    }
    for entry in limine_memmap_iter(memmap) {
        if entry.length == 0 {
            continue;
        }
        match entry.typ {
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

fn record_framebuffer_reservation() {
    let Some(fb) = framebuffer_reservation() else {
        return;
    };

    let mut phys_base = fb.address;
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
    let (_a, _b, _c, d) = cpu::cpuid(CPUID_LEAF_FEATURES);
    if (d & CPUID_FEAT_EDX_APIC) == 0 {
        return;
    }
    let apic_base_msr = cpu::read_msr(Msr::APIC_BASE);
    let apic_phys = apic_base_msr & ApicBaseMsr::ADDR_MASK;
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
    for i in (0..mm_region_count()).rev() {
        let Some(region) = mm_region_get(i) else {
            continue;
        };
        if region.kind != MmRegionKind::Usable || region.length < reserved_bytes {
            continue;
        }
        let region_end = region.phys_base + region.length;
        let mut candidate = align_down_u64(region_end - reserved_bytes, PAGE_SIZE_4KB);
        if candidate < region.phys_base {
            candidate = region.phys_base;
        }
        return candidate;
    }
    0
}

fn plan_allocator_metadata(
    _memmap: *const LimineMemmapResponse,
    hhdm_offset: u64,
) -> AllocatorPlan {
    let tracked_frames = INIT_STATS.lock().tracked_page_frames;
    if tracked_frames == 0 {
        panic!("MM: No tracked frames available for allocator sizing");
    }
    let desc_bytes = tracked_frames as u64 * page_allocator_descriptor_size() as u64;
    let mut aligned_bytes = align_up_u64(desc_bytes, DESC_ALIGN_BYTES);
    aligned_bytes = align_up_u64(aligned_bytes, PAGE_SIZE_4KB);
    INIT_STATS.lock().allocator_metadata_bytes = desc_bytes;

    let phys_base = select_allocator_window(aligned_bytes);
    if phys_base == 0 {
        panic!("MM: Failed to find window for allocator metadata");
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
        capacity_frames: tracked_frames,
    }
}

fn finalize_reserved_regions() {
    {
        let mut stats = INIT_STATS.lock();
        stats.reserved_region_count = mm_reservations_count();
        stats.reserved_device_bytes =
            mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);
    }

    log_reserved_regions();

    if mm_reservations_overflow_count() > 0 {
        panic!("MM: Reserved region capacity exceeded");
    }
}

fn log_reserved_regions() {
    {
        let count = mm_reservations_count();
        if count == 0 {
            klog_info!("MM: No device memory reservations detected");
            return;
        }
        let total_bytes = mm_reservations_total_bytes(MM_RESERVATION_FLAG_EXCLUDE_ALLOCATORS);
        klog_info!("MM: Reserved device regions ({})", count);
        for i in 0..count {
            let Some(region) = mm_reservations_get(i) else {
                continue;
            };
            let label_str = if region.label[0] != 0 {
                slopos_ostd::string::bytes_as_str(&region.label)
            } else {
                mm_reservation_type_name(region.type_)
            };
            let region_end = region.phys_base + region.length;
            klog_info!(
                "  {}: 0x{:x} - 0x{:x} ({} KB)",
                label_str,
                region.phys_base,
                region_end - 1,
                region.length / 1024
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
    let stats = init_stats_snapshot();
    klog_info!("\n========== SlopOS Memory System Initialized ==========");
    let early_paging_str = if EARLY_PAGING_INIT.is_set() {
        "OK"
    } else {
        "SKIPPED"
    };
    klog_info!("Early Paging:          {}", early_paging_str);
    klog_info!("Reserved Regions:      {}", stats.reserved_region_count);
    klog_info!("Tracked Frames:        {}", stats.tracked_page_frames);
    klog_info!(
        "Allocator Metadata:    {} KB",
        stats.allocator_metadata_bytes / 1024
    );
    klog_info!(
        "Reserved Device Mem:   {} KB",
        stats.reserved_device_bytes / 1024
    );
    klog_info!(
        "Total Memory:          {} MB",
        stats.total_memory_bytes / (1024 * 1024)
    );
    klog_info!(
        "Available Memory:      {} MB",
        stats.available_memory_bytes / (1024 * 1024)
    );
    klog_info!("Memory Regions:        {}", stats.memory_regions_count);
    klog_info!("HHDM Offset:           0x{:x}", stats.hhdm_offset);
    klog_info!("=====================================================");
}
/// Pre-typestate half of memory init. Runs at memory-phase
/// priority 2 — before `META_SLOTS` is installed and before the
/// OSTD `FrameAlloc` shim is registered.
///
/// On success: HHDM is live, the memmap is parsed into the region
/// store, the buddy allocator is up, the kernel-master PML4 is
/// re-published into `KERNEL_PML4_PHYS`, and PAT is programmed. The
/// `memmap` and `hhdm_offset` arguments are stashed in
/// `MEMORY_INIT_CTX` so the post-typestate half can reach them
/// without re-plumbing through the boot-step API.
///
/// After this returns, `META_SLOTS` install and `register_with_ostd`
/// run (priorities 5 and 6) so the typestate `Frame::<KernelMeta>`
/// alloc path is live before any caller in the post step touches it.
pub fn init_memory_system_pre_typestate(
    memmap: *const LimineMemmapResponse,
    hhdm_offset: u64,
    hhdm_available: bool,
    framebuffer: Option<(u64, &DisplayInfo)>,
) -> c_int {
    klog_debug!("========== SlopOS Memory System Initialization ==========");
    klog_debug!("Initializing complete memory management system...");

    *FRAMEBUFFER_RESERVATION.lock() = framebuffer.map(|(addr, info)| FramebufferReservation {
        address: addr,
        pitch: info.pitch as u64,
        height: info.height as u64,
    });

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
        panic!("MM: Missing Limine memory map");
    }

    init_kernel_bounds();
    if !crate::hhdm::is_available() {
        panic!("MM: HHDM unavailable; cannot translate physical addresses");
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

    EARLY_PAGING_INIT.mark_set();

    // Drive the buddy allocator's lifecycle:
    //   Uninit → Sized: install the boot-allocated frame descriptor table.
    //   Sized  → Seeded: seed the free-lists from the recorded memory map.
    //   Seeded → Live: enable per-CPU page caches for the order-0 fast path.
    BUDDY_ALLOCATOR.install_descriptor_table(
        allocator_plan.buffer as *mut u8,
        allocator_plan.capacity_frames,
    );
    BUDDY_ALLOCATOR.seed_from_memory_map();
    BUDDY_ALLOCATOR.enable_pcp();

    init_paging();
    crate::pat::pat_init();

    MEMORY_INIT_CTX.call_once(|| MemoryInitCtx {
        memmap_ptr: memmap as usize,
        hhdm_offset,
    });

    0
}

/// Post-typestate half of memory init. Runs at memory-phase
/// priority 10 — after `META_SLOTS` is installed (priority 5) and
/// the OSTD `FrameAlloc` shim is registered (priority 6).
///
/// On entry the buddy allocator and the typestate `Frame<_>` API are
/// both live, so every page allocation made here goes through
/// `Frame::<KernelMeta>::alloc` rather than the raw bootstrap path.
pub fn init_memory_system_post_typestate<'brand>(token: &BspToken<'brand>) -> c_int {
    let ctx = MEMORY_INIT_CTX
        .get()
        .expect("init_memory_system_post_typestate before init_memory_system_pre_typestate");
    let memmap = ctx.memmap_ptr as *const LimineMemmapResponse;
    let hhdm_offset = ctx.hhdm_offset;

    // Map ACPI reclaimable regions into HHDM so drivers can parse ACPI tables
    // This is required for Limine revision 3 which no longer maps these regions
    map_acpi_regions(memmap, hhdm_offset);

    // Bring up the kernel slab: state-machine transition to `Live`,
    // register the slab as OSTD's `KernelHeapBackend` so every global
    // allocation routes through `KERNEL_SLAB`, run the soft-reboot
    // coherency warmup (load-bearing for framebuffer perf after PS/2
    // soft reset — see the comment block in `mm/src/slab/mod.rs`).
    crate::slab::init_kernel_slab(token);
    crate::global_allocator_use_kernel_slab(token);
    crate::slab::warmup_for_soft_reboot();

    // Kernel-stack and SafeStack data-stack VA allocators — must come after
    // paging + heap so each region's `SpinLock` is usable and paging
    // primitives can be called.  Backs all task stacks via the
    // generic `TaskStack<R>` handle in `core::scheduler::task_stack`.
    crate::stack_va::init::<crate::stack_region::KstackRegion>();
    crate::stack_va::init::<crate::stack_region::UstackRegion>();
    // Arm the per-CPU magazine fast path. Safe to do here: each
    // `SlabAllocator<SIZE>` owns its own lock, so magazine re-entry
    // is bounded to a single size class and `IrqPreemptGuard` pins
    // the CPU for the duration of every push/pop.
    crate::slab::enable_heap_caches();

    if init_process_vm() != 0 {
        panic!("MM: Process VM initialization failed");
    }

    MEMORY_SYSTEM_INIT.mark_set();
    display_memory_summary();

    klog_info!("MM: Complete memory system initialization successful!");
    klog_debug!("MM: Ready for scheduler and video subsystem initialization");
    0
}
pub fn is_memory_system_initialized() -> c_int {
    MEMORY_SYSTEM_INIT.is_set() as c_int
}
pub fn get_memory_statistics(
    total_memory_out: &mut u64,
    available_memory_out: &mut u64,
    regions_count_out: &mut u32,
) {
    let stats = init_stats_snapshot();
    *total_memory_out = stats.total_memory_bytes;
    *available_memory_out = stats.available_memory_bytes;
    *regions_count_out = stats.memory_regions_count;
}
