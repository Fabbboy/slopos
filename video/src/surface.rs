use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::video_bridge::{DamageRegion, VideoError, VideoResult};
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames};
use slopos_mm::phys_virt::mm_phys_to_virt;
use slopos_lib::{klog_info, klog_debug};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use slopos_sched::MAX_TASKS;
use spin::Mutex;

use crate::framebuffer;
use crate::font;

// Window state constants
pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

const SURFACE_BG_COLOR: u32 = 0x0000_0000;

// Maximum number of damage regions tracked per surface before merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

/// A single damage rectangle in surface-local coordinates
#[derive(Copy, Clone, Default)]
struct DamageRect {
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

    /// Returns the intersection of two rectangles, or None if they don't overlap
    fn intersect(&self, other: &DamageRect) -> Option<DamageRect> {
        if !self.is_valid() || !other.is_valid() {
            return None;
        }
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        if x0 <= x1 && y0 <= y1 {
            Some(DamageRect { x0, y0, x1, y1 })
        } else {
            None
        }
    }

}

// Maximum number of visible region fragments for occlusion culling
const MAX_VISIBLE_RECTS: usize = 16;

/// Tracks the visible (non-occluded) portions of a damage region during front-to-back composition
#[derive(Clone, Copy)]
struct VisibleRegion {
    rects: [DamageRect; MAX_VISIBLE_RECTS],
    count: usize,
}

impl VisibleRegion {
    /// Create a new visible region from a screen-space damage rectangle
    fn new(initial: DamageRect) -> Self {
        let mut rects = [DamageRect::invalid(); MAX_VISIBLE_RECTS];
        if initial.is_valid() {
            rects[0] = initial;
            Self { rects, count: 1 }
        } else {
            Self { rects, count: 0 }
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Subtracts an occluding rectangle from all visible regions.
    /// Each rectangle may be split into up to 4 fragments (top, bottom, left, right strips).
    fn subtract(&mut self, occluder: &DamageRect) {
        if !occluder.is_valid() || self.count == 0 {
            return;
        }

        let mut new_rects = [DamageRect::invalid(); MAX_VISIBLE_RECTS];
        let mut new_count = 0usize;

        for i in 0..self.count {
            let rect = &self.rects[i];
            if !rect.is_valid() {
                continue;
            }

            // Check if occluder overlaps this rect
            if rect.x1 < occluder.x0 || rect.x0 > occluder.x1 ||
               rect.y1 < occluder.y0 || rect.y0 > occluder.y1 {
                // No overlap - keep rect as-is
                if new_count < MAX_VISIBLE_RECTS {
                    new_rects[new_count] = *rect;
                    new_count += 1;
                }
                continue;
            }

            // Occluder overlaps - split into up to 4 fragments
            // We generate strips in order: top, bottom, left, right
            // The left/right strips are bounded by the occluder's vertical extent

            // Top strip (full width, above occluder)
            if rect.y0 < occluder.y0 {
                let top = DamageRect {
                    x0: rect.x0,
                    y0: rect.y0,
                    x1: rect.x1,
                    y1: occluder.y0 - 1,
                };
                if top.is_valid() && new_count < MAX_VISIBLE_RECTS {
                    new_rects[new_count] = top;
                    new_count += 1;
                }
            }

            // Bottom strip (full width, below occluder)
            if rect.y1 > occluder.y1 {
                let bottom = DamageRect {
                    x0: rect.x0,
                    y0: occluder.y1 + 1,
                    x1: rect.x1,
                    y1: rect.y1,
                };
                if bottom.is_valid() && new_count < MAX_VISIBLE_RECTS {
                    new_rects[new_count] = bottom;
                    new_count += 1;
                }
            }

            // Vertical bounds for left/right strips (clamped to occluder's vertical range)
            let mid_y0 = rect.y0.max(occluder.y0);
            let mid_y1 = rect.y1.min(occluder.y1);

            // Left strip (between occluder's vertical extent, left of occluder)
            if rect.x0 < occluder.x0 && mid_y0 <= mid_y1 {
                let left = DamageRect {
                    x0: rect.x0,
                    y0: mid_y0,
                    x1: occluder.x0 - 1,
                    y1: mid_y1,
                };
                if left.is_valid() && new_count < MAX_VISIBLE_RECTS {
                    new_rects[new_count] = left;
                    new_count += 1;
                }
            }

            // Right strip (between occluder's vertical extent, right of occluder)
            if rect.x1 > occluder.x1 && mid_y0 <= mid_y1 {
                let right = DamageRect {
                    x0: occluder.x1 + 1,
                    y0: mid_y0,
                    x1: rect.x1,
                    y1: mid_y1,
                };
                if right.is_valid() && new_count < MAX_VISIBLE_RECTS {
                    new_rects[new_count] = right;
                    new_count += 1;
                }
            }
        }

        // If we're at capacity, merge smallest pair to make room
        while new_count > MAX_VISIBLE_RECTS {
            Self::merge_smallest_pair_static(&mut new_rects, &mut new_count);
        }

        self.rects = new_rects;
        self.count = new_count;
    }

    /// Merges the two rectangles with smallest combined area (static version for arrays)
    fn merge_smallest_pair_static(rects: &mut [DamageRect; MAX_VISIBLE_RECTS], count: &mut usize) {
        if *count < 2 {
            return;
        }

        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        for i in 0..*count {
            if !rects[i].is_valid() {
                continue;
            }
            for j in (i + 1)..*count {
                if !rects[j].is_valid() {
                    continue;
                }
                let combined = rects[i].combined_area(&rects[j]);
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Merge i and j into i
        rects[best_i] = rects[best_i].union(&rects[best_j]);

        // Remove j by swapping with last element
        if best_j < *count - 1 {
            rects[best_j] = rects[*count - 1];
        }
        rects[*count - 1] = DamageRect::invalid();
        *count -= 1;
    }
}

#[derive(Copy, Clone)]
struct Surface {
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u8,
    bytes_pp: u8,
    pixel_format: u8,
    // Back buffer damage (client draws here, accumulates damage)
    back_damage_regions: [DamageRect; MAX_DAMAGE_REGIONS],
    back_damage_count: u8,
    // Front buffer damage (compositor reads this, cleared after composite)
    front_damage_regions: [DamageRect; MAX_DAMAGE_REGIONS],
    front_damage_count: u8,
    // Double buffer pointers - Wayland-style commit model
    front_buffer: *mut u8,  // Compositor reads from here
    back_buffer: *mut u8,   // Client draws to here
    x: i32,
    y: i32,
    committed: bool,        // True when new content ready for compositor
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    const fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            bytes_pp: 0,
            pixel_format: 0,
            back_damage_regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            back_damage_count: 0,
            front_damage_regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            front_damage_count: 0,
            front_buffer: ptr::null_mut(),
            back_buffer: ptr::null_mut(),
            x: 0,
            y: 0,
            committed: false,
        }
    }

    /// Returns true if the surface has committed content ready for compositing
    fn is_dirty(&self) -> bool {
        self.committed && self.front_damage_count > 0
    }

    /// Computes the bounding box union of all front damage regions (for compositor)
    fn front_damage_union(&self) -> DamageRect {
        if self.front_damage_count == 0 {
            return DamageRect::invalid();
        }
        let mut result = self.front_damage_regions[0];
        for i in 1..self.front_damage_count as usize {
            result = result.union(&self.front_damage_regions[i]);
        }
        result
    }
}

#[derive(Copy, Clone)]
struct SurfaceSlot {
    active: bool,
    task_id: u32,
    surface: Surface,
    window_state: u8,
    z_order: u32,
    title: [c_char; 32],
}

unsafe impl Send for SurfaceSlot {}
unsafe impl Sync for SurfaceSlot {}
impl SurfaceSlot {
    const fn empty() -> Self {
        Self {
            active: false,
            task_id: 0,
            surface: Surface::empty(),
            window_state: WINDOW_STATE_NORMAL,
            z_order: 0,
            title: [0; 32],
        }
    }
}

static SURFACES: Mutex<[SurfaceSlot; MAX_TASKS]> =
    Mutex::new([SurfaceSlot::empty(); MAX_TASKS]);
static SURFACE_CREATE_LOGGED: [AtomicU8; MAX_TASKS] = {
    const ZERO: AtomicU8 = AtomicU8::new(0);
    [ZERO; MAX_TASKS]
};
static COMPOSITOR_LOGGED: AtomicU8 = AtomicU8::new(0);
static COMPOSITOR_EMPTY_LOGGED: AtomicU8 = AtomicU8::new(0);
static NEXT_Z_ORDER: AtomicU32 = AtomicU32::new(1);

/// Sort indices array by z-order using insertion sort.
/// O(n) for nearly-sorted data (typical case after window raise).
fn sort_indices_by_z_order(
    indices: &mut [usize; MAX_TASKS],
    count: usize,
    slots: &[SurfaceSlot; MAX_TASKS],
) {
    for i in 1..count {
        let key_idx = indices[i];
        let key_z = slots[key_idx].z_order;
        let mut j = i;
        while j > 0 && slots[indices[j - 1]].z_order > key_z {
            indices[j] = indices[j - 1];
            j -= 1;
        }
        indices[j] = key_idx;
    }
}

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

fn find_slot(slots: &[SurfaceSlot; MAX_TASKS], task_id: u32) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| {
            if slot.active && slot.task_id == task_id {
                Some(idx)
            } else {
                None
            }
        })
}

