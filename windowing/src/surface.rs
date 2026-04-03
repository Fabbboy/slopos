//! Shared memory surface for compositor-managed windows.
//!
//! `Surface` encapsulates the full lifecycle of a window's backing store:
//! display info query, pixel format negotiation, SHM allocation, and
//! compositor attachment. Applications use `Surface::frame()` to obtain
//! a `DrawBuffer` for rendering and [`RenderSurface::present()`] /
//! [`RenderSurface::present_region()`] to push completed frames to the compositor.
//!
//! All surface operations go through the compositor protocol socket.
//! SHM allocation uses kernel syscalls (memory management).

use slopos_abi::handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawWindowHandle, WindowHandle,
};
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::{DrawBuffer, RenderError, RenderSurface};

use crate::connection::ProtocolHandle;
use crate::shm::ShmBuffer;

#[derive(Debug, Clone, Copy)]
pub enum SurfaceError {
    NoDisplay,
    BadSize,
    ShmFailed,
    AttachFailed,
}

/// A compositor-managed shared memory surface.
///
/// Owns the `ShmBuffer` and all associated metadata (dimensions, pitch,
/// pixel format). Created once per window via `Surface::new()`.
pub struct Surface {
    handle: ProtocolHandle,
    shm: ShmBuffer,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
    protocol_surface_id: u32,
    protocol_toplevel_id: u32,
}

impl Surface {
    /// Create a new surface and attach it to the compositor.
    ///
    /// Queries the display for pixel format information, allocates a
    /// shared memory buffer of the appropriate size, and registers it
    /// via the compositor protocol.
    pub fn new(handle: ProtocolHandle, width: u32, height: u32) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::BadSize);
        }

        let mut client = handle.borrow_client();
        // Lazy sync point: receive OutputInfo if we haven't yet.
        // By this point the compositor has had time to accept + push it.
        client
            .ensure_output_info()
            .map_err(|_| SurfaceError::AttachFailed)?;
        let pixel_format =
            PixelFormat::from_u32(client.display_format()).unwrap_or(PixelFormat::Argb8888);

        let protocol_surface_id = client
            .create_surface()
            .map_err(|_| SurfaceError::AttachFailed)?;
        let protocol_toplevel_id = client
            .get_toplevel(protocol_surface_id)
            .map_err(|_| SurfaceError::AttachFailed)?;

        let bytes_pp = pixel_format.bytes_per_pixel();
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(SurfaceError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(SurfaceError::BadSize)?;

        let shm = ShmBuffer::create(buffer_size).map_err(|_| SurfaceError::ShmFailed)?;

        client
            .surface_attach(protocol_surface_id, shm.token(), width, height)
            .map_err(|_| SurfaceError::AttachFailed)?;

        drop(client);

        Ok(Self {
            handle,
            shm,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
            protocol_surface_id,
            protocol_toplevel_id,
        })
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
    pub fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    #[inline]
    pub fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    #[inline]
    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// Protocol handle (for callers that need direct client access).
    #[inline]
    pub fn protocol_handle(&self) -> &ProtocolHandle {
        &self.handle
    }
}

impl HasWindowHandle for Surface {
    fn window_handle(&self) -> WindowHandle<'_> {
        WindowHandle::new(RawWindowHandle {
            surface_id: self.protocol_surface_id,
            toplevel_id: self.protocol_toplevel_id,
            shm_token: self.shm.token(),
        })
    }
}

impl HasDisplayHandle for Surface {
    fn display_handle(&self) -> DisplayHandle<'_> {
        self.handle.display_handle()
    }
}

impl RenderSurface for Surface {
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
        let _ = client.surface_damage(
            self.protocol_surface_id,
            0,
            0,
            self.width as i32,
            self.height as i32,
        );
        let _ = client.surface_commit(self.protocol_surface_id);
    }

    fn present_region(&self, x: i32, y: i32, w: i32, h: i32) {
        let mut client = self.handle.borrow_client();
        let _ = client.surface_damage(self.protocol_surface_id, x, y, w, h);
        let _ = client.surface_commit(self.protocol_surface_id);
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
            .surface_attach(
                self.protocol_surface_id,
                new_shm.token(),
                new_width,
                new_height,
            )
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

impl Drop for Surface {
    fn drop(&mut self) {
        // Destroy compositor-side objects so we don't leak the limited
        // per-client surface slots (MAX_SURFACES = 32).
        // Use try_borrow_client to avoid panicking if the RefCell is already borrowed.
        if let Some(mut client) = self.handle.try_borrow_client() {
            if self.protocol_toplevel_id != 0 {
                let _ = client.toplevel_destroy(self.protocol_toplevel_id);
            }
            if self.protocol_surface_id != 0 {
                let _ = client.surface_destroy(self.protocol_surface_id);
            }
        } else {
            // Client RefCell already borrowed — defer the destroy so the event
            // loop flushes it at a safe point.
            self.handle
                .queue_destroy(self.protocol_toplevel_id, self.protocol_surface_id);
        }
        // ShmBuffer dropped automatically after this.
    }
}
