//! Event-Driven Compositor
//!
//! The compositor owns all surface state and processes events sequentially
//! in a single thread. This eliminates all per-surface and registry locks,
//! following the Wayland design philosophy.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_drivers::serial_println;
use slopos_lib::FramebufferInfo;
use slopos_mm::phys_virt::mm_phys_to_virt;

pub mod api;
pub mod events;
pub mod queue;
pub mod surface;

use events::{CompositorError, CompositorEvent, CompositorResult};
use queue::EventQueue;
use surface::{DamageRect, Surface, MAX_DAMAGE_REGIONS};

/// Maximum damage regions exposed per window
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = MAX_DAMAGE_REGIONS;

/// Exposed damage region for userland
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct WindowDamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl From<DamageRect> for WindowDamageRect {
    fn from(r: DamageRect) -> Self {
        Self {
            x0: r.x0,
            y0: r.y0,
            x1: r.x1,
            y1: r.y1,
        }
    }
}

/// Window info structure for enumeration
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

impl WindowInfo {
    fn from_surface(surface: &Surface) -> Self {
        let (width, height) = surface.dimensions();
        let front = surface.buffers.front();
        let damage_slice = front.damage_regions();

        let mut damage_regions = [WindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS];
        for (i, r) in damage_slice.iter().enumerate() {
            damage_regions[i] = WindowDamageRect::from(*r);
        }

        // Look up shared memory token
        let shm_token = slopos_mm::shared_memory::get_surface_for_task(surface.task_id).0;

        Self {
            task_id: surface.task_id,
            x: surface.x,
            y: surface.y,
            width,
            height,
            state: surface.window_state,
            damage_count: front.damage_count(),
            _padding: [0; 2],
            shm_token,
            damage_regions,
            title: [0; 32],
        }
    }
}

/// Framebuffer state (owned by compositor, no lock)
#[derive(Copy, Clone)]
pub struct FramebufferState {
    pub base: *mut u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

// SAFETY: FramebufferState contains a raw pointer to MMIO memory
unsafe impl Send for FramebufferState {}

impl FramebufferState {
    /// Copy from shared memory to framebuffer MMIO
    pub fn copy_from_shm(&self, shm_phys: u64, size: usize) -> c_int {
        let fb_size = (self.pitch * self.height) as usize;
        let copy_size = size.min(fb_size);
        if copy_size == 0 {
            return -1;
        }

        let shm_virt = mm_phys_to_virt(shm_phys);
        if shm_virt == 0 {
            return -1;
        }

        unsafe {
            ptr::copy_nonoverlapping(shm_virt as *const u8, self.base, copy_size);
        }
        0
    }
}

/// The Compositor owns all surface state and processes events sequentially.
///
/// This is the heart of the event-driven architecture. The compositor:
/// - Owns all surfaces in a BTreeMap (no registry lock)
/// - Processes events from the queue one at a time
/// - Has exclusive access to framebuffer state
pub struct Compositor {
    /// All surfaces, owned exclusively by compositor
    surfaces: BTreeMap<u32, Surface>,

    /// Z-order counter for stacking
    next_z_order: u32,

    /// Framebuffer state (owned, no lock needed)
    framebuffer: Option<FramebufferState>,

