//! Compositor-managed windowing surface.
//!
//! `Surface` handles the windowing lifecycle: display info query, pixel format
//! negotiation, compositor surface/toplevel creation, and protocol teardown.

use slopos_abi::handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawWindowHandle, WindowHandle,
};
use slopos_abi::pixel::PixelFormat;
use slopos_protocol::types::{SurfaceId, ToplevelId};

use crate::connection::ProtocolHandle;

#[derive(Debug, Clone, Copy)]
pub enum SurfaceError {
    NoDisplay,
    BadSize,
    ShmFailed,
    AttachFailed,
}

/// Owns the compositor surface and toplevel objects, not a pixel buffer.
pub struct Surface {
    handle: ProtocolHandle,
    width: u32,
    height: u32,
    bytes_pp: u8,
    pixel_format: PixelFormat,
    protocol_surface_id: SurfaceId,
    protocol_toplevel_id: ToplevelId,
}

impl Surface {
    /// Creates the compositor objects only; call
    /// [`SoftSurface::new()`](crate::soft_surface::SoftSurface::new) to set up rendering.
    pub fn new(handle: ProtocolHandle, width: u32, height: u32) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::BadSize);
        }

        let mut client = handle.borrow_client();
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

        drop(client);

        Ok(Self {
            handle,
            width,
            height,
            bytes_pp,
            pixel_format,
            protocol_surface_id,
            protocol_toplevel_id,
        })
    }

    #[inline]
    pub fn surface_id(&self) -> SurfaceId {
        self.protocol_surface_id
    }

    #[inline]
    pub fn toplevel_id(&self) -> ToplevelId {
        self.protocol_toplevel_id
    }

    #[inline]
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
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
    pub fn protocol_handle(&self) -> &ProtocolHandle {
        &self.handle
    }
}

impl HasWindowHandle for Surface {
    fn window_handle(&self) -> WindowHandle<'_> {
        WindowHandle::new(RawWindowHandle {
            surface_id: self.protocol_surface_id.raw(),
            toplevel_id: self.protocol_toplevel_id.raw(),
        })
    }
}

impl HasDisplayHandle for Surface {
    fn display_handle(&self) -> DisplayHandle<'_> {
        self.handle.display_handle()
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(mut client) = self.handle.try_borrow_client() {
            if self.protocol_toplevel_id != ToplevelId::NONE {
                let _ = client.toplevel_destroy(self.protocol_toplevel_id);
            }
            if self.protocol_surface_id != SurfaceId::NONE {
                let _ = client.surface_destroy(self.protocol_surface_id);
            }
        } else {
            self.handle
                .queue_destroy(self.protocol_toplevel_id, self.protocol_surface_id);
        }
    }
}
