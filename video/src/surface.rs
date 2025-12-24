use core::ffi::{c_char, c_int};
use core::ptr;

use alloc::sync::Arc;
use alloc::collections::BTreeMap;

use slopos_drivers::video_bridge::{DamageRegion, VideoError, VideoResult};
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use slopos_mm::phys_virt::mm_phys_to_virt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use slopos_sched::MAX_TASKS;
use spin::Mutex;

use crate::font;

// Window state constants
pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

// Maximum number of damage regions tracked per surface before merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

// Z-order counter for window stacking
static NEXT_Z_ORDER: AtomicU32 = AtomicU32::new(1);

/// Helper function to calculate bytes per pixel from bits per pixel
fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

/// A single damage rectangle in surface-local coordinates
#[derive(Copy, Clone, Default)]
pub(crate) struct DamageRect {
    x0: i32,
    y0: i32,
    x1: i32, // Inclusive
    y1: i32, // Inclusive
}

impl DamageRect {
    const fn invalid() -> Self {
        Self { x0: 0, y0: 0, x1: -1, y1: -1 }
    }

    fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    fn area(&self) -> i32 {
        if !self.is_valid() {
            0
        } else {
            (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)
        }
    }

    fn union(&self, other: &DamageRect) -> DamageRect {
        DamageRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn combined_area(&self, other: &DamageRect) -> i32 {
        self.union(other).area()
    }
}

// =============================================================================
// Safe Surface Types (Arc-based Compositor)
// =============================================================================

/// Safe owned buffer backed by page frame allocation.
/// Memory is automatically freed when dropped.
pub struct PageBuffer {
    /// Virtual address of buffer
    virt_ptr: *mut u8,
    /// Physical address (for freeing)
    phys_addr: u64,
    /// Size in bytes
    size: usize,
    /// Number of pages allocated
    pages: u32,
}

impl PageBuffer {
    /// Allocate a new zeroed buffer with the given size.
    pub fn new(size: usize) -> Result<Self, VideoError> {
        if size == 0 {
            return Err(VideoError::Invalid);
        }

        let pages = ((size as u64 + PAGE_SIZE_4KB - 1) / PAGE_SIZE_4KB) as u32;
        let phys_addr = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
        if phys_addr == 0 {
            return Err(VideoError::Invalid);
        }

        let virt_addr = mm_phys_to_virt(phys_addr);
        let virt_ptr = if virt_addr != 0 {
            virt_addr as *mut u8
        } else {
            phys_addr as *mut u8
        };

        Ok(Self {
            virt_ptr,
            phys_addr,
            size,
            pages,
        })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: Buffer is valid for self.size bytes, exclusively owned
        unsafe { core::slice::from_raw_parts(self.virt_ptr, self.size) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: Buffer is valid for self.size bytes, mutable exclusive access
        unsafe { core::slice::from_raw_parts_mut(self.virt_ptr, self.size) }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.virt_ptr
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.virt_ptr
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.size
    }
}

impl Drop for PageBuffer {
    fn drop(&mut self) {
        if self.phys_addr != 0 {
            // Free all allocated pages
            for i in 0..self.pages {
                let page_phys = self.phys_addr + (i as u64) * PAGE_SIZE_4KB;
                let _ = free_page_frame(page_phys);
            }
        }
    }
}

// SAFETY: PageBuffer owns its memory exclusively
unsafe impl Send for PageBuffer {}

/// A surface buffer with owned memory and metadata.
pub struct OwnedBuffer {
    /// Owned pixel data backed by page frames
    data: PageBuffer,
    /// Buffer width in pixels
    width: u32,
    /// Buffer height in pixels
    height: u32,
    /// Row stride in bytes
    pitch: usize,
    /// Bytes per pixel
    bytes_pp: u8,
    /// Damage tracking for this buffer
    damage_regions: [DamageRect; MAX_DAMAGE_REGIONS],
    damage_count: u8,
}

impl OwnedBuffer {
    pub fn new(width: u32, height: u32, bpp: u8) -> Result<Self, VideoError> {
        let bytes_pp = ((bpp as usize) + 7) / 8;
        let pitch = (width as usize) * bytes_pp;
        let size = pitch * (height as usize);

        let data = PageBuffer::new(size)?;

        Ok(Self {
            data,
            width,
            height,
            pitch,
            bytes_pp: bytes_pp as u8,
            damage_regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            damage_count: 0,
        })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        (y as usize) * self.pitch + (x as usize) * (self.bytes_pp as usize)
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn pitch(&self) -> usize {
        self.pitch
    }

    #[inline]
    pub fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    /// Add a damage region to this buffer
    pub fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        // Clip to buffer bounds
        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = x1.min(self.width as i32 - 1);
        let y1 = y1.min(self.height as i32 - 1);

        if x0 > x1 || y0 > y1 {
            return;
        }

        let new_rect = DamageRect { x0, y0, x1, y1 };

        // Merge if at capacity
        if (self.damage_count as usize) >= MAX_DAMAGE_REGIONS {
            self.merge_smallest_pair();
        }

        if (self.damage_count as usize) < MAX_DAMAGE_REGIONS {
            self.damage_regions[self.damage_count as usize] = new_rect;
            self.damage_count += 1;
        }
    }

