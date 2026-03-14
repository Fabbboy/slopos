//! Generic event loop for windowed applications.
//!
//! Provides the `WindowedApp` trait and a `run()` function that owns the
//! poll -> dispatch -> redraw -> present -> yield loop. All hot-path calls
//! are monomorphized (no trait objects).

use crate::gfx::DrawBuffer;
use crate::syscall::{InputEvent, core as sys_core, tty};

use super::event::Event;
use super::window::{self, Window};

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
}

/// Run a windowed application to completion.
///
/// Creates a `Window`, calls `app.init()`, then enters the main loop:
/// poll events -> dispatch -> redraw if requested -> present -> yield.
///
/// This function never returns normally; it calls `sys_core::exit()` on
/// `ControlFlow::Exit`.
pub fn run<A: WindowedApp>(mut app: A, width: u32, height: u32) -> ! {
    let mut win = match Window::new(width, height) {
        Ok(w) => w,
        Err(e) => {
            let msg: &[u8] = match e {
                super::surface::SurfaceError::NoDisplay => b"appkit: no display\n",
                super::surface::SurfaceError::BadSize => b"appkit: bad surface size\n",
                super::surface::SurfaceError::ShmFailed => b"appkit: shm alloc failed\n",
                super::surface::SurfaceError::AttachFailed => b"appkit: surface attach failed\n",
            };
            let _ = tty::write(msg);
            sys_core::exit_with_code(1);
        }
    };

    app.init(&mut win);

    let mut raw_buf = [InputEvent::default(); window::EVENT_BUF_LEN];

    loop {
        let count = win.poll_events_raw(&mut raw_buf);

        for raw in &raw_buf[..count] {
            let event = Event::from_raw(raw);
            win.track_pointer(&event);
            if app.on_event(&mut win, event) == ControlFlow::Exit {
                sys_core::exit();
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

        sys_core::yield_now();
    }
}
