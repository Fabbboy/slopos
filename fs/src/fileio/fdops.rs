use super::open_file_table::{
    alloc_open_file_entry, get_open_file_mut, incref_open_file, open_file_kind, release_open_file,
};
use super::*;

use slopos_abi::Errno;
use slopos_abi::fs::UserFsEntry;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_CLOEXEC, SEEK_CUR, SEEK_END,
    SEEK_SET,
};

use crate::pipe;
use crate::pipe_file_ops::{PIPE_READ_OPS, PIPE_WRITE_OPS};
use crate::vfs::{vfs_list, vfs_mkdir, vfs_stat, vfs_unlink};
use crate::vfs_file_ops::{VFS_FILE_OPS, vfs_open_handle_flags};

#[allow(non_camel_case_types)]
type ssize_t = isize;

fn pick_table_ptr(
    kernel: &mut FileTableSlot,
    processes: &mut [FileTableSlot; MAX_PROCESSES],
    process_id: u32,
) -> Option<*mut FileTableSlot> {
    let kernel_ptr = kernel as *mut FileTableSlot;
    let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
        t as *mut FileTableSlot
    } else if let Some(t) = find_free_table(processes) {
        t as *mut FileTableSlot
    } else {
        kernel_ptr
    };
    let table = unsafe { &mut *table_ptr };
    if !table.in_use {
        table.in_use = true;
        table.process_id = process_id;
        reset_table(table);
    }
    Some(table_ptr)
}

fn install_fd_entry(
    process_id: u32,
    ops: &'static dyn FileOps,
    handle: usize,
    mut flags: u32,
    call_tty_policy: Option<TtyIndex>,
) -> c_int {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table_ptr) = pick_table_ptr(kernel, processes, process_id) else {
            return Errno::ESRCH.raw();
        };
        let table = unsafe { &mut *table_ptr };
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            ops.release(handle);
            return Errno::EMFILE.raw();
        };

        let mut position = 0u64;
        if (flags & FILE_OPEN_APPEND) != 0 {
            if let Some(size) = ops.size(handle) {
                position = size;
            } else {
                drop(guard);
                ops.release(handle);
                return Errno::ENXIO.raw();
            }
        }

        let Some(open_file_idx) = alloc_open_file_entry(open_files, ops, handle, flags, position)
        else {
            drop(guard);
            ops.release(handle);
            return Errno::ENFILE.raw() as _;
        };

        if ops.kind() == FileKind::Socket {
            let mode_bits = flags & (FILE_OPEN_READ | FILE_OPEN_WRITE);
            flags = mode_bits;
            if let Some(open_file) = get_open_file_mut(open_files, open_file_idx) {
                open_file.status_flags = flags;
                let _ = ops.set_status_flags(handle, flags);
            }
        }

        table.descriptors[slot_idx] = FdEntry {
            open_file_idx,
            cloexec: (flags & O_CLOEXEC as u32) != 0,
            valid: true,
        };

        drop(guard);

        if let Some(tty_idx) = call_tty_policy {
            maybe_acquire_controlling_tty_on_open(tty_idx, flags);
        }

        slot_idx as c_int
    })
}

fn current_tty_ops() -> &'static dyn FileOps {
    with_tables(|_, _, _, external_ops| effective_tty_ops(external_ops))
}

fn current_socket_ops() -> Option<&'static dyn FileOps> {
    with_tables(|_, _, _, external_ops| external_socket_ops(external_ops))
}

pub fn file_open_for_process(process_id: u32, path: *const c_char, posix_flags: u32) -> c_int {
    let flags = posix_to_internal_flags(posix_flags);
    if path.is_null() {
        return Errno::EFAULT.raw() as _;
    }
    if (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return Errno::EINVAL.raw() as _;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return Errno::EINVAL.raw() as _;
    }

    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return Errno::EINVAL.raw() as _,
    };

    if path_bytes == b"/dev/tty" {
        let tty_idx = match current_task_controlling_tty() {
            Some(idx) => idx,
            None => return Errno::ENXIO.raw(),
        };
        if tty::open_ref(tty_idx).is_err() {
            return Errno::EBUSY.raw() as _;
        }
        let tty_ops = current_tty_ops();
        return install_fd_entry(process_id, tty_ops, tty_idx.0 as usize, flags, None);
    }

    if path_bytes == b"/dev/ptmx" {
        let master_idx = match tty::alloc_pty() {
            Ok(idx) => idx,
            Err(_) => return Errno::ENOMEM.raw() as _,
        };
        if tty::open_ref(master_idx).is_err() {
            return Errno::EBUSY.raw() as _;
        }
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            process_id,
            tty_ops,
            master_idx.0 as usize,
            flags | O_NOCTTY as u32,
            None,
        );
    }

    if let Some(slave_idx) = parse_pts_path(path_bytes) {
        if tty::open_pty_slave(slave_idx).is_err() {
            return Errno::EBUSY.raw() as _;
        }
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            process_id,
            tty_ops,
            slave_idx.0 as usize,
            flags,
            Some(slave_idx),
        );
    }

    let create = (flags & FILE_OPEN_CREAT) != 0;
    let exclusive = (posix_flags & slopos_abi::fs::O_EXCL) != 0;
    let truncate = (posix_flags & slopos_abi::fs::O_TRUNC) != 0;
    let open_flags = crate::vfs::ops::VfsOpenFlags {
        create,
        exclusive,
        truncate,
    };
    let Some(vfs_handle) = vfs_open_handle_flags(path_bytes, open_flags) else {
        return Errno::ENOENT.raw() as _;
    };
    install_fd_entry(process_id, &VFS_FILE_OPS, vfs_handle, flags, None)
}

