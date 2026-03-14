use crate::pal::Pal;
use crate::pal::Sys;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    match Sys::lseek(fd, offset, whence) {
        Ok(pos) => pos,
        Err(e) => -(e.raw() as i64),
    }
}

#[repr(C)]
pub struct SloposStat {
    pub st_mode: u32,
    pub st_size: u64,
    pub st_atime: i64,
    pub st_mtime: i64,
    pub st_ctime: i64,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fstat(fd: i32, stat_buf: *mut SloposStat) -> i32 {
    let mut raw = [0u8; 256];
    match Sys::fstat(fd, raw.as_mut_ptr()) {
        Ok(()) => {
            if !stat_buf.is_null() {
                unsafe {
                    (*stat_buf).st_mode = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    (*stat_buf).st_size = u64::from_le_bytes([
                        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
                    ]);
                    (*stat_buf).st_atime = 0;
                    (*stat_buf).st_mtime = 0;
                    (*stat_buf).st_ctime = 0;
                }
            }
            0
        }
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_stat(path: *const u8, stat_buf: *mut SloposStat) -> i32 {
    let mut raw = [0u8; 256];
    match Sys::stat(path, raw.as_mut_ptr()) {
        Ok(()) => {
            if !stat_buf.is_null() {
                unsafe {
                    (*stat_buf).st_mode = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                    (*stat_buf).st_size = u64::from_le_bytes([
                        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
                    ]);
                    (*stat_buf).st_atime = 0;
                    (*stat_buf).st_mtime = 0;
                    (*stat_buf).st_ctime = 0;
                }
            }
            0
        }
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_mkdir(path: *const u8, mode: u32) -> i32 {
    match Sys::mkdir(path, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_unlink(path: *const u8) -> i32 {
    match Sys::unlink(path) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_rename(old: *const u8, new: *const u8) -> i32 {
    match Sys::rename(old, new) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_dup(fd: i32) -> i32 {
    match Sys::dup(fd) {
        Ok(new_fd) => new_fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_dup2(old: i32, new: i32) -> i32 {
    match Sys::dup2(old, new) {
        Ok(fd) => fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_list(path: *const u8, buf: *mut u8, buf_len: usize) -> isize {
    match Sys::list(path, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_pipe(fds: *mut i32) -> i32 {
    let fds_arr = unsafe { &mut *(fds as *mut [i32; 2]) };
    match Sys::pipe(fds_arr) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_kill(pid: i32, sig: i32) -> i32 {
    match Sys::kill(pid, sig) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_futex_wait(
    addr: *const u32,
    expected: u32,
    timeout_ms: u64,
) -> i32 {
    match Sys::futex_wait(addr, expected, timeout_ms) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_futex_wake(addr: *const u32, count: u32) -> i32 {
    match Sys::futex_wake(addr, count) {
        Ok(n) => n,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_clock_gettime(clk_id: u64, sec: *mut i64, nsec: *mut i64) -> i32 {
    let mut raw = [0u8; 16];
    match Sys::clock_gettime(clk_id, raw.as_mut_ptr()) {
        Ok(()) => {
            if !sec.is_null() {
                unsafe {
                    *sec = i64::from_le_bytes([
                        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
                    ]);
                }
            }
            if !nsec.is_null() {
                unsafe {
                    *nsec = i64::from_le_bytes([
                        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
                    ]);
                }
            }
            0
        }
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_get_time_ms() -> u64 {
    Sys::get_time_ms()
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_sleep_ms(ms: u64) {
    Sys::sleep_ms(ms);
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_yield() {
    Sys::yield_now();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_mmap(
    addr: *mut u8,
    len: usize,
    prot: u64,
    flags: u64,
    fd: i32,
    offset: u64,
) -> *mut u8 {
    match Sys::mmap(addr, len, prot, flags, fd, offset) {
        Ok(ptr) => ptr,
        Err(_) => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_munmap(addr: *mut u8, len: usize) -> i32 {
    match Sys::munmap(addr, len) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}
