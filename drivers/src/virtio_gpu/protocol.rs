//! VirtIO GPU 2D wire protocol.
//!
//! Every struct is `#[repr(C)]` + `#[derive(Pod)]` so it can be written into
//! and read out of DMA pages via `Frame::{write_at, read_at}` with no `unsafe`
//! in the driver crate. Explicit `padding` fields make each layout exhaustively
//! defined; backing pages are zeroed before use, so padding is zero on the wire.
//!
//! Only the 2D command set is modelled. Response payloads with trailing arrays
//! (`GET_DISPLAY_INFO`, `GET_EDID`) are read field-by-field by the driver, so
//! this module defines only their fixed-size leading structs.

use slopos_ostd::Pod;

// ── PCI device IDs ──────────────────────────────────────────────────────────

/// Transitional (legacy) virtio-gpu PCI device id.
pub const VIRTIO_GPU_ID_LEGACY: u16 = 0x1010;
/// Modern (non-transitional) virtio-gpu PCI device id.
pub const VIRTIO_GPU_ID_MODERN: u16 = 0x1050;

// ── Feature bits ────────────────────────────────────────────────────────────

/// `VIRTIO_GPU_F_EDID` (device feature bit 1): the device supports
/// `VIRTIO_GPU_CMD_GET_EDID`. Requested as optional.
pub const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

// ── Device configuration space (device_cfg MMIO offsets) ────────────────────

/// `num_scanouts` field in `struct virtio_gpu_config`.
pub const VIRTIO_GPU_CFG_NUM_SCANOUTS: usize = 8;

/// Maximum scanouts the spec allows (size of the display-info pmodes array).
pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

// ── Control command types ───────────────────────────────────────────────────

pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
pub const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
pub const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;

// ── Cursor command types (cursor queue) ─────────────────────────────────────

pub const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
pub const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

// ── Response types ──────────────────────────────────────────────────────────

pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
/// First error response code; anything `>=` this is a device-reported error.
pub const VIRTIO_GPU_RESP_ERR_BASE: u32 = 0x1200;

/// `true` if a control-queue response header reports success (any `OK_*`).
#[inline]
pub fn is_ok_resp(resp_type: u32) -> bool {
    (VIRTIO_GPU_RESP_OK_NODATA..VIRTIO_GPU_RESP_ERR_BASE).contains(&resp_type)
}

// ── 2D pixel formats (host byte-order naming) ───────────────────────────────

/// In-memory byte order B,G,R,A — matches SlopOS `PixelFormat::Argb8888`.
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
/// In-memory byte order B,G,R,X — matches SlopOS `PixelFormat::Xrgb8888`.
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// Map a SlopOS framebuffer pixel format to the matching virtio-gpu 2D format.
///
/// Only the two 32-bit BGR-order formats the kernel framebuffer ever uses are
/// mapped exactly; anything else falls back to opaque `B8G8R8X8` (the safe
/// default QEMU's virtio-gpu always accepts).
#[inline]
pub fn format_from_pixel(format: slopos_abi::PixelFormat) -> u32 {
    use slopos_abi::PixelFormat;
    match format {
        PixelFormat::Argb8888 => VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Xrgb8888 => VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
        _ => VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
    }
}

// ── Wire structs ────────────────────────────────────────────────────────────

/// `struct virtio_gpu_ctrl_hdr` — leads every control and cursor command and
/// every response (24 bytes).
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
    /// A header for an unfenced command of `type_`.
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

/// `struct virtio_gpu_rect` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// `struct virtio_gpu_resource_create_2d`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceCreate2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// `struct virtio_gpu_resource_unref`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceUnref {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_set_scanout`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuSetScanout {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub scanout_id: u32,
    pub resource_id: u32,
}

/// `struct virtio_gpu_resource_flush`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceFlush {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_transfer_to_host_2d`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuTransferToHost2d {
    pub hdr: VirtioGpuCtrlHdr,
    pub r: VirtioGpuRect,
    pub offset: u64,
    pub resource_id: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_resource_attach_backing` — followed on the wire by
/// `nr_entries` × [`VirtioGpuMemEntry`].
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuResourceAttachBacking {
    pub hdr: VirtioGpuCtrlHdr,
    pub resource_id: u32,
    pub nr_entries: u32,
}

/// `struct virtio_gpu_mem_entry`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuMemEntry {
    pub addr: u64,
    pub length: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_get_edid`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuGetEdid {
    pub hdr: VirtioGpuCtrlHdr,
    pub scanout: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_cursor_pos`.
#[repr(C)]
#[derive(Clone, Copy, Pod)]
pub struct VirtioGpuCursorPos {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub padding: u32,
}

/// `struct virtio_gpu_update_cursor` (also used for `MOVE_CURSOR`).
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

// ── Offsets used to read trailing response fields piecewise ─────────────────

/// Byte offset of `pmodes[0].rect` inside a `virtio_gpu_resp_display_info`.
pub const DISPLAY_INFO_PMODE0_RECT: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
/// Byte offset of `pmodes[0].enabled` inside a `virtio_gpu_resp_display_info`.
pub const DISPLAY_INFO_PMODE0_ENABLED: usize =
    DISPLAY_INFO_PMODE0_RECT + core::mem::size_of::<VirtioGpuRect>();
/// Total size of `virtio_gpu_resp_display_info` (hdr + 16 × {rect,enabled,flags}).
pub const DISPLAY_INFO_RESP_LEN: usize = core::mem::size_of::<VirtioGpuCtrlHdr>()
    + VIRTIO_GPU_MAX_SCANOUTS * (core::mem::size_of::<VirtioGpuRect>() + 2 * 4);

/// Byte offset of the `size` field inside a `virtio_gpu_resp_edid`.
pub const EDID_RESP_SIZE_OFFSET: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
/// Byte offset of the `edid[]` blob inside a `virtio_gpu_resp_edid`.
pub const EDID_RESP_BLOB_OFFSET: usize = EDID_RESP_SIZE_OFFSET + 2 * 4;
/// `edid[]` blob length in `virtio_gpu_resp_edid`.
pub const EDID_BLOB_LEN: usize = 1024;
/// Total size of `virtio_gpu_resp_edid`.
pub const EDID_RESP_LEN: usize = EDID_RESP_BLOB_OFFSET + EDID_BLOB_LEN;

// ── Compile-time layout assertions (spec-mandated sizes) ────────────────────

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
    // The largest response (display-info) must fit in the response half of a
    // single 4 KiB DMA page (see `RESP_OFFSET` in the driver).
    assert!(DISPLAY_INFO_RESP_LEN <= 2048);
    assert!(EDID_RESP_LEN <= 2048);
};