    /// Merge the two damage regions with the smallest combined area
    fn merge_smallest_pair(&mut self) {
        if self.damage_count < 2 {
            return;
        }

        let count = self.damage_count as usize;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        // Find the pair with smallest combined area when merged
        for i in 0..count {
            for j in (i + 1)..count {
                let combined = self.damage_regions[i].combined_area(&self.damage_regions[j]);
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Merge i and j into i
        let merged = self.damage_regions[best_i].union(&self.damage_regions[best_j]);
        self.damage_regions[best_i] = merged;

        // Remove j by swapping with last element
        if best_j < count - 1 {
            self.damage_regions[best_j] = self.damage_regions[count - 1];
        }
        self.damage_count -= 1;
    }

    pub fn clear_damage(&mut self) {
        self.damage_count = 0;
    }

    pub fn damage_count(&self) -> u8 {
        self.damage_count
    }

    #[allow(dead_code)]
    pub(crate) fn damage_regions(&self) -> &[DamageRect] {
        &self.damage_regions[..self.damage_count as usize]
    }

    #[allow(dead_code)]
    pub(crate) fn damage_union(&self) -> DamageRect {
        if self.damage_count == 0 {
            return DamageRect::invalid();
        }
        let mut result = self.damage_regions[0];
        for i in 1..self.damage_count as usize {
            result = result.union(&self.damage_regions[i]);
        }
        result
    }
}

/// Double buffer pair for tear-free rendering.
pub struct DoubleBuffer {
    /// Front buffer - compositor reads from this
    front: OwnedBuffer,
    /// Back buffer - client draws to this
    back: OwnedBuffer,
}

impl DoubleBuffer {
    pub fn new(width: u32, height: u32, bpp: u8) -> Result<Self, VideoError> {
        Ok(Self {
            front: OwnedBuffer::new(width, height, bpp)?,
            back: OwnedBuffer::new(width, height, bpp)?,
        })
    }

    /// Get mutable access to back buffer for drawing
    #[inline]
    pub fn back_mut(&mut self) -> &mut OwnedBuffer {
        &mut self.back
    }

    /// Get immutable access to front buffer for compositing
    #[inline]
    pub fn front(&self) -> &OwnedBuffer {
        &self.front
    }

    /// Get mutable access to front buffer (for clearing damage after compositing)
    #[inline]
    pub fn front_mut(&mut self) -> &mut OwnedBuffer {
        &mut self.front
    }

    /// Commit: copy back to front and transfer damage
    pub fn commit(&mut self) {
        // Copy pixel data
        let src = self.back.as_slice();
        let dst = self.front.as_mut_slice();
        dst.copy_from_slice(src);

        // Transfer damage regions
        self.front.damage_regions = self.back.damage_regions;
        self.front.damage_count = self.back.damage_count;

        // Clear back damage
        self.back.clear_damage();
    }

    pub fn width(&self) -> u32 {
        self.front.width()
    }

    pub fn height(&self) -> u32 {
        self.front.height()
    }
}

/// Thread-safe surface with interior mutability for hot state.
pub struct SafeSurface {
    // === Immutable after creation ===
    pub task_id: u32,
    pub pixel_format: u8,

    // === Per-surface lock for buffer access ===
    buffers: Mutex<DoubleBuffer>,

    // === Atomic state (lock-free) ===
    dirty: AtomicBool,
    window_x: AtomicI32,
    window_y: AtomicI32,
    z_order: AtomicU32,
    visible: AtomicBool,
    window_state: AtomicU8,
}

impl SafeSurface {
    pub fn new(
        task_id: u32,
        width: u32,
        height: u32,
        bpp: u8,
        pixel_format: u8,
    ) -> Result<Self, VideoError> {
        Ok(Self {
            task_id,
            pixel_format,
            buffers: Mutex::new(DoubleBuffer::new(width, height, bpp)?),
            dirty: AtomicBool::new(true),
            window_x: AtomicI32::new(0),
            window_y: AtomicI32::new(0),
            z_order: AtomicU32::new(0),
            visible: AtomicBool::new(true),
            window_state: AtomicU8::new(WINDOW_STATE_NORMAL),
        })
    }

    /// Execute a drawing operation on the back buffer
    pub fn with_back_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut OwnedBuffer) -> R,
    {
        let mut buffers = self.buffers.lock();
        f(buffers.back_mut())
    }

    /// Read from front buffer (for compositor)
    pub fn with_front_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&OwnedBuffer) -> R,
    {
        let buffers = self.buffers.lock();
        f(buffers.front())
    }

    /// Commit back buffer to front
    pub fn commit(&self) {
        let mut buffers = self.buffers.lock();
        buffers.commit();
        self.dirty.store(true, Ordering::Release);
    }

    pub fn width(&self) -> u32 {
        self.buffers.lock().width()
    }

    pub fn height(&self) -> u32 {
        self.buffers.lock().height()
    }

    // Atomic accessors
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    /// Clear front buffer damage (called after compositor renders)
    pub fn clear_front_damage(&self) {
        let mut buffers = self.buffers.lock();
        buffers.front_mut().clear_damage();
    }

    /// Add damage directly to front buffer (for external damage like cursor)
    /// This avoids a full buffer copy - just marks region for re-compositing
    pub fn add_front_damage(&self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let mut buffers = self.buffers.lock();
        buffers.front_mut().add_damage(x0, y0, x1, y1);
    }

