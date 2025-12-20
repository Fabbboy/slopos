use core::arch::asm;
use core::ffi::{c_char, c_void};

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_READ_CHAR: u64 = 25;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_FB_INFO: u64 = 6;
pub const SYSCALL_GFX_FILL_RECT: u64 = 7;
pub const SYSCALL_GFX_DRAW_LINE: u64 = 8;
pub const SYSCALL_GFX_DRAW_CIRCLE: u64 = 9;
pub const SYSCALL_GFX_DRAW_CIRCLE_FILLED: u64 = 10;
pub const SYSCALL_FONT_DRAW: u64 = 11;
pub const SYSCALL_RANDOM_NEXT: u64 = 12;
pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
pub const SYSCALL_ROULETTE_DRAW: u64 = 24;
pub const SYSCALL_FS_OPEN: u64 = 14;
pub const SYSCALL_FS_CLOSE: u64 = 15;
pub const SYSCALL_FS_READ: u64 = 16;
pub const SYSCALL_FS_WRITE: u64 = 17;
pub const SYSCALL_FS_STAT: u64 = 18;
pub const SYSCALL_FS_MKDIR: u64 = 19;
pub const SYSCALL_FS_UNLINK: u64 = 20;
pub const SYSCALL_FS_LIST: u64 = 21;
pub const SYSCALL_SYS_INFO: u64 = 22;
pub const SYSCALL_HALT: u64 = 23;

pub const USER_FS_OPEN_READ: u32 = 0x1;
pub const USER_FS_OPEN_WRITE: u32 = 0x2;
pub const USER_FS_OPEN_CREAT: u32 = 0x4;
pub const USER_FS_OPEN_APPEND: u32 = 0x8;

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFbInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserLine {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserCircle {
    pub cx: i32,
    pub cy: i32,
    pub radius: i32,
    pub color: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserText {
    pub x: i32,
    pub y: i32,
    pub fg_color: u32,
    pub bg_color: u32,
    pub str_ptr: *const c_char,
    pub len: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsEntry {
    pub name: [u8; 64],
    pub r#type: u8,
    pub size: u32,
}

impl UserFsEntry {
    pub const fn new() -> Self {
        Self {
            name: [0; 64],
            r#type: 0,
            size: 0,
        }
    }
}

impl Default for UserFsEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsStat {
    pub r#type: u8,
    pub size: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsList {
    pub entries: *mut UserFsEntry,
    pub max_entries: u32,
    pub count: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserSysInfo {
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

#[inline(always)]
#[unsafe(link_section = ".user_text")]
unsafe fn syscall(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let mut ret = num;
    unsafe {
        asm!(
            "int 0x80",
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            inout("rax") ret,
            clobber_abi("sysv64"),
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_write(buf: &[u8]) -> i64 {
    unsafe { syscall(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}


#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read(buf: &mut [u8]) -> i64 {
    unsafe { syscall(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read_char() -> i64 {
    unsafe { syscall(SYSCALL_READ_CHAR, 0, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sleep_ms(ms: u32) {
    unsafe {
        syscall(SYSCALL_SLEEP_MS, ms as u64, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette() -> u64 {
    unsafe { syscall(SYSCALL_ROULETTE, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_result(fate_packed: u64) {
    unsafe {
        syscall(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_draw(fate: u32) -> i64 {
    unsafe { syscall(SYSCALL_ROULETTE_DRAW, fate as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exit() -> ! {
    unsafe {
        syscall(SYSCALL_EXIT, 0, 0, 0);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_info(out: &mut UserFbInfo) -> i64 {
    unsafe { syscall(SYSCALL_FB_INFO, out as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_gfx_fill_rect(rect: &UserRect) -> i64 {
    unsafe { syscall(SYSCALL_GFX_FILL_RECT, rect as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_gfx_draw_line(line: &UserLine) -> i64 {
    unsafe { syscall(SYSCALL_GFX_DRAW_LINE, line as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_gfx_draw_circle(circle: &UserCircle) -> i64 {
    unsafe { syscall(SYSCALL_GFX_DRAW_CIRCLE, circle as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_gfx_draw_circle_filled(circle: &UserCircle) -> i64 {
    unsafe {
        syscall(
            SYSCALL_GFX_DRAW_CIRCLE_FILLED,
            circle as *const _ as u64,
            0,
            0,
        ) as i64
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_font_draw(text: &UserText) -> i64 {
    unsafe { syscall(SYSCALL_FONT_DRAW, text as *const _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_random_next() -> u32 {
    unsafe { syscall(SYSCALL_RANDOM_NEXT, 0, 0, 0) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_open(path: *const c_char, flags: u32) -> i64 {
    unsafe { syscall(SYSCALL_FS_OPEN, path as u64, flags as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_close(fd: i32) -> i64 {
    unsafe { syscall(SYSCALL_FS_CLOSE, fd as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_read(fd: i32, buf: *mut c_void, len: usize) -> i64 {
    unsafe { syscall(SYSCALL_FS_READ, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_write(fd: i32, buf: *const c_void, len: usize) -> i64 {
    unsafe { syscall(SYSCALL_FS_WRITE, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_stat(path: *const c_char, out_stat: &mut UserFsStat) -> i64 {
    unsafe { syscall(SYSCALL_FS_STAT, path as u64, out_stat as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_mkdir(path: *const c_char) -> i64 {
    unsafe { syscall(SYSCALL_FS_MKDIR, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_unlink(path: *const c_char) -> i64 {
    unsafe { syscall(SYSCALL_FS_UNLINK, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_list(path: *const c_char, list: &mut UserFsList) -> i64 {
    unsafe { syscall(SYSCALL_FS_LIST, path as u64, list as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall(SYSCALL_SYS_INFO, info as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_halt() -> ! {
    unsafe {
        syscall(SYSCALL_HALT, 0, 0, 0);
    }
    loop {}
}
