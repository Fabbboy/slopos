//! High-level window abstraction combining surface, input, and redraw state.

use crate::syscall::{InputEvent, input, window};

use super::event::Event;
use super::surface::{Surface, SurfaceError};

pub(super) const EVENT_BUF_LEN: usize = 16;

/// A compositor-managed window with input handling and redraw tracking.
///
/// `Window` owns a [`Surface`] and adds pointer tracking, a redraw flag,
/// and batch event polling. Applications that use `appkit::run()` receive
/// a `Window` automatically; applications with custom event loops can
/// create one directly.
pub struct Window {
    surface: Surface,
    redraw_needed: bool,
    pointer_x: i32,
    pointer_y: i32,
}

impl Window {
    /// Create a new window of the given size.
    ///
    /// Internally creates and attaches a `Surface`.
    pub fn new(width: u32, height: u32) -> Result<Self, SurfaceError> {
        Ok(Self {
            surface: Surface::new(width, height)?,
            redraw_needed: true,
            pointer_x: 0,
            pointer_y: 0,
        })
    }

    /// Set the window title shown in the compositor title bar.
    pub fn set_title(&self, title: &str) {
        let _ = window::surface_set_title(title);
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

    /// Poll raw input events into `buf` without any processing.
    ///
    /// Returns the number of events written (always ≤ `buf.len()`).
    pub fn poll_events_raw(&mut self, buf: &mut [InputEvent]) -> usize {
        (input::poll_batch(buf) as usize).min(buf.len())
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
    /// Pointer state is updated per-event before the handler is called.
    pub fn poll_events<F: FnMut(Event)>(&mut self, mut handler: F) {
        let mut raw_events = [InputEvent::default(); EVENT_BUF_LEN];
        let count = self.poll_events_raw(&mut raw_events);
        for raw in &raw_events[..count] {
            let event = Event::from_raw(raw);
            self.track_pointer(&event);
            handler(event);
        }
    }
}
