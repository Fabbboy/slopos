
use core::{
    cell::UnsafeCell,
    ffi::{c_char, c_void, CStr},
    ptr,
};

use slopos_lib::{klog_debug, klog_info};

const LIMINE_COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];
const LIMINE_BASE_REVISION_MAGIC: [u64; 3] = [
    0xf9562b2d5c95a6c8,
    0x6a7b384944536bdc,
    1, /* base revision 1 */
];

const LIMINE_HHDM_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0x48dcf1cb8ad2b852,
    0x63984e959a98244b,
];
const LIMINE_MEMMAP_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0x67cf3d9d378a806f,
    0xe304acdfc50c3c62,
];
const LIMINE_FRAMEBUFFER_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0x9d5827dcd881dd75,
    0xa3148604f6fab11b,
];
const LIMINE_KERNEL_FILE_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0xad97e90e83f1ed67,
    0x31eb5d1c5ff23b69,
];
const LIMINE_RSDP_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0xc5e77b6b397e7b43,
    0x27637845accdcf3c,
];
const LIMINE_BOOTLOADER_INFO_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0xf55038d8e2a1202f,
    0x279426fcf5f59740,
];
const LIMINE_KERNEL_ADDRESS_ID: [u64; 4] = [
    LIMINE_COMMON_MAGIC[0],
    LIMINE_COMMON_MAGIC[1],
    0x71ba76863cc55f63,
    0xb2644a48c516a487,
];

const LIMINE_MEMMAP_USABLE: u64 = 0;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct LimineUuid {
    pub a: u32,
    pub b: u16,
    pub c: u16,
    pub d: [u8; 8],
}

#[repr(C)]
pub struct LimineFile {
    pub revision: u64,
    pub address: *const u8,
    pub size: u64,
    pub path: *const u8,
    pub cmdline: *const u8,
    pub media_type: u32,
    pub unused: u32,
    pub tftp_ip: u32,
    pub tftp_port: u32,
    pub partition_index: u32,
    pub mbr_disk_id: u32,
    pub gpt_disk_uuid: LimineUuid,
    pub gpt_part_uuid: LimineUuid,
    pub part_uuid: LimineUuid,
}

#[repr(C)]
pub struct LimineBaseRevision {
    pub revision: [u64; 3],
}

impl LimineBaseRevision {
    pub const fn new() -> Self {
        Self {
            revision: LIMINE_BASE_REVISION_MAGIC,
        }
    }

    pub fn supported(&self) -> bool {
        self.revision[2] == 0
    }
}

#[repr(C)]
pub struct LimineHhdmResponse {
    pub revision: u64,
    pub offset: u64,
}

#[repr(C)]
pub struct LimineHhdmRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineHhdmResponse,
}

impl LimineHhdmRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_HHDM_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineMemmapEntry {
    pub base: u64,
    pub length: u64,
    pub typ: u64,
}

#[repr(C)]
pub struct LimineMemmapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *const *const LimineMemmapEntry,
}

#[repr(C)]
pub struct LimineMemmapRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineMemmapResponse,
}

impl LimineMemmapRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_MEMMAP_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineFramebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
    pub unused: [u8; 7],
    pub edid_size: u64,
    pub edid: *const u8,
    pub mode_count: u64,
    pub modes: *const *const LimineVideoMode,
}

#[repr(C)]
pub struct LimineVideoMode {
    pub pitch: u64,
    pub width: u64,
    pub height: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[repr(C)]
pub struct LimineFramebufferResponse {
    pub revision: u64,
    pub framebuffer_count: u64,
    pub framebuffers: *const *const LimineFramebuffer,
}

#[repr(C)]
pub struct LimineFramebufferRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineFramebufferResponse,
}

impl LimineFramebufferRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_FRAMEBUFFER_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineBootloaderInfoResponse {
    pub revision: u64,
    pub name: *const c_char,
    pub version: *const c_char,
}

#[repr(C)]
pub struct LimineBootloaderInfoRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineBootloaderInfoResponse,
}

impl LimineBootloaderInfoRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_BOOTLOADER_INFO_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineKernelAddressResponse {
    pub revision: u64,
    pub physical_base: u64,
    pub virtual_base: u64,
}

#[repr(C)]
pub struct LimineKernelAddressRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineKernelAddressResponse,
}

