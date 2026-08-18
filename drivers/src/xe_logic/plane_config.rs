//! Decode/encode of the primary display plane's live configuration: the
//! `PLANE_CTL` control word, `PLANE_SIZE`, `PLANE_POS` and the linear
//! `PLANE_STRIDE` unit.
//!
//! Firmware may leave the plane Y-tiled and render-compressed, while SlopOS
//! scans out of a linear framebuffer, so the driver reads back whatever it finds
//! and re-points the plane — hence the decode / encode-repoint split here.

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

// Derived from the placed register constants so the register map stays the only
// source of truth, and `const` rather than literals so they work as match patterns.
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
    /// A tiling-field value this driver does not model.
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

    /// Zero for BGRX.
    pub const fn ctl_bit(self) -> u32 {
        match self {
            Self::Rgbx => PLANE_CTL_COLOR_ORDER_RGBX,
            Self::Bgrx => 0,
        }
    }
}

/// Hardware pixel-format code carried in the `PLANE_CTL` format field
/// (bits [27:24]). The silicon uses one 8:8:8:8 code for every 32-bit RGB
/// framebuffer; whether alpha blends and which channel order applies are
/// selected elsewhere (blend mode and [`ColorOrder`]), so all the 8888 ABI
/// formats collapse onto a single code here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneFormat {
    /// The 0b0100 8:8:8:8 code (XRGB / ARGB / XBGR / ABGR).
    Rgb8888,
    /// A format-field value this driver does not model, kept verbatim.
    Unknown(u32),
}

impl PlaneFormat {
    /// Classify the extracted 4-bit format field.
    pub const fn from_field(field: u32) -> Self {
        match field {
            FORMAT_FIELD_RGB8888 => Self::Rgb8888,
            other => Self::Unknown(other),
        }
    }

    /// The 4-bit format field value for this code.
    pub const fn to_field(self) -> u32 {
        match self {
            Self::Rgb8888 => FORMAT_FIELD_RGB8888,
            Self::Unknown(field) => field & 0xf,
        }
    }

    /// The hardware code that can scan out a given ABI pixel format, or `None`
    /// for the 24-bit packed formats the display plane cannot present directly.
    pub const fn from_pixel_format(fmt: PixelFormat) -> Option<Self> {
        match fmt {
            PixelFormat::Argb8888
            | PixelFormat::Xrgb8888
            | PixelFormat::Rgba8888
            | PixelFormat::Bgra8888 => Some(Self::Rgb8888),
            PixelFormat::Rgb888 | PixelFormat::Bgr888 => None,
        }
    }

    /// The canonical opaque little-endian ABI format for this code. The code
    /// alone records neither alpha participation nor channel order, so callers
    /// that need those consult the blend mode and [`ColorOrder`] separately.
    pub const fn to_pixel_format(self) -> Option<PixelFormat> {
        match self {
            Self::Rgb8888 => Some(PixelFormat::Xrgb8888),
            Self::Unknown(_) => None,
        }
    }
}

/// The fields decoded from a `PLANE_CTL` control word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneCtl {
    pub enable: bool,
    pub format: PlaneFormat,
    pub tiling: Tiling,
    pub color_order: ColorOrder,
    pub render_decompressed: bool,
}

/// A snapshot of the live primary-plane configuration, assembled from the plane
/// control, size, position, stride, and surface registers.
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
    /// `PLANE_SURF` GGTT address of the scanout surface.
    pub surf_ggtt: u32,
}

