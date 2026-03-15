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
                let kernel_type = raw[0];
                let posix_mode = match kernel_type {
                    1 => 0o040755u32,
                    _ => 0o100644u32,
                };
                let size = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as u64;
                unsafe {
                    (*stat_buf).st_mode = posix_mode;
                    (*stat_buf).st_size = size;
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
                let kernel_type = raw[0];
                let posix_mode = match kernel_type {
                    1 => 0o040755u32,
                    _ => 0o100644u32,
                };
                let size = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as u64;
                unsafe {
                    (*stat_buf).st_mode = posix_mode;
                    (*stat_buf).st_size = size;
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
    const MAX_ENTRIES: usize = 64;

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct Entry {
        name: [u8; 64],
        type_: u8,
        size: u32,
    }

    #[repr(C)]
    struct ListHdr {
        entries: *mut Entry,
        max_entries: u32,
        count: u32,
    }

    let mut entries = [unsafe { core::mem::zeroed::<Entry>() }; MAX_ENTRIES];
    let mut hdr = ListHdr {
        entries: entries.as_mut_ptr(),
        max_entries: MAX_ENTRIES as u32,
        count: 0,
    };

    let ret = unsafe {
        crate::pal::raw::syscall2(
            slopos_abi::syscall::SYSCALL_FS_LIST,
            path as u64,
            &mut hdr as *mut ListHdr as u64,
        )
    };

    let signed = ret as i64;
    if signed < 0 {
        return signed as isize;
    }

    let count = hdr.count as usize;
    let mut pos = 0usize;
    for i in 0..count {
        let entry = &entries[i];
        let name_len = entry
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(entry.name.len());
        if name_len == 0 {
            continue;
        }
        let needed = if pos == 0 { name_len } else { name_len + 1 };
        if pos + needed > buf_len {
            return -(34i64) as isize;
        }
        if pos > 0 {
            unsafe { *buf.add(pos) = b'\n' };
            pos += 1;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(entry.name.as_ptr(), buf.add(pos), name_len);
        }
        pos += name_len;
    }

    pos as isize
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
