//! Limine bootloader handoff layer.
//!
//! # Surviving `unsafe` sites — file-level SAFETY
//!
//! All remaining `unsafe` in this file falls into one of three irreducible
//! classes:
//!
//! 1. **`unsafe impl Send + Sync` markers over bootloader-published pointers.**
//!    `SystemInfo` carries `cmdline_ptr: *const c_char` and a framebuffer
//!    `*mut u8`. Both point at memory the bootloader installs and Limine
//!    promises is mapped read-only (cmdline) or device-side (framebuffer)
//!    for the kernel's lifetime — Inv. 8. Rust cannot encode that promise,
//!    so the contract is asserted via `unsafe impl`. `SyncMemmapPtrArray`
//!    has the same shape: it stores `*const LimineMemmapEntry` values that
//!    point into our own static `LEGACY_MEMMAP` cell, which never moves.
//!
//! 2. **The legacy `LimineMemmapResponse` C-ABI shim.** `init_legacy_memmap`
//!    builds a self-referential structure (`LimineMemmapResponse` whose
//!    `entries: *const LimineMemmapEntry` points into a sibling field of
//!    the same `SyncUnsafeCell`). Retiring the `SyncUnsafeCell` would
//!    require flipping the consumer contract from `*const` to `&[…]` —
//!    downstream `mm/` and `boot/` callers read through the `*const`
//!    pointer and the shape is part of the published handoff. Init is
//!    gated on a `swap(true, SeqCst)` so the cell is written exactly
//!    once, then exposed as a `*const` for life.
//!
//! 3. **`#[unsafe(link_section = "…")]` attributes.** These are Edition 2024
//!    syntactic markers — they tell the linker where to place the Limine
//!    request statics, and the `unsafe` keyword is required by the attribute
//!    grammar. They are not runtime unsafe.

use core::{
    cell::SyncUnsafeCell,
    ffi::{c_char, c_void},
    ptr,
};

use limine::{
    BaseRevision, memmap,
    request::{
        BootloaderInfoRequest, ExecutableAddressRequest, ExecutableFileRequest, FramebufferRequest,
        HhdmRequest, MemmapRequest, MpRequest, MpResponse, RsdpRequest,
    },
};

use slopos_abi::DisplayInfo;
use slopos_ostd::sync::OnceLock;
use slopos_utils::{klog_debug, klog_info};

pub use slopos_utils::boot_info::{
    BootFramebuffer, BootInfo, LimineMemmapEntry, LimineMemmapResponse, MemoryRegion,
    MemoryRegionKind,
};

#[used]
#[unsafe(link_section = ".limine_requests_start_marker")]
static LIMINE_REQUESTS_START_MARKER: [u64; 1] = [0];

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static KERNEL_FILE_REQUEST: ExecutableFileRequest = ExecutableFileRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static KERNEL_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MP_REQUEST: MpRequest = MpRequest::new(0);

#[used]
#[unsafe(link_section = ".limine_requests_end_marker")]
static LIMINE_REQUESTS_END_MARKER: [u64; 1] = [0];

fn convert_entry_type(entry_type: u64) -> MemoryRegionKind {
    match entry_type {
        memmap::MEMMAP_USABLE => MemoryRegionKind::Usable,
        memmap::MEMMAP_RESERVED => MemoryRegionKind::Reserved,
        memmap::MEMMAP_ACPI_RECLAIMABLE => MemoryRegionKind::AcpiReclaimable,
        memmap::MEMMAP_ACPI_NVS => MemoryRegionKind::AcpiNvs,
        memmap::MEMMAP_BAD_MEMORY => MemoryRegionKind::BadMemory,
        memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => MemoryRegionKind::BootloaderReclaimable,
        memmap::MEMMAP_EXECUTABLE_AND_MODULES => MemoryRegionKind::KernelAndModules,
        memmap::MEMMAP_FRAMEBUFFER => MemoryRegionKind::Framebuffer,
        _ => MemoryRegionKind::Reserved,
    }
}

