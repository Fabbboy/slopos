//! Lock-Free Surface Implementation
//!
//! Surfaces are now plain structs owned exclusively by the Compositor.
//! No interior mutability or locks - all mutations happen through events.

use slopos_drivers::video_bridge::VideoError;
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use slopos_mm::phys_virt::mm_phys_to_virt;

use super::events::WINDOW_STATE_NORMAL;

/// Maximum number of damage regions tracked per surface before merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

/// A single damage rectangle in surface-local coordinates
#[derive(Copy, Clone, Default, Debug)]
pub struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32, // Inclusive
    pub y1: i32, // Inclusive
}

impl DamageRect {
    pub const fn invalid() -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: -1,
            y1: -1,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    pub fn area(&self) -> i32 {
        if !self.is_valid() {
            0
        } else {
            (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)
        }
    }

    pub fn union(&self, other: &DamageRect) -> DamageRect {
        DamageRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn combined_area(&self, other: &DamageRect) -> i32 {
        self.union(other).area()
    }
}

/// Owned buffer backed by page frame allocation.
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

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
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

    pub fn damage_regions(&self) -> &[DamageRect] {
        &self.damage_regions[..self.damage_count as usize]
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

/// A surface owned by the compositor - no locks needed!
///
/// All mutations happen through the compositor's event loop,
/// ensuring single-threaded access.
pub struct Surface {
    // === Immutable after creation ===
    pub task_id: u32,
    pub pixel_format: u8,

    // === Owned buffers (no Mutex!) ===
    pub buffers: DoubleBuffer,

    // === Mutable state (direct access, no atomics) ===
    pub dirty: bool,
    pub x: i32,
    pub y: i32,
    pub z_order: u32,
    pub visible: bool,
    pub window_state: u8,

    /// Associated shared memory token (0 if not using SHM)
    pub shm_token: u32,
}

impl Surface {
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
            buffers: DoubleBuffer::new(width, height, bpp)?,
            dirty: true,
            x: 0,
            y: 0,
            z_order: 0,
            visible: true,
            window_state: WINDOW_STATE_NORMAL,
            shm_token: 0,
        })
    }

    /// Commit back buffer to front
    pub fn commit(&mut self) {
        self.buffers.commit();
        self.dirty = true;
    }

    /// Set window position
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        self.dirty = true;
    }

    /// Set window state
    pub fn set_window_state(&mut self, state: u8) {
        self.window_state = state;
        self.dirty = true;
    }

    /// Set visibility
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.dirty = true;
    }

    /// Add damage to front buffer
    pub fn add_front_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.buffers.front_mut().add_damage(x0, y0, x1, y1);
        self.dirty = true;
    }

    /// Clear front buffer damage
    pub fn clear_front_damage(&mut self) {
        self.buffers.front_mut().clear_damage();
    }

    /// Get surface dimensions
    pub fn dimensions(&self) -> (u32, u32) {
        (self.buffers.width(), self.buffers.height())
    }
}
