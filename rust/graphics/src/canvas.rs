//! Canvas API - High-level vector graphics interface
//!
//! Provides a seamless, stateful drawing API inspired by Vello/Skia/HTML5 Canvas.
//! Features:
//! - State stack (save/restore)
//! - Affine transforms (translate, rotate, scale)
//! - Fill and stroke operations
//! - Path building and rendering

use alloc::vec::Vec;
use euclid::{Transform2D, Point2D, Vector2D, Angle};
use crate::buffer::{BufferObject, DisplayBuffer, Color, Rect, PixelFormat};

// Define coordinate space types for transforms
pub struct CanvasSpace;
pub struct ScreenSpace;

/// Canvas rendering target
pub enum RenderTarget {
    /// Render to off-screen buffer
    Buffer(BufferObject),
    /// Render to display framebuffer
    Display(DisplayBuffer),
}

impl RenderTarget {
    /// Get width of render target
    pub fn width(&self) -> u32 {
        match self {
            RenderTarget::Buffer(buf) => buf.width(),
            RenderTarget::Display(buf) => buf.width(),
        }
    }

    /// Get height of render target
    pub fn height(&self) -> u32 {
        match self {
            RenderTarget::Buffer(buf) => buf.height(),
            RenderTarget::Display(buf) => buf.height(),
        }
    }

    /// Get pixel format
    pub fn format(&self) -> PixelFormat {
        match self {
            RenderTarget::Buffer(buf) => buf.format(),
            RenderTarget::Display(buf) => buf.format(),
        }
    }

    /// Clear render target
    pub fn clear(&mut self, color: Color) {
        match self {
            RenderTarget::Buffer(buf) => buf.clear(color),
            RenderTarget::Display(buf) => buf.clear(color),
        }
    }

    /// Set pixel (internal)
    fn set_pixel(&mut self, x: u32, y: u32, color: Color) -> bool {
        match self {
            RenderTarget::Buffer(buf) => buf.set_pixel(x, y, color),
            RenderTarget::Display(buf) => buf.set_pixel(x, y, color),
        }
    }

    /// Get pixel (internal)
    fn get_pixel(&self, x: u32, y: u32) -> Option<Color> {
        match self {
            RenderTarget::Buffer(buf) => buf.get_pixel(x, y),
            RenderTarget::Display(buf) => buf.get_pixel(x, y),
        }
    }
}

/// Canvas state (can be saved/restored)
#[derive(Clone)]
struct CanvasState {
    /// Current transformation matrix
    transform: Transform2D<f32, CanvasSpace, ScreenSpace>,
    /// Fill color
    fill_color: Color,
    /// Stroke color
    stroke_color: Color,
    /// Stroke width in pixels
    stroke_width: f32,
    /// Clipping rectangle (in canvas coordinates)
    clip_rect: Option<Rect>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            transform: Transform2D::identity(),
            fill_color: Color::black(),
            stroke_color: Color::black(),
            stroke_width: 1.0,
            clip_rect: None,
        }
    }
}

/// High-level canvas for vector graphics
pub struct Canvas {
    /// Render target (buffer or display)
    target: RenderTarget,
    /// Current drawing state
    state: CanvasState,
    /// State stack for save/restore
    state_stack: Vec<CanvasState>,
}

impl Canvas {
    /// Create canvas rendering to off-screen buffer
    pub fn new_buffer(width: u32, height: u32, format: PixelFormat) -> Option<Self> {
        let buffer = BufferObject::new(width, height, format)?;
        Some(Self {
            target: RenderTarget::Buffer(buffer),
            state: CanvasState::default(),
            state_stack: Vec::new(),
        })
    }

    /// Create canvas rendering to display framebuffer
    pub fn new_display() -> Option<Self> {
        let display = DisplayBuffer::from_limine()?;
        Some(Self {
            target: RenderTarget::Display(display),
            state: CanvasState::default(),
            state_stack: Vec::new(),
        })
    }

    /// Get canvas width
    pub fn width(&self) -> u32 {
        self.target.width()
    }

    /// Get canvas height
    pub fn height(&self) -> u32 {
        self.target.height()
    }

    // ========================================================================
    // STATE MANAGEMENT
    // ========================================================================

    /// Save current canvas state onto the stack
    pub fn save(&mut self) {
        self.state_stack.push(self.state.clone());
    }

    /// Restore canvas state from the stack
    pub fn restore(&mut self) {
        if let Some(state) = self.state_stack.pop() {
            self.state = state;
        }
    }

    // ========================================================================
    // TRANSFORMATION
    // ========================================================================