fn find_free_slot(slots: &[SurfaceSlot; MAX_TASKS]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| if !slot.active { Some(idx) } else { None })
}

/// Merges the two back buffer damage regions with the smallest combined area
fn merge_smallest_back_damage(surface: &mut Surface) {
    if surface.back_damage_count < 2 {
        return;
    }

    let count = surface.back_damage_count as usize;
    let mut best_i = 0;
    let mut best_j = 1;
    let mut best_area = i32::MAX;

    // Find the pair with smallest combined area when merged
    for i in 0..count {
        for j in (i + 1)..count {
            let combined = surface.back_damage_regions[i].combined_area(&surface.back_damage_regions[j]);
            if combined < best_area {
                best_area = combined;
                best_i = i;
                best_j = j;
            }
        }
    }

    // Merge i and j into i
    let merged = surface.back_damage_regions[best_i].union(&surface.back_damage_regions[best_j]);
    surface.back_damage_regions[best_i] = merged;

    // Remove j by swapping with last element
    if best_j < count - 1 {
        surface.back_damage_regions[best_j] = surface.back_damage_regions[count - 1];
    }
    surface.back_damage_count -= 1;
}

/// Adds a damage region to the back buffer. If the array is full, merges the two
/// closest regions first to make room.
fn add_back_damage_region(surface: &mut Surface, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
    // Clip to surface bounds
    if x0 > x1 || y0 > y1 {
        return;
    }
    if x0 < 0 {
        x0 = 0;
    }
    if y0 < 0 {
        y0 = 0;
    }
    let max_x = surface.width as i32 - 1;
    let max_y = surface.height as i32 - 1;
    if x1 > max_x {
        x1 = max_x;
    }
    if y1 > max_y {
        y1 = max_y;
    }
    if x0 > x1 || y0 > y1 {
        return;
    }

    let new_rect = DamageRect { x0, y0, x1, y1 };

    // If array is full, merge two closest regions to make room
    if (surface.back_damage_count as usize) >= MAX_DAMAGE_REGIONS {
        merge_smallest_back_damage(surface);
    }

    // Add the new region
    surface.back_damage_regions[surface.back_damage_count as usize] = new_rect;
    surface.back_damage_count += 1;
}

