//! Compositor surface wrapper for shell drawing.

use crate::gfx::DrawBuffer;
use crate::syscall::tty;
use slopos_gfx::RenderSurface;
use slopos_windowing::{HasWindowHandle, ProtocolHandle, SoftSurface, Surface};

use super::SyncUnsafeCell;

static SURFACE: SyncUnsafeCell<Option<Surface>> = SyncUnsafeCell::new(None);
static RENDERER: SyncUnsafeCell<Option<SoftSurface>> = SyncUnsafeCell::new(None);

/// Store a protocol handle for shell surface operations (title, app_id, cursor).
///
/// SAFETY: The shell is single-threaded — this cell is only accessed from the
/// main shell thread, same as all other SyncUnsafeCell statics in the shell.
static HANDLE: SyncUnsafeCell<Option<ProtocolHandle>> = SyncUnsafeCell::new(None);

fn with_renderer<R, F: FnOnce(&mut SoftSurface) -> R>(f: F) -> Option<R> {
    let slot = unsafe { &mut *RENDERER.get() };
    slot.as_mut().map(f)
}

/// Store the protocol handle for later use by surface operations.
pub fn init_handle(handle: ProtocolHandle) {
    unsafe {
        *HANDLE.get() = Some(handle);
    }
}

pub fn init(width: i32, height: i32) -> bool {
    let handle = unsafe { &*HANDLE.get() };
    let Some(handle) = handle.as_ref() else {
        let _ = tty::write(b"shell: no protocol handle\n");
        return false;
    };
    let surface = match Surface::new(handle.clone(), width as u32, height as u32) {
        Ok(s) => s,
        Err(_) => {
            let _ = tty::write(b"shell: surface init failed\n");
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
            let _ = tty::write(b"shell: renderer init failed\n");
            return false;
        }
    };
    unsafe {
        *SURFACE.get() = Some(surface);
        *RENDERER.get() = Some(renderer);
    }
    true
}

pub fn set_title(title: &str) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let toplevel_id = surface.window_handle().toplevel_id();
        let mut client = handle.borrow_client();
        let _ = client.toplevel_set_title(toplevel_id, title.as_bytes());
    }
}

pub fn set_app_id(app_id: &str) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let toplevel_id = surface.window_handle().toplevel_id();
        let mut client = handle.borrow_client();
        let _ = client.toplevel_set_app_id(toplevel_id, app_id.as_bytes());
    }
}

pub fn set_cursor_shape(shape: u8) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let surface_id = surface.window_handle().surface_id();
        let mut client = handle.borrow_client();
        let _ = client.set_cursor_shape(surface_id, shape);
    }
}

pub fn bytes_pp() -> u8 {
    let slot = unsafe { &*RENDERER.get() };
    slot.as_ref().map_or(4, |r| r.bytes_pp())
}

pub fn draw<R, F: FnOnce(&mut DrawBuffer) -> R>(f: F) -> Option<R> {
    with_renderer(|renderer| {
        let mut buf = renderer.frame()?;
        Some(f(&mut buf))
    })?
}

pub fn resize(new_width: u32, new_height: u32) -> bool {
    with_renderer(|renderer| renderer.resize(new_width, new_height).is_ok()).unwrap_or(false)
}

pub fn present() {
    let slot = unsafe { &*RENDERER.get() };
    if let Some(renderer) = slot.as_ref() {
        renderer.present();
    }
}
