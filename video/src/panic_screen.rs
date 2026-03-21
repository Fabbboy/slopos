//! Kernel panic screen display.
//!
//! Renders a full-screen panic message when the kernel encounters
//! an unrecoverable error. Designed to work with minimal dependencies
//! since most subsystems may be in undefined states during panic.

use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};
use slopos_gfx::canvas_font;
use slopos_utils::numfmt;

use crate::framebuffer;
use crate::graphics::GraphicsContext;

const PANIC_BG_COLOR: Color32 = Color32(0xFF8B0000);
const PANIC_FG_COLOR: Color32 = Color32(0xFFFFFFFF);
const PANIC_HEADER_COLOR: Color32 = Color32(0xFFFF4444);

fn draw_register_line(ctx: &mut GraphicsContext, x: i32, y: i32, label: &[u8], value: u64) {
    canvas_font::draw_string(ctx, x, y, label, PANIC_FG_COLOR, PANIC_BG_COLOR);

    let mut hex_buf = numfmt::NumBuf::<19>::new();
    let hex_text = hex_buf.format_hex_u64(value);
    let label_width = (label.len() as i32 - 1) * FONT_CHAR_WIDTH;
    canvas_font::draw_string(
        ctx,
        x + label_width,
        y,
        hex_text,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
}

/// Display the kernel panic screen.
///
/// Clears framebuffer to dark red, displays panic information,
/// and shows "Press Enter to shutdown" prompt.
///
/// # Arguments
/// * `message` - Optional panic message/reason
/// * `rip` - Optional instruction pointer value
/// * `rsp` - Optional stack pointer value
/// * `cr0` - CR0 control register value
/// * `cr3` - CR3 control register value (page directory base)
/// * `cr4` - CR4 control register value
///
/// # Returns
/// `true` if the panic screen was displayed, `false` if framebuffer unavailable.
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

    // Clear screen to dark red
    let bg_px = ctx.pixel_format().encode(PANIC_BG_COLOR);
    ctx.clear_canvas(bg_px);

    let width = ctx.width() as i32;
    let height = ctx.height() as i32;

    let char_height = FONT_CHAR_HEIGHT;
    let char_width = FONT_CHAR_WIDTH;

    let mut y = 60; // Start from top with margin

    // Draw header: "KERNEL PANIC"
    let header = b"=== KERNEL PANIC ===\0";
    let header_len = 21;
    let header_width = header_len * char_width;
    let header_x = (width - header_width) / 2;
    canvas_font::draw_string(
        &mut ctx,
        header_x,
        y,
        header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Draw subtitle
    let subtitle = b"An unrecoverable error has occurred\0";
    let subtitle_len = 36;
    let subtitle_width = subtitle_len * char_width;
    let subtitle_x = (width - subtitle_width) / 2;
    canvas_font::draw_string(
        &mut ctx,
        subtitle_x,
        y,
        subtitle,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Draw separator
    y += char_height;

    // Draw panic message if provided
    if let Some(msg) = message {
        let msg_label = b"Reason: \0";
        canvas_font::draw_string(&mut ctx, 40, y, msg_label, PANIC_FG_COLOR, PANIC_BG_COLOR);

        // Draw message character by character
        let mut x = 40 + 8 * char_width;
        let max_x = width - 40;
        for &byte in msg.as_bytes() {
            if byte == 0 {
                break;
            }
            if x + char_width > max_x {
                // Wrap to next line
                y += char_height;
                x = 40 + 8 * char_width;
                if y > height - 120 {
                    break; // Don't overflow into prompt area
                }
            }
            canvas_font::draw_char(&mut ctx, x, y, byte, PANIC_FG_COLOR, PANIC_BG_COLOR);
            x += char_width;
        }
        y += char_height * 2;
    }

    // Draw register info section
    y += char_height;
    let reg_header = b"CPU State:\0";
    canvas_font::draw_string(
        &mut ctx,
        40,
        y,
        reg_header,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height + 8;

    // Draw RIP if available
    if let Some(rip_val) = rip {
        draw_register_line(&mut ctx, 60, y, b"RIP: \0", rip_val);
        y += char_height + 4;
    }

    // Draw RSP if available
    if let Some(rsp_val) = rsp {
        draw_register_line(&mut ctx, 60, y, b"RSP: \0", rsp_val);
        y += char_height + 4;
    }

    // Draw control registers
    draw_register_line(&mut ctx, 60, y, b"CR0: \0", cr0);
    y += char_height + 4;

    draw_register_line(&mut ctx, 60, y, b"CR3: \0", cr3);
    y += char_height + 4;

    draw_register_line(&mut ctx, 60, y, b"CR4: \0", cr4);

    // Draw prompt at bottom
    let prompt = b"Press ENTER to shutdown\0";
    let prompt_len = 24;
    let prompt_width = prompt_len * char_width;
    let prompt_x = (width - prompt_width) / 2;
    let prompt_y = height - 60;
    canvas_font::draw_string(
        &mut ctx,
        prompt_x,
        prompt_y,
        prompt,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );

    // Draw a small note about serial output
    let serial_note = b"(Debug output also available on serial console)\0";
    let note_len = 47;
    let note_width = note_len * char_width;
    let note_x = (width - note_width) / 2;
    let note_y = height - 40;
    canvas_font::draw_string(
        &mut ctx,
        note_x,
        note_y,
        serial_note,
        Color32(0xFF888888), // Gray
        PANIC_BG_COLOR,
    );

    ctx.flush();

    true
}