/// Clears all front buffer damage regions (called after compositing)
fn clear_front_damage(surface: &mut Surface) {
    surface.front_damage_count = 0;
}

fn create_surface_for_task(
    slots: &mut [SurfaceSlot; MAX_TASKS],
    task_id: u32,
) -> Result<usize, VideoError> {
    let fb = framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)?;
    let bytes_pp = bytes_per_pixel(fb.bpp) as u8;
    if bytes_pp != 3 && bytes_pp != 4 {
        if (task_id as usize) < MAX_TASKS
            && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
        {
            klog_info!(
                "surface: invalid bytes_per_pixel {} for task {}",
                bytes_pp,
                task_id
            );
        }
        return Err(VideoError::Invalid);
    }

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
        let pitch = width.saturating_mul(bytes_pp as u32);
        let single_buffer_size = (pitch as u64).saturating_mul(height as u64);
        if single_buffer_size == 0 || single_buffer_size > (usize::MAX / 2) as u64 {
            continue;
        }
        // Allocate 2x memory for double buffering (front + back)
        let total_size = single_buffer_size.saturating_mul(2);
        let pages = (total_size + (PAGE_SIZE_4KB - 1)) / PAGE_SIZE_4KB;
        if pages == 0 || pages > u32::MAX as u64 {
            continue;
        }
        let phys = alloc_page_frames(pages as u32, ALLOC_FLAG_ZERO);
        if phys == 0 {
            continue;
        }
        let virt = mm_phys_to_virt(phys);
        let virt = if virt != 0 { virt } else { phys };

        // Set up double buffer pointers
        let front_buffer = virt as *mut u8;
        let back_buffer = unsafe { front_buffer.add(single_buffer_size as usize) };

        let slot = match find_free_slot(slots) {
            Some(idx) => idx,
            None => {
                if (task_id as usize) < MAX_TASKS
                    && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
                {
                    klog_info!("surface: no free slot for task {}", task_id);
                }
                return Err(VideoError::Invalid);
            }
        };

        // Calculate cascading window position
        let active_count = slots.iter().filter(|s| s.active).count();
        let cascade_offset = ((active_count as i32) * 32) % 200;
        let window_x = 100 + cascade_offset;
        let window_y = 100 + cascade_offset;

        // Assign z-order (higher = on top)
        let z_order = NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed);

        // Initialize damage regions with full surface dirty
        let initial_damage = DamageRect {
            x0: 0,
            y0: 0,
            x1: width as i32 - 1,
            y1: height as i32 - 1,
        };
        let mut front_damage_regions = [DamageRect::invalid(); MAX_DAMAGE_REGIONS];
        front_damage_regions[0] = initial_damage;
        let mut back_damage_regions = [DamageRect::invalid(); MAX_DAMAGE_REGIONS];
        back_damage_regions[0] = initial_damage;

        slots[slot] = SurfaceSlot {
            active: true,
            task_id,
            surface: Surface {
                width,
                height,
                pitch,
                bpp: fb.bpp,
                bytes_pp,
                pixel_format: fb.pixel_format,
                back_damage_regions,
                back_damage_count: 1,
                front_damage_regions,
                front_damage_count: 1,
                front_buffer,
                back_buffer,
                x: window_x,
                y: window_y,
                committed: true,  // Initial state needs compositing
            },
            window_state: WINDOW_STATE_NORMAL,
            z_order,
            title: [0; 32],
        };

        if SURFACE_BG_COLOR != 0 {
            surface_clear(&mut slots[slot].surface, SURFACE_BG_COLOR)?;
        }
        return Ok(slot);
    }

    if (task_id as usize) < MAX_TASKS
        && SURFACE_CREATE_LOGGED[task_id as usize].swap(1, Ordering::Relaxed) == 0
    {
        klog_info!("surface: page alloc failed for task {}", task_id);
    }
    Err(VideoError::Invalid)
}

/// Get or create surface slot for a task (no synchronization needed with double buffering)
fn get_or_create_surface(task_id: u32) -> Result<usize, VideoError> {
    let mut slots = SURFACES.lock();
    match find_slot(&slots, task_id) {
        Some(idx) => Ok(idx),
        None => create_surface_for_task(&mut slots, task_id),
    }
}

fn with_surface_mut(task_id: u32, f: impl FnOnce(&mut Surface) -> VideoResult) -> VideoResult {
    // With double buffering, no synchronization needed - clients draw to back_buffer
    // while compositor reads from front_buffer
    let slot = get_or_create_surface(task_id)?;

    // Get raw pointer while holding lock
    let surface_ptr = {
        let mut slots = SURFACES.lock();
        &mut slots[slot].surface as *mut Surface
    };

    // Execute drawing closure without holding lock (safe: back_buffer is independent)
    unsafe { f(&mut *surface_ptr) }
}

fn surface_clear(surface: &mut Surface, color: u32) -> VideoResult {
    if surface.back_buffer.is_null() {
        return Err(VideoError::Invalid);
    }
    let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
    let row_bytes = surface.width.saturating_mul(surface.bytes_pp as u32) as usize;
    for row in 0..surface.height as usize {
        let row_ptr = unsafe { surface.back_buffer.add(row * surface.pitch as usize) };
        for col in 0..surface.width as usize {
            let pixel_ptr = unsafe { row_ptr.add(col * surface.bytes_pp as usize) };
            unsafe { write_pixel(pixel_ptr, surface.bytes_pp, converted) };
        }
        let _ = row_bytes;
    }
    add_back_damage_region(
        surface,
        0,
        0,
        surface.width as i32 - 1,
        surface.height as i32 - 1,
    );
    Ok(())
}

unsafe fn write_pixel(ptr: *mut u8, bytes_pp: u8, color: u32) {
    match bytes_pp {
        4 => {
            let dst = ptr as *mut u32;
            dst.write_unaligned(color);
        }
        3 => {
            let bytes = color.to_le_bytes();
            ptr.copy_from_nonoverlapping(bytes.as_ptr(), 3);
        }
        _ => {}
    }
}

