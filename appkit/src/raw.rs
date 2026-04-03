//! Generic event loop for windowed applications.
//!
//! Provides the `WindowedApp` trait and a `run()` function that owns the
//! poll -> dispatch -> redraw -> present -> yield loop. All hot-path calls
//! are monomorphized (no trait objects).

use slopos_gfx::DrawBuffer;

use crate::platform::sys;
use slopos_protocol::types::Event as ProtocolEvent;

use super::platform::event::Event;
use super::platform::protocol_client;
use super::platform::window::{EVENT_BUF_LEN, Window};

/// Instructs the event loop what to do after processing an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlFlow {
    /// Continue the event loop.
    Continue,
    /// Exit the application.
    Exit,
}

/// Trait implemented by windowed applications.
///
/// The framework calls these methods from the main event loop in
/// `appkit::run()`. All methods have sensible defaults so apps only
/// need to override what they use.
pub trait WindowedApp {
    /// Called once after the window has been created and before the
    /// first frame. Use this to set the title and request an initial draw.
    fn init(&mut self, _win: &mut Window) {}

    /// Called for each input event. Return `ControlFlow::Exit` to quit.
    ///
    /// The default implementation exits on `CloseRequest`.
    fn on_event(&mut self, _win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => ControlFlow::Exit,
            _ => ControlFlow::Continue,
        }
    }

    /// Called when a redraw was requested via `Window::request_redraw()`.
    ///
    /// The `DrawBuffer` already has the correct pixel format set.
    /// Width and height are available via `fb.width()` / `fb.height()`.
    fn draw(&mut self, fb: &mut DrawBuffer<'_>);

    /// Optional periodic refresh interval in milliseconds.
    ///
    /// If this returns `Some(ms)`, the framework automatically requests a
    /// redraw every `ms` milliseconds even without user input. Useful for
    /// apps like system monitors that need to update live data.
    ///
    /// Default is `None` (redraw only on input events).
    fn refresh_interval_ms(&self) -> Option<u64> {
        None
    }
}

/// Run a windowed application to completion.
///
/// Creates a `Window`, calls `app.init()`, then enters the main loop:
/// poll events -> dispatch -> redraw if requested -> present -> yield.
///
/// This function never returns normally; it calls `std::process::exit(0)` on
/// `ControlFlow::Exit`.
pub fn run<A: WindowedApp>(mut app: A, width: u32, height: u32) -> ! {
    // Connect to the compositor protocol socket. Retries internally
    // since the compositor may still be starting.
    let handle = protocol_client::connect().expect("compositor not running");

    let mut win = match Window::new(handle.clone(), width, height) {
        Ok(w) => w,
        Err(e) => {
            let msg: &[u8] = match e {
                super::platform::surface::SurfaceError::NoDisplay => b"appkit: no display\n",
                super::platform::surface::SurfaceError::BadSize => b"appkit: bad surface size\n",
                super::platform::surface::SurfaceError::ShmFailed => b"appkit: shm alloc failed\n",
                super::platform::surface::SurfaceError::AttachFailed => {
                    b"appkit: surface attach failed\n"
                }
            };
            sys::tty_write(msg);
            std::process::exit(1);
        }
    };

    app.init(&mut win);

    let refresh_interval = app.refresh_interval_ms();
    let mut last_refresh = sys::get_time_ms();

    loop {
        // Flush any deferred Surface::drop destroy requests and execute
        // any closures posted by background threads via UiSender.
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let mut proto_buf: [ProtocolEvent; EVENT_BUF_LEN] =
            core::array::from_fn(|_| ProtocolEvent::FrameDone {
                surface: 0,
                timestamp_ms: 0,
            });
        let count = win.poll_protocol_events(&mut proto_buf);

        for pe in &proto_buf[..count] {
            if let Some(event) = Event::from_protocol(pe) {
                win.track_pointer(&event);

                // Auto-handle resize before dispatching to the app.
                if let Event::Configure { width, height } = event {
                    let _ = win.resize(width, height);
                }

                if app.on_event(&mut win, event) == ControlFlow::Exit {
                    std::process::exit(0);
                }
            }
        }

        // Auto-request redraws for apps with a periodic refresh interval.
        if let Some(interval) = refresh_interval {
            let now = sys::get_time_ms();
            if now.saturating_sub(last_refresh) >= interval {
                last_refresh = now;
                win.request_redraw();
            }
        }

        if win.take_redraw() {
            if let Some(mut fb) = win.surface_mut().frame() {
                app.draw(&mut fb);
                win.surface().present_full();
            } else {
                win.request_redraw();
            }
        }

        // Sleep until the compositor sends an event, a UiSender posts work,
        // or the next refresh is due.  Replaces the old yield_now()
        // busy-spin with a proper poll()-based sleep.
        let timeout_ms: i64 = if win.needs_redraw() {
            0
        } else if let Some(interval) = refresh_interval {
            let now = sys::get_time_ms();
            let elapsed = now.saturating_sub(last_refresh);
            if elapsed >= interval {
                0
            } else {
                (interval - elapsed) as i64
            }
        } else {
            -1
        };
        if timeout_ms != 0 {
            handle.wait_events(timeout_ms);
        }
    }
}
