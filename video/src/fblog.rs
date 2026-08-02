//! On-screen kernel-log console (fblog) renderer.
//!
//! Draws the tail of the serial capture ring ([`slopos_ostd::fblog`]) onto the
//! Limine framebuffer using the kernel glyph atlas — the panic-screen drawing
//! path, but for the live boot log. This is the only console available on real
//! hardware with no serial port.
//!
//! **Repaint only on change.** A full-screen clear-then-redraw every frame
//! flickers (a brief all-background flash at the tick rate). Like `fbcon`, this
//! redraws only when the log content (or visibility) actually changes — and
//! while fblog is shown the boot is paused (the Wheel of Fate freezes), so the
//! log is static and simply isn't repainted, which is what makes it flicker
//! free. Nothing else draws over it meanwhile: compositor flips are suppressed
//! and the wheel pauses *before* painting, so there is no bleed to chase.
//!
//! The renderer is registered with the ostd core and invoked from the scheduler
//! timer tick (never from inside a log call), so it works even when userland is
//! wedged. All shared state is taken with `try_lock`, so it is interrupt-safe.

use core::sync::atomic::{AtomicU64, Ordering};
use slopos_ostd::lock_class;

use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;
use slopos_ostd::fblog as core_fblog;
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

use crate::graphics::GraphicsContext;
use crate::kernel_font;

const BG: Color32 = Color32(0xFF0A0A0E);
const FG: Color32 = Color32(0xFFCBD3DE);
const HEADER_FG: Color32 = Color32(0xFF7FE08A);
const MARGIN: i32 = 8;

/// Scratch for the copied ring tail, kept off the stack (2 KiB frame cap).
const SCRATCH_LEN: usize = 24 * 1024;
static SCRATCH: SpinLock<[u8; SCRATCH_LEN]> = SpinLock::new(
    [0u8; SCRATCH_LEN],
    lock_class!("fblog.SCRATCH", LOCK_LEVEL_UNORDERED),
);

/// Ring seq at the last repaint — repaint again only when it changes (or on an
/// ESC toggle, signalled separately via `take_render_dirty`).
static LAST_SEQ: AtomicU64 = AtomicU64::new(u64::MAX);

/// Register the renderer with the ostd fblog core. Call once at video init.
pub fn init() {
    core_fblog::register_renderer(render);
}

fn render() {
    // Repaint when an ESC toggle happened (`dirty`) or the log content changed;
    // while the log is shown the boot is paused, so a static log isn't
    // repainted — that is what makes it flicker free. The dirty flag, set on
    // every toggle, ensures rapid ESC presses can't leave us stuck not
    // repainting (a stale `active` comparison would).
    let dirty = core_fblog::take_render_dirty();
    let active = core_fblog::is_active();
    let seq = core_fblog::ring_seq();

    if !dirty && seq == LAST_SEQ.load(Ordering::Relaxed) {
        return;
    }
    LAST_SEQ.store(seq, Ordering::Relaxed);

    if !active {
        // Hidden: the compositor (or vconsole) repaints the real screen.
        return;
    }

    let mut ctx = match GraphicsContext::new() {
        Ok(c) => c,
        Err(_) => return,
    };
    let atlas = match kernel_font::atlas() {
        Some(a) => a,
        None => return,
    };
    let mut scratch = match SCRATCH.try_lock() {
        Some(s) => s,
        None => return,
    };
    let n = core_fblog::ring_copy_tail(&mut scratch[..]);
    let data = &scratch[..n];

    draw_log(&mut ctx, &atlas, data);
    ctx.flush();
}

fn draw_log<C: Canvas>(canvas: &mut C, atlas: &GlyphAtlas, data: &[u8]) {
    let width = canvas.width() as i32;
    let height = canvas.height() as i32;
    let cw = atlas.cell_width().max(1);
    let cell_h = atlas.cell_height().max(1);
    let header_h = cell_h + 6;
    let cols = ((width - 2 * MARGIN) / cw).max(1) as usize;
    let max_rows = (((height - 2 * MARGIN - header_h) / cell_h).max(1)) as usize;

    let bg_px = canvas.pixel_format().encode(BG);
    canvas.clear_canvas(bg_px);
    atlas.draw_bytes(
        canvas,
        MARGIN,
        MARGIN,
        b"== SlopOS kernel log == (ESC toggles)\0",
        HEADER_FG,
        BG,
    );

    let start = tail_start(data, max_rows);
    let mut y = MARGIN + header_h;
    let mut line_start = start;
    let mut i = start;
    while i <= data.len() {
        let at_end = i == data.len();
        if at_end || data[i] == b'\n' {
            if i > line_start {
                draw_line(canvas, atlas, &data[line_start..i], cols, cw, y);
            }
            y += cell_h;
            if y + cell_h > height - MARGIN {
                break;
            }
            line_start = i + 1;
        }
        i += 1;
    }
}

fn draw_line<C: Canvas>(
    canvas: &mut C,
    atlas: &GlyphAtlas,
    line: &[u8],
    cols: usize,
    cw: i32,
    y: i32,
) {
    let mut x = MARGIN;
    for (count, &b) in line.iter().enumerate() {
        if count >= cols {
            break;
        }
        // Printable ASCII only; control bytes / stray ANSI render as a space.
        let glyph = if (0x20..0x7f).contains(&b) { b } else { b' ' };
        atlas.draw_char(canvas, x, y, glyph as u32, FG, BG);
        x += cw;
    }
}

/// Byte index where the last `rows` newline-delimited lines begin. A single
/// trailing newline is ignored so it doesn't count as an empty final line.
fn tail_start(data: &[u8], rows: usize) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut end = data.len();
    if data[end - 1] == b'\n' {
        end -= 1;
    }
    let mut seen = 0usize;
    let mut i = end;
    while i > 0 {
        if data[i - 1] == b'\n' {
            seen += 1;
            if seen >= rows {
                return i;
            }
        }
        i -= 1;
    }
    0
}
