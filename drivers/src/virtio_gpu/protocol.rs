//! VirtIO GPU 2D wire protocol. Every struct is `#[repr(C)]` + `Pod` so the
//! driver crate can move it through DMA pages with no `unsafe`; explicit
//! `padding` fields make each layout total, and backing pages are zeroed, so
//! padding is zero on the wire.
//!
//! Only the 2D command set is modelled. Responses with trailing arrays are read
//! field-by-field, so only their fixed-size leading structs appear here.

use slopos_ostd::Pod;

pub const VIRTIO_GPU_ID_LEGACY: u16 = 0x1010;
pub const VIRTIO_GPU_ID_MODERN: u16 = 0x1050;

/// Device feature bit 1: `VIRTIO_GPU_CMD_GET_EDID` is supported. Optional.
pub const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

/// MMIO offset of `num_scanouts` in `struct virtio_gpu_config`.
pub const VIRTIO_GPU_CFG_NUM_SCANOUTS: usize = 8;

/// Spec maximum, and the size of the display-info pmodes array.
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;

pub const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
pub const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
/// First error response code; anything `>=` this is a device-reported error.
pub const VIRTIO_GPU_RESP_ERR_BASE: u32 = 0x1200;

#[inline]
pub fn is_ok_resp(resp_type: u32) -> bool {
    (VIRTIO_GPU_RESP_OK_NODATA..VIRTIO_GPU_RESP_ERR_BASE).contains(&resp_type)
}

/// In-memory byte order B,G,R,A — matches SlopOS `PixelFormat::Argb8888`.
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
/// In-memory byte order B,G,R,X — matches SlopOS `PixelFormat::Xrgb8888`.
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// Anything but the two 32-bit BGR-order formats the kernel framebuffer uses
/// falls back to opaque `B8G8R8X8`, which QEMU's virtio-gpu always accepts.
#[inline]
pub fn format_from_pixel(format: slopos_abi::PixelFormat) -> u32 {
    use slopos_abi::PixelFormat;
    match format {
        PixelFormat::Argb8888 => VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Xrgb8888 => VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
        _ => VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
    }
}

/// Leads every control and cursor command, and every response.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuCtrlHdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
    pub padding: [u8; 3],
}

impl VirtioGpuCtrlHdr {
    /// An unfenced command header.
    #[inline]
    pub const fn cmd(type_: u32) -> Self {
        Self {
            type_,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            ring_idx: 0,
            padding: [0; 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceCreate2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceUnref {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuSetScanout {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceFlush {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub resource_id: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuTransferToHost2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

/// Followed on the wire by `nr_entries` × [`VirtioGpuMemEntry`].
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceAttachBacking {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub nr_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuMemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuGetEdid {
    pub hdr: VirtioGpuCtrlHdr,
    pub scanout: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuCursorPos {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub padding: u32,
}

/// Also carries `MOVE_CURSOR`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuUpdateCursor {
    pub hdr: VirtioGpuCtrlHdr,
    pub pos: VirtioGpuCursorPos,
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub padding: u32,
}

/// Offsets into `virtio_gpu_resp_display_info`, whose trailing pmodes array is
/// read field-by-field rather than modelled as a struct.
pub const DISPLAY_INFO_PMODE0_RECT: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
pub const DISPLAY_INFO_PMODE0_ENABLED: usize =
    DISPLAY_INFO_PMODE0_RECT + core::mem::size_of::<VirtioGpuRect>();
pub const DISPLAY_INFO_RESP_LEN: usize = core::mem::size_of::<VirtioGpuCtrlHdr>()
    + VIRTIO_GPU_MAX_SCANOUTS * (core::mem::size_of::<VirtioGpuRect>() + 2 * 4);

/// Offsets into `virtio_gpu_resp_edid`.
pub const EDID_RESP_SIZE_OFFSET: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
pub const EDID_RESP_BLOB_OFFSET: usize = EDID_RESP_SIZE_OFFSET + 2 * 4;
pub const EDID_BLOB_LEN: usize = 1024;
pub const EDID_RESP_LEN: usize = EDID_RESP_BLOB_OFFSET + EDID_BLOB_LEN;

const _: () = {
    use core::mem::size_of;
    assert!(size_of::<VirtioGpuCtrlHdr>() == 24);
    assert!(size_of::<VirtioGpuRect>() == 16);
    assert!(size_of::<VirtioGpuResourceCreate2d>() == 40);
    assert!(size_of::<VirtioGpuResourceUnref>() == 32);
    assert!(size_of::<VirtioGpuSetScanout>() == 48);
    assert!(size_of::<VirtioGpuResourceFlush>() == 48);
    assert!(size_of::<VirtioGpuTransferToHost2d>() == 56);
    assert!(size_of::<VirtioGpuResourceAttachBacking>() == 32);
    assert!(size_of::<VirtioGpuMemEntry>() == 16);
    assert!(size_of::<VirtioGpuGetEdid>() == 32);
    assert!(size_of::<VirtioGpuCursorPos>() == 16);
    assert!(size_of::<VirtioGpuUpdateCursor>() == 56);
    // Responses must fit the 2 KiB response half of the driver's single DMA page.
    assert!(DISPLAY_INFO_RESP_LEN <= 2048);
    assert!(EDID_RESP_LEN <= 2048);
};
