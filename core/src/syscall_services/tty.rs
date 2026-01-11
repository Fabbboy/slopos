use slopos_lib::ServiceCell;

#[repr(C)]
pub struct TtyServices {
    pub read_line: fn(*mut u8, usize) -> usize,
    pub read_char_blocking: fn(*mut u8) -> i32,
    pub set_focus: fn(u32) -> i32,
    pub get_focus: fn() -> u32,
}

static TTY: ServiceCell<TtyServices> = ServiceCell::new("tty");

pub fn register_tty_services(services: &'static TtyServices) {
    TTY.register(services);
}

pub fn is_tty_initialized() -> bool {
    TTY.is_initialized()
}

#[inline(always)]
pub fn tty_services() -> &'static TtyServices {
    TTY.get()
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