fn surface_set_pixel(surface: &mut Surface, x: i32, y: i32, color: u32) -> VideoResult {
    if x < 0 || y < 0 || x as u32 >= surface.width || y as u32 >= surface.height {
        return Err(VideoError::OutOfBounds);
    }
    if surface.back_buffer.is_null() {
        return Err(VideoError::Invalid);
    }

    let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
    let offset = (y as usize * surface.pitch as usize)
        + (x as usize * surface.bytes_pp as usize);
    unsafe {
        let ptr = surface.back_buffer.add(offset);
        write_pixel(ptr, surface.bytes_pp, converted);
    }
    Ok(())
}

pub fn surface_draw_rect_filled_fast(
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
    let result = with_surface_mut(task_id, |surface| {
        let mut x0 = x;
        let mut y0 = y;
        let mut x1 = x + w - 1;
        let mut y1 = y + h - 1;
        clip_rect(surface, &mut x0, &mut y0, &mut x1, &mut y1)?;
        if surface.back_buffer.is_null() {
            return Err(VideoError::Invalid);
        }
        let converted = framebuffer::framebuffer_convert_color_for(surface.pixel_format, color);
        let bytes_pp = surface.bytes_pp as usize;
        let pitch = surface.pitch as usize;
        let span_w = (x1 - x0 + 1) as usize;
        for row in y0..=y1 {
            let row_off = row as usize * pitch + x0 as usize * bytes_pp;
            unsafe {
                let dst = surface.back_buffer.add(row_off);
                match bytes_pp {
                    4 => {
                        if converted == 0 {
                            ptr::write_bytes(dst, 0, span_w * 4);
                        } else {
                            let dst32 = dst as *mut u32;
                            for col in 0..span_w {
                                dst32.add(col).write_unaligned(converted);
                            }
                        }
                    }
                    3 => {
                        let bytes = converted.to_le_bytes();
                        for col in 0..span_w {
                            let px = dst.add(col * 3);
                            px.write(bytes[0]);
                            px.add(1).write(bytes[1]);
                            px.add(2).write(bytes[2]);
                        }
                    }
                    _ => {}
                }
            }
        }
        add_back_damage_region(surface, x0, y0, x1, y1);
        Ok(())
    });
    result
}

pub fn surface_draw_line(
    task_id: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) -> VideoResult {
    with_surface_mut(task_id, |surface| {
        let mut x0 = x0;
        let mut y0 = y0;
        let x1 = x1;
        let y1 = y1;
        let min_x = x0.min(x1);
        let min_y = y0.min(y1);
        let max_x = x0.max(x1);
        let max_y = y0.max(y1);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            let _ = surface_set_pixel(surface, x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
        add_back_damage_region(surface, min_x, min_y, max_x, max_y);
        Ok(())
    })
}

pub fn surface_draw_circle(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        let mut x = radius;
        let mut y = 0;
        let mut err = 1 - radius;
        while x >= y {
            let _ = surface_set_pixel(surface, cx + x, cy + y, color);
            let _ = surface_set_pixel(surface, cx + y, cy + x, color);
            let _ = surface_set_pixel(surface, cx - y, cy + x, color);
            let _ = surface_set_pixel(surface, cx - x, cy + y, color);
            let _ = surface_set_pixel(surface, cx - x, cy - y, color);
            let _ = surface_set_pixel(surface, cx - y, cy - x, color);
            let _ = surface_set_pixel(surface, cx + y, cy - x, color);
            let _ = surface_set_pixel(surface, cx + x, cy - y, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
        add_back_damage_region(
            surface,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        Ok(())
    })
}

pub fn surface_draw_circle_filled(
    task_id: u32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) -> VideoResult {
    if radius <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        let mut x = radius;
        let mut y = 0;
        let mut err = 1 - radius;
        while x >= y {
            draw_hline(surface, cx - x, cx + x, cy + y, color);
            draw_hline(surface, cx - x, cx + x, cy - y, color);
            draw_hline(surface, cx - y, cx + y, cy + x, color);
            draw_hline(surface, cx - y, cx + y, cy - x, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
        add_back_damage_region(
            surface,
            cx - radius,
            cy - radius,
            cx + radius,
            cy + radius,
        );
        Ok(())
    })
}

pub fn surface_font_draw_string(
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
    let mut tmp = [0u8; 1024];
    let text = unsafe { c_str_to_bytes(str_ptr, &mut tmp) };
    let rc = with_surface_mut(task_id, |surface| {
        let mut cx = x;
        let mut cy = y;
        let mut dirty = false;
        let mut dirty_x0 = 0;
        let mut dirty_y0 = 0;
        let mut dirty_x1 = 0;
        let mut dirty_y1 = 0;
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
                    draw_glyph(surface, cx, cy, ch, fg_color, bg_color)?;
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
                    if cx + font::FONT_CHAR_WIDTH > surface.width as i32 {
                        cx = x;
                        cy += font::FONT_CHAR_HEIGHT;
                    }
                }
            }
            if cy >= surface.height as i32 {
                break;
            }
        }
        if dirty {
            add_back_damage_region(surface, dirty_x0, dirty_y0, dirty_x1, dirty_y1);
        }
        Ok(())
    });
    if rc.is_ok() { 0 } else { -1 }
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
    if width <= 0 || height <= 0 {
        return Err(VideoError::Invalid);
    }
    with_surface_mut(task_id, |surface| {
        if surface.back_buffer.is_null() {
            return Err(VideoError::Invalid);
        }
        let bytes_pp = surface.bytes_pp as usize;
        let mut w = width;
        let mut h = height;
        if src_x < 0 || src_y < 0 || dst_x < 0 || dst_y < 0 {
            return Err(VideoError::OutOfBounds);
        }
        if src_x + w > surface.width as i32 {
            w = surface.width as i32 - src_x;
        }
        if dst_x + w > surface.width as i32 {
            w = surface.width as i32 - dst_x;
        }
        if src_y + h > surface.height as i32 {
            h = surface.height as i32 - src_y;
        }
        if dst_y + h > surface.height as i32 {
            h = surface.height as i32 - dst_y;
        }
        if w <= 0 || h <= 0 {
            return Err(VideoError::Invalid);
        }
        for row in 0..h {
            let src_off = ((src_y + row) as usize * surface.pitch as usize)
                + (src_x as usize * bytes_pp);
            let dst_off = ((dst_y + row) as usize * surface.pitch as usize)
                + (dst_x as usize * bytes_pp);
            unsafe {
                let src_ptr = surface.back_buffer.add(src_off);
                let dst_ptr = surface.back_buffer.add(dst_off);
                ptr::copy(src_ptr, dst_ptr, (w as usize) * bytes_pp);
            }
        }
        add_back_damage_region(
            surface,
            dst_x,
            dst_y,
            dst_x + w - 1,
            dst_y + h - 1,
        );
        Ok(())
    })
}

