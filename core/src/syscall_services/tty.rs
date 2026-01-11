use core::sync::atomic::{AtomicPtr, Ordering};

#[repr(C)]
pub struct TtyServices {
    pub read_line: fn(*mut u8, usize) -> usize,
    pub read_char_blocking: fn(*mut u8) -> i32,
    pub set_focus: fn(u32) -> i32,
    pub get_focus: fn() -> u32,
}

static TTY: AtomicPtr<TtyServices> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_tty_services(services: &'static TtyServices) {
    let prev = TTY.swap(services as *const _ as *mut _, Ordering::Release);
    assert!(prev.is_null(), "tty services already registered");
}

pub fn is_tty_initialized() -> bool {
    !TTY.load(Ordering::Acquire).is_null()
}

#[inline(always)]
pub fn tty_services() -> &'static TtyServices {
    let ptr = TTY.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "tty services not initialized");
    unsafe { &*ptr }
}

#[inline(always)]
pub fn tty_read_line(buf: *mut u8, len: usize) -> usize {
    (tty_services().read_line)(buf, len)
}

#[inline(always)]
pub fn tty_read_char_blocking(buf: *mut u8) -> i32 {
    (tty_services().read_char_blocking)(buf)
}

#[inline(always)]
pub fn tty_set_focus(target: u32) -> i32 {
    (tty_services().set_focus)(target)
}

#[inline(always)]
pub fn tty_get_focus() -> u32 {
    (tty_services().get_focus)()
}
