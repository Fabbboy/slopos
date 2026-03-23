//! Alpha blending functions for compositing and anti-aliased rendering.
//!
//! All blending operates in the canonical Color32 (0xAARRGGBB) colour space.
//! Use `PixelFormat::decode`/`encode` when reading from or writing to surfaces
//! with different pixel layouts.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

/// Porter-Duff "source over" compositing operator (straight alpha).
///
/// Both `src` and `dst` are in 0xAARRGGBB format with **straight**
/// (non-premultiplied) alpha. Returns the composited pixel in the same
/// format.
///
/// Formula (per channel):
///   out_a = sa + da * (1 - sa/255)
///   out_c = (src_c * sa + dst_c * da * (255 - sa) / 255) / out_a
#[inline]
pub fn alpha_blend(src: u32, dst: u32) -> u32 {
    let sa = (src >> 24) & 0xFF;
    if sa == 0xFF {
        return src;
    }
    if sa == 0 {
        return dst;
    }

    let inv_sa = 255 - sa;

    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;

    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let da = (dst >> 24) & 0xFF;

    // out_a = sa + da * inv_sa / 255
    let out_a = sa + ((da * inv_sa + 127) / 255);
    if out_a == 0 {
        return 0;
    }

    // out_c = (src_c * sa + dst_c * da * inv_sa / 255) / out_a
    // For the common case of opaque destination (da == 255), this simplifies to:
    //   out_c = (src_c * sa + dst_c * inv_sa) / 255   (since out_a == 255)
    let r = ((sr * sa + ((dr * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let g = ((sg * sa + ((dg * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let b = ((sb * sa + ((db * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let a = out_a.min(255);

    (a << 24) | (r << 16) | (g << 8) | b
}

/// Blend a coverage value (0–255) with a foreground colour onto a
/// destination pixel.
///
/// This treats the coverage as the effective alpha of `fg`, compositing it
/// over `dst` using Porter-Duff "source over". Both `fg` and `dst` are in
/// 0xAARRGGBB format.
///
/// Primary use: rendering anti-aliased font glyphs where the rasteriser
/// outputs per-pixel coverage values.
#[inline]
pub fn blend_coverage(coverage: u8, fg: Color32, dst: u32) -> u32 {
    if coverage == 0 {
        return dst;
    }

    let fa = fg.alpha() as u32;
    // Effective alpha = font_alpha * coverage / 255
    let sa = (fa * coverage as u32 + 127) / 255;

    if sa == 0 {
        return dst;
    }
    if sa >= 255 {
        return fg.to_u32();
    }

    let inv_sa = 255 - sa;

    let sr = fg.red() as u32;
    let sg = fg.green() as u32;
    let sb = fg.blue() as u32;

    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let da = (dst >> 24) & 0xFF;

    let r = ((sr * sa + dr * inv_sa + 127) / 255).min(255);
    let g = ((sg * sa + dg * inv_sa + 127) / 255).min(255);
    let b = ((sb * sa + db * inv_sa + 127) / 255).min(255);
    let a = (sa + ((da * inv_sa + 127) / 255)).min(255);

    (a << 24) | (r << 16) | (g << 8) | b
}

/// Write a single alpha-blended pixel to a canvas (read-modify-write).
///
/// Reads the existing pixel at `(x, y)`, blends `color` over it using
/// Porter-Duff "source over", and writes the result back. Out-of-bounds
/// coordinates are silently ignored.
#[inline]
pub fn put_pixel_blended<T: Canvas>(target: &mut T, x: i32, y: i32, color: Color32) {
    if x < 0 || y < 0 || x >= target.width() as i32 || y >= target.height() as i32 {
        return;
    }

    let bpp = target.bytes_per_pixel() as usize;
    let off = (y as usize) * target.pitch_bytes() + (x as usize) * bpp;
    let fmt = target.pixel_format();

    let dst_raw = target.read_encoded_at(off);
    let dst_color = fmt.decode(dst_raw);

    let blended = alpha_blend(color.to_u32(), dst_color.to_u32());
    target.write_encoded_at(off, fmt.encode(Color32(blended)));
}

/// Write a pixel with a specific coverage (0–255) to a canvas.
///
/// Like `put_pixel_blended` but applies the coverage as an additional
/// alpha multiplier, useful for anti-aliased drawing primitives.
#[inline]
pub fn put_pixel_coverage<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    color: Color32,
    coverage: u8,
) {
    if coverage == 0 {
        return;
    }
    if x < 0 || y < 0 || x >= target.width() as i32 || y >= target.height() as i32 {
        return;
    }

    let bpp = target.bytes_per_pixel() as usize;
    let off = (y as usize) * target.pitch_bytes() + (x as usize) * bpp;
    let fmt = target.pixel_format();

    let dst_raw = target.read_encoded_at(off);
    let dst_color = fmt.decode(dst_raw);

    let blended = blend_coverage(coverage, color, dst_color.to_u32());
    target.write_encoded_at(off, fmt.encode(Color32(blended)));
}

/// Alpha-blend a filled rectangle onto a canvas.
///
/// Unlike `fill_rect` which writes opaque pixels, this blends `color`
/// (which may be semi-transparent) over the existing content.
pub fn fill_rect_blended<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
) -> Option<DamageRect> {
    if w <= 0 || h <= 0 {
        return None;
    }

    let alpha = color.alpha();
    if alpha == 0 {
        return None;
    }

    let buf_w = target.width() as i32;
    let buf_h = target.height() as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w - 1).min(buf_w - 1);
    let y1 = (y + h - 1).min(buf_h - 1);
    if x0 > x1 || y0 > y1 {
        return None;
    }

    // Opaque fast path: skip read-modify-write.
    if alpha == 0xFF {
        let px = target.pixel_format().encode(color);
        target.fill_rect_encoded(x0, y0, x1 - x0 + 1, y1 - y0 + 1, px);
    } else {
        for row in y0..=y1 {
            for col in x0..=x1 {
                put_pixel_blended(target, col, row, color);
            }
        }
    }

    let damage = DamageRect {
        x0,
        y0,
        x1,
        y1,
    };
    target.report_damage(damage);
    Some(damage)
}