/// Commits the back buffer to the front buffer (Wayland-style double buffering).
/// This atomically:
/// 1. Copies back buffer content to front buffer (maintains coherency)
/// 2. Transfers back damage to front damage
/// 3. Clears back damage for next frame
/// 4. Sets committed=true to signal compositor
///
/// Note: We copy instead of swap so that the back buffer retains the current
/// working state. This allows incremental rendering (like shells) to work
/// correctly without needing full redraws after each commit.
pub fn surface_commit(task_id: u32) -> VideoResult {
    let mut slots = SURFACES.lock();
    let slot_idx = match find_slot(&slots, task_id) {
        Some(idx) => idx,
        None => return Err(VideoError::Invalid),
    };

    let surface = &mut slots[slot_idx].surface;

    // Validate buffers exist
    if surface.front_buffer.is_null() || surface.back_buffer.is_null() {
        return Err(VideoError::Invalid);
    }

    // Copy back buffer to front buffer (maintains working state in back buffer)
    let buffer_size = (surface.pitch as usize) * (surface.height as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(
            surface.back_buffer,
            surface.front_buffer,
            buffer_size,
        );
    }

    // Accumulate damage from back to front (don't replace, merge!)
    // This allows multiple commits to accumulate damage until compositor runs
    for i in 0..surface.back_damage_count as usize {
        let back_region = surface.back_damage_regions[i];

        // If front damage is full, merge two closest regions to make room
        if (surface.front_damage_count as usize) >= MAX_DAMAGE_REGIONS {
            // Find two closest regions and merge them
            if surface.front_damage_count >= 2 {
                let mut best_i = 0;
                let mut best_j = 1;
                let mut best_area = i32::MAX;

                for ii in 0..surface.front_damage_count as usize {
                    for jj in (ii + 1)..surface.front_damage_count as usize {
                        let r1 = &surface.front_damage_regions[ii];
                        let r2 = &surface.front_damage_regions[jj];
                        let merged_x0 = r1.x0.min(r2.x0);
                        let merged_y0 = r1.y0.min(r2.y0);
                        let merged_x1 = r1.x1.max(r2.x1);
                        let merged_y1 = r1.y1.max(r2.y1);
                        let area = (merged_x1 - merged_x0 + 1) * (merged_y1 - merged_y0 + 1);
                        if area < best_area {
                            best_area = area;
                            best_i = ii;
                            best_j = jj;
                        }
                    }
                }

                // Merge best_i and best_j into best_i
                let r1 = surface.front_damage_regions[best_i];
                let r2 = surface.front_damage_regions[best_j];
                surface.front_damage_regions[best_i] = DamageRect {
                    x0: r1.x0.min(r2.x0),
                    y0: r1.y0.min(r2.y0),
                    x1: r1.x1.max(r2.x1),
                    y1: r1.y1.max(r2.y1),
                };

                // Remove best_j by shifting
                for k in best_j..(surface.front_damage_count as usize - 1) {
                    surface.front_damage_regions[k] = surface.front_damage_regions[k + 1];
                }
                surface.front_damage_count -= 1;
            }
        }

        // Add the back damage region to front
        if (surface.front_damage_count as usize) < MAX_DAMAGE_REGIONS {
            surface.front_damage_regions[surface.front_damage_count as usize] = back_region;
            surface.front_damage_count += 1;
        }
    }

    // Clear back damage for next frame
    surface.back_damage_count = 0;

    // Signal compositor that new content is ready
    surface.committed = true;

    Ok(())
}

