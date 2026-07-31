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
//! 3. **Limine request placement.** The three section labels the boot
//!    protocol reads belong to OSTD's `limine_request!`, so this module
//!    names a placement rather than a section string.

use core::{
    ffi::{c_char, c_void},
    ptr,
};

use limine::{
    BaseRevision, RequestsEndMarker, RequestsStartMarker, memmap,
    request::{
        BootloaderInfoRequest, EfiMemmapRequest, EfiRequest, ExecutableAddressRequest,
        ExecutableFileRequest, FramebufferRequest, HhdmRequest, MemmapRequest, ModulesRequest,
        MpRequest, MpResponse, RsdpRequest,
    },
};

use slopos_abi::DisplayInfo;
use slopos_ostd::sync::{InitInPlace, KernelSync, OnceLock};
use slopos_ostd::{klog_debug, klog_info};

pub use slopos_ostd::boot_info::{
    BootFramebuffer, BootInfo, LimineMemmapEntry, LimineMemmapResponse, MemoryRegion,
    MemoryRegionKind,
};

slopos_ostd::limine_request! {
    start_marker,
    static LIMINE_REQUESTS_START_MARKER: RequestsStartMarker = RequestsStartMarker::new();
}
slopos_ostd::limine_request! {
    request,
    static BASE_REVISION: BaseRevision = BaseRevision::new();
}
slopos_ostd::limine_request! {
    request,
    static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static MEMMAP_REQUEST: MemmapRequest = MemmapRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static KERNEL_FILE_REQUEST: ExecutableFileRequest = ExecutableFileRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static KERNEL_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static MP_REQUEST: MpRequest = MpRequest::new(0);
}
slopos_ostd::limine_request! {
    request,
    static MODULES_REQUEST: ModulesRequest = ModulesRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static EFI_REQUEST: EfiRequest = EfiRequest::new();
}
slopos_ostd::limine_request! {
    request,
    static EFI_MEMMAP_REQUEST: EfiMemmapRequest = EfiMemmapRequest::new();
}
slopos_ostd::limine_request! {
    end_marker,
    static LIMINE_REQUESTS_END_MARKER: RequestsEndMarker = RequestsEndMarker::new();
}

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
    /// Raw NUL-terminated cmdline pointer published by the bootloader.
    /// `KernelSync` wraps the raw pointer so the surrounding `SystemInfo`
    /// auto-derives `Send + Sync`; the cmdline buffer is read-only for
    /// the kernel's lifetime.
    cmdline_ptr: KernelSync<*const c_char>,
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
            cmdline_ptr: KernelSync::new(ptr::null()),
            flags: SystemFlags::new(),
        }
    }
}

// `SystemInfo` auto-derives `Send + Sync`: the two previously-
// problematic raw-pointer fields (`cmdline_ptr`,
// `framebuffer.address`) live behind `KernelSync<T>` wrappers, so
// the surrounding struct's auto-derived markers cover the contract
// without a hand-written `unsafe impl`.

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
            info.cmdline_ptr = KernelSync::new(cmdline_str.as_ptr() as *const c_char);
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

pub fn boot_info() -> slopos_ostd::boot_info::BootInfo {
    let info = sysinfo();
    slopos_ostd::boot_info::BootInfo {
        hhdm_offset: info.hhdm_offset,
        cmdline: info.cmdline,
        framebuffer: info.framebuffer,
        kernel_phys_base: info.kernel_phys_base,
        kernel_virt_base: info.kernel_virt_base,
        rsdp_address: info.rsdp_phys_addr,
    }
}

/// Bytes of the initramfs module loaded by Limine, if present.
///
/// The initramfs is a `newc` cpio archive declared in `limine.conf` with
/// `module_string: initramfs`; Limine maps it into the HHDM and exposes it via
/// the modules response. The returned slice borrows the bootloader-published
/// module memory, which Limine keeps mapped for the kernel's lifetime, so it is
/// `'static`. The `limine` crate owns the only `unsafe` here (`File::data`);
/// this crate stays `#![forbid(unsafe_code)]`.
pub fn initramfs() -> Option<&'static [u8]> {
    let response = MODULES_REQUEST.response()?;
    let modules = response.modules();
    modules
        .iter()
        .find(|module| module.cmdline() == "initramfs")
        .or_else(|| modules.first())
        .map(|module| module.data())
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
    *sysinfo().cmdline_ptr
}

pub fn kernel_cmdline_str() -> Option<&'static str> {
    sysinfo().cmdline
}