    /// Mark surface dirty (for external damage regions)
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn position(&self) -> (i32, i32) {
        (
            self.window_x.load(Ordering::Relaxed),
            self.window_y.load(Ordering::Relaxed),
        )
    }

    pub fn set_position(&self, x: i32, y: i32) {
        self.window_x.store(x, Ordering::Relaxed);
        self.window_y.store(y, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn z_order(&self) -> u32 {
        self.z_order.load(Ordering::Relaxed)
    }

    pub fn set_z_order(&self, z: u32) {
        self.z_order.store(z, Ordering::Relaxed);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn window_state(&self) -> u8 {
        self.window_state.load(Ordering::Relaxed)
    }

    pub fn set_window_state(&self, state: u8) {
        self.window_state.store(state, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
    }
}

// =============================================================================
// Phase 2: Safe Surface Registry
// =============================================================================

/// Reference-counted surface handle
pub type SurfaceRef = Arc<SafeSurface>;

/// New safe surface registry
/// Lock held briefly only to insert/remove/lookup Arc
static SAFE_SURFACES: Mutex<BTreeMap<u32, SurfaceRef>> = Mutex::new(BTreeMap::new());

/// Get or create a safe surface for a task
#[allow(dead_code)]
fn get_or_create_safe_surface(task_id: u32) -> Result<SurfaceRef, VideoError> {
    // Check if surface already exists
    {
        let registry = SAFE_SURFACES.lock();
        if let Some(surface) = registry.get(&task_id) {
            return Ok(Arc::clone(surface));
        }
    }

    // Create new surface - get framebuffer info for dimensions
    let fb = crate::framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)?;
    let bytes_pp = bytes_per_pixel(fb.bpp) as u8;
    if bytes_pp != 3 && bytes_pp != 4 {
        return Err(VideoError::Invalid);
    }

    // Try different resolutions (largest first)
    let candidates = [
        (fb.width, fb.height),
        (800, 600),
        (640, 480),
        (320, 240),
    ];

    for (width, height) in candidates {
        if width == 0 || height == 0 || width > fb.width || height > fb.height {
            continue;
        }

        match SafeSurface::new(task_id, width, height, fb.bpp, fb.pixel_format) {
            Ok(surface) => {
                let surface_ref = Arc::new(surface);

                // Set initial position (cascading)
                let active_count = {
                    let registry = SAFE_SURFACES.lock();
                    registry.len()
                };

                let cascade_offset = ((active_count as i32) * 32) % 200;
                surface_ref.set_position(100 + cascade_offset, 100 + cascade_offset);
                surface_ref.set_z_order(NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed));

                // Insert into registry
                {
                    let mut registry = SAFE_SURFACES.lock();
                    registry.insert(task_id, Arc::clone(&surface_ref));
                }

                return Ok(surface_ref);
            }
            Err(_) => continue,
        }
    }

    Err(VideoError::Invalid)
}

/// Get a surface reference (brief lock)
#[allow(dead_code)]
fn get_safe_surface(task_id: u32) -> Result<SurfaceRef, VideoError> {
    let registry = SAFE_SURFACES.lock();
    registry.get(&task_id).cloned().ok_or(VideoError::Invalid)
}

/// Get all surfaces for compositor (brief lock, returns stack array of Arc clones)
#[allow(dead_code)]
fn get_all_safe_surfaces() -> [Option<SurfaceRef>; MAX_TASKS] {
    let registry = SAFE_SURFACES.lock();
    let mut result: [Option<SurfaceRef>; MAX_TASKS] = core::array::from_fn(|_| None);

    for (i, surface) in registry.values().enumerate() {
        if i >= MAX_TASKS {
            break;
        }
        result[i] = Some(Arc::clone(surface));
    }

    result
}

/// Destroy a surface
#[allow(dead_code)]
fn destroy_safe_surface(task_id: u32) {
    let mut registry = SAFE_SURFACES.lock();
    registry.remove(&task_id);
    // Arc refcount decrements; surface freed when last reference dropped
}

// =============================================================================
// Phase 3: Safe Drawing Functions
// =============================================================================

/// Write a pixel to a buffer slice at the given offset
#[inline]
fn write_pixel_safe(data: &mut [u8], offset: usize, bytes_pp: u8, color: u32) {
    match bytes_pp {
        4 => {
            if offset + 4 <= data.len() {
                let bytes = color.to_le_bytes();
                data[offset..offset + 4].copy_from_slice(&bytes);
            }
        }
        3 => {
            if offset + 3 <= data.len() {
                let bytes = color.to_le_bytes();
                data[offset] = bytes[0];
                data[offset + 1] = bytes[1];
                data[offset + 2] = bytes[2];
            }
        }
        _ => {}
    }
}

/// Safe rectangle fill using slice-based operations
#[allow(dead_code)]
pub fn surface_draw_rect_filled_fast_safe(
    task_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    if w <= 0 || h <= 0 {
        return Err(VideoError::Invalid);
    }

    let surface = get_or_create_safe_surface(task_id)?;
    let pixel_format = surface.pixel_format;

    surface.with_back_buffer(|buffer| {
        let width = buffer.width() as i32;
        let height = buffer.height() as i32;

        // Clip to buffer bounds
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w - 1).min(width - 1);
        let y1 = (y + h - 1).min(height - 1);

        if x0 > x1 || y0 > y1 {
            return Err(VideoError::OutOfBounds);
        }

        let converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, color);
        let bytes_pp = buffer.bytes_pp() as usize;
        let pitch = buffer.pitch();
        let span_w = (x1 - x0 + 1) as usize;
        let data = buffer.as_mut_slice();

        for row in y0..=y1 {
            let row_off = (row as usize) * pitch + (x0 as usize) * bytes_pp;
            match bytes_pp {
                4 => {
                    let row_slice = &mut data[row_off..row_off + span_w * 4];
                    if converted == 0 {
                        row_slice.fill(0);
                    } else {
                        let bytes = converted.to_le_bytes();
                        for chunk in row_slice.chunks_exact_mut(4) {
                            chunk.copy_from_slice(&bytes);
                        }
                    }
                }
                3 => {
                    let bytes = converted.to_le_bytes();
                    for col in 0..span_w {
                        let off = row_off + col * 3;
                        data[off] = bytes[0];
                        data[off + 1] = bytes[1];
                        data[off + 2] = bytes[2];
                    }
                }
                _ => {}
            }
        }

        buffer.add_damage(x0, y0, x1, y1);
        Ok(())
    })
}

/// Safe line drawing using Bresenham's algorithm
#[allow(dead_code)]
pub fn surface_draw_line_safe(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    let surface = get_or_create_safe_surface(task_id)?;
    let pixel_format = surface.pixel_format;

    surface.with_back_buffer(|buffer| {
        let width = buffer.width() as i32;
        let height = buffer.height() as i32;
        let converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, color);
        let bytes_pp = buffer.bytes_pp();
        let pitch = buffer.pitch();
        let data = buffer.as_mut_slice();

        // Bresenham's line algorithm
        let mut x = x0;
        let mut y = y0;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            // Draw pixel if within bounds
            if x >= 0 && x < width && y >= 0 && y < height {
                let offset = (y as usize) * pitch + (x as usize) * (bytes_pp as usize);
                write_pixel_safe(data, offset, bytes_pp, converted);
            }

            if x == x1 && y == y1 {
                break;
            }

            let e2 = 2 * err;
            if e2 >= dy {
                if x == x1 {
                    break;
                }
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                if y == y1 {
                    break;
                }
                err += dx;
                y += sy;
            }
        }

        // Add damage for the line's bounding box
        let min_x = x0.min(x1).max(0);
        let min_y = y0.min(y1).max(0);
        let max_x = x0.max(x1).min(width - 1);
        let max_y = y0.max(y1).min(height - 1);
        buffer.add_damage(min_x, min_y, max_x, max_y);

        Ok(())
    })
}