    /// Translate canvas origin
    pub fn translate(&mut self, x: f32, y: f32) -> &mut Self {
        self.state.transform = self.state.transform.then_translate(Vector2D::new(x, y));
        self
    }

    /// Rotate canvas (angle in radians)
    pub fn rotate(&mut self, angle: f32) -> &mut Self {
        self.state.transform = self.state.transform.then_rotate(Angle::radians(angle));
        self
    }

    /// Scale canvas
    pub fn scale(&mut self, sx: f32, sy: f32) -> &mut Self {
        self.state.transform = self.state.transform.then_scale(sx, sy);
        self
    }

    /// Reset transformation to identity
    pub fn reset_transform(&mut self) -> &mut Self {
        self.state.transform = Transform2D::identity();
        self
    }

    /// Transform point from canvas space to screen space
    fn transform_point(&self, x: f32, y: f32) -> (i32, i32) {
        let point = self.state.transform.transform_point(Point2D::new(x, y));
        // Manual rounding for no_std
        ((point.x + 0.5) as i32, (point.y + 0.5) as i32)
    }

    // ========================================================================
    // DRAWING ATTRIBUTES
    // ========================================================================

    /// Set fill color
    pub fn set_fill_color(&mut self, color: Color) -> &mut Self {
        self.state.fill_color = color;
        self
    }

    /// Set stroke color
    pub fn set_stroke_color(&mut self, color: Color) -> &mut Self {
        self.state.stroke_color = color;
        self
    }

    /// Set stroke width
    pub fn set_stroke_width(&mut self, width: f32) -> &mut Self {
        self.state.stroke_width = width.max(0.0);
        self
    }

    /// Set clipping rectangle
    pub fn set_clip_rect(&mut self, rect: Option<Rect>) -> &mut Self {
        self.state.clip_rect = rect;
        self
    }

    // ========================================================================
    // BASIC DRAWING PRIMITIVES
    // ========================================================================

    /// Clear entire canvas to specified color
    pub fn clear(&mut self, color: Color) -> &mut Self {
        self.target.clear(color);
        self
    }

    /// Clear rectangle to transparent/background
    pub fn clear_rect(&mut self, x: f32, y: f32, width: f32, height: f32) -> &mut Self {
        self.fill_rect_internal(x, y, width, height, Color::transparent());
        self
    }

    /// Fill rectangle with current fill color
    pub fn fill_rect(&mut self, x: f32, y: f32, width: f32, height: f32) -> &mut Self {
        let color = self.state.fill_color;
        self.fill_rect_internal(x, y, width, height, color);
        self
    }

