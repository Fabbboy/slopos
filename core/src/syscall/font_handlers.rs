//! Font management syscall handlers (inspired by Linux KDFONTOP).

use slopos_mm::user_copy::copy_bytes_from_user;
use slopos_mm::user_ptr::UserBytes;
use slopos_utils::klog_info;

define_syscall!(syscall_font_set(ctx, args) {
    let data_ptr = args.arg0;
    let width = args.arg1 as u16;
    let height = args.arg2 as u16;
    let glyph_count = args.arg3 as usize;

    // Validate parameters.
    if data_ptr == 0 {
        return ctx.bad_address();
    }
    if width != 8 {
        return ctx.err(); // Only 8-pixel-wide fonts supported
    }
    if height == 0 || height > 32 {
        return ctx.err();
    }
    if glyph_count == 0 || glyph_count > 512 {
        return ctx.err();
    }

    let data_size = glyph_count * height as usize;
    if data_size > 16384 {
        return ctx.err(); // Sanity limit: 16KB max
    }

    // Copy bitmap data from user space.
    let user_bytes = match UserBytes::try_new(data_ptr, data_size) {
        Ok(b) => b,
        Err(_) => return ctx.bad_address(),
    };

    // Allocate kernel buffer and copy.
    extern crate alloc;
    let mut font_data = alloc::vec![0u8; data_size];
    if copy_bytes_from_user(user_bytes, &mut font_data).is_err() {
        return ctx.bad_address();
    }

    klog_info!(
        "FONT_SET: received {}x{} font ({} glyphs, {} bytes)",
        width,
        height,
        glyph_count,
        data_size,
    );

    // TODO: Rebuild the console glyph atlas from the uploaded bitmap data.
    // This requires converting the 1-bpp bitmap to the coverage format
    // used by GlyphAtlas and replacing the global atlas.
    // For now, accept the data but don't apply it.

    ctx.ok(0)
});