impl LimineKernelAddressRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_KERNEL_ADDRESS_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineKernelFileResponse {
    pub revision: u64,
    pub kernel_file: *const LimineFile,
}

#[repr(C)]
pub struct LimineKernelFileRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineKernelFileResponse,
}

impl LimineKernelFileRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_KERNEL_FILE_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

#[repr(C)]
pub struct LimineRsdpResponse {
    pub revision: u64,
    pub address: *const c_void,
}

#[repr(C)]
pub struct LimineRsdpRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *const LimineRsdpResponse,
}

impl LimineRsdpRequest {
    pub const fn new() -> Self {
        Self {
            id: LIMINE_RSDP_ID,
            revision: 0,
            response: ptr::null(),
        }
    }
}

unsafe impl Sync for LimineHhdmResponse {}
unsafe impl Sync for LimineMemmapResponse {}
unsafe impl Sync for LimineFramebufferResponse {}
unsafe impl Sync for LimineBootloaderInfoResponse {}
unsafe impl Sync for LimineKernelAddressResponse {}
unsafe impl Sync for LimineKernelFileResponse {}
unsafe impl Sync for LimineRsdpResponse {}
unsafe impl Sync for LimineHhdmRequest {}
unsafe impl Sync for LimineMemmapRequest {}
unsafe impl Sync for LimineFramebufferRequest {}
unsafe impl Sync for LimineBootloaderInfoRequest {}
unsafe impl Sync for LimineKernelAddressRequest {}
unsafe impl Sync for LimineKernelFileRequest {}
unsafe impl Sync for LimineRsdpRequest {}
unsafe impl Send for LimineHhdmRequest {}
unsafe impl Send for LimineMemmapRequest {}
unsafe impl Send for LimineFramebufferRequest {}
unsafe impl Send for LimineBootloaderInfoRequest {}
unsafe impl Send for LimineKernelAddressRequest {}
unsafe impl Send for LimineKernelFileRequest {}
unsafe impl Send for LimineRsdpRequest {}

#[used]
#[unsafe(link_section = ".limine_requests_start_marker")]
static LIMINE_REQUESTS_START_MARKER: [u64; 1] = [0];

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: LimineBaseRevision = LimineBaseRevision::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM_REQUEST: LimineHhdmRequest = LimineHhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP_REQUEST: LimineMemmapRequest = LimineMemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER_REQUEST: LimineFramebufferRequest = LimineFramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static KERNEL_FILE_REQUEST: LimineKernelFileRequest = LimineKernelFileRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP_REQUEST: LimineRsdpRequest = LimineRsdpRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BOOTLOADER_INFO_REQUEST: LimineBootloaderInfoRequest = LimineBootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static KERNEL_ADDRESS_REQUEST: LimineKernelAddressRequest = LimineKernelAddressRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_end_marker")]
static LIMINE_REQUESTS_END_MARKER: [u64; 1] = [0];

pub type FramebufferInfo = slopos_lib::FramebufferInfo;

#[derive(Clone, Copy, Debug)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub typ: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub hhdm_offset: u64,
    pub cmdline: Option<&'static str>,
    pub framebuffer: Option<FramebufferInfo>,
    pub memmap_entries: u64,
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
    framebuffer: Option<FramebufferInfo>,
    hhdm_offset: u64,
    kernel_phys_base: u64,
    kernel_virt_base: u64,
    rsdp_phys_addr: u64,
    rsdp_virt_addr: u64,
    memmap: Option<&'static LimineMemmapResponse>,
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
            memmap: None,
            cmdline: None,
            cmdline_ptr: ptr::null(),
            flags: SystemFlags::new(),
        }
    }
}

struct SystemInfoCell(UnsafeCell<SystemInfo>);

unsafe impl Sync for SystemInfoCell {}

static SYSTEM_INFO: SystemInfoCell = SystemInfoCell(UnsafeCell::new(SystemInfo::new()));

#[allow(static_mut_refs)]
fn sysinfo_mut() -> &'static mut SystemInfo {
    unsafe { &mut *SYSTEM_INFO.0.get() }
}

fn sysinfo() -> &'static SystemInfo {
    unsafe { &*SYSTEM_INFO.0.get() }
}

pub fn ensure_base_revision() {
    if !BASE_REVISION.supported() {
        panic!("Limine base revision not supported");
    }
}

