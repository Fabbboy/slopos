# Plan 001: Unified Graphics Architecture

**Status**: Approved, Ready for Implementation  
**Author**: Sisyphus  
**Date**: 2026-01-10  
**Priority**: High  
**Estimated Impact**: ~600 lines net code reduction, eliminates major DRY violation

---

## Problem Statement

SlopOS has ~890 lines of duplicated graphics rendering code between kernel (`video/`) and userland (`userland/src/gfx/`):

| Component | Kernel Location | Userland Location | Duplicated Lines |
|-----------|-----------------|-------------------|------------------|
| Bresenham Line | `video/src/graphics.rs:148-194` | `userland/src/gfx/primitives.rs:67-119` | ~100 |
| Midpoint Circle | `video/src/graphics.rs:288-345` | `userland/src/gfx/primitives.rs:122-174` | ~100 |
| Filled Circle | `video/src/graphics.rs:347-373` | `userland/src/gfx/primitives.rs:177-256` | ~150 |
| Fill Rect | `video/src/graphics.rs:215-286` | `userland/src/gfx/primitives.rs:9-64` | ~130 |
| Font Rendering | `video/src/font.rs:43-153` | `userland/src/gfx/font.rs:9-90` | ~100 |
| Pixel Format Conversion | `video/src/graphics.rs:53-63` | `userland/src/gfx/mod.rs:189-200` | ~30 |
| DamageRect/Tracker | `video/src/compositor_context.rs:31-160` | `userland/src/gfx/mod.rs:14-167` | ~280 |

**Root Cause**: Kernel code uses `write_volatile` to raw framebuffer pointers (unsafe), while userland uses safe `&mut [u8]` slice operations. The *algorithms* are identical; only the *pixel write mechanism* differs.

---

## Solution Overview

Create a `DrawTarget` trait in the `abi` crate that abstracts over pixel write operations. All drawing algorithms become generic functions that work with any `impl DrawTarget`.

**Inspiration**: 
- `embedded-graphics` crate's `DrawTarget` trait (industry standard for Rust embedded graphics)
- rCore/zCore OS projects use this same pattern

**Key Design Decisions**:
1. **Native trait, not dependency**: We create our own trait tailored to SlopOS, not import `embedded-graphics`
2. **u32 colors**: Use `u32` RGBA everywhere (no generic color types)
3. **Integrated with existing types**: Works with our `DrawPixelFormat` from `abi/src/pixel.rs`
4. **Optional damage tracking**: Separate `DamageTracking` trait for compositor use cases

---

## Architecture

```
+------------------------------------------------------------------+
|                         abi crate                                 |
+------------------------------------------------------------------+
|  pixel.rs          - PixelFormat, DrawPixelFormat (EXISTS)       |
|  font.rs           - FONT_DATA, get_glyph (EXISTS)               |
|  damage.rs         - DamageRect, DamageTracker (NEW)             |
|  draw.rs           - DrawTarget trait (NEW)                      |
|  draw_primitives.rs - line, circle, rect algorithms (NEW)        |
|  font_render.rs    - draw_char, draw_string algorithms (NEW)     |
+------------------------------------------------------------------+
                              |
          +-------------------+-------------------+
          v                   v                   v
+------------------+  +------------------+  +------------------+
|   video crate    |  | userland crate   |  |  future crates   |
+------------------+  +------------------+  +------------------+
| impl DrawTarget  |  | impl DrawTarget  |  | impl DrawTarget  |
| for Graphics-    |  | for DrawBuffer   |  | for TestBuffer   |
| Context          |  |                  |  | (unit tests)     |
| (volatile MMIO)  |  | (safe slices)    |  |                  |
+------------------+  +------------------+  +------------------+
```

---

## New Files

### 1. `abi/src/damage.rs` - Unified Damage Tracking

