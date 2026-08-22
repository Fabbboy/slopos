//! Bitmap scaling and compositing helpers.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::blend::alpha_blend;

#[derive(Clone, Copy, Debug)]
pub struct BitmapRef<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [Color32],
}

impl<'a> BitmapRef<'a> {
    pub fn new(width: u32, height: u32, pixels: &'a [Color32]) -> Option<Self> {
        let len = (width as usize).checked_mul(height as usize)?;
        if width == 0 || height == 0 || pixels.len() < len {
            return None;
        }
        Some(Self {
            width,
            height,
            pixels,
        })
    }

    #[inline]
    fn pixel(&self, x: u32, y: u32) -> Color32 {
        self.pixels[y as usize * self.width as usize + x as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFit {
    /// Draw at source pixel dimensions starting at the target origin.
    Actual,
    /// Preserve aspect ratio and fit wholly inside the target rectangle.
    #[default]
    Contain,
    /// Preserve aspect ratio and cover the target rectangle, clipping overflow.
    Cover,
    /// Stretch to exactly the target rectangle.
    Stretch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSampling {
    #[default]
    Nearest,
    Bilinear,
}

#[allow(clippy::too_many_arguments)]
pub fn draw_image<T: Canvas>(
    target: &mut T,
    bitmap: BitmapRef<'_>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fit: ImageFit,
    sampling: ImageSampling,
) -> Option<DamageRect> {
    let clip = DamageRect {
        x0: 0,
        y0: 0,
        x1: target.width() as i32 - 1,
        y1: target.height() as i32 - 1,
    };
    draw_image_clipped(target, bitmap, x, y, width, height, fit, sampling, &clip)
}

#[allow(clippy::too_many_arguments)]
pub fn draw_image_clipped<T: Canvas>(
    target: &mut T,
    bitmap: BitmapRef<'_>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fit: ImageFit,
    sampling: ImageSampling,
    clip: &DamageRect,
) -> Option<DamageRect> {
    if width <= 0 || height <= 0 || bitmap.width == 0 || bitmap.height == 0 {
        return None;
    }

    let Some(dst) = placement(bitmap.width, bitmap.height, x, y, width, height, fit) else {
        return None;
    };
    let target_rect = DamageRect {
        x0: x,
        y0: y,
        x1: x + width - 1,
        y1: y + height - 1,
    };
    let draw_bounds = DamageRect {
        x0: dst.x,
        y0: dst.y,
        x1: dst.x + dst.width - 1,
        y1: dst.y + dst.height - 1,
    };
    let Some(mut clipped) = intersect(&draw_bounds, clip) else {
        return None;
    };
    if matches!(fit, ImageFit::Cover) {
        clipped = intersect(&clipped, &target_rect)?;
    }
    clipped = clipped.clip(target.width() as i32, target.height() as i32);
    if !clipped.is_valid() {
        return None;
    }

    if sampling == ImageSampling::Nearest
        && dst.width == bitmap.width as i32
        && dst.height == bitmap.height as i32
    {
        blit_exact(target, bitmap, &dst, &clipped);
    } else {
        scale_blit(target, bitmap, &dst, &clipped, sampling);
    }

    target.report_damage(clipped);
    Some(clipped)
}

/// Largest whole-number magnification of `src` that fits in `max`, never
/// below 1:1. Pixel art only survives integer scaling; pairing this with
/// [`ImageFit::Stretch`] and [`ImageSampling::Nearest`] makes the blit's
/// source-coordinate divide exact, so no row or column is doubled or dropped.
pub fn integer_scale_to_fit(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    let scale = (max_w / src_w).min(max_h / src_h).max(1);
    (src_w * scale, src_h * scale)
}

#[derive(Clone, Copy)]
struct Placement {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn placement(
    src_w: u32,
    src_h: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fit: ImageFit,
) -> Option<Placement> {
    let sw = src_w as i64;
    let sh = src_h as i64;
    let tw = width as i64;
    let th = height as i64;
    let (dw, dh) = match fit {
        ImageFit::Actual => (sw, sh),
        ImageFit::Stretch => (tw, th),
        ImageFit::Contain => {
            if sw * th <= sh * tw {
                ((th * sw / sh).max(1), th)
            } else {
                (tw, (tw * sh / sw).max(1))
            }
        }
        ImageFit::Cover => {
            if sw * th >= sh * tw {
                ((th * sw + sh - 1) / sh, th)
            } else {
                (tw, (tw * sh + sw - 1) / sw)
            }
        }
    };
    let dw = i32::try_from(dw).ok()?;
    let dh = i32::try_from(dh).ok()?;
    if dw <= 0 || dh <= 0 {
        return None;
    }
    let (dx, dy) = match fit {
        ImageFit::Actual | ImageFit::Stretch => (x, y),
        ImageFit::Contain | ImageFit::Cover => (x + (width - dw) / 2, y + (height - dh) / 2),
    };
    Some(Placement {
        x: dx,
        y: dy,
        width: dw,
        height: dh,
    })
}

fn blit_exact<T: Canvas>(
    target: &mut T,
    bitmap: BitmapRef<'_>,
    dst: &Placement,
    clip: &DamageRect,
) {
    let fmt = target.pixel_format();
    let bpp = target.bytes_per_pixel() as usize;
    let pitch = target.pitch_bytes();
    for y in clip.y0..=clip.y1 {
        let sy = (y - dst.y) as u32;
        for x in clip.x0..=clip.x1 {
            let sx = (x - dst.x) as u32;
            let src = bitmap.pixel(sx, sy);
            write_source_over(target, fmt, bpp, pitch, x, y, src);
        }
    }
}

fn scale_blit<T: Canvas>(
    target: &mut T,
    bitmap: BitmapRef<'_>,
    dst: &Placement,
    clip: &DamageRect,
    sampling: ImageSampling,
) {
    let fmt = target.pixel_format();
    let bpp = target.bytes_per_pixel() as usize;
    let pitch = target.pitch_bytes();
    for y in clip.y0..=clip.y1 {
        let rel_y = y - dst.y;
        for x in clip.x0..=clip.x1 {
            let rel_x = x - dst.x;
            let src = match sampling {
                ImageSampling::Nearest => {
                    sample_nearest(bitmap, rel_x, rel_y, dst.width, dst.height)
                }
                ImageSampling::Bilinear => {
                    sample_bilinear(bitmap, rel_x, rel_y, dst.width, dst.height)
                }
            };
            write_source_over(target, fmt, bpp, pitch, x, y, src);
        }
    }
}

#[inline]
fn write_source_over<T: Canvas>(
    target: &mut T,
    fmt: slopos_abi::pixel::PixelFormat,
    bpp: usize,
    pitch: usize,
    x: i32,
    y: i32,
    src: Color32,
) {
    match src.alpha() {
        0 => {}
        255 => {
            let off = y as usize * pitch + x as usize * bpp;
            target.write_encoded_at(off, fmt.encode(src));
        }
        _ => {
            let off = y as usize * pitch + x as usize * bpp;
            let dst = fmt.decode(target.read_encoded_at(off));
            let blended = alpha_blend(src.to_u32(), dst.to_u32());
            target.write_encoded_at(off, fmt.encode(Color32(blended)));
        }
    }
}

#[inline]
fn sample_nearest(
    bitmap: BitmapRef<'_>,
    rel_x: i32,
    rel_y: i32,
    dst_w: i32,
    dst_h: i32,
) -> Color32 {
    let sx = ((rel_x as i64 * bitmap.width as i64) / dst_w as i64).clamp(0, bitmap.width as i64 - 1)
        as u32;
    let sy = ((rel_y as i64 * bitmap.height as i64) / dst_h as i64)
        .clamp(0, bitmap.height as i64 - 1) as u32;
    bitmap.pixel(sx, sy)
}

fn sample_bilinear(
    bitmap: BitmapRef<'_>,
    rel_x: i32,
    rel_y: i32,
    dst_w: i32,
    dst_h: i32,
) -> Color32 {
    if bitmap.width == 1 && bitmap.height == 1 {
        return bitmap.pixel(0, 0);
    }

    let x_fp = center_source_fp(rel_x, bitmap.width, dst_w);
    let y_fp = center_source_fp(rel_y, bitmap.height, dst_h);
    let x0 = (x_fp >> 16).clamp(0, bitmap.width as i64 - 1) as u32;
    let y0 = (y_fp >> 16).clamp(0, bitmap.height as i64 - 1) as u32;
    let x1 = (x0 + 1).min(bitmap.width - 1);
    let y1 = (y0 + 1).min(bitmap.height - 1);
    let fx = (x_fp & 0xFFFF) as u32;
    let fy = (y_fp & 0xFFFF) as u32;

    let c00 = bitmap.pixel(x0, y0);
    let c10 = bitmap.pixel(x1, y0);
    let c01 = bitmap.pixel(x0, y1);
    let c11 = bitmap.pixel(x1, y1);
    let top = lerp_color(c00, c10, fx);
    let bottom = lerp_color(c01, c11, fx);
    lerp_color(top, bottom, fy)
}

fn center_source_fp(rel: i32, src: u32, dst: i32) -> i64 {
    let value = (((rel as i64 * 2 + 1) * src as i64) << 16) / (dst as i64 * 2) - 0x8000;
    value.clamp(0, ((src as i64 - 1) << 16).max(0))
}

fn lerp_color(a: Color32, b: Color32, t: u32) -> Color32 {
    let inv = 65_536u32 - t;
    let mix =
        |av: u8, bv: u8| -> u8 { (((av as u32 * inv) + (bv as u32 * t) + 32_768) >> 16) as u8 };
    Color32::new(
        mix(a.red(), b.red()),
        mix(a.green(), b.green()),
        mix(a.blue(), b.blue()),
        mix(a.alpha(), b.alpha()),
    )
}

fn intersect(a: &DamageRect, b: &DamageRect) -> Option<DamageRect> {
    let rect = DamageRect {
        x0: a.x0.max(b.x0),
        y0: a.y0.max(b.y0),
        x1: a.x1.min(b.x1),
        y1: a.y1.min(b.y1),
    };
    rect.is_valid().then_some(rect)
}

#[cfg(test)]
mod scale_tests {
    use super::integer_scale_to_fit;

    #[test]
    fn magnifies_by_whole_numbers_only() {
        let (w, h) = integer_scale_to_fit(73, 18, 640, 360);
        assert_eq!(w % 73, 0);
        assert_eq!(h % 18, 0);
        assert_eq!(w / 73, h / 18);
    }

    #[test]
    fn stays_inside_the_budget() {
        for (mw, mh) in [(640u32, 360u32), (426, 240), (266, 200), (213, 160)] {
            let (w, h) = integer_scale_to_fit(73, 18, mw, mh);
            assert!(w <= mw && h <= mh, "{w}x{h} exceeds {mw}x{mh}");
        }
    }

    #[test]
    fn takes_the_limiting_axis() {
        assert_eq!(integer_scale_to_fit(10, 10, 100, 25), (20, 20));
        assert_eq!(integer_scale_to_fit(10, 10, 25, 100), (20, 20));
    }

    #[test]
    fn never_shrinks_below_native() {
        assert_eq!(integer_scale_to_fit(73, 18, 40, 10), (73, 18));
    }

    #[test]
    fn tolerates_an_empty_source() {
        assert_eq!(integer_scale_to_fit(0, 0, 1920, 1080), (0, 0));
        assert_eq!(integer_scale_to_fit(73, 0, 1920, 1080), (0, 0));
    }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use crate::{HeadlessSurface, RenderSurface};
    use slopos_abi::pixel::PixelFormat;

    fn bitmap() -> BitmapRef<'static> {
        static PIXELS: [Color32; 4] = [
            Color32::rgb(255, 0, 0),
            Color32::rgb(0, 255, 0),
            Color32::rgb(0, 0, 255),
            Color32::new(255, 255, 255, 128),
        ];
        BitmapRef::new(2, 2, &PIXELS).unwrap()
    }

    #[test]
    fn exact_blit_writes_pixels() {
        let mut surface = HeadlessSurface::new(4, 4, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            draw_image(
                &mut fb,
                bitmap(),
                1,
                1,
                2,
                2,
                ImageFit::Stretch,
                ImageSampling::Nearest,
            );
        }
        assert_eq!(surface.pixel_at(1, 1).unwrap(), Color32::rgb(255, 0, 0));
        assert_eq!(surface.pixel_at(2, 1).unwrap(), Color32::rgb(0, 255, 0));
        assert_eq!(surface.pixel_at(1, 2).unwrap(), Color32::rgb(0, 0, 255));
    }

    #[test]
    fn clipped_blit_limits_damage_and_pixels() {
        let mut surface = HeadlessSurface::new(4, 4, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            let damage = draw_image_clipped(
                &mut fb,
                bitmap(),
                0,
                0,
                2,
                2,
                ImageFit::Stretch,
                ImageSampling::Nearest,
                &DamageRect {
                    x0: 1,
                    y0: 1,
                    x1: 3,
                    y1: 3,
                },
            )
            .unwrap();
            assert_eq!(
                damage,
                DamageRect {
                    x0: 1,
                    y0: 1,
                    x1: 1,
                    y1: 1
                }
            );
        }
        assert_eq!(surface.pixel_at(0, 0).unwrap(), Color32::TRANSPARENT);
        assert_ne!(surface.pixel_at(1, 1).unwrap(), Color32::TRANSPARENT);
    }

    #[test]
    fn alpha_blends_over_destination() {
        let mut surface = HeadlessSurface::new(2, 2, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            crate::canvas_ops::fill_rect(&mut fb, 0, 0, 2, 2, Color32::rgb(0, 0, 0));
            draw_image(
                &mut fb,
                bitmap(),
                0,
                0,
                2,
                2,
                ImageFit::Stretch,
                ImageSampling::Nearest,
            );
        }
        let blended = surface.pixel_at(1, 1).unwrap();
        assert_eq!(blended.alpha(), 255);
        assert!((blended.red() as i32 - 128).abs() <= 1);
    }

    #[test]
    fn contain_centers_image() {
        let mut surface = HeadlessSurface::new(6, 4, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            draw_image(
                &mut fb,
                bitmap(),
                0,
                0,
                6,
                4,
                ImageFit::Contain,
                ImageSampling::Nearest,
            );
        }
        assert_eq!(surface.pixel_at(0, 0).unwrap(), Color32::TRANSPARENT);
        assert_eq!(surface.pixel_at(1, 0).unwrap(), Color32::rgb(255, 0, 0));
    }

    #[test]
    fn cover_clips_to_target_rect() {
        let mut surface = HeadlessSurface::new(6, 4, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            draw_image(
                &mut fb,
                bitmap(),
                0,
                0,
                6,
                4,
                ImageFit::Cover,
                ImageSampling::Nearest,
            );
        }
        assert_ne!(surface.pixel_at(0, 0).unwrap(), Color32::TRANSPARENT);
        assert_ne!(surface.pixel_at(5, 3).unwrap(), Color32::TRANSPARENT);
    }

    #[test]
    fn bilinear_samples_between_pixels() {
        static PIXELS: [Color32; 2] = [Color32::rgb(0, 0, 0), Color32::rgb(255, 0, 0)];
        let bitmap = BitmapRef::new(2, 1, &PIXELS).unwrap();
        let mut surface = HeadlessSurface::new(3, 1, PixelFormat::Argb8888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            draw_image(
                &mut fb,
                bitmap,
                0,
                0,
                3,
                1,
                ImageFit::Stretch,
                ImageSampling::Bilinear,
            );
        }
        let mid = surface.pixel_at(1, 0).unwrap();
        assert!((mid.red() as i32 - 128).abs() <= 2);
    }

    #[test]
    fn rgb888_target_format() {
        let mut surface = HeadlessSurface::new(2, 2, PixelFormat::Rgb888).unwrap();
        {
            let mut fb = surface.frame().unwrap();
            draw_image(
                &mut fb,
                bitmap(),
                0,
                0,
                2,
                2,
                ImageFit::Stretch,
                ImageSampling::Nearest,
            );
        }
        assert_eq!(surface.pixel_at(0, 0).unwrap(), Color32::rgb(255, 0, 0));
    }
}