/// Safe circle outline drawing using midpoint algorithm
#[allow(dead_code)]
pub fn surface_draw_circle_safe(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius < 0 {
        return Err(VideoError::Invalid);
    }

    let surface = get_or_create_safe_surface(task_id)?;
    let pixel_format = surface.pixel_format;

    surface.with_back_buffer(|buffer| {
        let width = buffer.width() as i32;
        let height = buffer.height() as i32;
        let converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, color);
        let bytes_pp = buffer.bytes_pp();
        let pitch = buffer.pitch();
        let data = buffer.as_mut_slice();

        let mut x = radius;
        let mut y = 0;
        let mut err = 0;

        while x >= y {
            // Draw 8 octants
            let points = [
                (cx + x, cy + y),
                (cx + y, cy + x),
                (cx - y, cy + x),
                (cx - x, cy + y),
                (cx - x, cy - y),
                (cx - y, cy - x),
                (cx + y, cy - x),
                (cx + x, cy - y),
            ];

            for (px, py) in points {
                if px >= 0 && px < width && py >= 0 && py < height {
                    let offset = (py as usize) * pitch + (px as usize) * (bytes_pp as usize);
                    write_pixel_safe(data, offset, bytes_pp, converted);
                }
            }

            y += 1;
            err += 1 + 2 * y;
            if 2 * (err - x) + 1 > 0 {
                x -= 1;
                err += 1 - 2 * x;
            }
        }

        // Add damage for the circle's bounding box
        let min_x = (cx - radius).max(0);
        let min_y = (cy - radius).max(0);
        let max_x = (cx + radius).min(width - 1);
        let max_y = (cy + radius).min(height - 1);
        buffer.add_damage(min_x, min_y, max_x, max_y);

        Ok(())
    })
}

/// Helper to draw a horizontal line safely
fn draw_hline_safe(
    data: &mut [u8],
    pitch: usize,
    bytes_pp: u8,
    width: i32,
    height: i32,
    x0: i32,
    x1: i32,
    y: i32,
    color: u32,
) {
    if y < 0 || y >= height {
        return;
    }
    let x0 = x0.max(0);
    let x1 = x1.min(width - 1);
    if x0 > x1 {
        return;
    }

    let row_off = (y as usize) * pitch;
    for x in x0..=x1 {
        let offset = row_off + (x as usize) * (bytes_pp as usize);
        write_pixel_safe(data, offset, bytes_pp, color);
    }
}