pub fn is_rsdp_available() -> i32 {
    sysinfo().flags.rsdp_available as i32
}

pub fn get_rsdp_phys_address() -> u64 {
    let info = sysinfo();
    if !info.flags.rsdp_available || info.rsdp_phys_addr == 0 {
        return 0;
    }
    // Limine v11 (base revision 6) returns the RSDP address as an HHDM
    // virtual pointer; older revisions returned a physical address.
    // Normalise to physical regardless.
    let addr = info.rsdp_phys_addr;
    if info.flags.hhdm_available && addr >= info.hhdm_offset {
        addr - info.hhdm_offset
    } else {
        addr
    }
}

/// Virtual address of the `EFI_SYSTEM_TABLE`, or `0` when the platform
/// was not booted via UEFI (BIOS / no EFI response). With base revision 6
/// Limine returns this as an HHDM-virtual pointer; the EFI runtime regions
/// it points into are mapped by [`crate::uefi_runtime`].
pub fn efi_system_table_addr() -> u64 {
    match EFI_REQUEST.response() {
        Some(resp) => resp.address as u64,
        None => 0,
    }
}

/// Borrow the raw UEFI memory-map blob and its per-descriptor stride
/// (`desc_size`), or `None` when not booted via UEFI. Must be consumed
/// early in boot: the array can live in bootloader-reclaimable memory.
pub fn efi_memmap() -> Option<(&'static [u8], usize)> {
    let resp = EFI_MEMMAP_REQUEST.response()?;
    Some((resp.memmap(), resp.desc_size as usize))
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

/// Wrapper for the legacy memmap pointer array. The inner array's
/// pointers self-reference `LEGACY_MEMMAP.entries[i]`, which lives in
/// a `'static` `SyncUnsafeCell` and is written exactly once (gated by
/// `LEGACY_MEMMAP_INIT.swap(true, SeqCst)` in `init_legacy_memmap`).
/// `KernelSync` provides the `Sync` impl for the surrounding cell;
/// the underlying memory is stable for the kernel's lifetime.
#[repr(transparent)]
struct SyncMemmapPtrArray(KernelSync<[*const LimineMemmapEntry; 256]>);

static LEGACY_MEMMAP: InitInPlace<LegacyMemmap> = InitInPlace::new(LegacyMemmap {
    entries: [LimineMemmapEntry {
        base: 0,
        length: 0,
        typ: 0,
    }; 256],
    ptrs: SyncMemmapPtrArray(KernelSync::new([ptr::null(); 256])),
    response: LimineMemmapResponse {
        revision: 0,
        entry_count: 0,
        entries: KernelSync::new(ptr::null()),
    },
});

fn init_legacy_memmap() {
    LEGACY_MEMMAP.init_once(|cell| {
        let Some(memmap) = MEMMAP_REQUEST.response() else {
            return;
        };

        let entries = memmap.entries();
        let count = entries.len().min(256);

        // The cell is at its final `'static` address (InitInPlace
        // contract); writing self-referential `*const` pointers into
        // `cell.ptrs.0[i] = &cell.entries[i]` produces addresses that
        // remain valid for the kernel's lifetime. The
        // `LimineMemmapResponse` C-ABI consumer contract requires
        // this self-referential layout — retiring the cell would
        // require flipping the consumer to `&[LimineMemmapEntry]`.
        for (i, entry) in entries.iter().take(count).enumerate() {
            cell.entries[i] = LimineMemmapEntry {
                base: entry.base,
                length: entry.length,
                typ: entry_type_to_u64(entry.type_),
            };
            cell.ptrs.0[i] = &cell.entries[i];
        }

        cell.response.entry_count = count as u64;
        cell.response.entries = KernelSync::new(cell.ptrs.0.as_ptr());
    });
}

pub fn limine_get_memmap_response() -> *const LimineMemmapResponse {
    init_legacy_memmap();
    // Post-init the response struct is read-only for the kernel's
    // lifetime; returning a `*const` to the cell is sound for the
    // boot consumer's downstream `*const` reads. `as_ptr` projects
    // the `*const LegacyMemmap` from the cell; we further `.cast` to
    // `*const LimineMemmapResponse` via the `response` field offset
    // (which is `repr(C)`-stable).
    let base = LEGACY_MEMMAP.as_ptr();
    let offset = core::mem::offset_of!(LegacyMemmap, response);
    (base as *const u8).wrapping_add(offset) as *const LimineMemmapResponse
}