pub fn file_read_fd(process_id: u32, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    if buf.len() == 0 {
        return 0;
    }

    let (open_file_idx, ops, handle, flags, offset, seekable) =
        with_tables(|kernel, processes, open_files, _| {
            let table = table_for_pid(kernel, processes, process_id)?;
            if !table.in_use {
                return None;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let fd_entry = unsafe { get_fd_entry(&mut *table_ptr, fd) }?;
            let open_file_idx = fd_entry.open_file_idx;
            let open_file = get_open_file_mut(open_files, open_file_idx)?;
            if (open_file.status_flags & FILE_OPEN_READ) == 0 {
                drop(guard);
                return None;
            }
            let ops = open_file.ops?;
            let snapshot = (
                open_file_idx,
                ops,
                open_file.handle,
                open_file.status_flags,
                open_file.position,
                ops.seekable(),
            );
            drop(guard);
            Some(snapshot)
        })
        .unwrap_or((u16::MAX, &VFS_FILE_OPS as &dyn FileOps, 0, 0, 0, false));

    if open_file_idx == u16::MAX {
        return Errno::EBADF.raw() as _;
    }

    let used_offset = if seekable { offset } else { 0 };
    let rc = ops.read(handle, buf, used_offset, flags);
    if rc > 0 && seekable {
        with_tables(|_, _, open_files, _| {
            if let Some(open_file) = get_open_file_mut(open_files, open_file_idx)
                && open_file.seekable_position_matches(ops, handle)
            {
                open_file.position = open_file.position.saturating_add(rc as u64);
            }
        });
    }
    rc
}

pub fn file_write_fd(process_id: u32, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    if buf.len() == 0 {
        return 0;
    }

    let (open_file_idx, ops, handle, flags, offset, seekable) =
        with_tables(|kernel, processes, open_files, _| {
            let table = table_for_pid(kernel, processes, process_id)?;
            if !table.in_use {
                return None;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let fd_entry = unsafe { get_fd_entry(&mut *table_ptr, fd) }?;
            let open_file_idx = fd_entry.open_file_idx;
            let open_file = get_open_file_mut(open_files, open_file_idx)?;
            if (open_file.status_flags & FILE_OPEN_WRITE) == 0 {
                drop(guard);
                return None;
            }
            let ops = open_file.ops?;
            let snapshot = (
                open_file_idx,
                ops,
                open_file.handle,
                open_file.status_flags,
                open_file.position,
                ops.seekable(),
            );
            drop(guard);
            Some(snapshot)
        })
        .unwrap_or((u16::MAX, &VFS_FILE_OPS as &dyn FileOps, 0, 0, 0, false));

    if open_file_idx == u16::MAX {
        return Errno::EBADF.raw() as _;
    }

    let used_offset = if seekable { offset } else { 0 };
    let rc = ops.write(handle, buf, used_offset, flags);
    if rc > 0 && seekable {
        with_tables(|_, _, open_files, _| {
            if let Some(open_file) = get_open_file_mut(open_files, open_file_idx)
                && open_file.seekable_position_matches(ops, handle)
            {
                open_file.position = open_file.position.saturating_add(rc as u64);
            }
        });
    }
    rc
}

trait OpenFileEntryGuard {
    fn seekable_position_matches(&self, ops: &'static dyn FileOps, handle: usize) -> bool;
}

impl OpenFileEntryGuard for OpenFileEntry {
    fn seekable_position_matches(&self, ops: &'static dyn FileOps, handle: usize) -> bool {
        self.valid
            && self.ops.map(core::ptr::from_ref) == Some(core::ptr::from_ref(ops))
            && self.handle == handle
    }
}

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as _;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
            drop(guard);
            return Errno::EBADF.raw() as _;
        };
        release_open_file(open_files, fd_entry.open_file_idx);
        reset_fd_entry(fd_entry);
        drop(guard);
        0
    })
}

