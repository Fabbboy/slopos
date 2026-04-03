use slopos_abi::draw::Color32;
use slopos_gfx::DrawBuffer;

use super::constraints::Rect;
use super::style::StyleSheet;

/// Paint context wrapping a DrawBuffer with clip rect, scroll offset, and theme.
///
/// Widgets only paint within their clip rect. The framework sets up
/// the clip rect before calling each widget's `paint()`.
pub struct PaintContext<'a> {
    pub buffer: &'a mut DrawBuffer<'a>,
    /// Current clip rect in window coordinates.
    pub clip: Rect,
    /// Accumulated scroll offset from ancestor ScrollViews.
    pub scroll_offset_x: i32,
    pub scroll_offset_y: i32,
    /// Whether keyboard focus indicators should be rendered.
    pub focus_visible: bool,
    /// Theme reference.
    pub style: &'a StyleSheet,
}

impl<'a> PaintContext<'a> {
    pub fn new(buffer: &'a mut DrawBuffer<'a>, style: &'a StyleSheet) -> Self {
        let w = buffer.width() as i32;
        let h = buffer.height() as i32;
        Self {
            buffer,
            clip: Rect::new(0, 0, w, h),
            scroll_offset_x: 0,
            scroll_offset_y: 0,
            focus_visible: false,
            style,
        }
    }

    /// Fill a rectangle, clipped to the current clip rect.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        let rect = Rect::new(x, y, w, h);
        if let Some(clipped) = rect.intersect(&self.clip) {
            let dr = clipped.to_damage_rect();
            slopos_gfx::canvas_ops::fill_rect_clipped(self.buffer, x, y, w, h, color, &dr);
        }
    }

    /// Fill a rectangle with alpha blending, clipped.
    pub fn fill_rect_blended(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        let rect = Rect::new(x, y, w, h);
        if let Some(clipped) = rect.intersect(&self.clip) {
            let dr = clipped.to_damage_rect();
            slopos_gfx::blend::fill_rect_blended_clipped(self.buffer, x, y, w, h, color, &dr);
        }
    }

    /// Draw a 1px outline rectangle, clipped.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        // Top edge
        self.fill_rect(x, y, w, 1, color);
        // Bottom edge
        self.fill_rect(x, y + h - 1, w, 1, color);
        // Left edge
        self.fill_rect(x, y + 1, 1, h - 2, color);
        // Right edge
        self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
    }

    /// Draw a rounded filled rectangle, clipped.
    pub fn fill_rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        color: Color32,
    ) {
        // Use the gfx crate's rounded rect, which handles its own bounds checking.
        slopos_gfx::canvas_ops::rounded_rect_filled(self.buffer, x, y, w, h, radius, color);
    }

    /// Draw a rounded outline rectangle.
    pub fn draw_rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        color: Color32,
    ) {
        slopos_gfx::canvas_ops::rounded_rect(self.buffer, x, y, w, h, radius, color);
    }

    /// Draw text at position, clipped to the current clip rect.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
        let dr = self.clip.to_damage_rect();
        crate::text::draw_str_clipped(self.buffer, x, y, text, fg, bg, &dr);
    }

    /// Draw text with a transparent background (alpha-blended onto existing content).
    pub fn draw_text_transparent(&mut self, x: i32, y: i32, text: &str, fg: Color32) {
        let dr = self.clip.to_damage_rect();
        crate::text::draw_str_clipped(self.buffer, x, y, text, fg, Color32::TRANSPARENT, &dr);
    }

    /// Measure text width in pixels.
    pub fn text_width(&self, text: &str) -> i32 {
        crate::text::string_width(text)
    }

    /// Cell height for the current font.
    pub fn text_height(&self) -> i32 {
        crate::text::cell_height()
    }

    /// Draw a standard focus ring around the given rect.
    /// 2px outline offset 1px outside the rect in the accent color.
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        if !self.focus_visible {
            return;
        }
        let offset = self.style.focus_ring_offset;
        let width = self.style.focus_ring_width;
        let color = self.style.focus_ring_color;
        let x = rect.x - offset - width;
        let y = rect.y - offset - width;
        let w = rect.width + (offset + width) * 2;
        let _h = rect.height + (offset + width) * 2;
        let inner_x = rect.x - offset;
        let inner_y = rect.y - offset;
        let inner_w = rect.width + offset * 2;
        let inner_h = rect.height + offset * 2;

        // Top
        self.fill_rect_blended(x, y, w, width, color);
        // Bottom
        self.fill_rect_blended(x, inner_y + inner_h, w, width, color);
        // Left
        self.fill_rect_blended(x, inner_y, width, inner_h, color);
        // Right
        self.fill_rect_blended(inner_x + inner_w, inner_y, width, inner_h, color);
    }

    /// Execute a closure with a tighter clip rect (intersection of current and new).
    /// Used when painting children that need clipping (e.g. scroll views).
    pub fn with_clip<F: FnOnce(&mut PaintContext<'_>)>(&mut self, new_clip: Rect, f: F) {
        let old_clip = self.clip;
        self.clip = self.clip.intersect(&new_clip).unwrap_or(Rect::ZERO);
        f(self);
        self.clip = old_clip;
    }

    /// Execute a closure with adjusted scroll offset.
    pub fn with_scroll_offset<F: FnOnce(&mut PaintContext<'_>)>(&mut self, dx: i32, dy: i32, f: F) {
        let old_x = self.scroll_offset_x;
        let old_y = self.scroll_offset_y;
        self.scroll_offset_x += dx;
        self.scroll_offset_y += dy;
        f(self);
        self.scroll_offset_x = old_x;
        self.scroll_offset_y = old_y;
    }

    /// Is the keyboard focus visible? (keyboard-driven navigation active)
    pub fn is_focus_visible(&self) -> bool {
        self.focus_visible
    }
}
