//! Wayland-style single-threaded compositor context.
//!
//! This module implements a Wayland-like compositor design:
//! - Single lock protects all compositor state
//! - CLIENT operations (commit, register, unregister) enqueue and return immediately
//! - COMPOSITOR operations (set_position, set_state, raise, enumerate) execute immediately
//! - Compositor drains the queue at the start of each frame

use core::ffi::{c_char, c_int};

use alloc::collections::{BTreeMap, VecDeque};

use slopos_drivers::video_bridge::{VideoError, VideoResult};
use slopos_mm::mm_constants::PAGE_SIZE_4KB;
use slopos_mm::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use slopos_mm::phys_virt::mm_phys_to_virt;
use spin::Mutex;

// =============================================================================
// Constants
// =============================================================================

pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

pub const MAX_DAMAGE_REGIONS: usize = 8;
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = MAX_DAMAGE_REGIONS;

// =============================================================================
// Buffer Types
// =============================================================================

#[derive(Copy, Clone, Default)]
struct DamageRect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl DamageRect {
    const fn invalid() -> Self {
        Self { x0: 0, y0: 0, x1: -1, y1: -1 }
    }
}

struct PageBuffer {
    virt_ptr: *mut u8,
    phys_addr: u64,
    size: usize,
    pages: u32,
}

impl PageBuffer {
    fn new(size: usize) -> Result<Self, VideoError> {
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
    fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.virt_ptr, self.size) }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt_ptr, self.size) }
    }
}

impl Drop for PageBuffer {
    fn drop(&mut self) {
        if self.phys_addr != 0 {
            for i in 0..self.pages {
                let page_phys = self.phys_addr + (i as u64) * PAGE_SIZE_4KB;
                let _ = free_page_frame(page_phys);
            }
        }
    }
}

unsafe impl Send for PageBuffer {}

struct OwnedBuffer {
    data: PageBuffer,
    width: u32,
    height: u32,
    damage_regions: [DamageRect; MAX_DAMAGE_REGIONS],
    damage_count: u8,
}

impl OwnedBuffer {
    fn new(width: u32, height: u32, bpp: u8) -> Result<Self, VideoError> {
        let bytes_pp = ((bpp as usize) + 7) / 8;
        let pitch = (width as usize) * bytes_pp;
        let size = pitch * (height as usize);

        let data = PageBuffer::new(size)?;

        Ok(Self {
            data,
            width,
            height,
            damage_regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            damage_count: 0,
        })
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    #[inline]
    fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    fn height(&self) -> u32 {
        self.height
    }

    fn clear_damage(&mut self) {
        self.damage_count = 0;
    }

    fn damage_count(&self) -> u8 {
        self.damage_count
    }

    fn damage_regions(&self) -> &[DamageRect] {
        &self.damage_regions[..self.damage_count as usize]
    }
}

// =============================================================================
// Public Types (exported for video_bridge)
// =============================================================================

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct WindowDamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

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
    pub shm_token: u32,
    pub damage_regions: [WindowDamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [c_char; 32],
}

// =============================================================================
// Client Operation Queue
// =============================================================================

/// Operations queued by CLIENT tasks (shell, apps).
/// These are processed when the compositor calls drain_queue().
enum ClientOp {
    Commit { task_id: u32 },
    Register {
        task_id: u32,
        width: u32,
        height: u32,
        bpp: u8,
        shm_token: u32,
    },
    Unregister { task_id: u32 },
}

// =============================================================================
// Surface State (no inner locks - compositor context serializes access)
// =============================================================================

struct SurfaceState {
    shm_token: u32,
    front_buffer: OwnedBuffer,
    back_buffer: OwnedBuffer,
    dirty: bool,
    window_x: i32,
    window_y: i32,
    z_order: u32,
    visible: bool,
    window_state: u8,
}

impl SurfaceState {
    fn new(width: u32, height: u32, bpp: u8, shm_token: u32) -> Result<Self, VideoError> {
        Ok(Self {
            shm_token,
            front_buffer: OwnedBuffer::new(width, height, bpp)?,
            back_buffer: OwnedBuffer::new(width, height, bpp)?,
            dirty: true,
            window_x: 0,
            window_y: 0,
            z_order: 0,
            visible: true,
            window_state: WINDOW_STATE_NORMAL,
        })
    }

    fn commit(&mut self) {
        let src = self.back_buffer.as_slice();
        let dst = self.front_buffer.as_mut_slice();
        dst.copy_from_slice(src);

        self.front_buffer.damage_regions = self.back_buffer.damage_regions;
        self.front_buffer.damage_count = self.back_buffer.damage_count;
        self.back_buffer.clear_damage();
        self.dirty = true;
    }
}

// =============================================================================
// Compositor Context (single lock for everything)
// =============================================================================

struct CompositorContext {
    surfaces: BTreeMap<u32, SurfaceState>,
    queue: VecDeque<ClientOp>,
    next_z_order: u32,
}

impl CompositorContext {
    const fn new() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            queue: VecDeque::new(),
            next_z_order: 1,
        }
    }
}

