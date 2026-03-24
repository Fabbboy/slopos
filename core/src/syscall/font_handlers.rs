//! Font management syscall handlers (inspired by Linux KDFONTOP).

use slopos_mm::user_io_buf::memdup_user;
use slopos_utils::klog_info;

use slopos_abi::syscall::{FONT_FORMAT_BITMAP, FONT_FORMAT_COVERAGE};

static FONT_WRITER_LOCK: slopos_sync::IrqMutex<()> = slopos_sync::IrqMutex::new(());

unsafe fn free_atlas_box(ptr: *mut u8) {
    unsafe {
        drop(alloc::boxed::Box::from_raw(
            ptr as *mut slopos_font::atlas::GlyphAtlas,
        ));
    }
}

fn replace_and_schedule_free(new_atlas: slopos_font::atlas::GlyphAtlas) {
    let _writer = FONT_WRITER_LOCK.lock();
    let old = slopos_font::atlas::replace_global(new_atlas);
    slopos_font::atlas::invoke_font_change_callback();
    if !old.is_null() {
        unsafe {
            slopos_sync::call_rcu(old as *mut u8, free_atlas_box);
        }
    }
}

define_syscall!(syscall_font_set(ctx, args) requires(let pid: process_id) {
    if pid > 1 {
        return ctx.err_with(slopos_abi::syscall::ERRNO_EPERM);
    }

    let data_ptr = args.arg0;
    let width = args.arg1 as u16;
    let height = args.arg2 as u16;
    let glyph_count = args.arg3 as usize;
    let format = args.arg4;

    if data_ptr == 0 {
        return ctx.bad_address();
    }
    if format == FONT_FORMAT_COVERAGE {
        if width == 0 || height == 0 || height > 32 {
            return ctx.err();
        }

        let stride = match (width as usize).checked_mul(height as usize) {
            Some(size) => size,
            None => return ctx.err(),
        };
        let coverage_size = match slopos_font::ASCII_COUNT.checked_mul(stride) {
            Some(size) => size,
            None => return ctx.err(),
        };
        let data_size = match (slopos_font::ASCII_COUNT + 1).checked_mul(stride) {
            Some(size) if size <= 65536 => size,
            _ => return ctx.err(),
        };

        let mut font_data = match memdup_user(data_ptr, data_size, 65536) {
            Ok(v) => v,
            Err(e) => return ctx.err_with(e.as_u64()),
        };

        let replacement = font_data.split_off(coverage_size);
        let coverage = font_data;
        match slopos_font::atlas::GlyphAtlas::from_raw_coverage(
            width, height, coverage, replacement, slopos_font::FontSource::Syscall,
        ) {
            Some(atlas) => {
                replace_and_schedule_free(atlas);
                klog_info!(
                    "FONT_SET: applied {}x{} coverage font (95 glyphs + replacement)",
                    width,
                    height,
                );
                ctx.ok(0)
            }
            None => ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM),
        }
    } else if format == FONT_FORMAT_BITMAP {
        if width != 8 {
            return ctx.err();
        }
        if height == 0 || height > 32 {
            return ctx.err();
        }
        if glyph_count == 0 || glyph_count > 512 {
            return ctx.err();
        }

        let data_size = match glyph_count.checked_mul(height as usize) {
            Some(size) if size <= 16384 => size,
            _ => return ctx.err(),
        };

        let font_data = match memdup_user(data_ptr, data_size, 16384) {
            Ok(v) => v,
            Err(e) => return ctx.err_with(e.as_u64()),
        };

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
                        ctx.ok(0)
                    }
                    None => ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM),
                }
            }
            None => ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM),
        }
    } else {
        ctx.err()
    }
});
