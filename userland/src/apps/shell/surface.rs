//! Compositor surface wrapper for shell drawing.

use crate::appkit::Surface;
use crate::gfx::DrawBuffer;
use crate::syscall::tty;

use super::SyncUnsafeCell;

static SURFACE: SyncUnsafeCell<Option<Surface>> = SyncUnsafeCell::new(None);

fn with_surface<R, F: FnOnce(&mut Surface) -> R>(f: F) -> Option<R> {
    let slot = unsafe { &mut *SURFACE.get() };
    slot.as_mut().map(f)
}

pub fn init(width: i32, height: i32) -> bool {
    match Surface::new(width as u32, height as u32) {
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
