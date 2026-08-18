//! Generic event loop for windowed applications.
//!
//! Provides the `WindowedApp` trait and a `run()` function that owns the
//! poll -> dispatch -> redraw -> present -> yield loop.

use slopos_gfx::{DrawBuffer, RenderSurface};

use crate::sys;
use slopos_protocol::types::Event as ProtocolEvent;

use crate::connection;
use crate::event::Event;
use crate::surface::SurfaceError;
use crate::window::{EVENT_BUF_LEN, Window};

/// Instructs the event loop what to do after processing an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlFlow {
    Continue,
    Exit,
}

/// Callbacks driven by the [`run()`] event loop.
pub trait WindowedApp {
    /// Called once after window creation, before the first frame.
    fn init(&mut self, _win: &mut Window) {}

    /// Called for each input event; return `ControlFlow::Exit` to quit.
    fn on_event(&mut self, _win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => ControlFlow::Exit,
            _ => ControlFlow::Continue,
        }
    }

    /// Called when a redraw was requested via `Window::request_redraw()`.
    fn draw(&mut self, fb: &mut DrawBuffer<'_>);

    /// `Some(ms)` makes the loop request a redraw every `ms` even without input.
    fn refresh_interval_ms(&self) -> Option<u64> {
        None
    }
}

/// Run a windowed application to completion.
///
/// Never returns: exits the process on `ControlFlow::Exit`.
pub fn run<A: WindowedApp>(mut app: A, width: u32, height: u32) -> ! {
    let handle = connection::connect().expect("compositor not running");

    let mut win = match Window::new(handle.clone(), width, height) {
        Ok(w) => w,
        Err(e) => {
            let msg: &[u8] = match e {
                SurfaceError::NoDisplay => b"windowing: no display\n",
                SurfaceError::BadSize => b"windowing: bad surface size\n",
                SurfaceError::ShmFailed => b"windowing: shm alloc failed\n",
                SurfaceError::AttachFailed => b"windowing: surface attach failed\n",
            };
            sys::tty_write(msg);
            std::process::exit(1);
        }
    };

    app.init(&mut win);

    let refresh_interval = app.refresh_interval_ms();
    let mut last_refresh = sys::get_time_ms();

    loop {
        handle.flush_pending_destroys();
        handle.drain_ui_queue();

        let mut proto_buf: [ProtocolEvent; EVENT_BUF_LEN] =
            core::array::from_fn(|_| ProtocolEvent::FrameDone {
                surface: slopos_protocol::types::SurfaceId::NONE,
                timestamp_ms: 0,
            });
        let count = win.poll_protocol_events(&mut proto_buf);

        for pe in &proto_buf[..count] {
            if let Some(event) = Event::from_protocol(pe) {
                win.track_pointer(&event);

                if let Event::Configure { width, height } = event {
                    let _ = win.resize(width, height);
                }

                if app.on_event(&mut win, event) == ControlFlow::Exit {
                    std::process::exit(0);
                }
            }
        }

        if let Some(interval) = refresh_interval {
            let now = sys::get_time_ms();
            if now.saturating_sub(last_refresh) >= interval {
                last_refresh = now;
                win.request_redraw();
            }
        }

        if win.take_redraw() {
            if let Some(mut fb) = win.renderer_mut().frame() {
                app.draw(&mut fb);
                win.renderer_mut().present();
            } else {
                win.request_redraw();
            }
        }

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