pub fn compositor_present() -> c_int {
    if COMPOSITOR_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        klog_info!("compositor: present loop online");
    }
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return -1,
    };
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let slots_snapshot = {
        let slots = SURFACES.lock();
        *slots
    };

    // With double buffering, no need to set compositing flag - we read from front_buffer
    // which is independent from the back_buffer that clients draw to

    let mut active = 0u32;
    let mut dirty_tasks = [0u32; MAX_TASKS];
    let mut dirty_count = 0usize;
    let mut did_work = false;
    let fb_width = fb.width as i32;
    let fb_height = fb.height as i32;

    // Build sorted indices array by z-order (back to front)
    let mut indices = [0usize; MAX_TASKS];
    let mut index_count = 0usize;
    for (idx, slot) in slots_snapshot.iter().enumerate() {
        if slot.active {
            indices[index_count] = idx;
            index_count += 1;
        }
    }
    // Sort indices by z-order (lowest first)
    // Uses insertion sort - O(n) for nearly-sorted arrays (typical after window raise)
    sort_indices_by_z_order(&mut indices, index_count, &slots_snapshot);

    // Iterate windows back-to-front
    for idx_pos in 0..index_count {
        let slot = &slots_snapshot[indices[idx_pos]];
        active = active.saturating_add(1);

        // Skip minimized windows
        if slot.window_state == WINDOW_STATE_MINIMIZED {
            continue;
        }

        let surface = &slot.surface;
        if surface.front_buffer.is_null() {
            continue;
        }
        if !surface.is_dirty() {
            continue;
        }
        if surface.bpp != fb.bpp {
            return -1;
        }

        // Get the union of all front damage regions (what compositor needs to render)
        let damage = surface.front_damage_union();
        if !damage.is_valid() {
            continue;
        }
        let mut src_x = damage.x0;
        let mut src_y = damage.y0;
        let mut src_x1 = damage.x1;
        let mut src_y1 = damage.y1;
        if src_x < 0 {
            src_x = 0;
        }
        if src_y < 0 {
            src_y = 0;
        }
        let max_x = surface.width as i32 - 1;
        let max_y = surface.height as i32 - 1;
        if src_x1 > max_x {
            src_x1 = max_x;
        }
        if src_y1 > max_y {
            src_y1 = max_y;
        }
        if src_x > src_x1 || src_y > src_y1 {
            continue;
        }

        let mut dst_x = surface.x + src_x;
        let mut dst_y = surface.y + src_y;
        let mut copy_w = src_x1 - src_x + 1;
        let mut copy_h = src_y1 - src_y + 1;
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
            continue;
        }

        for row in 0..copy_h {
            let src_row = (src_y + row) as usize * surface.pitch as usize;
            let dst_off = ((dst_y + row) as usize * fb.pitch as usize)
                + (dst_x as usize * bytes_pp);
            unsafe {
                let src_ptr = surface.front_buffer.add(src_row + (src_x as usize * bytes_pp));
                let dst_ptr = fb.base.add(dst_off);
                let row_bytes = copy_w as usize * bytes_pp;
                ptr::copy_nonoverlapping(src_ptr, dst_ptr, row_bytes);
            }
        }
        did_work = true;
        if dirty_count < MAX_TASKS {
            dirty_tasks[dirty_count] = slot.task_id;
            dirty_count += 1;
        }
    }
    if dirty_count > 0 {
        let mut slots = SURFACES.lock();
        for idx in 0..dirty_count {
            let task_id = dirty_tasks[idx];
            if let Some(slot_idx) = find_slot(&slots, task_id) {
                let surface = &mut slots[slot_idx].surface;
                // Clear committed flag and front damage after compositing
                surface.committed = false;
                clear_front_damage(surface);
            }
        }
    }
    if active == 0 && COMPOSITOR_EMPTY_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        klog_info!("compositor: no active surfaces to present");
    }
    if did_work { 1 } else { 0 }
}