    /// Composition dirty flag
    needs_compose: bool,
}

impl Compositor {
    /// Create a new compositor
    pub const fn new() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            next_z_order: 1,
            framebuffer: None,
            needs_compose: false,
        }
    }

    /// Initialize framebuffer (called once at boot)
    pub fn init_framebuffer(&mut self, info: FramebufferInfo) -> c_int {
        let virt_addr = mm_phys_to_virt(info.address as u64);
        let base = if virt_addr != 0 {
            virt_addr as *mut u8
        } else {
            info.address as *mut u8
        };

        self.framebuffer = Some(FramebufferState {
            base,
            width: info.width as u32,
            height: info.height as u32,
            pitch: info.pitch as u32,
            bpp: info.bpp as u8,
            pixel_format: 0,
        });

        serial_println!(
            "Compositor: framebuffer init {}x{} bpp={}",
            info.width,
            info.height,
            info.bpp
        );

        0
    }

    /// Get framebuffer snapshot
    pub fn framebuffer(&self) -> Option<&FramebufferState> {
        self.framebuffer.as_ref()
    }

    /// Handle a single event
    pub fn handle_event(&mut self, event: CompositorEvent) -> CompositorResult {
        match event {
            CompositorEvent::CreateSurface {
                task_id,
                width,
                height,
                bpp,
            } => self.create_surface(task_id, width, height, bpp),

            CompositorEvent::DestroySurface { task_id } => {
                self.surfaces.remove(&task_id);
                self.needs_compose = true;
                Ok(())
            }

            CompositorEvent::Commit { task_id } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.commit();
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::SetPosition { task_id, x, y } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.set_position(x, y);
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::SetWindowState { task_id, state } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.set_window_state(state);
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::RaiseWindow { task_id } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.z_order = self.next_z_order;
                    self.next_z_order += 1;
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::SetVisible { task_id, visible } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.set_visible(visible);
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::AddDamage {
                task_id,
                x0,
                y0,
                x1,
                y1,
            } => {
                if let Some(surface) = self.surfaces.get_mut(&task_id) {
                    surface.add_front_damage(x0, y0, x1, y1);
                    self.needs_compose = true;
                    Ok(())
                } else {
                    Err(CompositorError::SurfaceNotFound)
                }
            }

            CompositorEvent::PageFlip { shm_phys, size } => {
                if let Some(fb) = &self.framebuffer {
                    let result = fb.copy_from_shm(shm_phys, size);
                    if result == 0 {
                        Ok(())
                    } else {
                        Err(CompositorError::Invalid)
                    }
                } else {
                    Err(CompositorError::NoFramebuffer)
                }
            }
        }
    }

    /// Process all pending events from the queue
    pub fn process_events(&mut self, queue: &EventQueue) {
        let events = queue.drain();
        for event in events {
            let _ = self.handle_event(event);
        }
    }

    /// Create a new surface
    fn create_surface(
        &mut self,
        task_id: u32,
        width: u32,
        height: u32,
        bpp: u8,
    ) -> CompositorResult {
        if self.surfaces.contains_key(&task_id) {
            // Already exists - not an error, just a no-op
            return Ok(());
        }

        let mut surface = Surface::new(task_id, width, height, bpp, 0)
            .map_err(|_| CompositorError::AllocationFailed)?;

        // Assign initial z-order
        let z = self.next_z_order;
        self.next_z_order += 1;
        surface.z_order = z;

        // Set initial window position (cascading)
        let offset = (z as i32 % 10) * 30;
        surface.set_position(50 + offset, 50 + offset);

        self.surfaces.insert(task_id, surface);
        self.needs_compose = true;

        Ok(())
    }

    /// Enumerate visible windows sorted by z-order
    pub fn enumerate_windows(&self) -> Vec<WindowInfo> {
        let mut windows: Vec<_> = self
            .surfaces
            .values()
            .filter(|s| s.visible)
            .map(WindowInfo::from_surface)
            .collect();

        windows.sort_by_key(|w| {
            // Find the z_order for this window
            self.surfaces
                .get(&w.task_id)
                .map(|s| s.z_order)
                .unwrap_or(0)
        });

        windows
    }

    /// Get surface info for a specific task
    pub fn get_surface(&self, task_id: u32) -> Option<&Surface> {
        self.surfaces.get(&task_id)
    }

    /// Get mutable surface for a specific task
    pub fn get_surface_mut(&mut self, task_id: u32) -> Option<&mut Surface> {
        self.surfaces.get_mut(&task_id)
    }

    /// Check if composition is needed
    pub fn needs_compose(&self) -> bool {
        self.needs_compose
    }

    /// Clear the needs_compose flag
    pub fn clear_compose_flag(&mut self) {
        self.needs_compose = false;
    }

    /// Get the number of surfaces
    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}
