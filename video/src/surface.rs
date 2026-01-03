use core::ffi::{c_char, c_int};

use alloc::sync::Arc;
use alloc::collections::BTreeMap;

use slopos_drivers::video_bridge::{VideoError, VideoResult};
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use slopos_mm::phys_virt::mm_phys_to_virt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use spin::Mutex;

// Window state constants
pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

// Maximum number of damage regions tracked per surface before merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

// Z-order counter for window stacking
static NEXT_Z_ORDER: AtomicU32 = AtomicU32::new(1);

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
// Surface Types (Arc-based Compositor)
// =============================================================================

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

    pub(crate) fn damage_regions(&self) -> &[DamageRect] {
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

/// Thread-safe surface with interior mutability for hot state.
pub struct Surface {
    // === Immutable after creation ===
    pub task_id: u32,
    pub pixel_format: u8,
    /// Shared memory token for this surface (stored at registration to avoid lock nesting)
    pub shm_token: u32,

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

impl Surface {
    pub fn new(
        task_id: u32,
        width: u32,
        height: u32,
        bpp: u8,
        pixel_format: u8,
        shm_token: u32,
    ) -> Result<Self, VideoError> {
        Ok(Self {
            task_id,
            pixel_format,
            shm_token,
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
// Surface Registry
// =============================================================================

/// Reference-counted surface handle
pub type SurfaceRef = Arc<Surface>;

/// Surface registry - lock held briefly only to insert/remove/lookup Arc
static SURFACES: Mutex<BTreeMap<u32, SurfaceRef>> = Mutex::new(BTreeMap::new());

/// Get a surface reference (brief lock)
fn get_surface(task_id: u32) -> Result<SurfaceRef, VideoError> {
    let registry = SURFACES.lock();
    registry.get(&task_id).cloned().ok_or(VideoError::Invalid)
}

// =============================================================================
// Public API
// =============================================================================

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
    /// Shared memory token for this surface (0 if not using shared memory)
    pub shm_token: u32,
    // Individual damage regions
    pub damage_regions: [WindowDamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [c_char; 32],
}

/// Commits the back buffer to the front buffer (Wayland-style double buffering).
pub fn surface_commit(task_id: u32) -> VideoResult {
    let surface = get_surface(task_id)?;
    surface.commit();
    Ok(())
}

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    match get_surface(task_id) {
        Ok(surface) => {
            surface.set_position(x, y);
            0
        }
        Err(_) => -1,
    }
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    match get_surface(task_id) {
        Ok(surface) => {
            surface.set_window_state(state);
            0
        }
        Err(_) => -1,
    }
}

/// Raise window (increase z-order)
pub fn surface_raise_window(task_id: u32) -> c_int {
    match get_surface(task_id) {
        Ok(surface) => {
            let new_z = NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed);
            surface.set_z_order(new_z);
            0
        }
        Err(_) => -1,
    }
}

/// Enumerate all visible windows for compositor
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let registry = SURFACES.lock();
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

        // Use the shm_token stored in the surface (avoids nested lock acquisition)
        let shm_token = surface.shm_token;

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
            info.shm_token = shm_token;
            info.damage_regions = damage_regions;
            info.title = [0; 32]; // No title in Surface - return empty
        }
        count += 1;
    }
    count
}

/// Register a surface for a task when it calls surface_attach.
/// This creates a Surface entry so the compositor can see it.
/// The actual pixel data comes from the shared memory buffer.
/// The shm_token is stored in the surface to avoid nested lock acquisition during enumeration.
pub fn register_surface_for_task(task_id: u32, width: u32, height: u32, bpp: u8, shm_token: u32) -> c_int {
    // Check if already registered
    {
        let registry = SURFACES.lock();
        if registry.contains_key(&task_id) {
            // Already registered, update is fine
            return 0;
        }
    }

    // Create a new Surface for this task with the shm_token stored
    let surface = match Surface::new(task_id, width, height, bpp, 0, shm_token) {
        Ok(s) => Arc::new(s),
        Err(_) => return -1,
    };

    // Assign initial z-order
    let z = NEXT_Z_ORDER.fetch_add(1, Ordering::Relaxed);
    surface.set_z_order(z);

    // Set initial window position (offset from top-left, below title bar)
    // Each new window gets a slightly different position for stacking
    let offset = (z as i32 % 10) * 30;
    surface.set_position(50 + offset, 50 + offset);

    // Register in the global registry
    let mut registry = SURFACES.lock();
    registry.insert(task_id, surface);

    0
}

/// Unregister a surface for a task (called on task exit or surface destruction)
pub fn unregister_surface_for_task(task_id: u32) {
    let mut registry = SURFACES.lock();
    registry.remove(&task_id);
}
