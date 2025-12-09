#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(clippy::too_many_arguments)]

use core::ffi::{c_char, c_int, c_void};
use core::{mem, ptr, slice};

use crate::syscall_common::{
    syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_copy_user_str,
    syscall_disposition, syscall_return_err, syscall_return_ok, USER_IO_MAX_BYTES, USER_PATH_MAX,
};
use crate::syscall_types::{task_t, InterruptFrame, INVALID_PROCESS_ID};
use crate::wl_currency;

const USER_FS_MAX_ENTRIES: u32 = 64;

const RAMFS_TYPE_FILE: i32 = 0;
const RAMFS_TYPE_DIRECTORY: i32 = 1;

#[repr(C)]
pub struct ramfs_node_t {
    pub name: [c_char; 64],
    pub size: u64,
    pub type_: i32,
}

#[repr(C)]
pub struct user_fs_entry_t {
    pub name: [c_char; 64],
    pub type_: u8,
    pub size: u32,
}

#[repr(C)]
pub struct user_fs_stat_t {
    pub type_: u8,
    pub size: u32,
}

#[repr(C)]
pub struct user_fs_list_t {
    pub entries: *mut user_fs_entry_t,
    pub max_entries: u32,
    pub count: u32,
}

extern "C" {
    fn file_open_for_process(process_id: u32, path: *const c_char, flags: u32) -> c_int;
    fn file_close_fd(process_id: u32, fd: c_int) -> c_int;
    fn file_read_fd(process_id: u32, fd: c_int, buf: *mut c_char, len: usize) -> isize;
    fn file_write_fd(process_id: u32, fd: c_int, buf: *const c_char, len: usize) -> isize;
    fn file_unlink_path(path: *const c_char) -> c_int;

    fn ramfs_acquire_node(path: *const c_char) -> *mut ramfs_node_t;
    fn ramfs_get_size(node: *const ramfs_node_t) -> u64;
    fn ramfs_node_release(node: *mut ramfs_node_t);
    fn ramfs_list_directory(
        path: *const c_char,
        entries: *mut *mut *mut ramfs_node_t,
        count: *mut c_int,
    ) -> c_int;
    fn ramfs_release_list(entries: *mut *mut ramfs_node_t, count: c_int);
    fn ramfs_create_directory(path: *const c_char) -> c_int;

    fn kmalloc(size: usize) -> *mut c_void;
    fn kfree(ptr: *mut c_void);
    fn user_copy_from_user(dst: *mut c_void, src: *const c_void, len: usize) -> c_int;
    fn user_copy_to_user(dst: *mut c_void, src: *const c_void, len: usize) -> c_int;
}

fn syscall_fs_error(frame: *mut InterruptFrame) -> syscall_disposition {
    syscall_return_err(frame, u64::MAX)
}

pub fn syscall_fs_open(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID {
            return syscall_fs_error(frame);
        }
    }

    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe { (*frame).rdi as *const c_char })
        != 0
    {
        return syscall_fs_error(frame);
    }

    let flags = unsafe { (*frame).rsi as u32 };
    let fd = unsafe { file_open_for_process((*task).process_id, path.as_ptr(), flags) };
    if fd < 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, fd as u64)
}

pub fn syscall_fs_close(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID {
            return syscall_fs_error(frame);
        }
    }
    let rc = unsafe { file_close_fd((*task).process_id, (*frame).rdi as c_int) };
    if rc != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_read(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID || (*frame).rsi == 0 {
            return syscall_fs_error(frame);
        }
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let request_len = unsafe { (*frame).rdx as usize };
    let capped_len = request_len.min(USER_IO_MAX_BYTES);

    let bytes = unsafe {
        file_read_fd(
            (*task).process_id,
            (*frame).rdi as c_int,
            tmp.as_mut_ptr() as *mut c_char,
            capped_len,
        )
    };
    if bytes < 0 {
        return syscall_fs_error(frame);
    }

    let copy_len = bytes as usize;
    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rsi as *mut c_void },
        tmp.as_ptr() as *const c_void,
        copy_len,
    ) != 0
    {
        return syscall_fs_error(frame);
    }

    syscall_return_ok(frame, bytes as u64)
}

pub fn syscall_fs_write(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID || (*frame).rsi == 0 {
            return syscall_fs_error(frame);
        }
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let mut write_len: usize = 0;
    if syscall_bounded_from_user(
        tmp.as_mut_ptr() as *mut c_void,
        tmp.len(),
        unsafe { (*frame).rsi as *const c_void },
        unsafe { (*frame).rdx },
        USER_IO_MAX_BYTES,
        &mut write_len as *mut usize,
    ) != 0
    {
        return syscall_fs_error(frame);
    }

    let bytes = unsafe {
        file_write_fd(
            (*task).process_id,
            (*frame).rdi as c_int,
            tmp.as_ptr() as *const c_char,
            write_len,
        )
    };
    if bytes < 0 {
        return syscall_fs_error(frame);
    }

    syscall_return_ok(frame, bytes as u64)
}

