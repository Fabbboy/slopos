#![allow(dead_code)]

use core::ffi::c_char;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_fb_info {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

pub const USER_FS_OPEN_READ: u32 = 0x1;
pub const USER_FS_OPEN_WRITE: u32 = 0x2;
pub const USER_FS_OPEN_CREAT: u32 = 0x4;
pub const USER_FS_OPEN_APPEND: u32 = 0x8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_line {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_circle {
    pub cx: i32,
    pub cy: i32,
    pub radius: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_text {
    pub x: i32,
    pub y: i32,
    pub fg_color: u32,
    pub bg_color: u32,
    pub str_ptr: *const c_char, // user pointer to UTF-8 string
    pub len: u32,               // bytes to copy (excluding terminator), will be capped
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct user_fs_entry {
    pub name: [c_char; 64],
    pub type_: u8, // 0=file, 1=dir
    pub size: u32,
}

impl Default for user_fs_entry {
    fn default() -> Self {
        Self {
            name: [0; 64],
            type_: 0,
            size: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_fs_stat {
    pub type_: u8, // 0=file,1=dir,0xFF=missing
    pub size: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_fs_list {
    pub entries: *mut user_fs_entry, // user buffer
    pub max_entries: u32,            // capacity in entries
    pub count: u32,                  // filled by kernel
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct user_sys_info {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
}