/// Compositor present with damage tracking (Wayland-style)
/// Forces recomposition of windows in damaged regions, even if windows aren't dirty
pub fn compositor_present_with_damage(damage_regions: *const DamageRegion, damage_count: u32) -> c_int {
    if COMPOSITOR_LOGGED.swap(1, Ordering::Relaxed) == 0 {
        klog_info!("compositor: present loop online (with damage tracking)");
    }

    if damage_regions.is_null() || damage_count == 0 {
        // No damage - nothing to do!
        return 0;
    }

    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return -1,
    };
    let bytes_pp = bytes_per_pixel(fb.bpp) as usize;
    let slots_snapshot = {
        let slots = SURFACES.lock();
        *slots
    };

    // With double buffering, no need to set compositing flag - we read from front_buffer
    // which is independent from the back_buffer that clients draw to

    let fb_width = fb.width as i32;
    let fb_height = fb.height as i32;

    // Build sorted indices array by z-order (back to front)
    let mut indices = [0usize; MAX_TASKS];
    let mut index_count = 0usize;
    for (idx, slot) in slots_snapshot.iter().enumerate() {
        if slot.active {
            indices[index_count] = idx;
            index_count += 1;
        }
    }
    // Sort indices by z-order (lowest first)
    // Uses insertion sort - O(n) for nearly-sorted arrays (typical after window raise)
    sort_indices_by_z_order(&mut indices, index_count, &slots_snapshot);

    let mut did_work = false;
    let mut composited_tasks = [0u32; MAX_TASKS];
    let mut composited_count = 0usize;

    // Debug counters for occlusion culling effectiveness
    let mut windows_composited = 0u32;
    let mut windows_culled = 0u32;
    let mut early_exits = 0u32;

    // Per-surface coverage tracking (in surface-local coordinates)
    // Tracks which portions of each surface's dirty region were actually composited
    #[derive(Copy, Clone)]
    struct SurfaceCoverage {
        task_id: u32,
        covered: bool,        // True if at least one region was composited
        union_x0: i32,        // Union of all composited regions
        union_y0: i32,
        union_x1: i32,
        union_y1: i32,
    }

    let mut coverage_tracking = [SurfaceCoverage {
        task_id: 0,
        covered: false,
        union_x0: i32::MAX,
        union_y0: i32::MAX,
        union_x1: i32::MIN,
        union_y1: i32::MIN,
    }; MAX_TASKS];
    let mut coverage_count = 0usize;

    // For each damage region, composite windows with occlusion culling
    // Iterate front-to-back (highest z-order first) and track visible regions
    for damage_idx in 0..damage_count as usize {
        let damage = unsafe { &*damage_regions.add(damage_idx) };

        // Create a VisibleRegion to track what's still visible (not yet occluded)
        let initial_rect = DamageRect {
            x0: damage.x,
            y0: damage.y,
            x1: damage.x + damage.width - 1,
            y1: damage.y + damage.height - 1,
        };
        let mut visible = VisibleRegion::new(initial_rect);

        // Iterate windows FRONT-TO-BACK (highest z-order first = reverse iteration)
        // This allows us to skip compositing pixels that will be overwritten
        for idx_pos in (0..index_count).rev() {
            // Early exit if nothing visible remains (fully occluded by higher windows)
            if visible.is_empty() {
                early_exits += 1;
                windows_culled += (index_count - idx_pos) as u32;
                break;
            }

            let slot = &slots_snapshot[indices[idx_pos]];

            // Skip minimized windows (don't occlude and don't composite)
            if slot.window_state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let surface = &slot.surface;
            if surface.front_buffer.is_null() {
                continue;
            }
            if surface.bpp != fb.bpp {
                return -1;
            }

            // Window bounds in screen coordinates
            let win_bounds = DamageRect {
                x0: surface.x,
                y0: surface.y,
                x1: surface.x + surface.width as i32 - 1,
                y1: surface.y + surface.height as i32 - 1,
            };

            // Process each visible rect and composite any intersections with this window
            for vis_idx in 0..visible.count {
                let vis_rect = &visible.rects[vis_idx];
                if !vis_rect.is_valid() {
                    continue;
                }

                // Calculate intersection of visible rect with window bounds
                let isect = match vis_rect.intersect(&win_bounds) {
                    Some(r) => r,
                    None => continue, // No overlap
                };

                // Convert intersection to surface-relative coordinates
                let src_x = isect.x0 - surface.x;
                let src_y = isect.y0 - surface.y;
                let copy_w = isect.x1 - isect.x0 + 1;
                let copy_h = isect.y1 - isect.y0 + 1;

                if copy_w <= 0 || copy_h <= 0 {
                    continue;
                }

                // Clip to framebuffer bounds
                let mut dst_x = isect.x0;
                let mut dst_y = isect.y0;
                let mut final_w = copy_w;
                let mut final_h = copy_h;

                if dst_x < 0 {
                    final_w += dst_x;
                    dst_x = 0;
                }
                if dst_y < 0 {
                    final_h += dst_y;
                    dst_y = 0;
                }
                if dst_x + final_w > fb_width {
                    final_w = fb_width - dst_x;
                }
                if dst_y + final_h > fb_height {
                    final_h = fb_height - dst_y;
                }

                if final_w <= 0 || final_h <= 0 {
                    continue;
                }

                // Composite window into framebuffer (read from front_buffer)
                for row in 0..final_h {
                    let src_row = ((src_y + row) as usize) * surface.pitch as usize;
                    let dst_off = ((dst_y + row) as usize * fb.pitch as usize)
                        + (dst_x as usize * bytes_pp);
                    unsafe {
                        let src_ptr = surface.front_buffer.add(src_row + (src_x as usize * bytes_pp));
                        let dst_ptr = fb.base.add(dst_off);
                        let row_bytes = final_w as usize * bytes_pp;
                        ptr::copy_nonoverlapping(src_ptr, dst_ptr, row_bytes);
                    }
                }

                // Track coverage for this surface (in surface-local coordinates)
                let composited_x0 = src_x;
                let composited_y0 = src_y;
                let composited_x1 = src_x + copy_w - 1;
                let composited_y1 = src_y + copy_h - 1;

                // Find or create coverage entry for this task
                let mut coverage_idx = None;
                for i in 0..coverage_count {
                    if coverage_tracking[i].task_id == slot.task_id {
                        coverage_idx = Some(i);
                        break;
                    }
                }
                if coverage_idx.is_none() && coverage_count < MAX_TASKS {
                    coverage_idx = Some(coverage_count);
                    coverage_tracking[coverage_count].task_id = slot.task_id;
                    coverage_count += 1;
                }

                // Update union bounds to include this composited region
                if let Some(idx) = coverage_idx {
                    let cov = &mut coverage_tracking[idx];
                    if !cov.covered {
                        // First region for this surface
                        cov.covered = true;
                        cov.union_x0 = composited_x0;
                        cov.union_y0 = composited_y0;
                        cov.union_x1 = composited_x1;
                        cov.union_y1 = composited_y1;
                    } else {
                        // Expand union to include new region
                        cov.union_x0 = cov.union_x0.min(composited_x0);
                        cov.union_y0 = cov.union_y0.min(composited_y0);
                        cov.union_x1 = cov.union_x1.max(composited_x1);
                        cov.union_y1 = cov.union_y1.max(composited_y1);
                    }
                }

                // Track which tasks we've composited
                if composited_count < MAX_TASKS {
                    let task_id = slot.task_id;
                    let mut already_tracked = false;
                    for i in 0..composited_count {
                        if composited_tasks[i] == task_id {
                            already_tracked = true;
                            break;
                        }
                    }
                    if !already_tracked {
                        composited_tasks[composited_count] = task_id;
                        composited_count += 1;
                    }
                }

                did_work = true;
                windows_composited += 1;
            }

            // After compositing this window, subtract its bounds from visible region
            // This ensures lower windows won't composite to areas covered by this window
            visible.subtract(&win_bounds);
        }
    }

    // Clear or update dirty flags based on coverage
    // Only clear dirty flag if the ENTIRE dirty region was composited
    if coverage_count > 0 {
        let mut slots = SURFACES.lock();
        for i in 0..coverage_count {
            let cov = &coverage_tracking[i];
            if !cov.covered {
                continue;  // No coverage for this surface
            }

            if let Some(slot_idx) = find_slot(&slots, cov.task_id) {
                let surface = &mut slots[slot_idx].surface;

                // Skip dirty check if surface is not currently dirty (defensive check)
                if !surface.is_dirty() {
                    continue;
                }

                // Get the union of all front damage regions
                let damage = surface.front_damage_union();

                // Check if coverage fully contains damage region union
                let fully_covered = cov.union_x0 <= damage.x0
                    && cov.union_y0 <= damage.y0
                    && cov.union_x1 >= damage.x1
                    && cov.union_y1 >= damage.y1;

                if fully_covered {
                    // Clear committed flag and front damage regions completely
                    surface.committed = false;
                    clear_front_damage(surface);
                }
                // else: Partial coverage - keep damage regions as-is
                // The compositor will re-enumerate and process remaining damage next frame
            }
        }
    }

    // Debug logging for occlusion culling effectiveness (gated by boot.debug=on)
    if windows_culled > 0 || early_exits > 0 {
        klog_debug!(
            "compositor: occlusion culling: {} composited, {} culled, {} early exits",
            windows_composited, windows_culled, early_exits
        );
    }

    if did_work { 1 } else { 0 }
}