```rust
//! Damage tracking for compositor and rendering
//!
//! This replaces duplicate implementations in:
//! - video/src/compositor_context.rs (lines 31-160)
//! - userland/src/gfx/mod.rs (lines 14-167)

/// Maximum damage regions before automatic merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

/// A rectangular damage region in buffer-local coordinates
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,  // inclusive
    pub y1: i32,  // inclusive
}

impl DamageRect {
    /// Create an invalid (empty) damage rect
    #[inline]
    pub const fn invalid() -> Self {
        Self { x0: 0, y0: 0, x1: -1, y1: -1 }
    }

    /// Check if this rect is valid (non-empty)
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    /// Calculate the area of this rect
    #[inline]
    pub fn area(&self) -> i32 {
        if !self.is_valid() { 0 } else { (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1) }
    }

    /// Compute the union (bounding box) of two rects
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Clip this rect to buffer bounds
    #[inline]
    pub fn clip(&self, width: i32, height: i32) -> Self {
        Self {
            x0: self.x0.max(0),
            y0: self.y0.max(0),
            x1: self.x1.min(width - 1),
            y1: self.y1.min(height - 1),
        }
    }
}

/// Tracks damage regions with automatic merging when at capacity
#[derive(Clone)]
pub struct DamageTracker {
    regions: [DamageRect; MAX_DAMAGE_REGIONS],
    count: u8,
}

impl Default for DamageTracker {
    fn default() -> Self { Self::new() }
}

impl DamageTracker {
    /// Create an empty damage tracker
    pub const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            count: 0,
        }
    }

    /// Add a damage region, merging if at capacity
    pub fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() { return; }
        
        if (self.count as usize) >= MAX_DAMAGE_REGIONS {
            self.merge_smallest_pair();
        }
        
        if (self.count as usize) < MAX_DAMAGE_REGIONS {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        }
    }

    /// Add damage by coordinates
    #[inline]
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.add(DamageRect { x0, y0, x1, y1 });
    }

    fn merge_smallest_pair(&mut self) {
        if self.count < 2 { return; }
        
        let count = self.count as usize;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        for i in 0..count {
            for j in (i + 1)..count {
                let combined = self.regions[i].union(&self.regions[j]).area();
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        self.regions[best_i] = self.regions[best_i].union(&self.regions[best_j]);
        if best_j < count - 1 {
            self.regions[best_j] = self.regions[count - 1];
        }
        self.count -= 1;
    }

    /// Clear all damage
    #[inline]
    pub fn clear(&mut self) { self.count = 0; }

    /// Get the number of damage regions
    #[inline]
    pub fn count(&self) -> u8 { self.count }

    /// Get the damage regions slice
    #[inline]
    pub fn regions(&self) -> &[DamageRect] { &self.regions[..self.count as usize] }

    /// Get the bounding box of all damage
    pub fn bounding_box(&self) -> DamageRect {
        if self.count == 0 { return DamageRect::invalid(); }
        let mut result = self.regions[0];
        for i in 1..self.count as usize {
            result = result.union(&self.regions[i]);
        }
        result
    }

    /// Check if there is any damage
    #[inline]
    pub fn is_dirty(&self) -> bool { self.count > 0 }
}
```

### 2. `abi/src/draw.rs` - Core DrawTarget Trait