/// Safe filled circle drawing
#[allow(dead_code)]
pub fn surface_draw_circle_filled_safe(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius < 0 {
        return Err(VideoError::Invalid);
    }

    let surface = get_or_create_safe_surface(task_id)?;
    let pixel_format = surface.pixel_format;

    surface.with_back_buffer(|buffer| {
        let width = buffer.width() as i32;
        let height = buffer.height() as i32;
        let converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, color);
        let bytes_pp = buffer.bytes_pp();
        let pitch = buffer.pitch();
        let data = buffer.as_mut_slice();

        let mut x = radius;
        let mut y = 0;
        let mut err = 0;

        while x >= y {
            // Draw horizontal lines to fill the circle
            draw_hline_safe(data, pitch, bytes_pp, width, height, cx - x, cx + x, cy + y, converted);
            draw_hline_safe(data, pitch, bytes_pp, width, height, cx - x, cx + x, cy - y, converted);
            draw_hline_safe(data, pitch, bytes_pp, width, height, cx - y, cx + y, cy + x, converted);
            draw_hline_safe(data, pitch, bytes_pp, width, height, cx - y, cx + y, cy - x, converted);

            y += 1;
            err += 1 + 2 * y;
            if 2 * (err - x) + 1 > 0 {
                x -= 1;
                err += 1 - 2 * x;
            }
        }

        // Add damage for the circle's bounding box
        let min_x = (cx - radius).max(0);
        let min_y = (cy - radius).max(0);
        let max_x = (cx + radius).min(width - 1);
        let max_y = (cy + radius).min(height - 1);
        buffer.add_damage(min_x, min_y, max_x, max_y);

        Ok(())
    })
}

/// Safe surface clear
#[allow(dead_code)]
pub fn surface_clear_safe(task_id: u32, color: u32) -> VideoResult {
    let surface = get_or_create_safe_surface(task_id)?;
    let pixel_format = surface.pixel_format;

    surface.with_back_buffer(|buffer| {
        let converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, color);
        let bytes_pp = buffer.bytes_pp() as usize;
        let w = buffer.width() as i32;
        let h = buffer.height() as i32;
        let data = buffer.as_mut_slice();

        if converted == 0 {
            data.fill(0);
        } else {
            let bytes = converted.to_le_bytes();
            match bytes_pp {
                4 => {
                    for chunk in data.chunks_exact_mut(4) {
                        chunk.copy_from_slice(&bytes);
                    }
                }
                3 => {
                    for chunk in data.chunks_exact_mut(3) {
                        chunk[0] = bytes[0];
                        chunk[1] = bytes[1];
                        chunk[2] = bytes[2];
                    }
                }
                _ => {}
            }
        }

        buffer.add_damage(0, 0, w - 1, h - 1);
        Ok(())
    })
}

/// Safe surface commit
#[allow(dead_code)]
pub fn surface_commit_safe(task_id: u32) -> VideoResult {
    let surface = get_safe_surface(task_id)?;
    surface.commit();
    Ok(())
}

/// Helper to draw a single glyph on an OwnedBuffer
fn draw_glyph_safe(
    buffer: &mut OwnedBuffer,
    x: i32,
    y: i32,
    ch: u8,
    fg_color: u32,
    bg_color: u32,
    pixel_format: u8,
) {
    let glyph = font::font_glyph(ch).unwrap_or_else(|| {
        font::font_glyph(b' ').unwrap()
    });

    let width = buffer.width() as i32;
    let height = buffer.height() as i32;
    let bytes_pp = buffer.bytes_pp();
    let pitch = buffer.pitch();
    let data = buffer.as_mut_slice();

    let fg_converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, fg_color);
    let bg_converted = crate::framebuffer::framebuffer_convert_color_for(pixel_format, bg_color);

    for (row_idx, row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py >= height {
            continue;
        }
        for col in 0..font::FONT_CHAR_WIDTH {
            let px = x + col;
            if px < 0 || px >= width {
                continue;
            }
            let mask = 1u8 << (7 - col);
            let color = if (row_bits & mask) != 0 {
                fg_converted
            } else {
                bg_converted
            };
            let offset = (py as usize) * pitch + (px as usize) * (bytes_pp as usize);
            write_pixel_safe(data, offset, bytes_pp, color);
        }
    }
}