impl PlaneConfig {
    /// Assemble the full configuration from the raw register read-back.
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

/// Decode a `PLANE_CTL` control word into its enable/format/tiling/order fields.
pub const fn decode_ctl(plane_ctl: u32) -> PlaneCtl {
    PlaneCtl {
        enable: plane_ctl & PLANE_CTL_ENABLE != 0,
        format: PlaneFormat::from_field(reg_field_get(PLANE_CTL_FORMAT_MASK, plane_ctl)),
        tiling: Tiling::from_field(reg_field_get(PLANE_CTL_TILING_MASK, plane_ctl)),
        color_order: ColorOrder::from_ctl(plane_ctl),
        render_decompressed: plane_ctl & PLANE_CTL_RENDER_DECOMP_ENABLE != 0,
    }
}

/// Build the `PLANE_CTL` value for the linear repoint target: the requested
/// format and channel order, linear tiling, and render-decompression cleared.
/// RGB sources keep YUV range-correction disabled, matching the firmware plane.
pub const fn encode_ctl_linear(format: PlaneFormat, color_order: ColorOrder, enable: bool) -> u32 {
    let enable_bit = if enable { PLANE_CTL_ENABLE } else { 0 };
    // Linear tiling and render-decompression both contribute zero bits and are
    // therefore left out of the OR rather than spelled as `| 0`.
    reg_field_set(PLANE_CTL_FORMAT_MASK, format.to_field())
        | color_order.ctl_bit()
        | PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE
        | enable_bit
}

/// Decode `PLANE_SIZE` into `(width, height)`. Width occupies the low half,
/// height the high half, each stored as `value - 1`. Inverse of [`encode_size`].
pub const fn decode_size(plane_size: u32) -> (u32, u32) {
    let width = (plane_size & 0xffff) + 1;
    let height = ((plane_size >> 16) & 0xffff) + 1;
    (width, height)
}

/// Encode `(width, height)` into `PLANE_SIZE` as `((height-1) << 16) | (width-1)`.
pub const fn encode_size(width: u32, height: u32) -> u32 {
    ((height.wrapping_sub(1) & 0xffff) << 16) | (width.wrapping_sub(1) & 0xffff)
}

/// Decode `PLANE_POS` into `(x, y)`. X occupies the low half, Y the high half.
pub const fn decode_pos(plane_pos: u32) -> (u32, u32) {
    let x = plane_pos & 0xffff;
    let y = (plane_pos >> 16) & 0xffff;
    (x, y)
}

/// Encode `(x, y)` into `PLANE_POS` as `(y << 16) | x`.
pub const fn encode_pos(x: u32, y: u32) -> u32 {
    ((y & 0xffff) << 16) | (x & 0xffff)
}

/// Convert a linear scanline pitch in bytes to the `PLANE_STRIDE` register value
/// (a count of 64-byte units). The caller guarantees a 64-byte-aligned pitch.
pub const fn linear_stride_reg(pitch_bytes: u32) -> u32 {
    pitch_bytes / LINEAR_STRIDE_UNIT_BYTES
}

/// Recover the linear scanline pitch in bytes from a `PLANE_STRIDE` value.
pub const fn linear_stride_bytes(stride_reg: u32) -> u32 {
    stride_reg * LINEAR_STRIDE_UNIT_BYTES
}

/// Map an ABI pixel format to its placed `PLANE_CTL` format-field bits, or
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

/// Map placed `PLANE_CTL` format-field bits back to a canonical ABI pixel
/// format, or `None` for an unmodelled code.
pub const fn plane_to_pixel_format(plane_format_bits: u32) -> Option<PixelFormat> {
    PlaneFormat::from_field(reg_field_get(PLANE_CTL_FORMAT_MASK, plane_format_bits))
        .to_pixel_format()
}

// Compile-time proof that each encode/decode pair is an exact inverse for the
// representative live-panel cases (1920x1080 XRGB/ARGB, linear and tiled). These
// run on every kernel build, so a regression in the bit math fails the build.
const _: () = {
    // PLANE_SIZE: width low, height high; both stored biased by one.
    assert!(decode_size(encode_size(1920, 1080)).0 == 1920);
    assert!(decode_size(encode_size(1920, 1080)).1 == 1080);
    assert!(encode_size(1, 1) == 0);
    assert!(decode_size(0).0 == 1 && decode_size(0).1 == 1);

    // PLANE_POS round-trip.
    assert!(decode_pos(encode_pos(64, 48)).0 == 64);
    assert!(decode_pos(encode_pos(64, 48)).1 == 48);

    // Linear PLANE_STRIDE: an aligned pitch round-trips through the 64-byte unit.
    assert!(linear_stride_reg(1920 * 4) == 120);
    assert!(linear_stride_bytes(linear_stride_reg(1920 * 4)) == 1920 * 4);

    // The live DSPACNTR = 0x94009000 decodes to
    // enable | XRGB8888 | Y-tiled | render-decompressed | BGR order.
    let live = decode_ctl(0x9400_9000);
    assert!(live.enable);
    assert!(live.format.to_field() == FORMAT_FIELD_RGB8888);
    assert!(matches!(live.tiling, Tiling::YTiled));
    assert!(live.render_decompressed);
    assert!(matches!(live.color_order, ColorOrder::Bgrx));

    // Re-encoding the linear repoint target keeps format/order/enable while
    // forcing linear tiling and clearing render-decompression.
    let repoint = encode_ctl_linear(PlaneFormat::Rgb8888, ColorOrder::Bgrx, true);
    let back = decode_ctl(repoint);
    assert!(back.enable);
    assert!(back.format.to_field() == FORMAT_FIELD_RGB8888);
    assert!(matches!(back.tiling, Tiling::Linear));
    assert!(!back.render_decompressed);
    assert!(matches!(back.color_order, ColorOrder::Bgrx));

    // RGBX color order survives the same round-trip.
    let rgbx = decode_ctl(encode_ctl_linear(
        PlaneFormat::Rgb8888,
        ColorOrder::Rgbx,
        false,
    ));
    assert!(!rgbx.enable);
    assert!(matches!(rgbx.color_order, ColorOrder::Rgbx));

    // Tiling helpers round-trip for the X / Y / Yf field values.
    assert!(matches!(Tiling::from_field(TILING_FIELD_X), Tiling::XTiled));
    assert!(matches!(
        Tiling::from_field(TILING_FIELD_YF),
        Tiling::YfTiled
    ));
    assert!(Tiling::from_field(Tiling::XTiled.to_field()).to_field() == TILING_FIELD_X);

    // ABI pixel-format <-> placed plane-format-bits round-trip.
    assert!(pixel_format_to_plane(PixelFormat::Xrgb8888).is_some());
    assert!(pixel_format_to_plane(PixelFormat::Argb8888).is_some());
    assert!(pixel_format_to_plane(PixelFormat::Rgb888).is_none());
    assert!(plane_to_pixel_format(PLANE_CTL_FORMAT_XRGB8888).is_some());
    assert!(plane_to_pixel_format(reg_field_set(PLANE_CTL_FORMAT_MASK, 0xf)).is_none());
};
