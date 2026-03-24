//! Font management syscall handlers (inspired by Linux KDFONTOP).

use slopos_mm::user_copy::copy_bytes_from_user;
use slopos_mm::user_ptr::UserBytes;
use slopos_utils::klog_info;

define_syscall!(syscall_font_set(ctx, args) requires(let pid: process_id) {
    if pid > 1 {
        return ctx.err_with(slopos_abi::syscall::ERRNO_EPERM);
    }

    let data_ptr = args.arg0;
    let width = args.arg1 as u16;
    let height = args.arg2 as u16;
    let glyph_count = args.arg3 as usize;

    // Validate parameters.
    if data_ptr == 0 {
        return ctx.bad_address();
    }
    if width != 8 {
        return ctx.err_with(slopos_abi::syscall::ENOSYS_RETURN);
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

    // Copy bitmap data from user space.
    let user_bytes = match UserBytes::try_new(data_ptr, data_size) {
        Ok(b) => b,
        Err(_) => return ctx.bad_address(),
    };

    let mut font_data = alloc::vec![0u8; data_size];
    if copy_bytes_from_user(user_bytes, &mut font_data).is_err() {
        return ctx.bad_address();
    }

    match slopos_font::bitmap::bitmap_to_coverage(&font_data, width, height, glyph_count) {
        Some((coverage, replacement)) => {
            match slopos_font::atlas::GlyphAtlas::from_raw_coverage(
                width,
                height,
                coverage,
                replacement,
            ) {
                Some(atlas) => {
                    slopos_font::atlas::replace_global(atlas);
                    klog_info!(
                        "FONT_SET: applied {}x{} bitmap font ({} glyphs)",
                        width,
                        height,
                        glyph_count,
                    );
                    ctx.ok(0)
                }
                None => ctx.err(),
            }
        }
        None => ctx.err(),
    }
});
