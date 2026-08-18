//! Alpha blending for compositing and anti-aliased rendering.
//!
//! All blending operates in the canonical Color32 (0xAARRGGBB) colour space.
//!
//! Integer fixed point throughout: `(x * 255 + 127) / 255` is round-to-nearest
//! for a /255 divide, which is where the `+ 127` and `+ out_a / 2` terms below
//! come from.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};

/// Porter-Duff "source over" compositing operator.
///
/// `src`, `dst` and the result are 0xAARRGGBB with **straight**
/// (non-premultiplied) alpha.
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

    let out_a = sa + ((da * inv_sa + 127) / 255);
    if out_a == 0 {
        return 0;
    }

    let r = ((sr * sa + ((dr * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let g = ((sg * sa + ((dg * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let b = ((sb * sa + ((db * da * inv_sa + 127) / 255) + out_a / 2) / out_a).min(255);
    let a = out_a.min(255);

    (a << 24) | (r << 16) | (g << 8) | b
}

/// Composite `fg` over `dst` with `coverage` (0–255) scaling `fg`'s alpha.
#[inline]
pub fn blend_coverage(coverage: u8, fg: Color32, dst: u32) -> u32 {
    if coverage == 0 {
        return dst;
    }

    let fa = fg.alpha() as u32;
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

/// Blend `color` over the canvas pixel at `(x, y)`.
///
/// Out-of-bounds coordinates are silently ignored.
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

/// Blend `color` over the canvas pixel at `(x, y)`, scaled by `coverage`.
///
/// Out-of-bounds and scissored-out coordinates are silently ignored.
#[inline]
pub fn put_pixel_coverage<T: Canvas>(target: &mut T, x: i32, y: i32, color: Color32, coverage: u8) {
    if coverage == 0 {
        return;
    }
    if x < 0 || y < 0 || x >= target.width() as i32 || y >= target.height() as i32 {
        return;
    }
    if let Some(s) = target.scissor() {
        if !s.contains(x, y) {
            return;
        }
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
    let mut x0 = x.max(0);
    let mut y0 = y.max(0);
    let mut x1 = (x + w - 1).min(buf_w - 1);
    let mut y1 = (y + h - 1).min(buf_h - 1);
    // The blended path writes pixels directly, so it must apply the scissor
    // here; the opaque path gets it from `fill_rect_encoded`.
    if let Some(s) = target.scissor() {
        x0 = x0.max(s.x0);
        y0 = y0.max(s.y0);
        x1 = x1.min(s.x1);
        y1 = y1.min(s.y1);
    }
    if x0 > x1 || y0 > y1 {
        return None;
    }

    if alpha == 0xFF {
        let px = target.pixel_format().encode(color);
        target.fill_rect_encoded(x0, y0, x1 - x0 + 1, y1 - y0 + 1, px);
    } else {
        // RB/AG channel separation, applied in the native pixel format: it only
        // separates alternating byte pairs, so the layout does not matter.
        let sa = alpha as u32;
        let inv_sa = 255 - sa;
        let src_native = target.pixel_format().encode(color).to_u32();
        let src_rb = (src_native & 0x00FF00FF) * sa;
        let src_ag = ((src_native >> 8) & 0x00FF00FF) * sa;

        let bpp = target.bytes_per_pixel() as usize;
        let pitch = target.pitch_bytes();
        let col_count = (x1 - x0 + 1) as usize;

        for row in y0..=y1 {
            let base = (row as usize) * pitch + (x0 as usize) * bpp;
            for i in 0..col_count {
                let off = base + i * bpp;
                let dst = target.read_encoded_at(off);
                let dst_rb = dst & 0x00FF00FF;
                let dst_ag = (dst >> 8) & 0x00FF00FF;
                let rb = (src_rb + dst_rb * inv_sa + 0x00800080) >> 8 & 0x00FF00FF;
                let ag = (src_ag + dst_ag * inv_sa + 0x00800080) >> 8 & 0x00FF00FF;
                target.write_encoded_at(off, EncodedPixel(rb | (ag << 8)));
            }
        }
    }

    let damage = DamageRect { x0, y0, x1, y1 };
    target.report_damage(damage);
    Some(damage)
}

/// [`fill_rect_blended`] intersected with `clip`.
pub fn fill_rect_blended_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Color32,
    clip: &DamageRect,
) {
    if w <= 0 || h <= 0 || color.alpha() == 0 {
        return;
    }
    let x0 = x.max(clip.x0);
    let y0 = y.max(clip.y0);
    let x1 = (x + w - 1).min(clip.x1);
    let y1 = (y + h - 1).min(clip.y1);
    if x0 > x1 || y0 > y1 {
        return;
    }
    fill_rect_blended(target, x0, y0, x1 - x0 + 1, y1 - y0 + 1, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_fully_opaque_src_returns_src() {
        let src = Color32::rgb(200, 100, 50).to_u32();
        let dst = Color32::rgb(10, 20, 30).to_u32();
        assert_eq!(alpha_blend(src, dst), src);
    }

    #[test]
    fn blend_fully_transparent_src_returns_dst() {
        let src = Color32::new(200, 100, 50, 0).to_u32();
        let dst = Color32::rgb(10, 20, 30).to_u32();
        assert_eq!(alpha_blend(src, dst), dst);
    }

    #[test]
    fn blend_50_percent_alpha_on_opaque_dst() {
        let src = Color32::new(200, 0, 0, 128).to_u32();
        let dst = Color32::rgb(100, 0, 0).to_u32();
        let result = alpha_blend(src, dst);
        let c = Color32(result);
        assert_eq!(c.alpha(), 255);
        let r = c.red() as i32;
        assert!((r - 150).abs() <= 2, "Expected red ~150, got {r}");
    }

    #[test]
    fn blend_regression_not_premultiplied() {
        let src = Color32::new(200, 0, 0, 128).to_u32();
        let dst = Color32::rgb(100, 0, 0).to_u32();
        let result = alpha_blend(src, dst);
        let r = Color32(result).red();
        assert!(r < 200, "red={r} too bright — premultiplied bug");
    }

    #[test]
    fn blend_25_percent_alpha() {
        let src = Color32::new(255, 0, 0, 64).to_u32();
        let dst = Color32::rgb(0, 0, 255).to_u32();
        let c = Color32(alpha_blend(src, dst));
        assert_eq!(c.alpha(), 255);
        assert!((c.red() as i32 - 64).abs() <= 2, "red={}", c.red());
        assert!((c.blue() as i32 - 191).abs() <= 2, "blue={}", c.blue());
    }

    #[test]
    fn blend_both_semitransparent() {
        let src = Color32::new(255, 0, 0, 128).to_u32();
        let dst = Color32::new(0, 0, 255, 128).to_u32();
        let c = Color32(alpha_blend(src, dst));
        assert!((c.alpha() as i32 - 192).abs() <= 2, "alpha={}", c.alpha());
    }

    #[test]
    fn blend_black_on_white() {
        let src = Color32::new(0, 0, 0, 128).to_u32();
        let dst = Color32::WHITE.to_u32();
        let c = Color32(alpha_blend(src, dst));
        assert!((c.red() as i32 - 127).abs() <= 3, "r={}", c.red());
    }

    #[test]
    fn blend_white_on_black() {
        let src = Color32::new(255, 255, 255, 128).to_u32();
        let dst = Color32::BLACK.to_u32();
        let c = Color32(alpha_blend(src, dst));
        assert!((c.red() as i32 - 128).abs() <= 3, "r={}", c.red());
    }

    #[test]
    fn coverage_zero_returns_dst() {
        let dst = Color32::rgb(0, 0, 255).to_u32();
        assert_eq!(blend_coverage(0, Color32::rgb(255, 0, 0), dst), dst);
    }

    #[test]
    fn coverage_full_opaque_returns_fg() {
        let fg = Color32::rgb(255, 0, 0);
        let dst = Color32::rgb(0, 0, 255).to_u32();
        assert_eq!(blend_coverage(255, fg, dst), fg.to_u32());
    }

    #[test]
    fn coverage_half() {
        let fg = Color32::rgb(200, 0, 0);
        let dst = Color32::rgb(0, 0, 100).to_u32();
        let c = Color32(blend_coverage(128, fg, dst));
        assert!((c.red() as i32 - 100).abs() <= 3, "r={}", c.red());
        assert!((c.blue() as i32 - 50).abs() <= 3, "b={}", c.blue());
    }
}