```rust
//! Core drawing abstraction for SlopOS
//!
//! This module defines the `DrawTarget` trait that abstracts over different
//! pixel buffer implementations (kernel framebuffer with volatile writes,
//! userland shared memory with safe slice operations).
//!
//! Inspired by embedded-graphics but tailored for SlopOS:
//! - Uses u32 RGBA colors (not generic color types)
//! - Integrates with our DrawPixelFormat
//! - Includes optional damage tracking

use crate::pixel::DrawPixelFormat;

/// Core trait for any drawable surface.
///
/// This is the primary abstraction for rendering. Implementations exist for:
/// - Kernel GraphicsContext (volatile MMIO writes)
/// - Userland DrawBuffer (safe slice writes)
///
/// # Required Methods
/// Only `draw_pixel` is required. All other methods have default implementations
/// that use `draw_pixel`, but can be overridden for performance.
///
/// # Color Format
/// All colors are passed as pre-converted u32 values (already in the target's
/// pixel format). Use `pixel_format().convert_color(rgba)` before drawing.
pub trait DrawTarget {
    /// Surface width in pixels
    fn width(&self) -> u32;
    
    /// Surface height in pixels
    fn height(&self) -> u32;
    
    /// Row stride in bytes
    fn pitch(&self) -> usize;
    
    /// Bytes per pixel (3 or 4)
    fn bytes_pp(&self) -> u8;
    
    /// Pixel format for color conversion
    fn pixel_format(&self) -> DrawPixelFormat;

    /// Draw a single pixel with a pre-converted color value.
    ///
    /// Implementations must handle bounds checking internally.
    /// Out-of-bounds coordinates should be silently ignored (clipped).
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32);

    /// Draw a horizontal line (x0 to x1 inclusive).
    ///
    /// Default implementation calls `draw_pixel` in a loop.
    /// Override for better performance on contiguous memory.
    #[inline]
    fn draw_hline(&mut self, x0: i32, x1: i32, y: i32, color: u32) {
        let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        for x in x0..=x1 {
            self.draw_pixel(x, y, color);
        }
    }

    /// Draw a vertical line (y0 to y1 inclusive).
    #[inline]
    fn draw_vline(&mut self, x: i32, y0: i32, y1: i32, color: u32) {
        let (y0, y1) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in y0..=y1 {
            self.draw_pixel(x, y, color);
        }
    }

    /// Fill a rectangle with a solid color.
    ///
    /// Default uses `draw_hline` per row. Override for memset-style fills.
    #[inline]
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        if w <= 0 || h <= 0 { return; }
        for row in y..(y + h) {
            self.draw_hline(x, x + w - 1, row, color);
        }
    }

    /// Clear the entire surface with a color.
    #[inline]
    fn clear(&mut self, color: u32) {
        let w = self.width() as i32;
        let h = self.height() as i32;
        self.fill_rect(0, 0, w, h, color);
    }
}

/// Extension trait for DrawTarget with damage tracking.
///
/// Not all DrawTargets need damage tracking (e.g., kernel panic screen),
/// so this is a separate opt-in trait.
pub trait DamageTracking: DrawTarget {
    /// Add a damage region
    fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32);
    
    /// Clear all damage
    fn clear_damage(&mut self);
    
    /// Check if surface has pending damage
    fn is_dirty(&self) -> bool;
}
```

### 3. `abi/src/draw_primitives.rs` - Drawing Algorithms