pub fn file_seek_fd(process_id: u32, fd: c_int, offset: i64, whence: u32) -> i64 {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as i64;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as i64;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
            drop(guard);
            return Errno::EBADF.raw() as i64;
        };
        let Some(open_file) = get_open_file_mut(open_files, fd_entry.open_file_idx) else {
            drop(guard);
            return Errno::EBADF.raw() as i64;
        };
        let Some(ops) = open_file.ops else {
            drop(guard);
            return Errno::EBADF.raw() as i64;
        };
        if !ops.seekable() {
            drop(guard);
            return Errno::ESPIPE.raw() as i64;
        }

        let size = match ops.size(open_file.handle) {
            Some(v) => v as i64,
            None => {
                drop(guard);
                return Errno::EBADF.raw() as i64;
            }
        };

        let new_pos = match whence as u64 {
            SEEK_SET => offset,
            SEEK_CUR => (open_file.position as i64).saturating_add(offset),
            SEEK_END => size.saturating_add(offset),
            _ => {
                drop(guard);
                return Errno::EINVAL.raw() as i64;
            }
        };
        if new_pos < 0 {
            drop(guard);
            return Errno::EINVAL.raw() as i64;
        }

        open_file.position = new_pos as u64;
        drop(guard);
        new_pos
    })
}

pub fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return usize::MAX;
        };
        if !table.in_use {
            return usize::MAX;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let size = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
            .and_then(|open_file| open_file.ops.and_then(|ops| ops.size(open_file.handle)))
            .map(|v| v as usize)
            .unwrap_or(usize::MAX);
        drop(guard);
        size
    })
}

pub fn file_exists_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return 0,
    };
    let rc = vfs_stat(path_bytes);
    if let Ok((kind, _)) = rc {
        return if kind == FS_TYPE_FILE { 1 } else { 0 };
    }
    0
}

pub fn file_unlink_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return Errno::EFAULT.raw() as _;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return Errno::EINVAL.raw() as _,
    };
    if vfs_unlink(path_bytes).is_ok() {
        0
    } else {
        Errno::ENOENT.raw() as _
    }
}

pub fn file_mkdir_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return Errno::EFAULT.raw() as _;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return Errno::EINVAL.raw() as _,
    };
    if vfs_mkdir(path_bytes).is_ok() {
        0
    } else {
        Errno::EEXIST.raw() as _
    }
}

pub fn file_stat_path(path: *const c_char, out_type: &mut u8, out_size: &mut u32) -> c_int {
    if path.is_null() {
        return Errno::EFAULT.raw() as _;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return Errno::EINVAL.raw() as _,
    };
    if let Ok((kind, size)) = vfs_stat(path_bytes) {
        *out_type = kind;
        *out_size = size;
        return 0;
    }
    Errno::ENOENT.raw() as _
}

pub fn file_list_path(
    path: *const c_char,
    entries: *mut UserFsEntry,
    max: u32,
    out_count: &mut u32,
) -> c_int {
    if path.is_null() || entries.is_null() || max == 0 {
        return Errno::EINVAL.raw() as _;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return Errno::EINVAL.raw() as _,
    };
    let cap = max as usize;
    let out_slice = unsafe { slice::from_raw_parts_mut(entries, cap) };
    match vfs_list(path_bytes, out_slice) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => Errno::ENOENT.raw() as _,
    }
}

pub fn file_is_console_fd(process_id: u32, fd: c_int) -> bool {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return false;
        };
        if !table.in_use {
            return false;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let is_console = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| open_file_kind(open_files, fd_entry.open_file_idx))
            .map(kind_is_tty)
            .unwrap_or(false);
        drop(guard);
        is_console
    })
}

pub fn file_get_tty_index(process_id: u32, fd: c_int) -> Option<TtyIndex> {
    with_tables(|kernel, processes, open_files, _| {
        let table = table_for_pid(kernel, processes, process_id)?;
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let out = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
            .and_then(|open_file| {
                if open_file.ops?.kind() == FileKind::Tty {
                    Some(TtyIndex(open_file.handle as u8))
                } else {
                    None
                }
            });
        drop(guard);
        out
    })
}

pub fn file_open_tty_fd(process_id: u32, tty_idx: TtyIndex, flags: u32) -> c_int {
    let tty_ops = current_tty_ops();
    install_fd_entry(
        process_id,
        tty_ops,
        tty_idx.0 as usize,
        flags,
        Some(tty_idx),
    )
}

