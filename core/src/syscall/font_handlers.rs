//! Font management syscall handlers (inspired by Linux KDFONTOP).

use slopos_abi::Errno;
use slopos_abi::syscall::{FONT_FORMAT_BITMAP, FONT_FORMAT_COVERAGE};
use slopos_mm::user_io_buf::memdup_user;
use slopos_ostd::klog_info;

static FONT_WRITER_LOCK: slopos_ostd::sync::SpinLock<()> =
    slopos_ostd::sync::SpinLock::new((), slopos_ostd::sync::LOCK_LEVEL_RESOURCE);

fn replace_and_schedule_free(new_atlas: slopos_font::atlas::GlyphAtlas) {
    {
        let _writer = FONT_WRITER_LOCK.lock();
        slopos_font::atlas::replace_global(new_atlas);
    }
    slopos_font::atlas::invoke_font_change_callback();
}

define_syscall!(syscall_font_set
    (ctx, data_ptr: u64, width_raw: u32, height_raw: u32, glyph_count_raw: u32, format: u64)
    requires(console_admin)
    -> Result<(), Errno>
{
    let width = width_raw as u16;
    let height = height_raw as u16;
    let glyph_count = glyph_count_raw as usize;

    if data_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if format == FONT_FORMAT_COVERAGE {
        if width == 0 || height == 0 || height > 32 {
            return Err(Errno::EINVAL);
        }
        if glyph_count != slopos_font::GLYPH_COUNT {
            return Err(Errno::EINVAL);
        }

        // Upper bound: (GLYPH_COUNT + 1 replacement) cells of at most
        // 32×32 px = ~200 KiB; 256 KiB caps the upload comfortably.
        const MAX_COVERAGE_UPLOAD: usize = 256 * 1024;
        let stride = (width as usize).checked_mul(height as usize).ok_or(Errno::EINVAL)?;
        let coverage_size = slopos_font::GLYPH_COUNT.checked_mul(stride).ok_or(Errno::EINVAL)?;
        let data_size = (slopos_font::GLYPH_COUNT + 1)
            .checked_mul(stride)
            .filter(|&size| size <= MAX_COVERAGE_UPLOAD)
            .ok_or(Errno::EINVAL)?;

        let mut font_data = memdup_user(data_ptr, data_size, MAX_COVERAGE_UPLOAD)
            .map_err(|e| Errno::from_raw(e.raw()).unwrap_or(Errno::EINVAL))?;

        let replacement = font_data.split_off(coverage_size);
        let coverage = font_data;
        match slopos_font::atlas::GlyphAtlas::from_raw_coverage(
            width, height, coverage, replacement, slopos_font::FontSource::Syscall,
        ) {
            Some(atlas) => {
                replace_and_schedule_free(atlas);
                klog_info!(
                    "FONT_SET: applied {}x{} coverage font ({} glyphs + replacement)",
                    width,
                    height,
                    glyph_count,
                );
                Ok(())
            }
            None => Err(Errno::ENOMEM),
        }
    } else if format == FONT_FORMAT_BITMAP {
        if width != 8 {
            return Err(Errno::EINVAL);
        }
        if height == 0 || height > 32 {
            return Err(Errno::EINVAL);
        }
        if glyph_count == 0 || glyph_count > 512 {
            return Err(Errno::EINVAL);
        }

        let data_size = glyph_count
            .checked_mul(height as usize)
            .filter(|&size| size <= 16384)
            .ok_or(Errno::EINVAL)?;

        let font_data = memdup_user(data_ptr, data_size, 16384)
            .map_err(|e| Errno::from_raw(e.raw()).unwrap_or(Errno::EINVAL))?;

        match slopos_font::bitmap::bitmap_to_coverage(&font_data, width, height, glyph_count) {
            Some((coverage, replacement)) => {
                match slopos_font::atlas::GlyphAtlas::from_raw_coverage(
                    width,
                    height,
                    coverage,
                    replacement,
                    slopos_font::FontSource::Syscall,
                ) {
                    Some(atlas) => {
                        replace_and_schedule_free(atlas);
                        klog_info!(
                            "FONT_SET: applied {}x{} bitmap font ({} glyphs)",
                            width,
                            height,
                            glyph_count,
                        );
                        Ok(())
                    }
                    None => Err(Errno::ENOMEM),
                }
            }
            None => Err(Errno::EINVAL),
        }
    } else {
        Err(Errno::EINVAL)
    }
});
