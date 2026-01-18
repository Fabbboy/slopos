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

use slopos_abi::damage::{DamageRect, InternalDamageTracker};
use slopos_abi::video_traits::VideoResult;
use slopos_abi::{
    CompositorError, MAX_CHILDREN, MAX_WINDOW_DAMAGE_REGIONS, SurfaceRole, WINDOW_STATE_NORMAL,
    WindowDamageRect, WindowInfo,
};
use slopos_lib::IrqMutex;

type DamageTracker = InternalDamageTracker;

fn export_damage_to_window_format(
    tracker: &DamageTracker,
) -> ([DamageRect; MAX_WINDOW_DAMAGE_REGIONS], u8) {
    tracker.export_to_array::<MAX_WINDOW_DAMAGE_REGIONS>()
}

// =============================================================================
// Client Operation Queue
// =============================================================================

/// Operations queued by CLIENT tasks (shell, apps).
/// These are processed when the compositor calls drain_queue().
enum ClientOp {
    Commit {
        task_id: u32,
    },
    Register {
        task_id: u32,
        width: u32,
        height: u32,
        shm_token: u32,
    },
    Unregister {
        task_id: u32,
    },
    /// Request a frame callback (Wayland wl_surface.frame)
    RequestFrameCallback {
        task_id: u32,
    },
    /// Add damage region to pending damage (Wayland wl_surface.damage)
    AddDamage {
        task_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Set surface role (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
    SetRole {
        task_id: u32,
        role: SurfaceRole,
    },
    /// Set parent surface for subsurfaces
    SetParent {
        task_id: u32,
        parent_task_id: u32,
    },
    /// Set relative position for subsurfaces
    SetRelativePosition {
        task_id: u32,
        rel_x: i32,
        rel_y: i32,
    },
    /// Set window title (UTF-8, max 31 chars + null terminator)
    SetTitle {
        task_id: u32,
        title: [u8; 32],
    },
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
    /// Window title (UTF-8, null-terminated)
    title: [u8; 32],
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
            title: [0; 32],
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

    fn add_damage(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.pending_damage.add_merge_overlapping(DamageRect {
            x0: x,
            y0: y,
            x1: x.saturating_add(width).saturating_sub(1),
            y1: y.saturating_add(height).saturating_sub(1),
        });
    }

    fn export_damage(&self) -> ([DamageRect; MAX_WINDOW_DAMAGE_REGIONS], u8) {
        export_damage_to_window_format(&self.committed_damage)
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
        let mut ordered: Vec<(u32, u32)> = self
            .surfaces
            .iter()
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

static CONTEXT: IrqMutex<CompositorContext> = IrqMutex::new(CompositorContext::new());

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
pub fn register_surface_for_task(
    task_id: u32,
    width: u32,
    height: u32,
    shm_token: u32,
) -> Result<(), CompositorError> {
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

/// Maximum operations to process per drain call.
/// This prevents holding the lock for too long, allowing IRQ handlers to run.
/// Any remaining operations are processed on the next frame.
const MAX_OPS_PER_DRAIN: usize = 64;

/// Drain and process pending client operations with bounded iteration.
/// Called by COMPOSITOR at the start of each frame.
/// Processes up to MAX_OPS_PER_DRAIN operations per call to avoid holding
/// the spinlock for too long. Remaining operations are processed next frame.
pub fn drain_queue() {
    let mut ctx = CONTEXT.lock();
    let mut processed = 0;

    while processed < MAX_OPS_PER_DRAIN {
        let op = match ctx.queue.pop_front() {
            Some(op) => op,
            None => break,
        };

        match op {
            ClientOp::Commit { task_id } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    surface.commit();
                }
            }
            ClientOp::Register {
                task_id,
                width,
                height,
                shm_token,
            } => {
                // Skip if already registered
                if ctx.surfaces.contains_key(&task_id) {
                    processed += 1;
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
            ClientOp::AddDamage {
                task_id,
                x,
                y,
                width,
                height,
            } => {
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
            ClientOp::SetParent {
                task_id,
                parent_task_id,
            } => {
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
            ClientOp::SetRelativePosition {
                task_id,
                rel_x,
                rel_y,
            } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    // Only subsurfaces use relative positioning
                    if surface.role == SurfaceRole::Subsurface {
                        surface.relative_x = rel_x;
                        surface.relative_y = rel_y;
                        surface.dirty = true;
                    }
                }
            }
            ClientOp::SetTitle { task_id, title } => {
                if let Some(surface) = ctx.surfaces.get_mut(&task_id) {
                    surface.title = title;
                    surface.dirty = true;
                }
            }
        }
        processed += 1;
    }
    // Any remaining ops are processed next frame
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
///
/// Note: Damage is NOT cleared here. It persists until the next commit replaces it.
/// This ensures damage isn't lost if the compositor fails to render.
/// Static windows may report stale damage, but that's preferable to losing damage.
///
/// For subsurfaces, the absolute position is calculated as parent position + relative offset.
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let ctx = CONTEXT.lock();
    let mut count = 0u32;

    // First pass: collect task IDs and their info (need to look up parents)
    for (&task_id, surface) in ctx.surfaces.iter() {
        if count >= max_count {
            break;
        }

        // Skip invisible windows
        if !surface.visible {
            continue;
        }

        // Calculate absolute position
        // For subsurfaces: parent position + relative offset
        // For toplevel/popup: use window_x/window_y directly
        let (abs_x, abs_y) = if surface.role == SurfaceRole::Subsurface {
            if let Some(parent_id) = surface.parent_task {
                if let Some(parent) = ctx.surfaces.get(&parent_id) {
                    (
                        parent.window_x + surface.relative_x,
                        parent.window_y + surface.relative_y,
                    )
                } else {
                    // Parent not found, fall back to relative as absolute
                    (surface.relative_x, surface.relative_y)
                }
            } else {
                // No parent set, use relative as absolute
                (surface.relative_x, surface.relative_y)
            }
        } else {
            (surface.window_x, surface.window_y)
        };

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
            info.x = abs_x;
            info.y = abs_y;
            info.width = surface.width;
            info.height = surface.height;
            info.state = surface.window_state;
            info.damage_count = dmg_count;
            info._padding = [0; 2];
            info.shm_token = surface.shm_token;
            info.damage_regions = regions;
            info.title = surface.title;
        }

        // Note: We no longer clear damage here. The next commit will replace it.
        // This ensures damage isn't lost if the compositor fails to render.
        //
        // Trade-off: Static windows (no new commits) will report stale damage forever.
        // This is acceptable since the compositor currently does full redraws anyway.
        // When partial updates are implemented, add an explicit acknowledge mechanism.

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
    ctx.queue
        .push_back(ClientOp::RequestFrameCallback { task_id });
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
pub fn surface_add_damage(
    task_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::AddDamage {
        task_id,
        x,
        y,
        width,
        height,
    });
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
    ctx.queue.push_back(ClientOp::SetParent {
        task_id,
        parent_task_id,
    });
    Ok(())
}

/// Set the relative position of a subsurface. Called by CLIENT tasks.
/// The position is relative to the parent surface's top-left corner.
/// Only valid for surfaces with role Subsurface.
pub fn surface_set_relative_position(
    task_id: u32,
    rel_x: i32,
    rel_y: i32,
) -> Result<(), CompositorError> {
    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetRelativePosition {
        task_id,
        rel_x,
        rel_y,
    });
    Ok(())
}

/// Set the window title. Called by CLIENT tasks.
/// Title is UTF-8, max 31 characters (null-terminated in 32-byte buffer).
pub fn surface_set_title(task_id: u32, title: &[u8]) -> Result<(), CompositorError> {
    let mut title_buf = [0u8; 32];
    let copy_len = title.len().min(31); // Leave room for null terminator
    title_buf[..copy_len].copy_from_slice(&title[..copy_len]);
    // Ensure null termination
    title_buf[copy_len] = 0;

    let mut ctx = CONTEXT.lock();
    ctx.queue.push_back(ClientOp::SetTitle {
        task_id,
        title: title_buf,
    });
    Ok(())
}