/// Safe font string drawing
#[allow(dead_code)]
pub fn surface_font_draw_string_safe(
    task_id: u32,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    if str_ptr.is_null() {
        return -1;
    }

    // Convert C string to bytes (safe copy)
    let mut tmp = [0u8; 1024];
    let text = unsafe { c_str_to_bytes_safe(str_ptr, &mut tmp) };

    let surface = match get_or_create_safe_surface(task_id) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let pixel_format = surface.pixel_format;

    let result: VideoResult = surface.with_back_buffer(|buffer| {
        let width = buffer.width() as i32;
        let height = buffer.height() as i32;

        let mut cx = x;
        let mut cy = y;
        let mut dirty = false;
        let mut dirty_x0 = 0i32;
        let mut dirty_y0 = 0i32;
        let mut dirty_x1 = 0i32;
        let mut dirty_y1 = 0i32;

        for &ch in text {
            match ch {
                b'\n' => {
                    cx = x;
                    cy += font::FONT_CHAR_HEIGHT;
                }
                b'\r' => {
                    cx = x;
                }
                b'\t' => {
                    let tab_width = 4 * font::FONT_CHAR_WIDTH;
                    cx = ((cx - x + tab_width) / tab_width) * tab_width + x;
                }
                _ => {
                    draw_glyph_safe(buffer, cx, cy, ch, fg_color, bg_color, pixel_format);
                    let gx0 = cx;
                    let gy0 = cy;
                    let gx1 = cx + font::FONT_CHAR_WIDTH - 1;
                    let gy1 = cy + font::FONT_CHAR_HEIGHT - 1;
                    if !dirty {
                        dirty = true;
                        dirty_x0 = gx0;
                        dirty_y0 = gy0;
                        dirty_x1 = gx1;
                        dirty_y1 = gy1;
                    } else {
                        dirty_x0 = dirty_x0.min(gx0);
                        dirty_y0 = dirty_y0.min(gy0);
                        dirty_x1 = dirty_x1.max(gx1);
                        dirty_y1 = dirty_y1.max(gy1);
                    }
                    cx += font::FONT_CHAR_WIDTH;
                    if cx + font::FONT_CHAR_WIDTH > width {
                        cx = x;
                        cy += font::FONT_CHAR_HEIGHT;
                    }
                }
            }
            if cy >= height {
                break;
            }
        }

        if dirty {
            buffer.add_damage(dirty_x0, dirty_y0, dirty_x1, dirty_y1);
        }
        Ok(())
    });

    if result.is_ok() { 0 } else { -1 }
}

/// Safe C string to bytes conversion
unsafe fn c_str_to_bytes_safe<'a>(ptr: *const c_char, buf: &'a mut [u8]) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    let mut len = 0usize;
    while len < buf.len() {
        let ch = unsafe { *ptr.add(len) };
        if ch == 0 {
            break;
        }
        buf[len] = ch as u8;
        len += 1;
    }
    &buf[..len]
}

// =============================================================================
// Phase 4: Safe Compositor
// =============================================================================

/// Safe compositor using Arc references
/// Key safety: Arc clones keep surfaces alive during iteration - no stale pointers
#[allow(dead_code)]
pub fn compositor_present_safe() -> c_int {
    let fb = match crate::framebuffer::snapshot() {
        Some(fb) => fb,
        None => return -1,
    };

    // Get Arc clones - surfaces stay alive for entire render
    let surfaces = get_all_safe_surfaces();
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let fb_width = fb.width as i32;
    let fb_height = fb.height as i32;

    // Count active surfaces and build sorted index list
    let mut indices: [usize; MAX_TASKS] = [0; MAX_TASKS];
    let mut index_count = 0usize;

    for (i, surface_opt) in surfaces.iter().enumerate() {
        if surface_opt.is_some() {
            indices[index_count] = i;
            index_count += 1;
        }
    }

    if index_count == 0 {
        return 0;
    }

    // Sort by z-order (insertion sort, O(n) for nearly-sorted)
    for i in 1..index_count {
        let key_idx = indices[i];
        let key_z = surfaces[key_idx].as_ref().unwrap().z_order();
        let mut j = i;
        while j > 0 {
            let prev_z = surfaces[indices[j - 1]].as_ref().unwrap().z_order();
            if prev_z <= key_z {
                break;
            }
            indices[j] = indices[j - 1];
            j -= 1;
        }
        indices[j] = key_idx;
    }

    let mut did_work = false;

    // Render each surface (back to front)
    for idx_pos in 0..index_count {
        let surface = surfaces[indices[idx_pos]].as_ref().unwrap();

        // Skip minimized windows
        if surface.window_state() == WINDOW_STATE_MINIMIZED {
            continue;
        }

        // Skip if not dirty
        if !surface.is_dirty() {
            continue;
        }

        let (wx, wy) = surface.position();

        // Read from front buffer with per-surface lock
        let rendered = surface.with_front_buffer(|buffer| {
            let damage = buffer.damage_union();
            if !damage.is_valid() {
                return false;
            }

            // Clip damage to buffer bounds
            let mut src_x = damage.x0.max(0);
            let mut src_y = damage.y0.max(0);
            let src_x1 = damage.x1.min(buffer.width() as i32 - 1);
            let src_y1 = damage.y1.min(buffer.height() as i32 - 1);

            if src_x > src_x1 || src_y > src_y1 {
                return false;
            }

            // Calculate destination and clipping
            let mut dst_x = wx + src_x;
            let mut dst_y = wy + src_y;
            let mut copy_w = src_x1 - src_x + 1;
            let mut copy_h = src_y1 - src_y + 1;

            // Clip to framebuffer bounds
            if dst_x < 0 {
                let delta = -dst_x;
                src_x += delta;
                copy_w -= delta;
                dst_x = 0;
            }
            if dst_y < 0 {
                let delta = -dst_y;
                src_y += delta;
                copy_h -= delta;
                dst_y = 0;
            }
            if dst_x + copy_w > fb_width {
                copy_w = fb_width - dst_x;
            }
            if dst_y + copy_h > fb_height {
                copy_h = fb_height - dst_y;
            }
            if copy_w <= 0 || copy_h <= 0 {
                return false;
            }

            let src_data = buffer.as_slice();
            let src_pitch = buffer.pitch();

            // Blit to framebuffer (only unsafe operation - MMIO write)
            for row in 0..copy_h {
                let src_row_off = ((src_y + row) as usize) * src_pitch
                    + (src_x as usize) * bytes_pp;
                let dst_off = ((dst_y + row) as usize) * (fb.pitch as usize)
                    + (dst_x as usize) * bytes_pp;
                let row_bytes = (copy_w as usize) * bytes_pp;

                unsafe {
                    let src_ptr = src_data.as_ptr().add(src_row_off);
                    let dst_ptr = fb.base.add(dst_off);
                    ptr::copy_nonoverlapping(src_ptr, dst_ptr, row_bytes);
                }
            }

            true
        });

        if rendered {
            // Clear both dirty flag and front buffer damage after rendering
            surface.clear_front_damage();
            surface.clear_dirty();
            did_work = true;
        }
    }

    if did_work { 1 } else { 0 }
}