pub fn syscall_fs_stat(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if task.is_null() || (*frame).rdi == 0 || (*frame).rsi == 0 {
            return syscall_fs_error(frame);
        }
    }

    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe { (*frame).rdi as *const c_char })
        != 0
    {
        return syscall_fs_error(frame);
    }

    let node = unsafe { ramfs_acquire_node(path.as_ptr()) };
    if node.is_null() {
        return syscall_fs_error(frame);
    }

    let mut stat = user_fs_stat_t { type_: 0, size: 0 };
    unsafe {
        stat.size = ramfs_get_size(node) as u32;
        let kind = (*node).type_;
        stat.type_ = if kind == RAMFS_TYPE_DIRECTORY {
            1
        } else if kind == RAMFS_TYPE_FILE {
            0
        } else {
            0xFF
        };
        ramfs_node_release(node);
    }

    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rsi as *mut c_void },
        &stat as *const _ as *const c_void,
        mem::size_of::<user_fs_stat_t>(),
    ) != 0
    {
        return syscall_fs_error(frame);
    }

    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_mkdir(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe { (*frame).rdi as *const c_char })
        != 0
    {
        return syscall_fs_error(frame);
    }

    if unsafe { ramfs_create_directory(path.as_ptr()) } == 0 {
        return syscall_return_ok(frame, 0);
    }
    syscall_fs_error(frame)
}

pub fn syscall_fs_unlink(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe { (*frame).rdi as *const c_char })
        != 0
    {
        return syscall_fs_error(frame);
    }

    if unsafe { file_unlink_path(path.as_ptr()) } != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_list(task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    unsafe {
        if syscall_copy_user_str(path.as_mut_ptr(), path.len(), (*frame).rdi as *const c_char) != 0
            || (*frame).rsi == 0
        {
            return syscall_fs_error(frame);
        }
    }

    let mut list_hdr = user_fs_list_t {
        entries: ptr::null_mut(),
        max_entries: 0,
        count: 0,
    };

    unsafe {
        if user_copy_from_user(
            &mut list_hdr as *mut _ as *mut c_void,
            (*frame).rsi as *const c_void,
            mem::size_of::<user_fs_list_t>(),
        ) != 0
        {
            return syscall_fs_error(frame);
        }
    }

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return syscall_fs_error(frame);
    }

    let mut entries: *mut *mut ramfs_node_t = ptr::null_mut();
    let mut count: c_int = 0;
    let rc = unsafe { ramfs_list_directory(path.as_ptr(), &mut entries, &mut count) };
    if rc != 0 {
        return syscall_fs_error(frame);
    }

    if count < 0 {
        count = 0;
    }
    if (count as u32) > cap {
        count = cap as c_int;
    }

    let tmp_size = mem::size_of::<user_fs_entry_t>() * cap as usize;
    let tmp_ptr = unsafe { kmalloc(tmp_size) as *mut user_fs_entry_t };
    if tmp_ptr.is_null() {
        if !entries.is_null() {
            unsafe {
                ramfs_release_list(entries, count);
                kfree(entries as *mut c_void);
            }
        }
        return syscall_fs_error(frame);
    }

    for i in 0..(count as usize) {
        unsafe {
            let entry_ptr = *entries.add(i);
            let dst = &mut *tmp_ptr.add(i);
            if entry_ptr.is_null() {
                dst.name.fill(0);
                dst.type_ = 0;
                dst.size = 0;
                continue;
            }

            let name_src = &(*entry_ptr).name;
            let mut nlen = name_src.len();
            if nlen > dst.name.len() {
                nlen = dst.name.len();
            }
            ptr::copy_nonoverlapping(name_src.as_ptr(), dst.name.as_mut_ptr(), nlen);
            if nlen < dst.name.len() {
                dst.name[nlen] = 0;
            }
            dst.type_ = if (*entry_ptr).type_ == RAMFS_TYPE_DIRECTORY {
                1
            } else {
                0
            };
            dst.size = (*entry_ptr).size as u32;
        }
    }

    list_hdr.count = count as u32;

    let mut rc_user = unsafe {
        user_copy_to_user(
            list_hdr.entries as *mut c_void,
            tmp_ptr as *const c_void,
            mem::size_of::<user_fs_entry_t>() * count as usize,
        )
    };
    if rc_user == 0 {
        rc_user = unsafe {
            user_copy_to_user(
                (*frame).rsi as *mut c_void,
                &list_hdr as *const _ as *const c_void,
                mem::size_of::<user_fs_list_t>(),
            )
        };
    }

    unsafe {
        if !entries.is_null() {
            ramfs_release_list(entries, count);
            kfree(entries as *mut c_void);
        }
        kfree(tmp_ptr as *mut c_void);
    }

    if rc_user != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}

