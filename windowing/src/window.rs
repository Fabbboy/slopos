//! High-level window abstraction combining surface, rendering, input, and redraw state.

use slopos_abi::handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle, WindowHandle};
use slopos_gfx::{RenderError, RenderSurface};
use slopos_protocol::types::Event as ProtocolEvent;

use crate::connection::ProtocolHandle;
use crate::event::Event;
use crate::soft_surface::SoftSurface;
use crate::surface::{Surface, SurfaceError};

pub const EVENT_BUF_LEN: usize = 16;

/// A compositor-managed window with input handling and redraw tracking.
///
/// Owns a [`Surface`] (windowing lifecycle) and a [`SoftSurface`] (rendering).
pub struct Window {
    handle: ProtocolHandle,
    surface: Surface,
    renderer: SoftSurface,
    redraw_needed: bool,
    pointer_x: i32,
    pointer_y: i32,
}

impl Window {
    /// Create a new window of the given size.
    ///
    /// Internally creates a [`Surface`] (compositor objects) and a
    /// [`SoftSurface`] (SHM pixel buffer).
    pub fn new(handle: ProtocolHandle, width: u32, height: u32) -> Result<Self, SurfaceError> {
        let surface = Surface::new(handle.clone(), width, height)?;
        let renderer = SoftSurface::new(
            handle.clone(),
            surface.surface_id(),
            surface.pixel_format(),
            width,
            height,
        )?;
        Ok(Self {
            handle,
            surface,
            renderer,
            redraw_needed: true,
            pointer_x: 0,
            pointer_y: 0,
        })
    }

    /// Set the window title shown in the compositor title bar.
    pub fn set_title(&self, title: &str) {
        let toplevel_id = self.surface.toplevel_id();
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_title(toplevel_id, title.as_bytes());
    }

    /// Set the application identifier (e.g. "org.slopos.files").
    /// The compositor uses this for window-to-dock matching instead of the title.
    pub fn set_app_id(&self, app_id: &str) {
        let toplevel_id = self.surface.toplevel_id();
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_app_id(toplevel_id, app_id.as_bytes());
    }

    /// Resize the window's backing surface to a new size.
    ///
    /// Allocates a new SHM buffer, re-attaches to the compositor, and
    /// requests a redraw. Called automatically on `Event::Configure`.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        self.renderer.resize(width, height)?;
        self.surface.set_size(width, height);
        self.redraw_needed = true;
        Ok(())
    }

    /// Request a redraw on the next frame.
    #[inline]
    pub fn request_redraw(&mut self) {
        self.redraw_needed = true;
    }

    /// Consume and return the redraw flag.
    #[inline]
    pub fn take_redraw(&mut self) -> bool {
        let redraw = self.redraw_needed;
        self.redraw_needed = false;
        redraw
    }

    /// Check whether a redraw is pending without consuming the flag.
    #[inline]
    pub fn needs_redraw(&self) -> bool {
        self.redraw_needed
    }

    /// Last known pointer position in window-local coordinates.
    ///
    /// Returns `(0, 0)` until the first `PointerMotion` event is received.
    #[inline]
    pub fn pointer(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    /// Borrow the underlying windowing surface.
    #[inline]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Mutably borrow the underlying windowing surface.
    #[inline]
    pub fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    /// Borrow the software rendering backend.
    #[inline]
    pub fn renderer(&self) -> &SoftSurface {
        &self.renderer
    }

    /// Mutably borrow the software rendering backend (needed for `frame()`).
    #[inline]
    pub fn renderer_mut(&mut self) -> &mut SoftSurface {
        &mut self.renderer
    }

    /// Access the renderer as a [`RenderSurface`] trait object.
    ///
    /// Use this when code should be generic over the rendering backend.
    #[inline]
    pub fn render_surface(&mut self) -> &mut dyn RenderSurface {
        &mut self.renderer
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.renderer.width()
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.renderer.height()
    }

    /// Poll protocol events from the compositor socket.
    ///
    /// Returns the number of events written (always <= `buf.len()`).
    ///
    /// `BufferRelease` events are consumed internally — they drive the
    /// double-buffer reuse bookkeeping in [`SoftSurface`] and are never surfaced
    /// to the application.
    pub fn poll_protocol_events(&mut self, buf: &mut [ProtocolEvent]) -> usize {
        // Buffer releases are collected and applied after the client borrow is
        // dropped, so the renderer (a sibling field) can be borrowed mutably.
        let mut released = [0u32; 8];
        let mut released_n = 0usize;
        let mut count = 0;
        {
            let mut client = self.handle.borrow_client();
            while count < buf.len() {
                match client.poll_event() {
                    Ok(Some(ProtocolEvent::BufferRelease { buffer_id, .. })) => {
                        if released_n < released.len() {
                            released[released_n] = buffer_id;
                            released_n += 1;
                        }
                    }
                    Ok(Some(evt)) => {
                        buf[count] = evt;
                        count += 1;
                    }
                    _ => break,
                }
            }
        }
        for &buffer_id in &released[..released_n] {
            self.renderer.release_buffer(buffer_id);
        }
        count
    }

    /// Update internal pointer state from a converted event.
    ///
    /// Call this per-event *before* dispatch so `pointer()` reflects the
    /// position at the time of each event, not the end of the batch.
    #[inline]
    pub fn track_pointer(&mut self, event: &Event) {
        if let Event::PointerMotion { x, y } = *event {
            self.pointer_x = x;
            self.pointer_y = y;
        }
    }

    /// Poll input events, convert them, and call `handler` for each.
    ///
    /// Reads from the compositor protocol socket.
    /// Pointer state is updated per-event before the handler is called.
    pub fn poll_events<F: FnMut(Event)>(&mut self, mut handler: F) {
        let mut proto_events: [ProtocolEvent; EVENT_BUF_LEN] =
            core::array::from_fn(|_| ProtocolEvent::FrameDone {
                surface: slopos_protocol::types::SurfaceId::NONE,
                timestamp_ms: 0,
            });
        let count = self.poll_protocol_events(&mut proto_events);
        for pe in &proto_events[..count] {
            if let Some(event) = Event::from_protocol(pe) {
                self.track_pointer(&event);
                handler(event);
            }
        }
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> WindowHandle<'_> {
        self.surface.window_handle()
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> DisplayHandle<'_> {
        self.surface.display_handle()
    }
}