/// Compositor present with external damage regions.
/// Marks surfaces dirty if they overlap with provided damage regions,
/// then composites all dirty surfaces.
#[allow(dead_code)]
pub fn compositor_present_with_damage_safe(
    damage_regions: *const DamageRegion,
    damage_count: u32,
) -> c_int {
    if damage_count == 0 {
        return compositor_present_safe();
    }

    // Get all surfaces
    let surfaces = get_all_safe_surfaces();

    // Mark surfaces dirty if they overlap with any external damage region
    for i in 0..MAX_TASKS {
        if let Some(ref surface) = surfaces[i] {
            if surface.window_state() == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let (wx, wy) = surface.position();
            let sw = surface.width() as i32;
            let sh = surface.height() as i32;

            // Check each external damage region
            for d in 0..damage_count as usize {
                let region = unsafe { &*damage_regions.add(d) };

                // Convert damage region to screen coordinates
                let dx0 = region.x;
                let dy0 = region.y;
                let dx1 = region.x + region.width as i32 - 1;
                let dy1 = region.y + region.height as i32 - 1;

                // Check if surface overlaps with damage region
                let sx0 = wx;
                let sy0 = wy;
                let sx1 = wx + sw - 1;
                let sy1 = wy + sh - 1;

                if sx0 <= dx1 && sx1 >= dx0 && sy0 <= dy1 && sy1 >= dy0 {
                    // Surface overlaps with damage - mark dirty and add damage
                    surface.mark_dirty();

                    // Add damage to back buffer covering the overlap
                    surface.with_back_buffer(|buffer| {
                        let local_x0 = (dx0 - wx).max(0);
                        let local_y0 = (dy0 - wy).max(0);
                        let local_x1 = (dx1 - wx).min(sw - 1);
                        let local_y1 = (dy1 - wy).min(sh - 1);
                        buffer.add_damage(local_x0, local_y0, local_x1, local_y1);
                    });
                    // Commit to transfer damage to front buffer
                    surface.commit();
                    break; // Only need to mark dirty once per surface
                }
            }
        }
    }

    // Now render all dirty surfaces (including newly marked ones)
    compositor_present_safe()
}

/// Safe window position update
#[allow(dead_code)]
pub fn surface_set_window_position_safe(task_id: u32, x: i32, y: i32) -> c_int {
    match get_safe_surface(task_id) {
        Ok(surface) => {
            surface.set_position(x, y);
            0
        }
        Err(_) => -1,
    }
}

/// Safe window state update
#[allow(dead_code)]
pub fn surface_set_window_state_safe(task_id: u32, state: u8) -> c_int {
    match get_safe_surface(task_id) {
        Ok(surface) => {
            surface.set_window_state(state);
            0
        }
        Err(_) => -1,
    }
}

/// Safe window raise (increase z-order)
#[allow(dead_code)]
pub fn surface_raise_window_safe(task_id: u32) -> c_int {
    match get_safe_surface(task_id) {
        Ok(surface) => {
            let new_z = NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed);
            surface.set_z_order(new_z);
            0
        }
        Err(_) => -1,
    }
}