fn entry_type_to_u64(entry_type: u64) -> u64 {
    match entry_type {
        memmap::MEMMAP_USABLE => 0,
        memmap::MEMMAP_RESERVED => 1,
        memmap::MEMMAP_ACPI_RECLAIMABLE => 2,
        memmap::MEMMAP_ACPI_NVS => 3,
        memmap::MEMMAP_BAD_MEMORY => 4,
        memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => 5,
        memmap::MEMMAP_EXECUTABLE_AND_MODULES => 6,
        memmap::MEMMAP_FRAMEBUFFER => 7,
        _ => 1,
    }
}

fn limine_entry_to_region(entry: &memmap::Entry) -> MemoryRegion {
    MemoryRegion::new(entry.base, entry.length, convert_entry_type(entry.type_))
}

#[derive(Clone, Copy, Debug)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub typ: u64,
}

#[derive(Clone, Copy)]
struct SystemFlags {
    framebuffer_available: bool,
    memmap_available: bool,
    hhdm_available: bool,
    rsdp_available: bool,
    kernel_cmdline_available: bool,
}

impl SystemFlags {
    const fn new() -> Self {
        Self {
            framebuffer_available: false,
            memmap_available: false,
            hhdm_available: false,
            rsdp_available: false,
            kernel_cmdline_available: false,
        }
    }
}

struct SystemInfo {
    total_memory: u64,
    available_memory: u64,
    framebuffer: Option<BootFramebuffer>,
    hhdm_offset: u64,
    kernel_phys_base: u64,
    kernel_virt_base: u64,
    rsdp_phys_addr: u64,
    rsdp_virt_addr: u64,
    memmap_entry_count: u64,
    cmdline: Option<&'static str>,
    cmdline_ptr: *const c_char,
    flags: SystemFlags,
}

impl SystemInfo {
    const fn new() -> Self {
        Self {
            total_memory: 0,
            available_memory: 0,
            framebuffer: None,
            hhdm_offset: 0,
            kernel_phys_base: 0,
            kernel_virt_base: 0,
            rsdp_phys_addr: 0,
            rsdp_virt_addr: 0,
            memmap_entry_count: 0,
            cmdline: None,
            cmdline_ptr: ptr::null(),
            flags: SystemFlags::new(),
        }
    }
}

// SAFETY (Inv. 8): every field of `SystemInfo` is plain data with one
// exception each for Send and Sync. The `cmdline_ptr: *const c_char`
// points into the bootloader-published kernel cmdline string which
// Limine maps read-only for the kernel's lifetime; the `framebuffer:
// Option<BootFramebuffer>` carries a `*mut u8` pointer at a stable
// device-side MMIO address. Both pointers are read-only or device-side
// and never aliased mutably from kernel code, so concurrent reads are
// safe and ownership transfer is meaningful. Retiring this `unsafe
// impl` would require a wrapper newtype with manual Send/Sync — same
// `unsafe impl` count, no soundness improvement.
unsafe impl Sync for SystemInfo {}
unsafe impl Send for SystemInfo {}

static SYSTEM_INFO: OnceLock<SystemInfo> = OnceLock::new();

fn sysinfo() -> &'static SystemInfo {
    SYSTEM_INFO
        .get()
        .expect("init_limine_protocol must run before sysinfo")
}

pub fn ensure_base_revision() {
    if !BASE_REVISION.is_supported() {
        panic!("Limine base revision not supported");
    }
}

pub fn mp_response() -> Option<&'static MpResponse> {
    MP_REQUEST.response()
}