pub fn file_pipe_create(
    process_id: u32,
    flags: u32,
    out_read_fd: &mut c_int,
    out_write_fd: &mut c_int,
) -> c_int {
    if flags & !(O_NONBLOCK as u32 | O_CLOEXEC as u32) != 0 {
        return Errno::EINVAL.raw() as _;
    }

    let pipe_id = match pipe::alloc_slot() {
        Some(id) => id,
        None => return Errno::ENOMEM.raw() as _,
    };

    let rc = with_tables(|kernel, processes, open_files, _| {
        let Some(table_ptr) = pick_table_ptr(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };

        let table = unsafe { &mut *table_ptr };
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(read_idx) = find_free_slot(table) else {
            drop(guard);
            return Errno::EMFILE.raw() as _;
        };
        table.descriptors[read_idx].valid = true;

        let Some(write_idx) = find_free_slot(table) else {
            reset_fd_entry(&mut table.descriptors[read_idx]);
            drop(guard);
            return Errno::EMFILE.raw() as _;
        };

        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let cloexec = (flags & O_CLOEXEC as u32) != 0;
        let read_flags = FILE_OPEN_READ | if nonblock { O_NONBLOCK as u32 } else { 0 };
        let write_flags = FILE_OPEN_WRITE | if nonblock { O_NONBLOCK as u32 } else { 0 };

        let Some(read_open_idx) =
            alloc_open_file_entry(open_files, &PIPE_READ_OPS, pipe_id as usize, read_flags, 0)
        else {
            reset_fd_entry(&mut table.descriptors[read_idx]);
            drop(guard);
            return Errno::ENFILE.raw() as _;
        };
        let Some(write_open_idx) = alloc_open_file_entry(
            open_files,
            &PIPE_WRITE_OPS,
            pipe_id as usize,
            write_flags,
            0,
        ) else {
            release_open_file(open_files, read_open_idx);
            reset_fd_entry(&mut table.descriptors[read_idx]);
            drop(guard);
            return Errno::ENFILE.raw() as _;
        };

        {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
                release_open_file(open_files, read_open_idx);
                release_open_file(open_files, write_open_idx);
                reset_fd_entry(&mut table.descriptors[read_idx]);
                drop(guard);
                return Errno::ENOMEM.raw() as _;
            };
            slot.readers = 1;
            slot.writers = 1;
        }

        table.descriptors[read_idx] = FdEntry {
            open_file_idx: read_open_idx,
            cloexec,
            valid: true,
        };
        table.descriptors[write_idx] = FdEntry {
            open_file_idx: write_open_idx,
            cloexec,
            valid: true,
        };

        *out_read_fd = read_idx as c_int;
        *out_write_fd = write_idx as c_int;
        drop(guard);
        0
    });

    if rc != 0 {
        let mut pipe_state = pipe::PIPE_STATE.lock();
        if let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) {
            *slot = pipe::PipeSlot::new();
        }
    }

    rc
}

pub fn file_dup_fd(process_id: u32, old_fd: c_int) -> c_int {
    file_dup_fd_min(process_id, old_fd, 0)
}

fn file_dup_fd_min(process_id: u32, old_fd: c_int, min_fd: usize) -> c_int {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as _;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(src) = (unsafe { get_fd_entry(&mut *table_ptr, old_fd) }) else {
            drop(guard);
            return Errno::EBADF.raw() as _;
        };
        if !incref_open_file(open_files, src.open_file_idx) {
            drop(guard);
            return Errno::EBADF.raw() as _;
        }

        let table = unsafe { &mut *table_ptr };
        let Some(new_idx) = find_free_slot_from(table, min_fd) else {
            release_open_file(open_files, src.open_file_idx);
            drop(guard);
            return Errno::EMFILE.raw() as _;
        };

        table.descriptors[new_idx] = FdEntry {
            open_file_idx: src.open_file_idx,
            cloexec: false,
            valid: true,
        };
        drop(guard);
        new_idx as c_int
    })
}

pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    if old_fd == new_fd {
        return with_tables(|kernel, processes, _, _| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return Errno::ESRCH.raw() as _;
            };
            if !table.in_use {
                return Errno::EBADF.raw() as _;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let valid = unsafe { get_fd_entry(&mut *table_ptr, old_fd) }.is_some();
            drop(guard);
            if valid {
                new_fd
            } else {
                Errno::EBADF.raw() as _
            }
        });
    }

    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as _;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(src) = (unsafe { get_fd_entry(&mut *table_ptr, old_fd) }) else {
            drop(guard);
            return Errno::EBADF.raw() as _;
        };
        if !incref_open_file(open_files, src.open_file_idx) {
            drop(guard);
            return Errno::EBADF.raw() as _;
        }

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            release_open_file(open_files, table.descriptors[new_fd as usize].open_file_idx);
        }
        table.descriptors[new_fd as usize] = FdEntry {
            open_file_idx: src.open_file_idx,
            cloexec: false,
            valid: true,
        };
        drop(guard);
        new_fd
    })
}

pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return Errno::EINVAL.raw() as _;
    }
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }

    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as _;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(src) = (unsafe { get_fd_entry(&mut *table_ptr, old_fd) }) else {
            drop(guard);
            return Errno::EBADF.raw() as _;
        };
        if !incref_open_file(open_files, src.open_file_idx) {
            drop(guard);
            return Errno::EBADF.raw() as _;
        }

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            release_open_file(open_files, table.descriptors[new_fd as usize].open_file_idx);
        }
        table.descriptors[new_fd as usize] = FdEntry {
            open_file_idx: src.open_file_idx,
            cloexec: (flags & FD_CLOEXEC as u32) != 0,
            valid: true,
        };
        drop(guard);
        new_fd
    })
}

pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => with_tables(|kernel, processes, _, _| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if !table.in_use {
                return Errno::EBADF.raw() as i64;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
                drop(guard);
                return Errno::EBADF.raw() as i64;
            };
            let val = if desc.cloexec { FD_CLOEXEC as i64 } else { 0 };
            drop(guard);
            val
        }),
        F_SETFD => with_tables(|kernel, processes, _, _| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if !table.in_use {
                return Errno::EBADF.raw() as i64;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
                drop(guard);
                return Errno::EBADF.raw() as i64;
            };
            desc.cloexec = (arg & FD_CLOEXEC) != 0;
            drop(guard);
            0
        }),
        F_GETFL => with_tables(|kernel, processes, open_files, _| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if !table.in_use {
                return Errno::EBADF.raw() as i64;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let val = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
                .and_then(|f| get_open_file_mut(open_files, f.open_file_idx))
                .map(|o| o.status_flags as i64)
                .unwrap_or(Errno::EBADF.raw() as i64);
            drop(guard);
            val
        }),
        F_SETFL => with_tables(|kernel, processes, open_files, _| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if !table.in_use {
                return Errno::EBADF.raw() as i64;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
                drop(guard);
                return Errno::EBADF.raw() as i64;
            };
            let Some(open_file) = get_open_file_mut(open_files, fd_entry.open_file_idx) else {
                drop(guard);
                return Errno::EBADF.raw() as i64;
            };
            let mode_bits = open_file.status_flags & (FILE_OPEN_READ | FILE_OPEN_WRITE);
            let sticky_flags = open_file.status_flags & (O_NOCTTY as u32);
            let mut next_flags = mode_bits | sticky_flags | (arg as u32 & FILE_OPEN_APPEND);
            if (arg & O_NONBLOCK) != 0 {
                next_flags |= O_NONBLOCK as u32;
            }
            open_file.status_flags = next_flags;
            if let Some(ops) = open_file.ops {
                let _ = ops.set_status_flags(open_file.handle, next_flags);
            }
            drop(guard);
            0
        }),
        _ => Errno::EINVAL.raw() as i64,
    }
}

pub fn file_fstat_fd(
    process_id: u32,
    fd: c_int,
    out_stat: &mut slopos_abi::fs::UserFsStat,
) -> c_int {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        if !table.in_use {
            return Errno::EBADF.raw() as _;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let rc = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
            .and_then(|open_file| {
                let ops = open_file.ops?;
                Some(ops.stat(open_file.handle, out_stat))
            })
            .unwrap_or(Errno::EBADF.raw() as _);
        drop(guard);
        rc
    })
}

pub fn fileio_open_socket_fd(process_id: u32, socket_idx: u32) -> i32 {
    let Some(socket_ops) = current_socket_ops() else {
        return Errno::ENOTSOCK.raw() as _;
    };
    install_fd_entry(
        process_id,
        socket_ops,
        socket_idx as usize,
        FILE_OPEN_READ | FILE_OPEN_WRITE,
        None,
    )
}

pub fn fileio_get_open_file_handle(process_id: u32, fd: i32) -> Option<(FileKind, usize)> {
    with_tables(|kernel, processes, open_files, _| {
        let table = table_for_pid(kernel, processes, process_id)?;
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let out = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
            .and_then(|open_file| Some((open_file.ops?.kind(), open_file.handle)));
        drop(guard);
        out
    })
}
