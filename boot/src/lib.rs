#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

use core::{ffi::{CStr, c_char}, ptr, str};

const LIMINE_COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];
const LIMINE_BASE_REVISION_MAGIC: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 0];
const LIMINE_HHDM_ID: [u64; 4] = [LIMINE_COMMON_MAGIC[0], LIMINE_COMMON_MAGIC[1], 0x48dcf1cb8ad2b852, 0x63984e959a98244b];
const LIMINE_MEMMAP_ID: [u64; 4] = [LIMINE_COMMON_MAGIC[0], LIMINE_COMMON_MAGIC[1], 0x67cf3d9d378a806f, 0xe304acdfc50c3c62];
const LIMINE_FRAMEBUFFER_ID: [u64; 4] = [LIMINE_COMMON_MAGIC[0], LIMINE_COMMON_MAGIC[1], 0x9d5827dcd881dd75, 0xa3148604f6fab11b];
const LIMINE_KERNEL_FILE_ID: [u64; 4] = [LIMINE_COMMON_MAGIC[0], LIMINE_COMMON_MAGIC[1], 0xad97e90e83f1ed67, 0x31eb5d1c5ff23b69];

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
#[derive(Copy, Clone)]
pub struct LimineUuid {
    pub a: u32,
    pub b: u16,
    pub c: u16,
    pub d: [u8; 8],
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

#[used]
#[link_section = ".limine_requests_start_marker"]
static LIMINE_REQUESTS_START_MARKER: [u64; 1] = [0];

#[used]
#[link_section = ".limine_requests"]
static BASE_REVISION: LimineBaseRevision = LimineBaseRevision::new();

#[used]
#[link_section = ".limine_requests"]
static HHDM_REQUEST: LimineHhdmRequest = LimineHhdmRequest::new();

#[used]
#[link_section = ".limine_requests"]
static MEMMAP_REQUEST: LimineMemmapRequest = LimineMemmapRequest::new();

#[used]
#[link_section = ".limine_requests"]
static FRAMEBUFFER_REQUEST: LimineFramebufferRequest = LimineFramebufferRequest::new();

#[used]
#[link_section = ".limine_requests"]
static KERNEL_FILE_REQUEST: LimineKernelFileRequest = LimineKernelFileRequest::new();

#[used]
#[link_section = ".limine_requests_end_marker"]
static LIMINE_REQUESTS_END_MARKER: [u64; 1] = [0];

#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
}

#[derive(Clone, Copy)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub typ: u64,
}

#[derive(Clone, Copy)]
pub struct BootInfo {
    pub hhdm_offset: u64,
    pub cmdline: Option<&'static str>,
    pub framebuffer: Option<FramebufferInfo>,
    pub memmap_entries: u64,
}

pub fn ensure_base_revision() {
    if !BASE_REVISION.supported() {
        panic!("Limine base revision not supported");
    }
}

pub fn init() -> BootInfo {
    let hhdm_offset = unsafe {
        HHDM_REQUEST
            .response
            .as_ref()
            .map(|resp| resp.offset)
            .unwrap_or(0)
    };

    let cmdline = unsafe {
        KERNEL_FILE_REQUEST
            .response
            .as_ref()
            .and_then(|resp| resp.kernel_file.as_ref())
            .and_then(|file| {
                if file.cmdline.is_null() {
                    return None;
                }
                let cstr = CStr::from_ptr(file.cmdline as *const c_char);
                cstr.to_str().ok()
            })
    };

    let framebuffer = unsafe {
        FRAMEBUFFER_REQUEST
            .response
            .as_ref()
            .and_then(|resp| {
                if resp.framebuffer_count == 0 {
                    return None;
                }
                let fb_ptr = *resp.framebuffers;
                fb_ptr.as_ref().map(|fb| FramebufferInfo {
                    address: fb.address,
                    width: fb.width,
                    height: fb.height,
                    pitch: fb.pitch,
                    bpp: fb.bpp,
                })
            })
    };

    BootInfo {
        hhdm_offset,
        cmdline,
        framebuffer,
        memmap_entries: unsafe {
            MEMMAP_REQUEST
                .response
                .as_ref()
                .map(|resp| resp.entry_count)
                .unwrap_or(0)
        },
    }
}