fn clip_rect(
    surface: &Surface,
    x0: &mut i32,
    y0: &mut i32,
    x1: &mut i32,
    y1: &mut i32,
) -> VideoResult {
    if *x0 < 0 {
        *x0 = 0;
    }
    if *y0 < 0 {
        *y0 = 0;
    }
    if *x1 >= surface.width as i32 {
        *x1 = surface.width as i32 - 1;
    }
    if *y1 >= surface.height as i32 {
        *y1 = surface.height as i32 - 1;
    }
    if *x0 > *x1 || *y0 > *y1 {
        return Err(VideoError::OutOfBounds);
    }
    Ok(())
}

fn draw_hline(surface: &mut Surface, x0: i32, x1: i32, y: i32, color: u32) {
    let mut x0 = x0;
    let mut x1 = x1;
    if y < 0 || y >= surface.height as i32 {
        return;
    }
    if x0 < 0 {
        x0 = 0;
    }
    if x1 >= surface.width as i32 {
        x1 = surface.width as i32 - 1;
    }
    for x in x0..=x1 {
        let _ = surface_set_pixel(surface, x, y, color);
    }
}

fn draw_glyph(
    surface: &mut Surface,
    x: i32,
    y: i32,
    ch: u8,
    fg_color: u32,
    bg_color: u32,
) -> VideoResult {
    let glyph = font::font_glyph(ch).unwrap_or_else(|| {
        font::font_glyph(b' ').unwrap()
    });
    for (row_idx, row_bits) in glyph.iter().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py >= surface.height as i32 {
            continue;
        }
        for col in 0..font::FONT_CHAR_WIDTH {
            let px = x + col;
            if px < 0 || px >= surface.width as i32 {
                continue;
            }
            let mask = 1u8 << (7 - col);
            let color = if (row_bits & mask) != 0 {
                fg_color
            } else {
                bg_color
            };
            let _ = surface_set_pixel(surface, px, py, color);
        }
    }
    Ok(())
}

unsafe fn c_str_len(ptr: *const c_char) -> usize {
    if ptr.is_null() {
        return 0;
    }
    let mut len = 0usize;
    let mut p = ptr;
    while unsafe { *p } != 0 {
        len += 1;
        p = unsafe { p.add(1) };
    }
    len
}

unsafe fn c_str_to_bytes<'a>(ptr: *const c_char, buf: &'a mut [u8]) -> &'a [u8] {
    if ptr.is_null() {
        return &[];
    }
    let len = unsafe { c_str_len(ptr) }.min(buf.len());
    for i in 0..len {
        unsafe {
            buf[i] = *ptr.add(i) as u8;
        }
    }
    &buf[..len]
}

// Window management functions

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    let mut slots = SURFACES.lock();
    let slot_idx = match find_slot(&slots, task_id) {
        Some(idx) => idx,
        None => return -1,
    };
    slots[slot_idx].surface.x = x;
    slots[slot_idx].surface.y = y;
    // Note: Don't mark surface dirty - content is unchanged, only position changed.
    // The userland compositor tracks old/new positions and adds screen-level damage.
    0
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    if state > WINDOW_STATE_MAXIMIZED {
        return -1;
    }
    let mut slots = SURFACES.lock();
    let slot_idx = match find_slot(&slots, task_id) {
        Some(idx) => idx,
        None => return -1,
    };
    slots[slot_idx].window_state = state;
    // Mark entire surface dirty to trigger redraw when state changes (minimize/restore)
    let surface = &mut slots[slot_idx].surface;
    add_back_damage_region(surface, 0, 0, surface.width as i32 - 1, surface.height as i32 - 1);
    // Auto-commit when window state changes so compositor sees the update
    surface.front_damage_regions = surface.back_damage_regions;
    surface.front_damage_count = surface.back_damage_count;
    surface.back_damage_count = 0;
    surface.committed = true;
    0
}

pub fn surface_raise_window(task_id: u32) -> c_int {
    let mut slots = SURFACES.lock();
    let slot_idx = match find_slot(&slots, task_id) {
        Some(idx) => idx,
        None => return -1,
    };
    // Assign new z-order to bring window to front
    slots[slot_idx].z_order = NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed);
    0
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
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }
    let slots = SURFACES.lock();
    let mut count = 0u32;

    // Copy all active windows (compositor will sort by z-order if needed)
    for slot in slots.iter() {
        if !slot.active {
            continue;
        }
        if count >= max_count {
            break;
        }
        unsafe {
            let info = &mut *out_buffer.add(count as usize);
            info.task_id = slot.task_id;
            info.x = slot.surface.x;
            info.y = slot.surface.y;
            info.width = slot.surface.width;
            info.height = slot.surface.height;
            info.state = slot.window_state;
            // Report front damage (what compositor cares about)
            info.damage_count = slot.surface.front_damage_count;
            info._padding = [0; 2];
            // Copy individual front damage regions
            for i in 0..MAX_WINDOW_DAMAGE_REGIONS {
                if i < slot.surface.front_damage_count as usize {
                    let r = &slot.surface.front_damage_regions[i];
                    info.damage_regions[i] = WindowDamageRect {
                        x0: r.x0,
                        y0: r.y0,
                        x1: r.x1,
                        y1: r.y1,
                    };
                } else {
                    info.damage_regions[i] = WindowDamageRect::default();
                }
            }
            info.title = slot.title;
        }
        count += 1;
    }
    count
}