```rust
//! Geometric drawing primitives
//!
//! All algorithms are implemented generically over `DrawTarget`.
//! This replaces duplicate code in:
//! - video/src/graphics.rs
//! - userland/src/gfx/primitives.rs

use crate::draw::DrawTarget;

/// Draw a line using Bresenham's algorithm
pub fn line<T: DrawTarget>(target: &mut T, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let raw = target.pixel_format().convert_color(color);
    let w = target.width() as i32;
    let h = target.height() as i32;
    
    // Early reject if entirely outside bounds
    if (x0 < 0 && x1 < 0) || (y0 < 0 && y1 < 0) 
        || (x0 >= w && x1 >= w) || (y0 >= h && y1 >= h) {
        return;
    }

    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;

    loop {
        target.draw_pixel(x, y, raw);  // bounds check is in draw_pixel
        if x == x1 && y == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x += sx; }
        if e2 <= dx { err += dx; y += sy; }
    }
}

/// Draw a rectangle outline
pub fn rect<T: DrawTarget>(target: &mut T, x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 { return; }
    let raw = target.pixel_format().convert_color(color);
    target.draw_hline(x, x + w - 1, y, raw);           // top
    target.draw_hline(x, x + w - 1, y + h - 1, raw);   // bottom
    target.draw_vline(x, y, y + h - 1, raw);           // left
    target.draw_vline(x + w - 1, y, y + h - 1, raw);   // right
}

/// Fill a rectangle with a solid color
pub fn fill_rect<T: DrawTarget>(target: &mut T, x: i32, y: i32, w: i32, h: i32, color: u32) {
    let raw = target.pixel_format().convert_color(color);
    target.fill_rect(x, y, w, h, raw);
}

/// Draw a circle outline using the midpoint algorithm
pub fn circle<T: DrawTarget>(target: &mut T, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius <= 0 { return; }
    let raw = target.pixel_format().convert_color(color);
    
    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    while x <= y {
        // Draw 8 octants
        target.draw_pixel(cx + x, cy + y, raw);
        target.draw_pixel(cx - x, cy + y, raw);
        target.draw_pixel(cx + x, cy - y, raw);
        target.draw_pixel(cx - x, cy - y, raw);
        target.draw_pixel(cx + y, cy + x, raw);
        target.draw_pixel(cx - y, cy + x, raw);
        target.draw_pixel(cx + y, cy - x, raw);
        target.draw_pixel(cx - y, cy - x, raw);

        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            y -= 1;
            d += 2 * (x - y) + 1;
        }
    }
}

/// Draw a filled circle
pub fn circle_filled<T: DrawTarget>(target: &mut T, cx: i32, cy: i32, radius: i32, color: u32) {
    if radius <= 0 { return; }
    let raw = target.pixel_format().convert_color(color);
    
    let mut x = 0i32;
    let mut y = radius;
    let mut d = 1 - radius;

    // Draw initial horizontal line
    target.draw_hline(cx - radius, cx + radius, cy, raw);

    while x < y {
        x += 1;
        if d < 0 {
            d += 2 * x + 1;
        } else {
            // Draw horizontal spans for current y before decrementing
            target.draw_hline(cx - x + 1, cx + x - 1, cy + y, raw);
            target.draw_hline(cx - x + 1, cx + x - 1, cy - y, raw);
            y -= 1;
            d += 2 * (x - y) + 1;
        }
        
        // Draw horizontal spans
        target.draw_hline(cx - y, cx + y, cy + x, raw);
        target.draw_hline(cx - y, cx + y, cy - x, raw);
    }
}

/// Draw a filled triangle using scanline algorithm
pub fn triangle_filled<T: DrawTarget>(
    target: &mut T,
    mut x0: i32, mut y0: i32,
    mut x1: i32, mut y1: i32,
    mut x2: i32, mut y2: i32,
    color: u32,
) {
    let raw = target.pixel_format().convert_color(color);
    
    // Sort vertices by y coordinate
    if y0 > y1 { core::mem::swap(&mut y0, &mut y1); core::mem::swap(&mut x0, &mut x1); }
    if y1 > y2 { core::mem::swap(&mut y1, &mut y2); core::mem::swap(&mut x1, &mut x2); }
    if y0 > y1 { core::mem::swap(&mut y0, &mut y1); core::mem::swap(&mut x0, &mut x1); }

    let total_height = y2 - y0;
    if total_height == 0 { return; }

    for y in y0..=y2 {
        let second_half = y > y1 || y1 == y0;
        let segment_height = if second_half { y2 - y1 } else { y1 - y0 };
        if segment_height == 0 { continue; }
        
        let dy = y - if second_half { y1 } else { y0 };
        let alpha = ((y - y0) as i64 * 65536) / total_height as i64;
        let beta = (dy as i64 * 65536) / segment_height as i64;

        let ax = x0 + (((x2 - x0) as i64 * alpha) >> 16) as i32;
        let bx = if second_half {
            x1 + (((x2 - x1) as i64 * beta) >> 16) as i32
        } else {
            x0 + (((x1 - x0) as i64 * beta) >> 16) as i32
        };

        let (xa, xb) = if ax < bx { (ax, bx) } else { (bx, ax) };
        target.draw_hline(xa, xb, y, raw);
    }
}
```

### 4. `abi/src/font_render.rs` - Font Rendering Algorithms

