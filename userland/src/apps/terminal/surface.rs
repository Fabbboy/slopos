//! Compositor surface wrapper for terminal drawing.

use std::cell::RefCell;

use crate::gfx::DrawBuffer;
use crate::syscall::tty;
use slopos_gfx::RenderSurface;
use slopos_protocol::types::{SurfaceId, ToplevelId};
use slopos_windowing::{HasWindowHandle, ProtocolHandle, SoftSurface, Surface};

thread_local! {
    static SURFACE: RefCell<Option<Surface>> = RefCell::new(None);
    static RENDERER: RefCell<Option<SoftSurface>> = RefCell::new(None);
    static HANDLE: RefCell<Option<ProtocolHandle>> = RefCell::new(None);
}

fn with_renderer<R, F: FnOnce(&mut SoftSurface) -> R>(f: F) -> Option<R> {
    RENDERER.with(|r| r.borrow_mut().as_mut().map(f))
}

pub fn init_handle(handle: ProtocolHandle) {
    HANDLE.with(|h| *h.borrow_mut() = Some(handle));
}

pub fn init(width: i32, height: i32) -> bool {
    HANDLE.with(|h| {
        let h = h.borrow();
        let Some(handle) = h.as_ref() else {
            let _ = tty::write(b"terminal: no protocol handle\n");
            return false;
        };
        let surface = match Surface::new(handle.clone(), width as u32, height as u32) {
            Ok(s) => s,
            Err(_) => {
                let _ = tty::write(b"terminal: surface init failed\n");
                return false;
            }
        };
        let renderer = match SoftSurface::new(
            handle.clone(),
            surface.surface_id(),
            surface.pixel_format(),
            width as u32,
            height as u32,
        ) {
            Ok(r) => r,
            Err(_) => {
                let _ = tty::write(b"terminal: renderer init failed\n");
                return false;
            }
        };
        SURFACE.with(|s| *s.borrow_mut() = Some(surface));
        RENDERER.with(|r| *r.borrow_mut() = Some(renderer));
        true
    })
}

pub fn set_title(title: &str) {
    SURFACE.with(|s| {
        HANDLE.with(|h| {
            let s = s.borrow();
            let h = h.borrow();
            if let (Some(surface), Some(handle)) = (s.as_ref(), h.as_ref()) {
                let toplevel_id = ToplevelId::from_raw(surface.window_handle().toplevel_id());
                let mut client = handle.borrow_client();
                let _ = client.toplevel_set_title(toplevel_id, title.as_bytes());
            }
        });
    });
}

pub fn set_app_id(app_id: &str) {
    SURFACE.with(|s| {
        HANDLE.with(|h| {
            let s = s.borrow();
            let h = h.borrow();
            if let (Some(surface), Some(handle)) = (s.as_ref(), h.as_ref()) {
                let toplevel_id = ToplevelId::from_raw(surface.window_handle().toplevel_id());
                let mut client = handle.borrow_client();
                let _ = client.toplevel_set_app_id(toplevel_id, app_id.as_bytes());
            }
        });
    });
}

pub fn set_cursor_shape(shape: u8) {
    SURFACE.with(|s| {
        HANDLE.with(|h| {
            let s = s.borrow();
            let h = h.borrow();
            if let (Some(surface), Some(handle)) = (s.as_ref(), h.as_ref()) {
                let surface_id = SurfaceId::from_raw(surface.window_handle().surface_id());
                let mut client = handle.borrow_client();
                let _ = client.set_cursor_shape(surface_id, shape);
            }
        });
    });
}

pub fn draw<R, F: FnOnce(&mut DrawBuffer) -> R>(f: F) -> Option<R> {
    with_renderer(|renderer| {
        let mut buf = renderer.frame()?;
        Some(f(&mut buf))
    })?
}

/// Age of the buffer the next [`draw`] will paint into, per the
/// `EGL_EXT_buffer_age` convention (`0` = contents undefined). The renderer's
/// slot choice is deterministic between presents, so reading this before the
/// draw gives the same answer as reading it after.
pub fn buffer_age() -> u32 {
    with_renderer(|renderer| renderer.buffer_age()).unwrap_or(0)
}

pub fn resize(new_width: u32, new_height: u32) -> bool {
    with_renderer(|renderer| renderer.resize(new_width, new_height).is_ok()).unwrap_or(false)
}

pub fn release_buffer(buffer_id: u32) {
    with_renderer(|renderer| renderer.release_buffer(buffer_id));
}

pub fn present() {
    RENDERER.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(renderer) = r.as_mut() {
            renderer.present();
        }
    });
}

/// Commit the frame, reporting only `(x, y, w, h)` as damaged.
pub fn present_region(x: i32, y: i32, w: i32, h: i32) {
    RENDERER.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(renderer) = r.as_mut() {
            renderer.present_region(x, y, w, h);
        }
    });
}
