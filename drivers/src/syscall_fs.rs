#![allow(clippy::too_many_arguments)]

use core::ffi::{CStr, c_char, c_int, c_void};
use core::{mem, ptr};

use crate::syscall_common::{
    SyscallDisposition, USER_IO_MAX_BYTES, USER_PATH_MAX, syscall_bounded_from_user,
    syscall_copy_to_user_bounded, syscall_copy_user_str, syscall_return_err, syscall_return_ok,
};
use crate::syscall_types::{INVALID_PROCESS_ID, InterruptFrame, Task};
const USER_FS_MAX_ENTRIES: u32 = 64;
type RamfsNode = ramfs_node_t;

#[repr(C)]
pub struct UserFsEntry {
    pub name: [c_char; 64],
    pub type_: u8,
    pub size: u32,
}

#[repr(C)]
pub struct UserFsStat {
    pub type_: u8,
    pub size: u32,
}

#[repr(C)]
pub struct UserFsList {
    pub entries: *mut UserFsEntry,
    pub max_entries: u32,
    pub count: u32,
}

use slopos_fs::fileio::{
    file_close_fd, file_open_for_process, file_read_fd, file_unlink_path, file_write_fd,
};
use slopos_fs::ramfs::{
    RAMFS_TYPE_DIRECTORY, RAMFS_TYPE_FILE, ramfs_acquire_node, ramfs_create_directory,
    ramfs_get_size, ramfs_list_directory, ramfs_node_release, ramfs_node_t, ramfs_release_list,
};

use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::user_copy::{user_copy_from_user, user_copy_to_user};

fn syscall_fs_error(frame: *mut InterruptFrame) -> SyscallDisposition {
    syscall_return_err(frame, u64::MAX)
}

pub fn syscall_fs_open(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID {
            return syscall_fs_error(frame);
        }
    }

    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe {
        (*frame).rdi as *const c_char
    }) != 0
    {
        return syscall_fs_error(frame);
    }

    let flags = unsafe { (*frame).rsi as u32 };
    let pid = unsafe { (*task).process_id };
    let fd = file_open_for_process(pid, path.as_ptr(), flags);
    if fd < 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, fd as u64)
}

pub fn syscall_fs_close(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID {
            return syscall_fs_error(frame);
        }
    }
    let pid = unsafe { (*task).process_id };
    let fd = unsafe { (*frame).rdi as c_int };
    let rc = file_close_fd(pid, fd);
    if rc != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_read(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        if task.is_null() || (*task).process_id == INVALID_PROCESS_ID || (*frame).rsi == 0 {
            return syscall_fs_error(frame);
        }
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let request_len = unsafe { (*frame).rdx as usize };
    let capped_len = request_len.min(USER_IO_MAX_BYTES);

    let bytes = file_read_fd(
        unsafe { (*task).process_id },
        unsafe { (*frame).rdi as c_int },
        tmp.as_mut_ptr() as *mut c_char,
        capped_len,
    );
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

pub fn syscall_fs_write(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
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

    let bytes = file_write_fd(
        unsafe { (*task).process_id },
        unsafe { (*frame).rdi as c_int },
        tmp.as_ptr() as *const c_char,
        write_len,
    );
    if bytes < 0 {
        return syscall_fs_error(frame);
    }

    syscall_return_ok(frame, bytes as u64)
}

pub fn syscall_fs_stat(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        if task.is_null() || (*frame).rdi == 0 || (*frame).rsi == 0 {
            return syscall_fs_error(frame);
        }
    }

    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe {
        (*frame).rdi as *const c_char
    }) != 0
    {
        return syscall_fs_error(frame);
    }

    let node = ramfs_acquire_node(path.as_ptr());
    if node.is_null() {
        return syscall_fs_error(frame);
    }

    let mut stat = UserFsStat { type_: 0, size: 0 };
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
        mem::size_of::<UserFsStat>(),
    ) != 0
    {
        return syscall_fs_error(frame);
    }

    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_mkdir(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe {
        (*frame).rdi as *const c_char
    }) != 0
    {
        return syscall_fs_error(frame);
    }

    let created = ramfs_create_directory(path.as_ptr());
    if !created.is_null() {
        return syscall_return_ok(frame, 0);
    }
    syscall_fs_error(frame)
}

pub fn syscall_fs_unlink(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str(path.as_mut_ptr(), path.len(), unsafe {
        (*frame).rdi as *const c_char
    }) != 0
    {
        return syscall_fs_error(frame);
    }

    if file_unlink_path(path.as_ptr()) != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}

pub fn syscall_fs_list(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let _ = task;
    let mut path = [0i8; USER_PATH_MAX];
    unsafe {
        if syscall_copy_user_str(path.as_mut_ptr(), path.len(), (*frame).rdi as *const c_char) != 0
            || (*frame).rsi == 0
        {
            return syscall_fs_error(frame);
        }
    }

    let mut list_hdr = UserFsList {
        entries: ptr::null_mut(),
        max_entries: 0,
        count: 0,
    };

    unsafe {
        if user_copy_from_user(
            &mut list_hdr as *mut _ as *mut c_void,
            (*frame).rsi as *const c_void,
            mem::size_of::<UserFsList>(),
        ) != 0
        {
            return syscall_fs_error(frame);
        }
    }

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return syscall_fs_error(frame);
    }

    let mut entries: *mut *mut RamfsNode = ptr::null_mut();
    let mut count: c_int = 0;
    let rc = ramfs_list_directory(path.as_ptr(), &mut entries, &mut count);
    if rc != 0 {
        return syscall_fs_error(frame);
    }

    if count < 0 {
        count = 0;
    }
    if (count as u32) > cap {
        count = cap as c_int;
    }

    let tmp_size = mem::size_of::<UserFsEntry>() * cap as usize;
    let tmp_ptr = kmalloc(tmp_size) as *mut UserFsEntry;
    if tmp_ptr.is_null() {
        if !entries.is_null() {
            ramfs_release_list(entries, count);
            kfree(entries as *mut c_void);
        }
        return syscall_fs_error(frame);
    }
    unsafe {
        core::ptr::write_bytes(tmp_ptr as *mut u8, 0, tmp_size);
    }

    for i in 0..(count as usize) {
        unsafe {
            let entry_ptr = *entries.add(i);
            let dst = &mut *tmp_ptr.add(i);
            if entry_ptr.is_null() {
                dst.type_ = 0;
                dst.size = 0;
                continue;
            }

            let cstr = CStr::from_ptr((*entry_ptr).name);
            let name_bytes = cstr.to_bytes();
            let nlen = name_bytes.len().min(dst.name.len());
            for (dst_byte, src_byte) in dst.name[..nlen].iter_mut().zip(name_bytes[..nlen].iter()) {
                *dst_byte = *src_byte as i8;
            }
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

    let mut rc_user = user_copy_to_user(
        list_hdr.entries as *mut c_void,
        tmp_ptr as *const c_void,
        mem::size_of::<UserFsEntry>() * count as usize,
    );
    if rc_user == 0 {
        rc_user = unsafe {
            user_copy_to_user(
                (*frame).rsi as *mut c_void,
                &list_hdr as *const _ as *const c_void,
                mem::size_of::<UserFsList>(),
            )
        };
    }

    if !entries.is_null() {
        ramfs_release_list(entries, count);
        kfree(entries as *mut c_void);
    }
    kfree(tmp_ptr as *mut c_void);

    if rc_user != 0 {
        return syscall_fs_error(frame);
    }
    syscall_return_ok(frame, 0)
}
