//! Kernel panic screen display.
//!
//! Renders a full-screen panic message when the kernel encounters
//! an unrecoverable error. Uses the pre-rasterized glyph atlas so
//! no heap allocation is needed at render time.

use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;
use slopos_utils::numfmt;

use crate::framebuffer;
use crate::graphics::GraphicsContext;
use crate::kernel_font;

const PANIC_BG_COLOR: Color32 = Color32(0xFF8B0000);
const PANIC_FG_COLOR: Color32 = Color32(0xFFFFFFFF);
const PANIC_HEADER_COLOR: Color32 = Color32(0xFFFF4444);

fn draw_register_line(
    ctx: &mut GraphicsContext,
    atlas: &GlyphAtlas,
    x: i32,
    y: i32,
    label: &[u8],
    value: u64,
) {
    atlas.draw_bytes(ctx, x, y, label, PANIC_FG_COLOR, PANIC_BG_COLOR);

    let mut hex_buf = numfmt::NumBuf::<19>::new();
    let hex_text = hex_buf.format_hex_u64(value);
    let label_width = atlas.bytes_width(label);
    atlas.draw_bytes(
        ctx,
        x + label_width,
        y,
        hex_text,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
}

/// Display the kernel panic screen.
pub fn display_panic_screen(
    message: Option<&str>,
    rip: Option<u64>,
    rsp: Option<u64>,
    cr0: u64,
    cr3: u64,
    cr4: u64,
) -> bool {
    if framebuffer::snapshot().is_none() {
        return false;
    }

    let mut ctx = match GraphicsContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return false,
    };

    let atlas = match kernel_font::atlas() {
        Some(a) => a,
        None => return false,
    };

    let bg_px = ctx.pixel_format().encode(PANIC_BG_COLOR);
    ctx.clear_canvas(bg_px);

    let width = ctx.width() as i32;
    let height = ctx.height() as i32;

    let char_height = atlas.cell_height();
    let char_width = atlas.cell_width();

    let mut y = 60;

    // Header
    let header = b"=== KERNEL PANIC ===\0";
    let header_width = atlas.bytes_width(header);
    let header_x = (width - header_width) / 2;
    atlas.draw_bytes(
        &mut ctx,
        header_x,
        y,
        header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Subtitle
    let subtitle = b"An unrecoverable error has occurred\0";
    let subtitle_width = atlas.bytes_width(subtitle);
    let subtitle_x = (width - subtitle_width) / 2;
    atlas.draw_bytes(
        &mut ctx,
        subtitle_x,
        y,
        subtitle,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Separator
    y += char_height;

    // Panic message
    if let Some(msg) = message {
        let msg_label = b"Reason: \0";
        atlas.draw_bytes(&mut ctx, 40, y, msg_label, PANIC_FG_COLOR, PANIC_BG_COLOR);

        let mut x = 40 + 8 * char_width;
        let max_x = width - 40;
        for &byte in msg.as_bytes() {
            if byte == 0 {
                break;
            }
            if x + char_width > max_x {
                y += char_height;
                x = 40 + 8 * char_width;
                if y > height - 120 {
                    break;
                }
            }
            atlas.draw_char(&mut ctx, x, y, byte as u32, PANIC_FG_COLOR, PANIC_BG_COLOR);
            x += char_width;
        }
        y += char_height * 2;
    }

    // Register info
    y += char_height;
    let reg_header = b"CPU State:\0";
    atlas.draw_bytes(
        &mut ctx,
        40,
        y,
        reg_header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height + 8;

    if let Some(rip_val) = rip {
        draw_register_line(&mut ctx, &atlas, 60, y, b"RIP: \0", rip_val);
        y += char_height + 4;
    }

    if let Some(rsp_val) = rsp {
        draw_register_line(&mut ctx, &atlas, 60, y, b"RSP: \0", rsp_val);
        y += char_height + 4;
    }

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR0: \0", cr0);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR3: \0", cr3);
    y += char_height + 4;

    draw_register_line(&mut ctx, &atlas, 60, y, b"CR4: \0", cr4);

    // Prompt at bottom
    let prompt = b"Press ENTER to shutdown\0";
    let prompt_width = atlas.bytes_width(prompt);
    let prompt_x = (width - prompt_width) / 2;
    let prompt_y = height - 60;
    atlas.draw_bytes(
        &mut ctx,
        prompt_x,
        prompt_y,
        prompt,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );

    let serial_note = b"(Debug output also available on serial console)\0";
    let note_width = atlas.bytes_width(serial_note);
    let note_x = (width - note_width) / 2;
    let note_y = height - 40;
    atlas.draw_bytes(
        &mut ctx,
        note_x,
        note_y,
        serial_note,
        Color32(0xFF888888),
        PANIC_BG_COLOR,
    );

    ctx.flush();

    true
}