#[unsafe(no_mangle)]
pub fn init_limine_protocol() -> i32 {
    if !BASE_REVISION.supported() {
        klog_info!("ERROR: Limine base revision not supported!");
        return -1;
    }

    let info = sysinfo_mut();

    unsafe {
        if let Some(resp) = BOOTLOADER_INFO_REQUEST.response.as_ref() {
            if !resp.name.is_null() && !resp.version.is_null() {
                klog_debug!(
                    "Bootloader: {} version {}",
                    CStr::from_ptr(resp.name as *const c_char)
                        .to_str()
                        .unwrap_or("<invalid utf-8>"),
                    CStr::from_ptr(resp.version as *const c_char)
                        .to_str()
                        .unwrap_or("<invalid utf-8>")
                );
            }
        }
    }

    unsafe {
        if let Some(hhdm) = HHDM_REQUEST.response.as_ref() {
            info.hhdm_offset = hhdm.offset;
            info.flags.hhdm_available = true;
            klog_debug!("HHDM offset: 0x{:x}", hhdm.offset);
        }
    }

    unsafe {
        if let Some(ka) = KERNEL_ADDRESS_REQUEST.response.as_ref() {
            info.kernel_phys_base = ka.physical_base;
            info.kernel_virt_base = ka.virtual_base;
            klog_debug!(
                "Kernel phys base: 0x{:x} virt base: 0x{:x}",
                ka.physical_base,
                ka.virtual_base
            );
        }
    }

    unsafe {
        if let Some(rsdp) = RSDP_REQUEST.response.as_ref() {
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
    }

    unsafe {
        if let Some(kf_resp) = KERNEL_FILE_REQUEST.response.as_ref() {
            if let Some(kernel_file) = kf_resp.kernel_file.as_ref() {
                if !kernel_file.cmdline.is_null() {
                    let raw = kernel_file.cmdline as *const c_char;
                    info.cmdline_ptr = raw;
                    info.cmdline = CStr::from_ptr(raw).to_str().ok();
                    info.flags.kernel_cmdline_available = true;

                    if let Some(cmd) = info.cmdline {
                        if !cmd.is_empty() {
                            klog_debug!("Kernel cmdline: {}", cmd);
                        } else {
                            klog_debug!("Kernel cmdline: <empty>");
                        }
                    }
                }
            }
        }
    }

    unsafe {
        if let Some(memmap) = MEMMAP_REQUEST.response.as_ref() {
            let mut total = 0u64;
            let mut available = 0u64;

            for idx in 0..memmap.entry_count {
                let entry_ptr = *memmap.entries.add(idx as usize);
                if let Some(entry) = entry_ptr.as_ref() {
                    total = total.saturating_add(entry.length);
                    if entry.typ == LIMINE_MEMMAP_USABLE {
                        available = available.saturating_add(entry.length);
                    }
                }
            }

            info.total_memory = total;
            info.available_memory = available;
            info.memmap = Some(memmap);
            info.flags.memmap_available = true;

            klog_debug!(
                "Memory map: {} entries, total {} MB, available {} MB",
                memmap.entry_count,
                total / (1024 * 1024),
                available / (1024 * 1024)
            );
        } else {
            klog_info!("WARNING: No memory map available from Limine");
        }
    }

    unsafe {
        if let Some(fb_resp) = FRAMEBUFFER_REQUEST.response.as_ref() {
            if fb_resp.framebuffer_count > 0 {
                if let Some(fb) = (*fb_resp.framebuffers).as_ref() {
                    info.framebuffer = Some(FramebufferInfo {
                        address: fb.address,
                        width: fb.width,
                        height: fb.height,
                        pitch: fb.pitch,
                        bpp: fb.bpp,
                    });
                    info.flags.framebuffer_available = true;

                    klog_debug!(
                        "Framebuffer: {}x{} @ {} bpp",
                        fb.width,
                        fb.height,
                        fb.bpp
                    );
                    klog_debug!(
                        "Framebuffer addr: 0x{:x} pitch: {}",
                        fb.address as u64,
                        fb.pitch
                    );
                }
            } else {
                klog_info!("WARNING: No framebuffer provided by Limine");
                info.flags.framebuffer_available = false;
            }
        } else {
            klog_info!("WARNING: No framebuffer response from Limine");
            info.flags.framebuffer_available = false;
        }
    }

    0
}

pub fn boot_info() -> BootInfo {
    let info = sysinfo();
    BootInfo {
        hhdm_offset: info.hhdm_offset,
        cmdline: info.cmdline,
        framebuffer: info.framebuffer,
        memmap_entries: info.memmap.map(|m| m.entry_count).unwrap_or(0),
    }
}

#[unsafe(no_mangle)]
pub fn get_framebuffer_info(
    addr: *mut u64,
    width: *mut u32,
    height: *mut u32,
    pitch: *mut u32,
    bpp: *mut u8,
) -> i32 {
    let info = sysinfo();
    if let Some(fb) = info.framebuffer {
        unsafe {
            if !addr.is_null() {
                *addr = fb.address as u64;
            }
            if !width.is_null() {
                *width = fb.width as u32;
            }
            if !height.is_null() {
                *height = fb.height as u32;
            }
            if !pitch.is_null() {
                *pitch = fb.pitch as u32;
            }
            if !bpp.is_null() {
                *bpp = fb.bpp as u8;
            }
        }
        1
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub fn is_framebuffer_available() -> i32 {
    sysinfo().flags.framebuffer_available as i32
}

#[unsafe(no_mangle)]
pub fn get_total_memory() -> u64 {
    sysinfo().total_memory
}

#[unsafe(no_mangle)]
pub fn get_available_memory() -> u64 {
    sysinfo().available_memory
}

#[unsafe(no_mangle)]
pub fn is_memory_map_available() -> i32 {
    sysinfo().flags.memmap_available as i32
}

// Exported extern "C" functions for mm crate to call via extern "C" blocks
#[unsafe(no_mangle)]
pub extern "C" fn get_hhdm_offset() -> u64 {
    sysinfo().hhdm_offset
}

#[unsafe(no_mangle)]
pub extern "C" fn is_hhdm_available() -> i32 {
    sysinfo().flags.hhdm_available as i32
}

// Rust function wrappers for callback registration
pub fn get_hhdm_offset_rust() -> u64 {
    get_hhdm_offset()
}

pub fn is_hhdm_available_rust() -> i32 {
    is_hhdm_available()
}

#[unsafe(no_mangle)]
pub fn get_kernel_phys_base() -> u64 {
    sysinfo().kernel_phys_base
}

#[unsafe(no_mangle)]
pub fn get_kernel_virt_base() -> u64 {
    sysinfo().kernel_virt_base
}

#[unsafe(no_mangle)]
pub fn get_kernel_cmdline() -> *const c_char {
    sysinfo().cmdline_ptr
}

pub fn kernel_cmdline_str() -> Option<&'static str> {
    sysinfo().cmdline
}

#[unsafe(no_mangle)]
pub fn limine_get_memmap_response() -> *const LimineMemmapResponse {
    sysinfo()
        .memmap
        .map(|m| m as *const _)
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub fn limine_get_hhdm_response() -> *const LimineHhdmResponse {
    HHDM_REQUEST.response
}

#[unsafe(no_mangle)]
pub fn is_rsdp_available() -> i32 {
    sysinfo().flags.rsdp_available as i32
}

#[unsafe(no_mangle)]
pub fn get_rsdp_phys_address() -> u64 {
    sysinfo().rsdp_phys_addr
}

#[unsafe(no_mangle)]
pub fn get_rsdp_address() -> *const c_void {
    let info = sysinfo();
    if !info.flags.rsdp_available {
        return ptr::null();
    }

    if info.rsdp_virt_addr != 0 {
        info.rsdp_virt_addr as *const c_void
    } else if info.flags.hhdm_available && info.rsdp_phys_addr != 0 {
        (info.rsdp_phys_addr + info.hhdm_offset) as *const c_void
    } else {
        info.rsdp_phys_addr as *const c_void
    }
}

pub fn get_memmap_entry(index: usize) -> Option<MemmapEntry> {
    let memmap = sysinfo().memmap?;
    if index >= memmap.entry_count as usize {
        return None;
    }
    unsafe {
        let entry_ptr = *memmap.entries.add(index);
        entry_ptr.as_ref().map(|entry| MemmapEntry {
            base: entry.base,
            length: entry.length,
            typ: entry.typ,
        })
    }
}
