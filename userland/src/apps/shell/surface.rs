//! Compositor surface wrapper for shell drawing.

use crate::gfx::DrawBuffer;
use crate::syscall::tty;
use slopos_appkit::platform::protocol_client::ProtocolHandle;
use slopos_appkit::platform::surface::Surface;

use super::SyncUnsafeCell;

static SURFACE: SyncUnsafeCell<Option<Surface>> = SyncUnsafeCell::new(None);

/// Store a protocol handle for shell surface operations (title, app_id, cursor).
///
/// SAFETY: The shell is single-threaded — this cell is only accessed from the
/// main shell thread, same as all other SyncUnsafeCell statics in the shell.
static HANDLE: SyncUnsafeCell<Option<ProtocolHandle>> = SyncUnsafeCell::new(None);

fn with_surface<R, F: FnOnce(&mut Surface) -> R>(f: F) -> Option<R> {
    let slot = unsafe { &mut *SURFACE.get() };
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
    match Surface::new(handle.clone(), width as u32, height as u32) {
        Ok(s) => {
            unsafe {
                *SURFACE.get() = Some(s);
            }
            true
        }
        Err(_) => {
            let _ = tty::write(b"shell: surface init failed\n");
            false
        }
    }
}

pub fn set_title(title: &str) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let mut client = handle.borrow_client();
        let _ = client.toplevel_set_title(surface.protocol_toplevel_id(), title.as_bytes());
    }
}

pub fn set_app_id(app_id: &str) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let mut client = handle.borrow_client();
        let _ = client.toplevel_set_app_id(surface.protocol_toplevel_id(), app_id.as_bytes());
    }
}

pub fn set_cursor_shape(shape: u8) {
    let slot = unsafe { &*SURFACE.get() };
    let handle = unsafe { &*HANDLE.get() };
    if let (Some(surface), Some(handle)) = (slot.as_ref(), handle.as_ref()) {
        let mut client = handle.borrow_client();
        let _ = client.set_cursor_shape(surface.protocol_surface_id(), shape);
    }
}

pub fn bytes_pp() -> u8 {
    let slot = unsafe { &*SURFACE.get() };
    slot.as_ref().map_or(4, |s| s.bytes_pp())
}

pub fn draw<R, F: FnOnce(&mut DrawBuffer) -> R>(f: F) -> Option<R> {
    with_surface(|surface| {
        let mut buf = surface.frame()?;
        Some(f(&mut buf))
    })?
}

pub fn resize(new_width: u32, new_height: u32) -> bool {
    let slot = unsafe { &mut *SURFACE.get() };
    if let Some(surface) = slot.as_mut() {
        surface.resize(new_width, new_height).is_ok()
    } else {
        false
    }
}

pub fn present_full() {
    let slot = unsafe { &*SURFACE.get() };
    if let Some(surface) = slot.as_ref() {
        surface.present_full();
    }
}