static CONTEXT: Mutex<CompositorContext> = Mutex::new(CompositorContext::new());

// =============================================================================
// PUBLIC API - Client Operations (ENQUEUE and return immediately)
// =============================================================================

/// Commits the back buffer to the front buffer (Wayland-style double buffering).
/// Called by CLIENT tasks. Enqueues the commit for processing by compositor.
pub fn surface_commit(task_id: u32) -> VideoResult {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::Commit { task_id });
    Ok(())
}

/// Register a surface for a task when it calls surface_attach.
/// Called by CLIENT tasks. Enqueues the registration for processing by compositor.
pub fn register_surface_for_task(task_id: u32, width: u32, height: u32, bpp: u8, shm_token: u32) -> c_int {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::Register {
        task_id,
        width,
        height,
        bpp,
        shm_token,
    });
    0
}

/// Unregister a surface for a task (called on task exit or surface destruction).
/// Called by kernel during task cleanup. Enqueues the unregistration.
pub fn unregister_surface_for_task(task_id: u32) {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::Unregister { task_id });
}

// =============================================================================
// PUBLIC API - Compositor Operations (IMMEDIATE execution)
// =============================================================================

/// Drain and process all pending client operations.
/// Called by COMPOSITOR at the start of each frame.
pub fn drain_queue() {
    let mut ctx = CONTEXT.lock();

    while let Some(op) = ctx.queue.pop_front() {
        match op {
            ClientOp::Commit { task_id } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    surface.commit();
                }
            }
            ClientOp::Register { task_id, width, height, bpp, shm_token } => {
                // Skip if already registered
                if ctx.surfaces.contains_key(&task_id) {
                    continue;
                }

                // Create new surface
                if let Ok(mut surface) = SurfaceState::new(width, height, bpp, shm_token) {
                    // Assign z-order and position
                    let z = ctx.next_z_order;
                    ctx.next_z_order += 1;
                    surface.z_order = z;

                    let offset = (z as i32 % 10) * 30;
                    surface.window_x = 50 + offset;
                    surface.window_y = 50 + offset;

                    ctx.surfaces.insert(task_id, surface);
                }
            }
            ClientOp::Unregister { task_id } => {
                ctx.surfaces.remove(&task_id);
            }
        }
    }
}

/// Set window position. IMMEDIATE - called by COMPOSITOR only.
pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    let mut ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.window_x = x;
        surface.window_y = y;
        surface.dirty = true;
        0
    } else {
        -1
    }
}

/// Set window state. IMMEDIATE - called by COMPOSITOR only.
pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    let mut ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.window_state = state;
        surface.dirty = true;
        0
    } else {
        -1
    }
}

/// Raise window (increase z-order). IMMEDIATE - called by COMPOSITOR only.
pub fn surface_raise_window(task_id: u32) -> c_int {
    let mut ctx = CONTEXT.lock();
    if !ctx.surfaces.contains_key(&task_id) {
        return -1;
    }
    let new_z = ctx.next_z_order;
    ctx.next_z_order += 1;
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.z_order = new_z;
    }
    0
}

/// Enumerate all visible windows. IMMEDIATE - called by COMPOSITOR only.
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let ctx = CONTEXT.lock();
    let mut count = 0u32;

    for (&task_id, surface) in ctx.surfaces.iter() {
        if count >= max_count {
            break;
        }

        // Skip invisible windows
        if !surface.visible {
            continue;
        }

        // Get damage from front buffer
        let damage_slice = surface.front_buffer.damage_regions();
        let dmg_count = surface.front_buffer.damage_count();
        let mut regions = [WindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS];
        for (i, r) in damage_slice.iter().enumerate() {
            regions[i] = WindowDamageRect {
                x0: r.x0,
                y0: r.y0,
                x1: r.x1,
                y1: r.y1,
            };
        }

        unsafe {
            let info = &mut *out_buffer.add(count as usize);
            info.task_id = task_id;
            info.x = surface.window_x;
            info.y = surface.window_y;
            info.width = surface.front_buffer.width();
            info.height = surface.front_buffer.height();
            info.state = surface.window_state;
            info.damage_count = dmg_count;
            info._padding = [0; 2];
            info.shm_token = surface.shm_token;
            info.damage_regions = regions;
            info.title = [0; 32];
        }
        count += 1;
    }
    count
}
