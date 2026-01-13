slopos_lib::define_service! {
    tty => TtyServices {
        read_line(buf: *mut u8, len: usize) -> usize;
        read_char_blocking(buf: *mut u8) -> i32;
        set_focus(target: u32) -> i32;
        get_focus() -> u32;
    }
}

#[inline(always)]
pub fn tty_read_line(buf: *mut u8, len: usize) -> usize {
    read_line(buf, len)
}

#[inline(always)]
pub fn tty_read_char_blocking(buf: *mut u8) -> i32 {
    read_char_blocking(buf)
}

#[inline(always)]
pub fn tty_set_focus(target: u32) -> i32 {
    set_focus(target)
}

#[inline(always)]
pub fn tty_get_focus() -> u32 {
    get_focus()
}