```rust
//! Font rendering for DrawTarget surfaces
//!
//! Uses the bitmap font data from font.rs and renders generically
//! to any DrawTarget implementation.

use crate::draw::DrawTarget;
use crate::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, get_glyph_or_space};

/// Draw a single character at (x, y)
pub fn draw_char<T: DrawTarget>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: u32,
    bg: u32,
) {
    let fmt = target.pixel_format();
    let fg_raw = fmt.convert_color(fg);
    let bg_raw = fmt.convert_color(bg);
    let glyph = get_glyph_or_space(ch);

    for (row_idx, &row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        for col in 0..FONT_CHAR_WIDTH {
            let px = x + col;
            let is_fg = (row_bits & (0x80 >> col)) != 0;
            let color = if is_fg { fg_raw } else { bg_raw };
            target.draw_pixel(px, py, color);
        }
    }
}

/// Draw a string, handling newlines and tabs
pub fn draw_string<T: DrawTarget>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &[u8],
    fg: u32,
    bg: u32,
) {
    let w = target.width() as i32;
    let h = target.height() as i32;
    let mut cx = x;
    let mut cy = y;

    for &ch in text {
        match ch {
            0 => break,  // null terminator
            b'\n' => {
                cx = x;
                cy += FONT_CHAR_HEIGHT;
            }
            b'\r' => {
                cx = x;
            }
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
            }
            _ => {
                draw_char(target, cx, cy, ch, fg, bg);
                cx += FONT_CHAR_WIDTH;
                if cx + FONT_CHAR_WIDTH > w {
                    cx = x;
                    cy += FONT_CHAR_HEIGHT;
                }
            }
        }
        if cy >= h { break; }
    }
}

/// Draw a string from a Rust &str
#[inline]
pub fn draw_str<T: DrawTarget>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: u32,
    bg: u32,
) {
    draw_string(target, x, y, text.as_bytes(), fg, bg);
}

/// Calculate the pixel width of a string (first line only)
pub fn string_width(text: &[u8]) -> i32 {
    let mut width = 0i32;
    for &ch in text {
        match ch {
            0 | b'\n' => break,
            b'\t' => {
                let tab_width = 4 * FONT_CHAR_WIDTH;
                width = ((width + tab_width - 1) / tab_width) * tab_width;
            }
            _ => width += FONT_CHAR_WIDTH,
        }
    }
    width
}

/// Count the number of lines in a string
pub fn string_lines(text: &[u8]) -> i32 {
    let mut lines = 1i32;
    for &ch in text {
        if ch == 0 { break; }
        if ch == b'\n' { lines += 1; }
    }
    lines
}
```

### 5. Update `abi/src/lib.rs`

Add to existing lib.rs:
```rust
pub mod damage;
pub mod draw;
pub mod draw_primitives;
pub mod font_render;

pub use damage::*;
pub use draw::*;
// Note: don't pub use draw_primitives/font_render - use qualified paths
```

---

## Implementation Changes

### Kernel (`video/` crate)

Implement `DrawTarget` for `GraphicsContext`:

```rust
use slopos_abi::draw::DrawTarget;
use slopos_abi::pixel::DrawPixelFormat;

impl DrawTarget for GraphicsContext {
    #[inline] fn width(&self) -> u32 { self.fb.width }
    #[inline] fn height(&self) -> u32 { self.fb.height }
    #[inline] fn pitch(&self) -> usize { self.fb.pitch as usize }
    #[inline] fn bytes_pp(&self) -> u8 { self.fb.bpp }
    
    fn pixel_format(&self) -> DrawPixelFormat {
        DrawPixelFormat::from_pixel_format_code(self.fb.pixel_format)
    }

    #[inline]
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.fb.width as i32 || y >= self.fb.height as i32 {
            return;
        }
        let bytes_pp = ((self.fb.bpp as usize) + 7) / 8;
        let offset = y as usize * self.fb.pitch as usize + x as usize * bytes_pp;
        let ptr = unsafe { self.fb.base.add(offset) };
        
        unsafe {
            match bytes_pp {
                4 => (ptr as *mut u32).write_volatile(color),
                3 => {
                    ptr.write_volatile((color & 0xFF) as u8);
                    ptr.add(1).write_volatile(((color >> 8) & 0xFF) as u8);
                    ptr.add(2).write_volatile(((color >> 16) & 0xFF) as u8);
                }
                2 => (ptr as *mut u16).write_volatile(color as u16),
                _ => {}
            }
        }
    }

    // Override for performance - MMIO benefits from sequential writes
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        // ... optimized implementation with row-based writes
    }
}
```

### Userland (`userland/` crate)

Implement `DrawTarget` for `DrawBuffer`:

```rust
use slopos_abi::draw::{DrawTarget, DamageTracking};
use slopos_abi::damage::DamageTracker;
use slopos_abi::pixel::DrawPixelFormat;

impl DrawTarget for DrawBuffer<'_> {
    #[inline] fn width(&self) -> u32 { self.width }
    #[inline] fn height(&self) -> u32 { self.height }
    #[inline] fn pitch(&self) -> usize { self.pitch }
    #[inline] fn bytes_pp(&self) -> u8 { self.bytes_pp }
    #[inline] fn pixel_format(&self) -> DrawPixelFormat { self.pixel_format }

    #[inline]
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let offset = y as usize * self.pitch + x as usize * self.bytes_pp as usize;
        match self.bytes_pp {
            4 => {
                if offset + 4 <= self.data.len() {
                    self.data[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
                }
            }
            3 => {
                if offset + 3 <= self.data.len() {
                    let bytes = color.to_le_bytes();
                    self.data[offset] = bytes[0];
                    self.data[offset + 1] = bytes[1];
                    self.data[offset + 2] = bytes[2];
                }
            }
            _ => {}
        }
    }
}

impl DamageTracking for DrawBuffer<'_> {
    fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.damage.add_rect(x0, y0, x1, y1);
    }
    fn clear_damage(&mut self) { self.damage.clear(); }
    fn is_dirty(&self) -> bool { self.damage.is_dirty() }
}
```

