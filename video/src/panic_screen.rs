//! Kernel panic screen display.
//!
//! Renders a full-screen panic message when the kernel encounters
//! an unrecoverable error. Designed to work with minimal dependencies
//! since most subsystems may be in undefined states during panic.

use core::ffi::c_char;

use crate::{font, framebuffer};

// Colors (ARGB format)
const PANIC_BG_COLOR: u32 = 0xFF8B0000; // Dark red
const PANIC_FG_COLOR: u32 = 0xFFFFFFFF; // White
const PANIC_HEADER_COLOR: u32 = 0xFFFF4444; // Bright red for header

/// Format a u64 value as a hex string into the provided buffer.
/// Returns a slice to the formatted string (null-terminated).
fn format_hex(value: u64, buf: &mut [u8; 19]) -> &[u8] {
    const HEX_CHARS: &[u8] = b"0123456789ABCDEF";
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = ((value >> (60 - i * 4)) & 0xF) as usize;
        buf[2 + i] = HEX_CHARS[nibble];
    }
    buf[18] = 0; // Null terminator
    &buf[..19]
}

/// Draw a single line of text with a label and hex value.
fn draw_register_line(x: i32, y: i32, label: &[u8], value: u64) {
    // Draw label
    font::font_draw_string(
        x,
        y,
        label.as_ptr() as *const c_char,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );

    // Draw hex value
    let mut hex_buf = [0u8; 19];
    let _ = format_hex(value, &mut hex_buf);
    let label_width = (label.len() as i32 - 1) * font::FONT_CHAR_WIDTH; // -1 for null terminator
    font::font_draw_string(
        x + label_width,
        y,
        hex_buf.as_ptr() as *const c_char,
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
    if framebuffer::framebuffer_is_initialized() == 0 {
        return false;
    }

    // Clear screen to dark red
    framebuffer::framebuffer_clear(PANIC_BG_COLOR);

    let width = framebuffer::framebuffer_get_width() as i32;
    let height = framebuffer::framebuffer_get_height() as i32;

    let char_height = font::FONT_CHAR_HEIGHT;
    let char_width = font::FONT_CHAR_WIDTH;

    let mut y = 60; // Start from top with margin

    // Draw header: "KERNEL PANIC"
    let header = b"=== KERNEL PANIC ===\0";
    let header_len = 21;
    let header_width = header_len * char_width;
    let header_x = (width - header_width) / 2;
    font::font_draw_string(
        header_x,
        y,
        header.as_ptr() as *const c_char,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Draw subtitle
    let subtitle = b"An unrecoverable error has occurred\0";
    let subtitle_len = 36;
    let subtitle_width = subtitle_len * char_width;
    let subtitle_x = (width - subtitle_width) / 2;
    font::font_draw_string(
        subtitle_x,
        y,
        subtitle.as_ptr() as *const c_char,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height * 2;

    // Draw separator
    y += char_height;

    // Draw panic message if provided
    if let Some(msg) = message {
        let msg_label = b"Reason: \0";
        font::font_draw_string(
            40,
            y,
            msg_label.as_ptr() as *const c_char,
            PANIC_FG_COLOR,
            PANIC_BG_COLOR,
        );

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
            font::font_draw_char(x, y, byte as c_char, PANIC_FG_COLOR, PANIC_BG_COLOR);
            x += char_width;
        }
        y += char_height * 2;
    }

    // Draw register info section
    y += char_height;
    let reg_header = b"CPU State:\0";
    font::font_draw_string(
        40,
        y,
        reg_header.as_ptr() as *const c_char,
        PANIC_HEADER_COLOR,
        PANIC_BG_COLOR,
    );
    y += char_height + 8;

    // Draw RIP if available
    if let Some(rip_val) = rip {
        draw_register_line(60, y, b"RIP: \0", rip_val);
        y += char_height + 4;
    }

    // Draw RSP if available
    if let Some(rsp_val) = rsp {
        draw_register_line(60, y, b"RSP: \0", rsp_val);
        y += char_height + 4;
    }

    // Draw control registers
    draw_register_line(60, y, b"CR0: \0", cr0);
    y += char_height + 4;

    draw_register_line(60, y, b"CR3: \0", cr3);
    y += char_height + 4;

    draw_register_line(60, y, b"CR4: \0", cr4);

    // Draw prompt at bottom
    let prompt = b"Press ENTER to shutdown\0";
    let prompt_len = 24;
    let prompt_width = prompt_len * char_width;
    let prompt_x = (width - prompt_width) / 2;
    let prompt_y = height - 60;
    font::font_draw_string(
        prompt_x,
        prompt_y,
        prompt.as_ptr() as *const c_char,
        PANIC_FG_COLOR,
        PANIC_BG_COLOR,
    );

    // Draw a small note about serial output
    let serial_note = b"(Debug output also available on serial console)\0";
    let note_len = 47;
    let note_width = note_len * char_width;
    let note_x = (width - note_width) / 2;
    let note_y = height - 40;
    font::font_draw_string(
        note_x,
        note_y,
        serial_note.as_ptr() as *const c_char,
        0xFF888888, // Gray
        PANIC_BG_COLOR,
    );

    framebuffer::framebuffer_flush();

    true
}
