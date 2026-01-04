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

/// Maximum number of child subsurfaces per surface
pub const MAX_CHILDREN: usize = 8;

// =============================================================================
// Surface Roles (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Role of a surface in the compositor hierarchy.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceRole {
    /// No role assigned yet (surface exists but has no role)
    #[default]
    None = 0,
    /// Top-level window (regular application window)
    Toplevel = 1,
    /// Popup surface (menus, tooltips, dropdowns)
    Popup = 2,
    /// Subsurface (child surface positioned relative to parent)
    Subsurface = 3,
}

impl SurfaceRole {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::None),
            1 => Some(Self::Toplevel),
            2 => Some(Self::Popup),
            3 => Some(Self::Subsurface),
            _ => None,
        }
    }
}

/// Maximum damage regions exported in WindowInfo (ABI-stable)
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = 8;

/// Maximum damage regions tracked internally (higher resolution)
pub const MAX_INTERNAL_DAMAGE_REGIONS: usize = 32;

/// Maximum buffer age before it's considered invalid (for damage accumulation)
pub const MAX_BUFFER_AGE: u8 = 8;

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

    fn is_valid(&self) -> bool {
        self.x1 >= self.x0 && self.y1 >= self.y0
    }

    fn intersects(&self, other: &Self) -> bool {
        self.x0 <= other.x1 && self.x1 >= other.x0 &&
        self.y0 <= other.y1 && self.y1 >= other.y0
    }

    fn merge(&self, other: &Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// Advanced damage tracker with 32 regions and automatic merging.
struct DamageTracker {
    regions: [DamageRect; MAX_INTERNAL_DAMAGE_REGIONS],
    count: u8,
    /// Set when damage exceeds capacity - means entire surface is dirty
    full_damage: bool,
}

impl DamageTracker {
    const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); MAX_INTERNAL_DAMAGE_REGIONS],
            count: 0,
            full_damage: false,
        }
    }

    fn clear(&mut self) {
        self.count = 0;
        self.full_damage = false;
    }

    fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        // If already full damage, nothing to do
        if self.full_damage {
            return;
        }

        // Try to merge with existing regions
        for i in 0..(self.count as usize) {
            if self.regions[i].intersects(&rect) {
                self.regions[i] = self.regions[i].merge(&rect);
                self.merge_overlapping();
                return;
            }
        }

        // Add as new region if space available
        if (self.count as usize) < MAX_INTERNAL_DAMAGE_REGIONS {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        } else {
            // No space - mark full damage
            self.full_damage = true;
        }
    }

    /// Merge overlapping regions to reduce count
    fn merge_overlapping(&mut self) {
        if self.count <= 1 {
            return;
        }

        let mut i = 0;
        while i < self.count as usize {
            let mut j = i + 1;
            while j < self.count as usize {
                if self.regions[i].intersects(&self.regions[j]) {
                    self.regions[i] = self.regions[i].merge(&self.regions[j]);
                    // Remove region j by swapping with last
                    self.count -= 1;
                    self.regions[j] = self.regions[self.count as usize];
                    // Don't increment j - check the swapped region
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    /// Export to the smaller WindowInfo format (max 8 regions)
    fn export_to_window_format(&self) -> ([DamageRect; MAX_WINDOW_DAMAGE_REGIONS], u8) {
        let mut out = [DamageRect::invalid(); MAX_WINDOW_DAMAGE_REGIONS];

        if self.full_damage {
            // Return empty list with u8::MAX count to indicate full damage
            return (out, u8::MAX);
        }

        let export_count = (self.count as usize).min(MAX_WINDOW_DAMAGE_REGIONS);
        for i in 0..export_count {
            out[i] = self.regions[i];
        }

        // If we had to truncate, indicate full damage
        if (self.count as usize) > MAX_WINDOW_DAMAGE_REGIONS {
            return (out, u8::MAX);
        }

        (out, export_count as u8)
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

// SAFETY: PageBuffer contains virt_ptr pointing to kernel-allocated pages.
// Thread-safety is guaranteed because:
// 1. Memory is allocated exclusively via alloc_page_frames() from kernel heap
// 2. Only accessed through CONTEXT Mutex (single global lock serializes all access)
// 3. Freed in Drop before any dangling access is possible
// 4. No external aliasing - pointer is internal to PageBuffer only
// 5. The kernel runs on a single CPU (no SMP) so no concurrent access possible
unsafe impl Send for PageBuffer {}

struct OwnedBuffer {
    data: PageBuffer,
    width: u32,
    height: u32,
    damage: DamageTracker,
    /// Buffer age: 0 = current, 1 = previous frame, 2 = two frames ago, etc.
    /// Used for damage accumulation when using buffer swapping.
    age: u8,
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
            damage: DamageTracker::new(),
            age: 0,
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

    fn add_damage(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.damage.add(DamageRect {
            x0: x,
            y0: y,
            x1: x.saturating_add(width).saturating_sub(1),
            y1: y.saturating_add(height).saturating_sub(1),
        });
    }

    fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// Export damage to WindowInfo format
    fn export_damage(&self) -> ([DamageRect; MAX_WINDOW_DAMAGE_REGIONS], u8) {
        self.damage.export_to_window_format()
    }

    fn increment_age(&mut self) {
        if self.age < MAX_BUFFER_AGE {
            self.age += 1;
        }
    }

    fn reset_age(&mut self) {
        self.age = 0;
    }

    fn buffer_age(&self) -> u8 {
        self.age
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
    /// Request a frame callback (Wayland wl_surface.frame)
    RequestFrameCallback { task_id: u32 },
    /// Add damage region to back buffer (Wayland wl_surface.damage)
    AddDamage {
        task_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Set surface role (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
    SetRole { task_id: u32, role: SurfaceRole },
    /// Set parent surface for subsurfaces
    SetParent { task_id: u32, parent_task_id: u32 },
    /// Set relative position for subsurfaces
    SetRelativePosition { task_id: u32, rel_x: i32, rel_y: i32 },
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
    /// True if client has requested a frame callback (Wayland wl_surface.frame)
    frame_callback_pending: bool,
    /// Timestamp (ms) when the frame was presented, 0 if not yet presented
    last_present_time_ms: u64,
    /// Role of this surface (toplevel, popup, subsurface)
    role: SurfaceRole,
    /// Parent task ID for subsurfaces (None for toplevel/popup)
    parent_task: Option<u32>,
    /// Child subsurface task IDs
    children: [Option<u32>; MAX_CHILDREN],
    /// Number of active children
    child_count: u8,
    /// Position relative to parent (for subsurfaces)
    relative_x: i32,
    relative_y: i32,
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
            frame_callback_pending: false,
            last_present_time_ms: 0,
            role: SurfaceRole::None,
            parent_task: None,
            children: [None; MAX_CHILDREN],
            child_count: 0,
            relative_x: 0,
            relative_y: 0,
        })
    }

    fn commit(&mut self) {
        // Swap buffers by copying data
        let src = self.back_buffer.as_slice();
        let dst = self.front_buffer.as_mut_slice();
        dst.copy_from_slice(src);

        // Transfer damage from back to front buffer
        // We need to copy the damage tracker state
        core::mem::swap(&mut self.front_buffer.damage, &mut self.back_buffer.damage);
        self.back_buffer.clear_damage();

        // Update buffer ages
        self.front_buffer.reset_age();
        self.back_buffer.increment_age();

        self.dirty = true;
    }

    /// Add damage to the back buffer (called via syscall)
    fn add_damage(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.back_buffer.add_damage(x, y, width, height);
    }

    /// Get back buffer age (for damage accumulation)
    fn back_buffer_age(&self) -> u8 {
        self.back_buffer.buffer_age()
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
            ClientOp::RequestFrameCallback { task_id } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    surface.frame_callback_pending = true;
                }
            }
            ClientOp::AddDamage { task_id, x, y, width, height } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    surface.add_damage(x, y, width, height);
                }
            }
            ClientOp::SetRole { task_id, role } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    // Can only set role once (Wayland semantics)
                    if surface.role == SurfaceRole::None {
                        surface.role = role;
                    }
                }
            }
            ClientOp::SetParent { task_id, parent_task_id } => {
                // First verify parent exists and has capacity
                let can_add = if let Some(parent) = ctx.surfaces.get(&parent_task_id) {
                    (parent.child_count as usize) < MAX_CHILDREN
                } else {
                    false
                };

                if can_add {
                    // Set parent on child surface
                    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                        // Only subsurfaces can have parents
                        if surface.role == SurfaceRole::Subsurface {
                            surface.parent_task = Some(parent_task_id);
                        }
                    }

                    // Add child to parent's children list
                    if let Some(parent) = ctx.surfaces.get_mut(&parent_task_id) {
                        for slot in parent.children.iter_mut() {
                            if slot.is_none() {
                                *slot = Some(task_id);
                                parent.child_count += 1;
                                break;
                            }
                        }
                    }
                }
            }
            ClientOp::SetRelativePosition { task_id, rel_x, rel_y } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    // Only subsurfaces use relative positioning
                    if surface.role == SurfaceRole::Subsurface {
                        surface.relative_x = rel_x;
                        surface.relative_y = rel_y;
                        surface.dirty = true;
                    }
                }
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

        // Export damage from front buffer using the new DamageTracker
        let (damage_rects, dmg_count) = surface.front_buffer.export_damage();
        let mut regions = [WindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS];
        for i in 0..MAX_WINDOW_DAMAGE_REGIONS {
            regions[i] = WindowDamageRect {
                x0: damage_rects[i].x0,
                y0: damage_rects[i].y0,
                x1: damage_rects[i].x1,
                y1: damage_rects[i].y1,
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

// =============================================================================
// Frame Callback Protocol (Wayland wl_surface.frame)
// =============================================================================

/// Request a frame callback. Called by CLIENT tasks.
/// Enqueues the request for processing by compositor.
pub fn surface_request_frame_callback(task_id: u32) -> c_int {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::RequestFrameCallback { task_id });
    0
}

/// Mark frame as done for all surfaces with pending callbacks.
/// Called by COMPOSITOR after presenting a frame.
/// Sets last_present_time_ms for surfaces that had frame_callback_pending.
pub fn surface_mark_frames_done(present_time_ms: u64) {
    let mut ctx = CONTEXT.lock();

    for surface in ctx.surfaces.values_mut() {
        if surface.frame_callback_pending {
            surface.last_present_time_ms = present_time_ms;
            surface.frame_callback_pending = false;
        }
    }
}

/// Poll for frame completion. Called by CLIENT tasks.
/// Returns the presentation timestamp if frame was done, 0 if still pending.
/// Clears last_present_time_ms after returning it (one-shot).
pub fn surface_poll_frame_done(task_id: u32) -> u64 {
    let mut ctx = CONTEXT.lock();

    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        let timestamp = surface.last_present_time_ms;
        if timestamp > 0 {
            surface.last_present_time_ms = 0; // Clear after reading
        }
        timestamp
    } else {
        0
    }
}

