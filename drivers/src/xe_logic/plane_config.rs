//! Decode/encode of the primary display plane's live configuration: `PLANE_CTL`,
//! `PLANE_SIZE`, `PLANE_POS` and the linear `PLANE_STRIDE` unit.
//!
//! Firmware may leave the plane Y-tiled and render-compressed while SlopOS scans
//! out of a linear framebuffer, hence the decode / encode-repoint split.

use slopos_abi::PixelFormat;

use super::regs::{
    PLANE_CTL_COLOR_ORDER_RGBX, PLANE_CTL_ENABLE, PLANE_CTL_FORMAT_MASK, PLANE_CTL_FORMAT_XRGB8888,
    PLANE_CTL_RENDER_DECOMP_ENABLE, PLANE_CTL_TILING_LINEAR, PLANE_CTL_TILING_MASK,
    PLANE_CTL_TILING_X, PLANE_CTL_TILING_Y, PLANE_CTL_TILING_YF,
    PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE, reg_field_get, reg_field_set,
};

/// Linear surfaces count `PLANE_STRIDE` in 64-byte units; tiled surfaces use a
/// different unit (X-tile 512 B, Y-tile 128 B), so this is linear-only.
pub const LINEAR_STRIDE_UNIT_BYTES: u32 = 64;

// Derived from the register map, and `const` so they work as match patterns.
const FORMAT_FIELD_RGB8888: u32 = reg_field_get(PLANE_CTL_FORMAT_MASK, PLANE_CTL_FORMAT_XRGB8888);
const TILING_FIELD_LINEAR: u32 = reg_field_get(PLANE_CTL_TILING_MASK, PLANE_CTL_TILING_LINEAR);
const TILING_FIELD_X: u32 = reg_field_get(PLANE_CTL_TILING_MASK, PLANE_CTL_TILING_X);
const TILING_FIELD_Y: u32 = reg_field_get(PLANE_CTL_TILING_MASK, PLANE_CTL_TILING_Y);
const TILING_FIELD_YF: u32 = reg_field_get(PLANE_CTL_TILING_MASK, PLANE_CTL_TILING_YF);

/// Surface memory layout encoded in the `PLANE_CTL` tiling field (bits [12:10]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tiling {
    Linear,
    XTiled,
    YTiled,
    YfTiled,
    Unknown,
}

impl Tiling {
    pub const fn from_field(field: u32) -> Self {
        match field {
            TILING_FIELD_LINEAR => Self::Linear,
            TILING_FIELD_X => Self::XTiled,
            TILING_FIELD_Y => Self::YTiled,
            TILING_FIELD_YF => Self::YfTiled,
            _ => Self::Unknown,
        }
    }

    /// `Unknown` is not a writable layout, so it falls back to linear — the only
    /// layout the repoint path ever programs.
    pub const fn to_field(self) -> u32 {
        match self {
            Self::Linear => TILING_FIELD_LINEAR,
            Self::XTiled => TILING_FIELD_X,
            Self::YTiled => TILING_FIELD_Y,
            Self::YfTiled => TILING_FIELD_YF,
            Self::Unknown => TILING_FIELD_LINEAR,
        }
    }
}

/// Channel order selected by `PLANE_CTL` bit 20.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorOrder {
    /// Bit clear: BGRX in memory, the byte order of a little-endian ARGB/XRGB
    /// framebuffer, and the firmware default.
    Bgrx,
    /// Bit set: RGBX in memory.
    Rgbx,
}

impl ColorOrder {
    pub const fn from_ctl(plane_ctl: u32) -> Self {
        if plane_ctl & PLANE_CTL_COLOR_ORDER_RGBX != 0 {
            Self::Rgbx
        } else {
            Self::Bgrx
        }
    }

    pub const fn ctl_bit(self) -> u32 {
        match self {
            Self::Rgbx => PLANE_CTL_COLOR_ORDER_RGBX,
            Self::Bgrx => 0,
        }
    }
}

/// Hardware pixel-format code in the `PLANE_CTL` format field (bits [27:24]).
/// The silicon has one 8:8:8:8 code for every 32-bit RGB framebuffer — alpha
/// participation and channel order come from the blend mode and [`ColorOrder`] —
/// so all the 8888 ABI formats collapse onto a single code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneFormat {
    /// The 0b0100 8:8:8:8 code (XRGB / ARGB / XBGR / ABGR).
    Rgb8888,
    /// An unmodelled format-field value, kept verbatim.
    Unknown(u32),
}

