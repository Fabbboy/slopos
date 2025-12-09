//! C FFI Layer - Export Rust graphics to C
//!
//! Provides C-compatible functions for canvas creation, drawing primitives,
//! and buffer operations. Existing C code (roulette, splash) can use these.

use core::ptr;
use alloc::boxed::Box;
use crate::buffer::{BufferObject, DisplayBuffer, Color, PixelFormat};
use crate::canvas::{Canvas, RenderTarget};

/// Opaque handle for Canvas (passed to C as pointer)
#[repr(C)]
pub struct RustCanvas {
    _private: [u8; 0],
}

/// Convert Color to/from u32 in RGBA8888 format
fn color_to_u32(color: Color) -> u32 {
    ((color.a as u32) << 24) | ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32)
}

fn color_from_u32(value: u32) -> Color {
    Color::rgba(
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
        ((value >> 24) & 0xFF) as u8,
    )
}

// ========================================================================
// CANVAS CREATION & DESTRUCTION
// ========================================================================

/// Create a canvas rendering to an off-screen buffer
///
/// # Arguments
/// * `width` - Buffer width in pixels
/// * `height` - Buffer height in pixels
/// * `bpp` - Bits per pixel (16, 24, or 32)
///
/// # Returns
/// Opaque canvas handle, or NULL on error
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_create_buffer(width: u32, height: u32, bpp: u8) -> *mut RustCanvas {
    let format = match PixelFormat::from_bpp(bpp) {
        Some(f) => f,
        None => return ptr::null_mut(),
    };

    match Canvas::new_buffer(width, height, format) {
        Some(canvas) => Box::into_raw(Box::new(canvas)) as *mut RustCanvas,
        None => ptr::null_mut(),
    }
}

/// Create a canvas rendering to the display framebuffer
///
/// # Returns
/// Opaque canvas handle, or NULL if no framebuffer available
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_create_display() -> *mut RustCanvas {
    match Canvas::new_display() {
        Some(canvas) => Box::into_raw(Box::new(canvas)) as *mut RustCanvas,
        None => ptr::null_mut(),
    }
}

/// Destroy a canvas and free its resources
///
/// # Safety
/// The canvas pointer must be valid and not used after this call
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_destroy(canvas: *mut RustCanvas) {
    if !canvas.is_null() {
        let _ = unsafe { Box::from_raw(canvas as *mut Canvas) };
    }
}

/// Get canvas width
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_width(canvas: *const RustCanvas) -> u32 {
    if canvas.is_null() {
        return 0;
    }
    let canvas = unsafe { &*(canvas as *const Canvas) };
    canvas.width()
}

/// Get canvas height
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_height(canvas: *const RustCanvas) -> u32 {
    if canvas.is_null() {
        return 0;
    }
    let canvas = unsafe { &*(canvas as *const Canvas) };
    canvas.height()
}

// ========================================================================
// CANVAS STATE MANAGEMENT
// ========================================================================

/// Save current canvas state onto the stack
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_save(canvas: *mut RustCanvas) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.save();
}

/// Restore canvas state from the stack
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_restore(canvas: *mut RustCanvas) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.restore();
}

// ========================================================================
// TRANSFORMATIONS
// ========================================================================

/// Translate canvas origin
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_translate(canvas: *mut RustCanvas, x: f32, y: f32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.translate(x, y);
}

/// Rotate canvas (angle in radians)
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_rotate(canvas: *mut RustCanvas, angle: f32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.rotate(angle);
}

/// Scale canvas
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_scale(canvas: *mut RustCanvas, sx: f32, sy: f32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.scale(sx, sy);
}

/// Reset transformation to identity
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_reset_transform(canvas: *mut RustCanvas) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.reset_transform();
}

// ========================================================================
// DRAWING ATTRIBUTES
// ========================================================================

/// Set fill color (RGBA8888 format: 0xAABBGGRR)
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_set_fill_color(canvas: *mut RustCanvas, color: u32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.set_fill_color(color_from_u32(color));
}

/// Set stroke color (RGBA8888 format: 0xAABBGGRR)
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_set_stroke_color(canvas: *mut RustCanvas, color: u32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.set_stroke_color(color_from_u32(color));
}

/// Set stroke width
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_set_stroke_width(canvas: *mut RustCanvas, width: f32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.set_stroke_width(width);
}

// ========================================================================
// DRAWING PRIMITIVES
// ========================================================================

/// Clear entire canvas to specified color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_clear(canvas: *mut RustCanvas, color: u32) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.clear(color_from_u32(color));
}

/// Fill rectangle with current fill color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_fill_rect(
    canvas: *mut RustCanvas,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.fill_rect(x, y, width, height);
}

/// Stroke rectangle outline with current stroke color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_stroke_rect(
    canvas: *mut RustCanvas,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.stroke_rect(x, y, width, height);
}

/// Draw line with current stroke color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_draw_line(
    canvas: *mut RustCanvas,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.draw_line(x1, y1, x2, y2);
}

/// Fill circle with current fill color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_fill_circle(
    canvas: *mut RustCanvas,
    cx: f32,
    cy: f32,
    radius: f32,
) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.fill_circle(cx, cy, radius);
}

/// Stroke circle outline with current stroke color
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_stroke_circle(
    canvas: *mut RustCanvas,
    cx: f32,
    cy: f32,
    radius: f32,
) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &mut *(canvas as *mut Canvas) };
    canvas.stroke_circle(cx, cy, radius);
}

// ========================================================================
// COLOR HELPERS
// ========================================================================

/// Create RGBA color value (0xAABBGGRR format)
#[no_mangle]
pub extern "C" fn rust_color_rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    color_to_u32(Color::rgba(r, g, b, a))
}

/// Create RGB color value with full alpha (0xAABBGGRR format)
#[no_mangle]
pub extern "C" fn rust_color_rgb(r: u8, g: u8, b: u8) -> u32 {
    color_to_u32(Color::rgb(r, g, b))
}

// ========================================================================
// BUFFER OPERATIONS
// ========================================================================

/// Present off-screen canvas buffer to display
///
/// Only works if canvas was created with rust_canvas_create_buffer()
/// and a display framebuffer is available.
#[no_mangle]
pub unsafe extern "C" fn rust_canvas_present_to_display(canvas: *const RustCanvas) {
    if canvas.is_null() {
        return;
    }
    let canvas = unsafe { &*(canvas as *const Canvas) };

    // Try to get display buffer
    if let Some(mut display) = DisplayBuffer::from_limine() {
        canvas.present_to_display(&mut display);
    }
}