// =============================================================================
// Damage Tracking Protocol (Wayland wl_surface.damage)
// =============================================================================

/// Add damage region to surface's back buffer. Called by CLIENT tasks.
/// Enqueues the damage for processing by compositor on next drain_queue().
/// The damage rect specifies what region has changed and needs redrawing.
pub fn surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::AddDamage { task_id, x, y, width, height });
    0
}

/// Get the back buffer age for a surface. Called by CLIENT tasks.
/// Returns 0 if the buffer content is undefined (must redraw everything).
/// Returns 1 if the buffer contains the previous frame's content.
/// Returns N if the buffer contains content from N frames ago.
/// Returns u8::MAX if buffer is too old for damage accumulation.
pub fn surface_get_buffer_age(task_id: u32) -> u8 {
    let ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get(&task_id) {
        surface.back_buffer_age()
    } else {
        0 // Unknown surface - return 0 (undefined content)
    }
}

// =============================================================================
// Surface Role Protocol (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Set the role of a surface. Called by CLIENT tasks.
/// Role can only be set once per surface (Wayland semantics).
/// Returns 0 on success, -1 if role already set or invalid.
pub fn surface_set_role(task_id: u32, role: u8) -> c_int {
    let role = match SurfaceRole::from_u8(role) {
        Some(r) => r,
        None => return -1,
    };

    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetRole { task_id, role });
    0
}

/// Set the parent surface for a subsurface. Called by CLIENT tasks.
/// Only valid for surfaces with role Subsurface.
/// Returns 0 on success (operation queued), -1 on immediate failure.
pub fn surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetParent { task_id, parent_task_id });
    0
}

/// Set the relative position of a subsurface. Called by CLIENT tasks.
/// The position is relative to the parent surface's top-left corner.
/// Only valid for surfaces with role Subsurface.
pub fn surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetRelativePosition { task_id, rel_x, rel_y });
    0
}

/// Get the role of a surface. Returns the role as u8, or 0 (None) if not found.
pub fn surface_get_role(task_id: u32) -> u8 {
    let ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get(&task_id) {
        surface.role as u8
    } else {
        SurfaceRole::None as u8
    }
}

/// Get the parent task ID for a subsurface. Returns 0 if no parent.
pub fn surface_get_parent(task_id: u32) -> u32 {
    let ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get(&task_id) {
        surface.parent_task.unwrap_or(0)
    } else {
        0
    }
}
