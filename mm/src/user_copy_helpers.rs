use core::ffi::c_int;

use crate::user_copy::user_copy_from_user;

#[repr(C)]
pub struct UserRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: u32,
}

#[repr(C)]
pub struct UserLine {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub color: u32,
}

#[repr(C)]
pub struct UserCircle {
    pub cx: i32,
    pub cy: i32,
    pub radius: i32,
    pub color: u32,
}

#[repr(C)]
pub struct UserText {
    pub x: i32,
    pub y: i32,
    pub fg_color: u32,
    pub bg_color: u32,
    pub str_ptr: *const u8,
    pub len: u32,
}

pub const USER_TEXT_MAX_BYTES: u32 = 256;

#[unsafe(no_mangle)]
pub fn user_copy_rect_checked(dst: *mut UserRect, user_rect: *const UserRect) -> c_int {
    if dst.is_null() || user_rect.is_null() {
        return -1;
    }
    if user_copy_from_user(
        dst as *mut _,
        user_rect as *const _,
        core::mem::size_of::<UserRect>(),
    ) != 0
    {
        return -1;
    }
    unsafe {
        if (*dst).width <= 0 || (*dst).height <= 0 {
            return -1;
        }
        if (*dst).width > 8192 || (*dst).height > 8192 {
            return -1;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub fn user_copy_line_checked(dst: *mut UserLine, user_line: *const UserLine) -> c_int {
    if dst.is_null() || user_line.is_null() {
        return -1;
    }
    if user_copy_from_user(
        dst as *mut _,
        user_line as *const _,
        core::mem::size_of::<UserLine>(),
    ) != 0
    {
        return -1;
    }
    0
}

#[unsafe(no_mangle)]
pub fn user_copy_circle_checked(dst: *mut UserCircle, user_circle: *const UserCircle) -> c_int {
    if dst.is_null() || user_circle.is_null() {
        return -1;
    }
    if user_copy_from_user(
        dst as *mut _,
        user_circle as *const _,
        core::mem::size_of::<UserCircle>(),
    ) != 0
    {
        return -1;
    }
    unsafe {
        if (*dst).radius <= 0 || (*dst).radius > 4096 {
            return -1;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub fn user_copy_text_header(dst: *mut UserText, user_text: *const UserText) -> c_int {
    if dst.is_null() || user_text.is_null() {
        return -1;
    }
    if user_copy_from_user(
        dst as *mut _,
        user_text as *const _,
        core::mem::size_of::<UserText>(),
    ) != 0
    {
        return -1;
    }
    unsafe {
        if (*dst).str_ptr.is_null() || (*dst).len == 0 {
            return -1;
        }
        if (*dst).len >= USER_TEXT_MAX_BYTES {
            (*dst).len = USER_TEXT_MAX_BYTES - 1;
        }
    }
    0
}