---

## Migration Phases

### Phase 1: Add New Code (No Breaking Changes)
1. Create `abi/src/damage.rs`
2. Create `abi/src/draw.rs`
3. Create `abi/src/draw_primitives.rs`
4. Create `abi/src/font_render.rs`
5. Update `abi/src/lib.rs` with exports
6. Run `cargo build` - should succeed with no changes to other crates

### Phase 2: Kernel Integration
7. Add `impl DrawTarget for GraphicsContext` in `video/src/graphics.rs`
8. Update `video/src/font.rs` to delegate to `abi::font_render`
9. Keep old functions as thin wrappers for now (backward compat)
10. Run `make boot` - verify kernel renders correctly

### Phase 3: Userland Integration
11. Update `DrawBuffer` to use `slopos_abi::damage::DamageTracker`
12. Add `impl DrawTarget for DrawBuffer`
13. Update `userland/src/gfx/primitives.rs` to use `abi::draw_primitives`
14. Update `userland/src/gfx/font.rs` to use `abi::font_render`
15. Run `make boot` - verify compositor and shell work

### Phase 4: Cleanup
16. Remove old algorithm implementations from `video/src/graphics.rs`
17. Remove old algorithm implementations from `userland/src/gfx/primitives.rs`
18. Remove duplicate `DamageRect`/`DamageTracker` from `video/src/compositor_context.rs`
19. Remove duplicate `DamageRect`/`DamageTracker`/`PixelFormat` from `userland/src/gfx/mod.rs`
20. Update `video/src/compositor_context.rs` to use `slopos_abi::damage`
21. Run full test suite

### Phase 5: RouletteBackend (Optional Future Work)
22. Consider migrating `RouletteBackend` from function pointers to `&mut dyn DrawTarget`
23. This is lower priority since it works and is isolated

---

## Expected Results

### Files Deleted/Reduced After Migration

| File | Lines Removed |
|------|---------------|
| `video/src/graphics.rs` (algorithm code) | ~280 lines |
| `userland/src/gfx/primitives.rs` | ~400 lines (entire file or most of it) |
| `video/src/compositor_context.rs` (DamageRect/Tracker) | ~130 lines |
| `userland/src/gfx/mod.rs` (DamageRect/Tracker/PixelFormat) | ~200 lines |
| **Total Removed** | **~1010 lines** |

### Files Added

| File | Lines Added |
|------|-------------|
| `abi/src/damage.rs` | ~100 lines |
| `abi/src/draw.rs` | ~80 lines |
| `abi/src/draw_primitives.rs` | ~150 lines |
| `abi/src/font_render.rs` | ~80 lines |
| **Total Added** | **~410 lines** |

### Net Result
- **~600 lines of code eliminated** (net reduction)
- **Single source of truth** for all drawing algorithms
- **Type-safe abstraction** that works for both kernel and userland
- **No external dependencies** added
- **Zero runtime overhead** (monomorphization inlines everything)

---

## Verification Checklist

After each phase, verify:

- [ ] `cargo build` succeeds for all crates
- [ ] `make boot` boots to shell
- [ ] Compositor renders window decorations correctly
- [ ] Shell text rendering works
- [ ] Roulette wheel animation plays on boot
- [ ] Panic screen renders correctly (if triggerable)
- [ ] `make test` passes

---

## References

- `embedded-graphics` DrawTarget pattern: https://github.com/embedded-graphics/embedded-graphics
- rCore-Tutorial-v3 Display impl: https://github.com/rcore-os/rCore-Tutorial-v3/blob/main/user/src/io.rs
- zCore GraphicConsole: https://github.com/rcore-os/zCore/blob/master/drivers/src/utils/graphic_console.rs
