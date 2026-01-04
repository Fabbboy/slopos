//! Wayland-style single-threaded compositor context.
//!
//! This module implements a Wayland-like compositor design:
//! - Single lock protects all compositor state
//! - CLIENT operations (commit, register, unregister) enqueue and return immediately
//! - COMPOSITOR operations (set_position, set_state, raise, enumerate) execute immediately
//! - Compositor drains the queue at the start of each frame
//!
//! Buffer Ownership Model (Wayland-aligned):
//! - Client owns the buffer (ShmBuffer in userland)
//! - Client draws directly to their buffer
//! - Client calls damage() to mark changed regions
//! - Client calls commit() to make changes visible
//! - Compositor reads directly from client buffer via shm_token
//! - NO kernel-side buffer copies

use alloc::collections::{BTreeMap, VecDeque};

use slopos_abi::{
    CompositorError, SurfaceRole, WindowDamageRect, WindowInfo,
    MAX_CHILDREN, MAX_INTERNAL_DAMAGE_REGIONS, MAX_WINDOW_DAMAGE_REGIONS,
    WINDOW_STATE_NORMAL,
};
use slopos_drivers::video_bridge::VideoResult;
use spin::Mutex;

// =============================================================================
// Damage Tracking Types
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

    fn is_empty(&self) -> bool {
        self.count == 0 && !self.full_damage
    }

    fn set_full_damage(&mut self) {
        self.full_damage = true;
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
        shm_token: u32,
    },
    Unregister { task_id: u32 },
    /// Request a frame callback (Wayland wl_surface.frame)
    RequestFrameCallback { task_id: u32 },
    /// Add damage region to pending damage (Wayland wl_surface.damage)
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
// Surface State (Wayland-aligned - no kernel buffers)
// =============================================================================

/// Surface state without kernel-side buffer copies.
///
/// The client owns the actual pixel buffer (via ShmBuffer/shm_token).
/// The compositor reads directly from the client's buffer.
/// This struct only tracks metadata and damage regions.
struct SurfaceState {
    /// Token referencing client's shared memory buffer
    shm_token: u32,
    /// Surface dimensions (from client's buffer)
    width: u32,
    height: u32,
    /// Damage accumulated since last commit (pending state)
    pending_damage: DamageTracker,
    /// Damage from last commit (committed state, visible to compositor)
    committed_damage: DamageTracker,
    /// True if surface has uncommitted changes
    dirty: bool,
    /// Window position on screen
    window_x: i32,
    window_y: i32,
    /// Z-order for stacking
    z_order: u32,
    /// Whether window is visible
    visible: bool,
    /// Window state (normal, minimized, maximized)
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
    /// Create a new surface state. No kernel buffer allocation - just metadata.
    fn new(width: u32, height: u32, shm_token: u32) -> Self {
        Self {
            shm_token,
            width,
            height,
            pending_damage: DamageTracker::new(),
            committed_damage: DamageTracker::new(),
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
        }
    }

    /// Commit pending state to committed state (Wayland-style atomic commit).
    ///
    /// This is now a zero-copy operation - we just swap damage trackers.
    /// The compositor reads directly from the client's buffer via shm_token.
    fn commit(&mut self) {
        // If client didn't explicitly add damage, assume full surface damage
        // This maintains backwards compatibility with simple clients that don't call damage()
        if self.pending_damage.is_empty() {
            self.pending_damage.set_full_damage();
        }

        // Transfer pending damage to committed - NO BUFFER COPY
        core::mem::swap(&mut self.committed_damage, &mut self.pending_damage);
        self.pending_damage.clear();
        self.dirty = true;
    }

    /// Add damage to pending state (called via syscall before commit)
    fn add_damage(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.pending_damage.add(DamageRect {
            x0: x,
            y0: y,
            x1: x.saturating_add(width).saturating_sub(1),
            y1: y.saturating_add(height).saturating_sub(1),
        });
    }

    /// Export committed damage to WindowInfo format
    fn export_damage(&self) -> ([DamageRect; MAX_WINDOW_DAMAGE_REGIONS], u8) {
        self.committed_damage.export_to_window_format()
    }

    /// Clear committed damage after compositor acknowledges it
    fn clear_committed_damage(&mut self) {
        self.committed_damage.clear();
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

    /// Normalize z-order values to prevent overflow.
    /// Called automatically when z-order gets too high.
    fn normalize_z_order(&mut self) {
        use alloc::vec::Vec;

        // Collect (task_id, z_order) pairs
        let mut ordered: Vec<(u32, u32)> = self.surfaces.iter()
            .map(|(&task_id, s)| (task_id, s.z_order))
            .collect();

        // Sort by z_order
        ordered.sort_by_key(|(_, z)| *z);

        // Reassign sequential z_order values starting from 1
        for (i, (task_id, _)) in ordered.iter().enumerate() {
            if let Some(surface) = self.surfaces.get_mut(task_id) {
                surface.z_order = (i + 1) as u32;
            }
        }

        // Reset next_z_order
        self.next_z_order = (ordered.len() + 1) as u32;
    }

    /// Check if z-order normalization is needed (approaching u32 overflow)
    fn needs_z_order_normalization(&self) -> bool {
        self.next_z_order > 0xFFFF_0000
    }
}

static CONTEXT: Mutex<CompositorContext> = Mutex::new(CompositorContext::new());

// =============================================================================
// PUBLIC API - Client Operations (ENQUEUE and return immediately)
// =============================================================================

/// Commits pending state to committed state (Wayland-style atomic commit).
/// Called by CLIENT tasks. Enqueues the commit for processing by compositor.
///
/// Note: This is now zero-copy. The compositor reads directly from the client's
/// shared memory buffer. Only damage tracking is transferred on commit.
pub fn surface_commit(task_id: u32) -> VideoResult {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::Commit { task_id });
    Ok(())
}