/// Safe blit within surface (copy region from one location to another)
#[allow(dead_code)]
pub fn surface_blit_safe(
    task_id: u32,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> VideoResult {
    if width <= 0 || height <= 0 {
        return Err(VideoError::Invalid);
    }

    let surface = get_or_create_safe_surface(task_id)?;

    surface.with_back_buffer(|buffer| {
        let buf_width = buffer.width() as i32;
        let buf_height = buffer.height() as i32;
        let bytes_pp = buffer.bytes_pp() as usize;
        let pitch = buffer.pitch();

        // Clip source region
        let src_x0 = src_x.max(0);
        let src_y0 = src_y.max(0);
        let src_x1 = (src_x + width - 1).min(buf_width - 1);
        let src_y1 = (src_y + height - 1).min(buf_height - 1);

        if src_x0 > src_x1 || src_y0 > src_y1 {
            return Err(VideoError::OutOfBounds);
        }

        let actual_width = (src_x1 - src_x0 + 1) as usize;
        let actual_height = (src_y1 - src_y0 + 1) as usize;

        // Clip destination
        let dst_x0 = dst_x.max(0);
        let dst_y0 = dst_y.max(0);
        let dst_x1 = (dst_x + actual_width as i32 - 1).min(buf_width - 1);
        let dst_y1 = (dst_y + actual_height as i32 - 1).min(buf_height - 1);

        if dst_x0 > dst_x1 || dst_y0 > dst_y1 {
            return Err(VideoError::OutOfBounds);
        }

        let copy_width = ((dst_x1 - dst_x0 + 1) as usize).min(actual_width);
        let copy_height = ((dst_y1 - dst_y0 + 1) as usize).min(actual_height);
        let row_bytes = copy_width * bytes_pp;

        let data = buffer.as_mut_slice();

        // Handle overlapping regions by copying in correct order
        if dst_y0 < src_y0 || (dst_y0 == src_y0 && dst_x0 < src_x0) {
            // Copy top-to-bottom, left-to-right
            for row in 0..copy_height {
                let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
                let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
                data.copy_within(src_off..src_off + row_bytes, dst_off);
            }
        } else {
            // Copy bottom-to-top, right-to-left
            for row in (0..copy_height).rev() {
                let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
                let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
                data.copy_within(src_off..src_off + row_bytes, dst_off);
            }
        }

        buffer.add_damage(dst_x0, dst_y0, dst_x1, dst_y1);
        Ok(())
    })
}

/// Safe implementation of surface_enumerate_windows using SAFE_SURFACES registry
#[allow(dead_code)]
pub fn surface_enumerate_windows_safe(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let registry = SAFE_SURFACES.lock();
    let mut count = 0u32;

    for (&task_id, surface) in registry.iter() {
        if count >= max_count {
            break;
        }

        // Skip invisible windows
        if !surface.is_visible() {
            continue;
        }

        // Get window info from atomics and buffers
        let x = surface.window_x.load(Ordering::Relaxed);
        let y = surface.window_y.load(Ordering::Relaxed);
        let state = surface.window_state.load(Ordering::Relaxed);

        // Get dimensions and damage from front buffer
        let (width, height, damage_count, damage_regions) = {
            let buffers = surface.buffers.lock();
            let front = buffers.front();
            let damage_slice = front.damage_regions();
            let dmg_count = front.damage_count();
            let mut regions = [WindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS];
            for (i, r) in damage_slice.iter().enumerate() {
                regions[i] = WindowDamageRect {
                    x0: r.x0,
                    y0: r.y0,
                    x1: r.x1,
                    y1: r.y1,
                };
            }
            (front.width(), front.height(), dmg_count, regions)
        };

        unsafe {
            let info = &mut *out_buffer.add(count as usize);
            info.task_id = task_id;
            info.x = x;
            info.y = y;
            info.width = width;
            info.height = height;
            info.state = state;
            info.damage_count = damage_count;
            info._padding = [0; 2];
            info.damage_regions = damage_regions;
            info.title = [0; 32]; // No title in SafeSurface - return empty
        }
        count += 1;
    }
    count
}

// =============================================================================
// Public API (delegating to safe implementations)
// =============================================================================

pub fn surface_draw_rect_filled_fast(
    task_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) -> VideoResult {
    surface_draw_rect_filled_fast_safe(task_id, x, y, w, h, color)
}

pub fn surface_draw_line(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    surface_draw_line_safe(task_id, x0, y0, x1, y1, color)
}

pub fn surface_draw_circle(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    surface_draw_circle_safe(task_id, cx, cy, radius, color)
}

pub fn surface_draw_circle_filled(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    surface_draw_circle_filled_safe(task_id, cx, cy, radius, color)
}

pub fn surface_font_draw_string(
    task_id: u32,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: u32,
    bg_color: u32,
) -> c_int {
    surface_font_draw_string_safe(task_id, x, y, str_ptr, fg_color, bg_color)
}

pub fn surface_blit(
    task_id: u32,
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> VideoResult {
    surface_blit_safe(task_id, src_x, src_y, dst_x, dst_y, width, height)
}

/// Commits the back buffer to the front buffer (Wayland-style double buffering).
pub fn surface_commit(task_id: u32) -> VideoResult {
    surface_commit_safe(task_id)
}

pub fn compositor_present() -> c_int {
    compositor_present_safe()
}

/// Compositor present with damage tracking (Wayland-style)
/// Uses external damage regions to mark overlapping surfaces for re-compositing
pub fn compositor_present_with_damage(damage_regions: *const DamageRegion, damage_count: u32) -> c_int {
    compositor_present_with_damage_safe(damage_regions, damage_count)
}

// Window management functions

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    surface_set_window_position_safe(task_id, x, y)
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    surface_set_window_state_safe(task_id, state)
}

pub fn surface_raise_window(task_id: u32) -> c_int {
    surface_raise_window_safe(task_id)
}

/// Exposed damage region for userland (matches DamageRect layout)
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct WindowDamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Maximum damage regions exposed per window (must match MAX_DAMAGE_REGIONS)
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = MAX_DAMAGE_REGIONS;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WindowInfo {
    pub task_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: u8,
    pub damage_count: u8,
    pub _padding: [u8; 2],
    // Individual damage regions
    pub damage_regions: [WindowDamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [c_char; 32],
}

pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    surface_enumerate_windows_safe(out_buffer, max_count)
}