fn build_system_info() -> SystemInfo {
    let mut info = SystemInfo::new();

    if let Some(resp) = BOOTLOADER_INFO_REQUEST.response() {
        let name = resp.name();
        let version = resp.version();
        klog_debug!("Bootloader: {} version {}", name, version);
    }

    if let Some(hhdm) = HHDM_REQUEST.response() {
        info.hhdm_offset = hhdm.offset;
        info.flags.hhdm_available = true;
        klog_debug!("HHDM offset: 0x{:x}", hhdm.offset);
    }

    if let Some(ka) = KERNEL_ADDRESS_REQUEST.response() {
        info.kernel_phys_base = ka.physical_base;
        info.kernel_virt_base = ka.virtual_base;
        klog_debug!(
            "Kernel phys base: 0x{:x} virt base: 0x{:x}",
            ka.physical_base,
            ka.virtual_base
        );
    }

    if let Some(rsdp) = RSDP_REQUEST.response() {
        let rsdp_ptr = rsdp.address as u64;
        info.rsdp_phys_addr = rsdp_ptr;
        info.rsdp_virt_addr = rsdp_ptr;
        info.flags.rsdp_available = rsdp_ptr != 0;

        if rsdp_ptr != 0 {
            klog_debug!("ACPI RSDP pointer: 0x{:x}", rsdp_ptr);
        } else {
            klog_info!("ACPI: Limine returned null RSDP pointer");
        }
    }

    if let Some(kf_resp) = KERNEL_FILE_REQUEST.response() {
        let kernel_file = kf_resp.executable_file();
        let cmdline_str = kernel_file.cmdline();
        if !cmdline_str.is_empty() {
            info.cmdline_ptr = cmdline_str.as_ptr() as *const c_char;
            info.cmdline = Some(cmdline_str);
            info.flags.kernel_cmdline_available = true;

            klog_debug!("Kernel cmdline: {}", cmdline_str);
        } else {
            klog_debug!("Kernel cmdline: <empty>");
        }
    }

    if let Some(memmap) = MEMMAP_REQUEST.response() {
        let entries = memmap.entries();
        let mut total = 0u64;
        let mut available = 0u64;

        for entry in entries {
            total = total.saturating_add(entry.length);
            if entry.type_ == memmap::MEMMAP_USABLE {
                available = available.saturating_add(entry.length);
            }
        }

        info.total_memory = total;
        info.available_memory = available;
        info.memmap_entry_count = entries.len() as u64;
        info.flags.memmap_available = true;

        klog_debug!(
            "Memory map: {} entries, total {} MB, available {} MB",
            entries.len(),
            total / (1024 * 1024),
            available / (1024 * 1024)
        );
    } else {
        klog_info!("WARNING: No memory map available from Limine");
    }

    if let Some(fb_resp) = FRAMEBUFFER_REQUEST.response() {
        let framebuffers = fb_resp.framebuffers();
        if let Some(fb) = framebuffers.first() {
            let display_info = DisplayInfo::from_raw(fb.width, fb.height, fb.pitch, fb.bpp);
            info.framebuffer = Some(BootFramebuffer::new(fb.address() as *mut u8, display_info));
            info.flags.framebuffer_available = true;

            klog_debug!("Framebuffer: {}x{} @ {} bpp", fb.width, fb.height, fb.bpp);
            klog_debug!(
                "Framebuffer addr: 0x{:x} pitch: {}",
                fb.address() as u64,
                fb.pitch
            );
        } else {
            klog_info!("WARNING: No framebuffer provided by Limine");
            info.flags.framebuffer_available = false;
        }
    } else {
        klog_info!("WARNING: No framebuffer response from Limine");
        info.flags.framebuffer_available = false;
    }

    info
}

pub fn init_limine_protocol() -> i32 {
    if !BASE_REVISION.is_supported() {
        klog_info!("ERROR: Limine base revision not supported!");
        return -1;
    }

    SYSTEM_INFO.call_once(build_system_info);
    0
}