impl PlaneFormat {
    pub const fn from_field(field: u32) -> Self {
        match field {
            FORMAT_FIELD_RGB8888 => Self::Rgb8888,
            other => Self::Unknown(other),
        }
    }

    pub const fn to_field(self) -> u32 {
        match self {
            Self::Rgb8888 => FORMAT_FIELD_RGB8888,
            Self::Unknown(field) => field & 0xf,
        }
    }

    /// `None` for the 24-bit packed formats the plane cannot present directly.
    pub const fn from_pixel_format(fmt: PixelFormat) -> Option<Self> {
        match fmt {
            PixelFormat::Argb8888
            | PixelFormat::Xrgb8888
            | PixelFormat::Rgba8888
            | PixelFormat::Bgra8888 => Some(Self::Rgb8888),
            PixelFormat::Rgb888 | PixelFormat::Bgr888 => None,
        }
    }

    /// The canonical opaque little-endian ABI format: the code alone records
    /// neither alpha participation nor channel order.
    pub const fn to_pixel_format(self) -> Option<PixelFormat> {
        match self {
            Self::Rgb8888 => Some(PixelFormat::Xrgb8888),
            Self::Unknown(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneCtl {
    pub enable: bool,
    pub format: PlaneFormat,
    pub tiling: Tiling,
    pub color_order: ColorOrder,
    pub render_decompressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneConfig {
    pub enable: bool,
    pub format: PlaneFormat,
    pub tiling: Tiling,
    pub color_order: ColorOrder,
    pub render_decompressed: bool,
    /// Raw `PLANE_STRIDE` register value; its unit depends on `tiling`.
    pub stride_reg: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub surf_ggtt: u32,
}

impl PlaneConfig {
    pub const fn from_registers(
        plane_ctl: u32,
        plane_size: u32,
        plane_pos: u32,
        plane_stride: u32,
        plane_surf: u32,
    ) -> Self {
        let ctl = decode_ctl(plane_ctl);
        let (width, height) = decode_size(plane_size);
        let (x, y) = decode_pos(plane_pos);
        Self {
            enable: ctl.enable,
            format: ctl.format,
            tiling: ctl.tiling,
            color_order: ctl.color_order,
            render_decompressed: ctl.render_decompressed,
            stride_reg: plane_stride,
            width,
            height,
            x,
            y,
            surf_ggtt: plane_surf,
        }
    }
}

pub const fn decode_ctl(plane_ctl: u32) -> PlaneCtl {
    PlaneCtl {
        enable: plane_ctl & PLANE_CTL_ENABLE != 0,
        format: PlaneFormat::from_field(reg_field_get(PLANE_CTL_FORMAT_MASK, plane_ctl)),
        tiling: Tiling::from_field(reg_field_get(PLANE_CTL_TILING_MASK, plane_ctl)),
        color_order: ColorOrder::from_ctl(plane_ctl),
        render_decompressed: plane_ctl & PLANE_CTL_RENDER_DECOMP_ENABLE != 0,
    }
}

/// The linear repoint target: requested format and channel order, linear tiling,
/// render-decompression cleared, YUV range correction disabled as the firmware
/// plane leaves it.
pub const fn encode_ctl_linear(format: PlaneFormat, color_order: ColorOrder, enable: bool) -> u32 {
    let enable_bit = if enable { PLANE_CTL_ENABLE } else { 0 };
    // Linear tiling and render-decompression are all-zero bits, hence absent here.
    reg_field_set(PLANE_CTL_FORMAT_MASK, format.to_field())
        | color_order.ctl_bit()
        | PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE
        | enable_bit
}

/// Width in the low half, height in the high half, each stored as `value - 1`.
pub const fn decode_size(plane_size: u32) -> (u32, u32) {
    let width = (plane_size & 0xffff) + 1;
    let height = ((plane_size >> 16) & 0xffff) + 1;
    (width, height)
}

pub const fn encode_size(width: u32, height: u32) -> u32 {
    ((height.wrapping_sub(1) & 0xffff) << 16) | (width.wrapping_sub(1) & 0xffff)
}

/// X in the low half, Y in the high half.
pub const fn decode_pos(plane_pos: u32) -> (u32, u32) {
    let x = plane_pos & 0xffff;
    let y = (plane_pos >> 16) & 0xffff;
    (x, y)
}

pub const fn encode_pos(x: u32, y: u32) -> u32 {
    ((y & 0xffff) << 16) | (x & 0xffff)
}

/// The caller guarantees a 64-byte-aligned pitch.
pub const fn linear_stride_reg(pitch_bytes: u32) -> u32 {
    pitch_bytes / LINEAR_STRIDE_UNIT_BYTES
}

pub const fn linear_stride_bytes(stride_reg: u32) -> u32 {
    stride_reg * LINEAR_STRIDE_UNIT_BYTES
}

/// `None` for formats the plane cannot scan out directly.
pub const fn pixel_format_to_plane(fmt: PixelFormat) -> Option<u32> {
    match PlaneFormat::from_pixel_format(fmt) {
        Some(plane_format) => Some(reg_field_set(
            PLANE_CTL_FORMAT_MASK,
            plane_format.to_field(),
        )),
        None => None,
    }
}

/// `None` for an unmodelled code.
pub const fn plane_to_pixel_format(plane_format_bits: u32) -> Option<PixelFormat> {
    PlaneFormat::from_field(reg_field_get(PLANE_CTL_FORMAT_MASK, plane_format_bits))
        .to_pixel_format()
}

// Compile-time proof that each encode/decode pair inverts.
const _: () = {
    assert!(decode_size(encode_size(1920, 1080)).0 == 1920);
    assert!(decode_size(encode_size(1920, 1080)).1 == 1080);
    assert!(encode_size(1, 1) == 0);
    assert!(decode_size(0).0 == 1 && decode_size(0).1 == 1);

    assert!(decode_pos(encode_pos(64, 48)).0 == 64);
    assert!(decode_pos(encode_pos(64, 48)).1 == 48);

    assert!(linear_stride_reg(1920 * 4) == 120);
    assert!(linear_stride_bytes(linear_stride_reg(1920 * 4)) == 1920 * 4);

    // 0x94009000 is the live DSPACNTR left by the firmware modeset.
    let live = decode_ctl(0x9400_9000);
    assert!(live.enable);
    assert!(live.format.to_field() == FORMAT_FIELD_RGB8888);
    assert!(matches!(live.tiling, Tiling::YTiled));
    assert!(live.render_decompressed);
    assert!(matches!(live.color_order, ColorOrder::Bgrx));

    let repoint = encode_ctl_linear(PlaneFormat::Rgb8888, ColorOrder::Bgrx, true);
    let back = decode_ctl(repoint);
    assert!(back.enable);
    assert!(back.format.to_field() == FORMAT_FIELD_RGB8888);
    assert!(matches!(back.tiling, Tiling::Linear));
    assert!(!back.render_decompressed);
    assert!(matches!(back.color_order, ColorOrder::Bgrx));

    let rgbx = decode_ctl(encode_ctl_linear(
        PlaneFormat::Rgb8888,
        ColorOrder::Rgbx,
        false,
    ));
    assert!(!rgbx.enable);
    assert!(matches!(rgbx.color_order, ColorOrder::Rgbx));

    assert!(matches!(Tiling::from_field(TILING_FIELD_X), Tiling::XTiled));
    assert!(matches!(
        Tiling::from_field(TILING_FIELD_YF),
        Tiling::YfTiled
    ));
    assert!(Tiling::from_field(Tiling::XTiled.to_field()).to_field() == TILING_FIELD_X);

    assert!(pixel_format_to_plane(PixelFormat::Xrgb8888).is_some());
    assert!(pixel_format_to_plane(PixelFormat::Argb8888).is_some());
    assert!(pixel_format_to_plane(PixelFormat::Rgb888).is_none());
    assert!(plane_to_pixel_format(PLANE_CTL_FORMAT_XRGB8888).is_some());
    assert!(plane_to_pixel_format(reg_field_set(PLANE_CTL_FORMAT_MASK, 0xf)).is_none());
};