/// Register a surface for a task when it calls surface_attach.
/// Called by CLIENT tasks. Enqueues the registration for processing by compositor.
pub fn register_surface_for_task(task_id: u32, width: u32, height: u32, shm_token: u32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::Register {
        task_id,
        width,
        height,
        shm_token,
    });
    Ok(())
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
            ClientOp::Register { task_id, width, height, shm_token } => {
                // Skip if already registered
                if ctx.surfaces.contains_key(&task_id) {
                    continue;
                }

                // Create new surface - now infallible (no buffer allocation)
                let mut surface = SurfaceState::new(width, height, shm_token);

                // Assign z-order and position
                let z = ctx.next_z_order;
                ctx.next_z_order += 1;
                surface.z_order = z;

                let offset = (z as i32 % 10) * 30;
                surface.window_x = 50 + offset;
                surface.window_y = 50 + offset;

                ctx.surfaces.insert(task_id, surface);
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
pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.window_x = x;
        surface.window_y = y;
        surface.dirty = true;
        Ok(())
    } else {
        Err(CompositorError::SurfaceNotFound)
    }
}

/// Set window state. IMMEDIATE - called by COMPOSITOR only.
pub fn surface_set_window_state(task_id: u32, state: u8) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.window_state = state;
        surface.dirty = true;
        Ok(())
    } else {
        Err(CompositorError::SurfaceNotFound)
    }
}

/// Raise window (increase z-order). IMMEDIATE - called by COMPOSITOR only.
pub fn surface_raise_window(task_id: u32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    if !ctx.surfaces.contains_key(&task_id) {
        return Err(CompositorError::SurfaceNotFound);
    }

    // Normalize z-order if approaching overflow
    if ctx.needs_z_order_normalization() {
        ctx.normalize_z_order();
    }

    let new_z = ctx.next_z_order;
    ctx.next_z_order += 1;
    if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
        surface.z_order = new_z;
    }
    Ok(())
}

/// Enumerate all visible windows. IMMEDIATE - called by COMPOSITOR only.
/// This function exports damage and then clears it (Wayland-style acknowledge).
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let mut ctx = CONTEXT.lock();
    let mut count = 0u32;

    for (&task_id, surface) in ctx.surfaces.iter_mut() {
        if count >= max_count {
            break;
        }

        // Skip invisible windows
        if !surface.visible {
            continue;
        }

        // Export damage from committed state
        let (damage_rects, dmg_count) = surface.export_damage();
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
            info.width = surface.width;
            info.height = surface.height;
            info.state = surface.window_state;
            info.damage_count = dmg_count;
            info._padding = [0; 2];
            info.shm_token = surface.shm_token;
            info.damage_regions = regions;
            info.title = [0; 32];
        }

        // Clear damage after export (Wayland-style: compositor acknowledges damage)
        surface.clear_committed_damage();

        count += 1;
    }
    count
}

// =============================================================================
// Frame Callback Protocol (Wayland wl_surface.frame)
// =============================================================================

/// Request a frame callback. Called by CLIENT tasks.
/// Enqueues the request for processing by compositor.
pub fn surface_request_frame_callback(task_id: u32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::RequestFrameCallback { task_id });
    Ok(())
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

/// Add damage region to surface's pending state. Called by CLIENT tasks.
/// Enqueues the damage for processing by compositor on next drain_queue().
/// The damage rect specifies what region has changed and needs redrawing.
pub fn surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::AddDamage { task_id, x, y, width, height });
    Ok(())
}

/// Get the buffer age for a surface. Called by CLIENT tasks.
///
/// NOTE: With the Wayland-aligned buffer ownership model, the kernel does not
/// track buffer content. This always returns 0 (undefined content).
///
/// For client-side double-buffering with proper buffer age, clients would need
/// to manage multiple buffers themselves. This is a potential future enhancement.
pub fn surface_get_buffer_age(_task_id: u32) -> u8 {
    // Buffer age is not tracked by kernel - client manages buffer content
    // Return 0 = undefined content (client must redraw everything)
    0
}

// =============================================================================
// Surface Role Protocol (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Set the role of a surface. Called by CLIENT tasks.
/// Role can only be set once per surface (Wayland semantics).
/// Returns Ok(()) on success, Err if invalid role.
pub fn surface_set_role(task_id: u32, role: u8) -> Result<(), CompositorError> {
    let role = match SurfaceRole::from_u8(role) {
        Some(r) => r,
        None => return Err(CompositorError::InvalidRole),
    };

    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetRole { task_id, role });
    Ok(())
}

/// Set the parent surface for a subsurface. Called by CLIENT tasks.
/// Only valid for surfaces with role Subsurface.
pub fn surface_set_parent(task_id: u32, parent_task_id: u32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetParent { task_id, parent_task_id });
    Ok(())
}

/// Set the relative position of a subsurface. Called by CLIENT tasks.
/// The position is relative to the parent surface's top-left corner.
/// Only valid for surfaces with role Subsurface.
pub fn surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetRelativePosition { task_id, rel_x, rel_y });
    Ok(())
}
