//! Software rendering surface backed by shared memory.
//!
//! `SoftSurface` owns a [`ShmBuffer`] and presents frames to the compositor
//! via the display protocol.  It implements [`RenderSurface`] so it can be
//! used as a drop-in CPU rendering backend for any compositor-managed window.
//!
//! This type is the rendering half of the windowing/rendering split.
//! [`Surface`](crate::surface::Surface) handles the windowing lifecycle
//! (create/destroy compositor objects, set title, handle input), while
//! `SoftSurface` handles the rendering lifecycle (allocate pixel buffers,
//! present frames, resize buffers).
//!
//! A future GPU rendering backend would implement [`RenderSurface`] with
//! GPU-allocated buffers instead of shared memory, targeting the same
//! compositor surface via its `surface_id`.

use slopos_abi::pixel::PixelFormat;
use slopos_gfx::{DrawBuffer, RenderError, RenderSurface};

use crate::connection::ProtocolHandle;
use crate::shm::ShmBuffer;
use crate::surface::SurfaceError;

/// A CPU/SHM rendering backend for compositor-managed surfaces.
///
/// Created after a [`Surface`](crate::surface::Surface) establishes the
/// windowing objects.  Takes the `surface_id` from the windowing surface
/// and manages its own shared-memory pixel buffer independently.
pub struct SoftSurface {
    handle: ProtocolHandle,
    shm: ShmBuffer,
    surface_id: u32,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

impl SoftSurface {
    /// Create a new software rendering surface and attach it to the compositor.
    ///
    /// Allocates a shared-memory buffer and registers it with the compositor
    /// via `surface_attach`.  The `surface_id` must come from an already-created
    /// [`Surface`](crate::surface::Surface).
    pub fn new(
        handle: ProtocolHandle,
        surface_id: u32,
        pixel_format: PixelFormat,
        width: u32,
        height: u32,
    ) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::BadSize);
        }

        let bytes_pp = pixel_format.bytes_per_pixel();
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(SurfaceError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(SurfaceError::BadSize)?;

        let shm = ShmBuffer::create(buffer_size).map_err(|_| SurfaceError::ShmFailed)?;

        let mut client = handle.borrow_client();
        client
            .surface_attach(surface_id, shm.token(), width, height)
            .map_err(|_| SurfaceError::AttachFailed)?;
        drop(client);

        Ok(Self {
            handle,
            shm,
            surface_id,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
        })
    }
}

impl RenderSurface for SoftSurface {
    fn frame(&mut self) -> Option<DrawBuffer<'_>> {
        let mut buf = DrawBuffer::new(
            self.shm.as_mut_slice(),
            self.width,
            self.height,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    fn present(&self) {
        let mut client = self.handle.borrow_client();
        let _ = client.surface_damage(self.surface_id, 0, 0, self.width as i32, self.height as i32);
        let _ = client.surface_commit(self.surface_id);
    }

    fn present_region(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut client = self.handle.borrow_client();
        let _ = client.surface_damage(self.surface_id, x, y, w, h);
        let _ = client.surface_commit(self.surface_id);
    }

    fn resize(&mut self, new_width: u32, new_height: u32) -> Result<(), RenderError> {
        if new_width == 0 || new_height == 0 {
            return Err(RenderError::BadSize);
        }
        if new_width == self.width && new_height == self.height {
            return Ok(());
        }

        let new_pitch = (new_width as usize)
            .checked_mul(self.bytes_pp as usize)
            .ok_or(RenderError::BadSize)?;
        let buffer_size = new_pitch
            .checked_mul(new_height as usize)
            .ok_or(RenderError::BadSize)?;

        let new_shm = ShmBuffer::create(buffer_size).map_err(|_| RenderError::BufferUnavailable)?;

        let mut client = self.handle.borrow_client();
        client
            .surface_attach(self.surface_id, new_shm.token(), new_width, new_height)
            .map_err(|_| RenderError::BufferUnavailable)?;

        self.shm = new_shm;
        self.width = new_width;
        self.height = new_height;
        self.pitch = new_pitch;

        Ok(())
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    fn pitch(&self) -> usize {
        self.pitch
    }
}