pub fn boot_info() -> slopos_utils::boot_info::BootInfo {
    let info = sysinfo();
    slopos_utils::boot_info::BootInfo {
        hhdm_offset: info.hhdm_offset,
        cmdline: info.cmdline,
        framebuffer: info.framebuffer,
        kernel_phys_base: info.kernel_phys_base,
        kernel_virt_base: info.kernel_virt_base,
        rsdp_address: info.rsdp_phys_addr,
    }
}

pub fn get_framebuffer_info(
    addr: *mut u64,
    width: *mut u32,
    height: *mut u32,
    pitch: *mut u32,
    bpp: *mut u8,
) -> i32 {
    let info = sysinfo();
    if let Some(boot_fb) = info.framebuffer {
        unsafe {
            if !addr.is_null() {
                *addr = boot_fb.address as u64;
            }
            if !width.is_null() {
                *width = boot_fb.info.width;
            }
            if !height.is_null() {
                *height = boot_fb.info.height;
            }
            if !pitch.is_null() {
                *pitch = boot_fb.info.pitch;
            }
            if !bpp.is_null() {
                *bpp = boot_fb.info.format.bytes_per_pixel() * 8;
            }
        }
        1
    } else {
        0
    }
}

pub fn is_framebuffer_available() -> i32 {
    sysinfo().flags.framebuffer_available as i32
}

pub fn get_total_memory() -> u64 {
    sysinfo().total_memory
}

pub fn get_available_memory() -> u64 {
    sysinfo().available_memory
}

pub fn is_memory_map_available() -> i32 {
    sysinfo().flags.memmap_available as i32
}

pub fn get_hhdm_offset() -> u64 {
    sysinfo().hhdm_offset
}

pub fn is_hhdm_available() -> i32 {
    sysinfo().flags.hhdm_available as i32
}

pub fn get_kernel_phys_base() -> u64 {
    sysinfo().kernel_phys_base
}

pub fn get_kernel_virt_base() -> u64 {
    sysinfo().kernel_virt_base
}

pub fn get_kernel_cmdline() -> *const c_char {
    sysinfo().cmdline_ptr
}

pub fn kernel_cmdline_str() -> Option<&'static str> {
    sysinfo().cmdline
}

pub fn is_rsdp_available() -> i32 {
    sysinfo().flags.rsdp_available as i32
}

pub fn get_rsdp_phys_address() -> u64 {
    sysinfo().rsdp_phys_addr
}

pub fn get_rsdp_address() -> *const c_void {
    let info = sysinfo();
    if !info.flags.rsdp_available || info.rsdp_phys_addr == 0 {
        return ptr::null();
    }

    let addr = info.rsdp_phys_addr;

    // With base revision 6 (Limine v11), the RSDP address is returned as a
    // virtual (HHDM) pointer again (unlike revision 3 which returned physical).
    // Detect whether the address is already in the HHDM range or needs conversion.
    if addr >= info.hhdm_offset && info.flags.hhdm_available {
        // Already an HHDM virtual address
        addr as *const c_void
    } else if info.flags.hhdm_available {
        // Physical address - convert to HHDM virtual
        (addr + info.hhdm_offset) as *const c_void
    } else {
        // Fallback: return as-is (will likely fault)
        addr as *const c_void
    }
}

pub fn get_memmap_entry(index: usize) -> Option<MemmapEntry> {
    let memmap = MEMMAP_REQUEST.response()?;
    let entries = memmap.entries();
    let entry = entries.get(index)?;
    Some(MemmapEntry {
        base: entry.base,
        length: entry.length,
        typ: entry_type_to_u64(entry.type_),
    })
}

pub fn memmap_entry_count() -> usize {
    MEMMAP_REQUEST
        .response()
        .map(|r| r.entries().len())
        .unwrap_or(0)
}

pub fn memory_regions() -> impl Iterator<Item = MemoryRegion> {
    MEMMAP_REQUEST
        .response()
        .into_iter()
        .flat_map(|r| r.entries().iter())
        .map(|e| limine_entry_to_region(e))
}

