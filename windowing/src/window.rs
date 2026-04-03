//! High-level window abstraction combining surface, input, and redraw state.

use slopos_protocol::types::Event as ProtocolEvent;

use crate::connection::ProtocolHandle;
use crate::event::Event;
use crate::surface::{Surface, SurfaceError};

pub const EVENT_BUF_LEN: usize = 16;

/// A compositor-managed window with input handling and redraw tracking.
///
/// `Window` owns a [`Surface`] and adds pointer tracking, a redraw flag,
/// and batch event polling. Applications that use `run()` receive
/// a `Window` automatically; applications with custom event loops can
/// create one directly.
pub struct Window {
    handle: ProtocolHandle,
    surface: Surface,
    redraw_needed: bool,
    pointer_x: i32,
    pointer_y: i32,
}

impl Window {
    /// Create a new window of the given size.
    ///
    /// Internally creates and attaches a `Surface`.
    pub fn new(handle: ProtocolHandle, width: u32, height: u32) -> Result<Self, SurfaceError> {
        let surface = Surface::new(handle.clone(), width, height)?;
        Ok(Self {
            handle,
            surface,
            redraw_needed: true,
            pointer_x: 0,
            pointer_y: 0,
        })
    }

    /// Set the window title shown in the compositor title bar.
    pub fn set_title(&self, title: &str) {
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_title(self.surface.protocol_toplevel_id(), title.as_bytes());
    }

    /// Set the application identifier (e.g. "org.slopos.files").
    /// The compositor uses this for window-to-dock matching instead of the title.
    pub fn set_app_id(&self, app_id: &str) {
        let mut client = self.handle.borrow_client();
        let _ = client.toplevel_set_app_id(self.surface.protocol_toplevel_id(), app_id.as_bytes());
    }

    /// Resize the window's backing surface to a new size.
    ///
    /// Allocates a new SHM buffer, re-attaches to the compositor, and
    /// requests a redraw. Called automatically on `Event::Configure`.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), SurfaceError> {
        self.surface.resize(width, height)?;
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

    /// Borrow the underlying surface.
    #[inline]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Mutably borrow the underlying surface (needed for `frame()`).
    #[inline]
    pub fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.surface.width()
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.surface.height()
    }

    /// Poll protocol events from the compositor socket.
    ///
    /// Returns the number of events written (always <= `buf.len()`).
    pub fn poll_protocol_events(&mut self, buf: &mut [ProtocolEvent]) -> usize {
        let mut client = self.handle.borrow_client();
        let mut count = 0;
        while count < buf.len() {
            match client.poll_event() {
                Ok(Some(evt)) => {
                    buf[count] = evt;
                    count += 1;
                }
                _ => break,
            }
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
                surface: 0,
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