    /// Internal rectangle fill implementation
    fn fill_rect_internal(&mut self, x: f32, y: f32, width: f32, height: f32, color: Color) {
        // Transform corners
        let (x1, y1) = self.transform_point(x, y);
        let (x2, y2) = self.transform_point(x + width, y + height);

        // Calculate bounds
        let min_x = x1.min(x2).max(0);
        let min_y = y1.min(y2).max(0);
        let max_x = x1.max(x2).min(self.width() as i32 - 1);
        let max_y = y1.max(y2).min(self.height() as i32 - 1);

        // Fill pixels
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                // Check clipping
                if let Some(clip) = &self.state.clip_rect {
                    if !clip.contains(px, py) {
                        continue;
                    }
                }

                self.target.set_pixel(px as u32, py as u32, color);
            }
        }
    }

    /// Stroke rectangle outline with current stroke color
    pub fn stroke_rect(&mut self, x: f32, y: f32, width: f32, height: f32) -> &mut Self {
        let color = self.state.stroke_color;
        let stroke_width = self.state.stroke_width;

        // Draw four sides
        self.draw_line_internal(x, y, x + width, y, color, stroke_width);
        self.draw_line_internal(x + width, y, x + width, y + height, color, stroke_width);
        self.draw_line_internal(x + width, y + height, x, y + height, color, stroke_width);
        self.draw_line_internal(x, y + height, x, y, color, stroke_width);

        self
    }

    /// Draw line from (x1, y1) to (x2, y2) with current stroke color
    pub fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) -> &mut Self {
        let color = self.state.stroke_color;
        let width = self.state.stroke_width;
        self.draw_line_internal(x1, y1, x2, y2, color, width);
        self
    }

    /// Internal line drawing (Bresenham's algorithm)
    fn draw_line_internal(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, color: Color, _width: f32) {
        // Transform endpoints
        let (mut x1, mut y1) = self.transform_point(x1, y1);
        let (x2, y2) = self.transform_point(x2, y2);

        // Bresenham's line algorithm
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;

        loop {
            // Draw pixel if in bounds
            if x1 >= 0 && y1 >= 0 && x1 < self.width() as i32 && y1 < self.height() as i32 {
                // Check clipping
                if let Some(clip) = &self.state.clip_rect {
                    if clip.contains(x1, y1) {
                        self.target.set_pixel(x1 as u32, y1 as u32, color);
                    }
                } else {
                    self.target.set_pixel(x1 as u32, y1 as u32, color);
                }
            }

            if x1 == x2 && y1 == y2 {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x1 += sx;
            }
            if e2 < dx {
                err += dx;
                y1 += sy;
            }
        }
    }

    /// Fill circle with current fill color
    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32) -> &mut Self {
        let color = self.state.fill_color;
        self.fill_circle_internal(cx, cy, radius, color);
        self
    }

    /// Internal circle fill implementation
    fn fill_circle_internal(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        // Transform center
        let (cx, cy) = self.transform_point(cx, cy);
        // Manual rounding for no_std
        let radius = ((radius * self.state.transform.m11.abs()) + 0.5) as i32;

        let radius_sq = radius * radius;

        // Bounding box
        let min_x = (cx - radius).max(0);
        let min_y = (cy - radius).max(0);
        let max_x = (cx + radius).min(self.width() as i32 - 1);
        let max_y = (cy + radius).min(self.height() as i32 - 1);

        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let dx = x - cx;
                let dy = y - cy;

                if dx * dx + dy * dy <= radius_sq {
                    // Check clipping
                    if let Some(clip) = &self.state.clip_rect {
                        if !clip.contains(x, y) {
                            continue;
                        }
                    }

                    self.target.set_pixel(x as u32, y as u32, color);
                }
            }
        }
    }

    /// Stroke circle outline with current stroke color
    pub fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32) -> &mut Self {
        let color = self.state.stroke_color;
        self.stroke_circle_internal(cx, cy, radius, color);
        self
    }

    /// Internal circle stroke implementation (midpoint circle algorithm)
    fn stroke_circle_internal(&mut self, cx: f32, cy: f32, radius: f32, color: Color) -> &mut Self {
        // Transform center
        let (cx, cy) = self.transform_point(cx, cy);
        // Manual rounding for no_std
        let radius = ((radius * self.state.transform.m11.abs()) + 0.5) as i32;

        let mut x = 0;
        let mut y = radius;
        let mut d = 1 - radius;

        // Helper to draw 8 octants
        let mut plot = |x: i32, y: i32| {
            let points = [
                (cx + x, cy + y),
                (cx - x, cy + y),
                (cx + x, cy - y),
                (cx - x, cy - y),
                (cx + y, cy + x),
                (cx - y, cy + x),
                (cx + y, cy - x),
                (cx - y, cy - x),
            ];

            for (px, py) in points {
                if px >= 0 && py >= 0 && px < self.width() as i32 && py < self.height() as i32 {
                    // Check clipping
                    if let Some(clip) = &self.state.clip_rect {
                        if !clip.contains(px, py) {
                            continue;
                        }
                    }

                    self.target.set_pixel(px as u32, py as u32, color);
                }
            }
        };

        plot(x, y);

        while x < y {
            if d < 0 {
                d += 2 * x + 3;
            } else {
                d += 2 * (x - y) + 5;
                y -= 1;
            }
            x += 1;

            plot(x, y);
        }

        self
    }

    // ========================================================================
    // BUFFER OPERATIONS
    // ========================================================================

    /// Get mutable reference to underlying buffer (if rendering to BufferObject)
    pub fn buffer_mut(&mut self) -> Option<&mut BufferObject> {
        match &mut self.target {
            RenderTarget::Buffer(buf) => Some(buf),
            _ => None,
        }
    }

    /// Get reference to underlying buffer (if rendering to BufferObject)
    pub fn buffer(&self) -> Option<&BufferObject> {
        match &self.target {
            RenderTarget::Buffer(buf) => Some(buf),
            _ => None,
        }
    }

    /// Blit canvas buffer to display (if canvas is rendering to BufferObject)
    pub fn present_to_display(&self, display: &mut DisplayBuffer) {
        if let RenderTarget::Buffer(buf) = &self.target {
            let src_rect = buf.rect();
            display.blit_from(buf, src_rect, 0, 0);
        }
    }
}

/// Builder pattern helpers
impl Canvas {
    /// Fill rectangle (builder style)
    pub fn with_fill_rect(mut self, x: f32, y: f32, width: f32, height: f32) -> Self {
        self.fill_rect(x, y, width, height);
        self
    }

    /// Set fill color (builder style)
    pub fn with_fill_color(mut self, color: Color) -> Self {
        self.set_fill_color(color);
        self
    }
}