/// Single backing cell for the C-ABI legacy memmap shim. Kept as a
/// [`SyncUnsafeCell`] (rather than a `OnceLock`) because the three
/// fields are self-referential — `ptrs[i]` points into `entries[i]`
/// and `response.entries` points into `ptrs.0` — so the storage must
/// be initialised in place at its final static address. A single
/// [`AtomicBool`] gates one-shot initialisation; the previous
/// three-static layout collapses into this one.
struct LegacyMemmap {
    entries: [LimineMemmapEntry; 256],
    ptrs: SyncMemmapPtrArray,
    response: LimineMemmapResponse,
}

#[repr(transparent)]
struct SyncMemmapPtrArray([*const LimineMemmapEntry; 256]);
// SAFETY (Inv. 8): the inner pointers self-reference `LEGACY_MEMMAP.entries[i]`,
// which lives in a `'static` `SyncUnsafeCell` and is written exactly once
// (gated by `LEGACY_MEMMAP_INIT.swap(true, SeqCst)` in `init_legacy_memmap`).
// The cell never moves and the entries it brackets never move, so the
// stored pointers stay valid for the kernel's lifetime. Retiring this
// `unsafe impl` would require flipping the `LimineMemmapResponse`
// consumer contract from `*const` to `&[…]`, which is a separate scope.
unsafe impl Sync for SyncMemmapPtrArray {}

static LEGACY_MEMMAP: SyncUnsafeCell<LegacyMemmap> = SyncUnsafeCell::new(LegacyMemmap {
    entries: [LimineMemmapEntry {
        base: 0,
        length: 0,
        typ: 0,
    }; 256],
    ptrs: SyncMemmapPtrArray([ptr::null(); 256]),
    response: LimineMemmapResponse {
        revision: 0,
        entry_count: 0,
        entries: ptr::null(),
    },
});

static LEGACY_MEMMAP_INIT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

fn init_legacy_memmap() {
    use core::sync::atomic::Ordering;

    if LEGACY_MEMMAP_INIT.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(memmap) = MEMMAP_REQUEST.response() else {
        return;
    };

    let entries = memmap.entries();
    let count = entries.len().min(256);

    // SAFETY (Inv. 8): `LEGACY_MEMMAP_INIT.swap(true, SeqCst)` above
    // gates this branch to a single thread for the kernel's lifetime;
    // every other path returns early. The cell is `'static`, so the
    // self-referential `cell.ptrs.0[i] = &cell.entries[i]` writes
    // produce pointers that stay valid until the kernel exits. The
    // `LimineMemmapResponse` C-ABI consumer contract requires this
    // self-referential layout — retiring the `SyncUnsafeCell` would
    // require flipping the consumer to `&[LimineMemmapEntry]`.
    unsafe {
        let cell = &mut *LEGACY_MEMMAP.get();

        for (i, entry) in entries.iter().take(count).enumerate() {
            cell.entries[i] = LimineMemmapEntry {
                base: entry.base,
                length: entry.length,
                typ: entry_type_to_u64(entry.type_),
            };
            cell.ptrs.0[i] = &cell.entries[i];
        }

        cell.response.entry_count = count as u64;
        cell.response.entries = cell.ptrs.0.as_ptr();
    }
}

pub fn limine_get_memmap_response() -> *const LimineMemmapResponse {
    init_legacy_memmap();
    // SAFETY (Inv. 8): post-init the response struct is read-only for
    // the kernel's lifetime; returning a `*const` to the static cell
    // is sound for the boot consumer's downstream `*const` reads. The
    // `*const`-only return type is the published C-ABI contract;
    // returning `&'static LimineMemmapResponse` would let consumers
    // observe stale `entries` should the cell ever be re-init'd, which
    // the `LEGACY_MEMMAP_INIT.swap` gate intentionally forbids.
    unsafe { &(*LEGACY_MEMMAP.get()).response as *const LimineMemmapResponse }
}
