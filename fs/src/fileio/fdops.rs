use core::ffi::c_int;

use super::open_file_table::{
    alloc_open_file_entry, get_open_file_mut, incref_open_file, release_open_file,
};
use super::*;

use slopos_abi::Errno;
use slopos_abi::fs::UserFsEntry;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_CLOEXEC, O_NOCTTY, O_NONBLOCK,
    SEEK_CUR, SEEK_END, SEEK_SET,
};

use crate::pipe;
use crate::pipe_file_ops::{PIPE_READ_OPS, PIPE_WRITE_OPS};
use crate::vfs::{vfs_list, vfs_mkdir, vfs_stat, vfs_unlink};
use crate::vfs_file_ops::{VFS_FILE_OPS, vfs_open_handle_flags};

#[allow(non_camel_case_types)]
type ssize_t = isize;

/// Install an FD entry in `process_id`'s table. Allocates an
/// `OpenFileEntry`, picks a free FD slot, and (if the file is a TTY) may
/// acquire a controlling terminal afterwards.
fn install_fd_entry(
    process_id: u32,
    ops: &'static dyn FileOps,
    handle: usize,
    mut flags: OpenMode,
    call_tty_policy: Option<TtyIndex>,
) -> c_int {
    let Some(mut inner) = pick_pid_slot_locked(process_id) else {
        return Errno::ESRCH.raw();
    };

    let result = with_open_files(|state| {
        let Some(slot_idx) = find_free_slot(&inner) else {
            return Err(Errno::EMFILE);
        };

        let mut position = 0u64;
        if flags.contains(OpenMode::APPEND) {
            if let Some(size) = ops.size(handle) {
                position = size;
            } else {
                return Err(Errno::ENXIO);
            }
        }

        let Some(open_file) =
            alloc_open_file_entry(&mut state.open_files, ops, handle, flags, position)
        else {
            return Err(Errno::ENFILE);
        };

        if ops.kind() == FileKind::Socket {
            let mode_bits = flags & (OpenMode::READ | OpenMode::WRITE);
            flags = mode_bits;
            if let Some(slot) = get_open_file_mut(&mut state.open_files, open_file) {
                slot.status_flags = flags;
                let _ = ops.set_status_flags(handle, flags.bits());
            }
        }

        inner.descriptors[slot_idx] = FdEntry {
            open_file,
            cloexec: (flags.bits() & O_CLOEXEC as u32) != 0,
            valid: true,
        };
        Ok(slot_idx as c_int)
    });

    drop(inner);

    match result {
        Ok(fd) => {
            if let Some(tty_idx) = call_tty_policy {
                maybe_acquire_controlling_tty_on_open(tty_idx, flags.bits());
            }
            fd
        }
        Err(e) => {
            ops.release(handle);
            e.raw()
        }
    }
}

fn current_tty_ops() -> &'static dyn FileOps {
    with_open_files(|state| effective_tty_ops(&state.external_ops))
}

fn current_socket_ops() -> Option<&'static dyn FileOps> {
    with_open_files(|state| external_socket_ops(&state.external_ops))
}

pub fn file_open_for_process(process_id: u32, path: &[u8], posix_flags: u32) -> c_int {
    let flags = posix_to_open_mode(posix_flags);
    if !flags.intersects(OpenMode::READ | OpenMode::WRITE) {
        return Errno::EINVAL.raw() as _;
    }
    if flags.contains(OpenMode::APPEND) && !flags.contains(OpenMode::WRITE) {
        return Errno::EINVAL.raw() as _;
    }

    if path == b"/dev/tty" {
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

    if path == b"/dev/ptmx" {
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
            flags.with_raw(O_NOCTTY as u32),
            None,
        );
    }

    if let Some(slave_idx) = parse_pts_path(path) {
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

    let create = flags.contains(OpenMode::CREAT);
    let exclusive = (posix_flags & slopos_abi::fs::O_EXCL) != 0;
    let truncate = (posix_flags & slopos_abi::fs::O_TRUNC) != 0;
    let writable = flags.contains(OpenMode::WRITE);
    let open_flags = crate::vfs::ops::VfsOpenFlags {
        create,
        exclusive,
        truncate,
        writable,
    };
    let vfs_handle = match vfs_open_handle_flags(path, open_flags) {
        Ok(h) => h,
        Err(e) => return e.raw() as _,
    };
    install_fd_entry(process_id, &VFS_FILE_OPS, vfs_handle, flags, None)
}

trait OpenFileGuard {
    fn seekable_position_matches(&self, ops: &'static dyn FileOps, handle: usize) -> bool;
}

impl OpenFileGuard for OpenFile {
    fn seekable_position_matches(&self, ops: &'static dyn FileOps, handle: usize) -> bool {
        self.ops.map(core::ptr::from_ref) == Some(core::ptr::from_ref(ops)) && self.handle == handle
    }
}

pub fn file_read_fd(process_id: u32, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) if s.status_flags.contains(OpenMode::READ) => s,
            _ => return Errno::EBADF.raw() as _,
        }
    };

    let Some(ops) = snap.ops else {
        return Errno::EBADF.raw() as _;
    };

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { snap.position } else { 0 };
    let rc = ops.read(snap.handle, buf, used_offset, snap.status_flags.bits());
    if rc > 0 && seekable {
        with_open_files(|state| {
            if let Some(open_file) = get_open_file_mut(&mut state.open_files, snap.open_file)
                && open_file.seekable_position_matches(ops, snap.handle)
            {
                open_file.position = open_file.position.saturating_add(rc as u64);
            }
        });
    }
    rc
}

pub fn file_write_fd(process_id: u32, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) if s.status_flags.contains(OpenMode::WRITE) => s,
            _ => return Errno::EBADF.raw() as _,
        }
    };

    let Some(ops) = snap.ops else {
        return Errno::EBADF.raw() as _;
    };

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { snap.position } else { 0 };
    let rc = ops.write(snap.handle, buf, used_offset, snap.status_flags.bits());
    if rc > 0 && seekable {
        with_open_files(|state| {
            if let Some(open_file) = get_open_file_mut(&mut state.open_files, snap.open_file)
                && open_file.seekable_position_matches(ops, snap.handle)
            {
                open_file.position = open_file.position.saturating_add(rc as u64);
            }
        });
    }
    rc
}

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    let result = with_pid_slot(process_id, |inner| {
        let Some(fd_entry) = get_fd_entry(inner, fd) else {
            return Errno::EBADF.raw() as _;
        };
        let ofi = fd_entry.open_file;
        with_open_files(|state| {
            release_open_file(&mut state.open_files, ofi);
        });
        if let Some(entry) = get_fd_entry(inner, fd) {
            reset_fd_entry(entry);
        }
        0
    });
    result.unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_seek_fd(process_id: u32, fd: c_int, offset: i64, whence: u32) -> i64 {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::ESRCH.raw() as i64;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as i64,
        }
    };

    let Some(ops) = snap.ops else {
        return Errno::EBADF.raw() as i64;
    };
    if !ops.seekable() {
        return Errno::ESPIPE.raw() as i64;
    }

    let size = match ops.size(snap.handle) {
        Some(v) => v as i64,
        None => return Errno::EBADF.raw() as i64,
    };

    let new_pos = match whence as u64 {
        SEEK_SET => offset,
        SEEK_CUR => (snap.position as i64).saturating_add(offset),
        SEEK_END => size.saturating_add(offset),
        _ => return Errno::EINVAL.raw() as i64,
    };
    if new_pos < 0 {
        return Errno::EINVAL.raw() as i64;
    }

    with_open_files(|state| {
        if let Some(open_file) = get_open_file_mut(&mut state.open_files, snap.open_file)
            && open_file.seekable_position_matches(ops, snap.handle)
        {
            open_file.position = new_pos as u64;
        }
    });
    new_pos
}

pub fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return usize::MAX;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return usize::MAX,
        }
    };
    snap.ops
        .and_then(|ops| ops.size(snap.handle))
        .map(|v| v as usize)
        .unwrap_or(usize::MAX)
}

pub fn file_exists_path(path: &[u8]) -> c_int {
    let rc = vfs_stat(path);
    if let Ok((kind, _)) = rc {
        return if kind == FS_TYPE_FILE { 1 } else { 0 };
    }
    0
}

pub fn file_unlink_path(path: &[u8]) -> c_int {
    if vfs_unlink(path).is_ok() {
        0
    } else {
        Errno::ENOENT.raw() as _
    }
}

pub fn file_mkdir_path(path: &[u8]) -> c_int {
    match vfs_mkdir(path) {
        Ok(()) => 0,
        Err(crate::vfs::VfsError::AlreadyExists) => Errno::EEXIST.raw() as _,
        Err(crate::vfs::VfsError::NotFound) => Errno::ENOENT.raw() as _,
        Err(crate::vfs::VfsError::NotDirectory) => Errno::ENOTDIR.raw() as _,
        Err(crate::vfs::VfsError::PermissionDenied) => Errno::EACCES.raw() as _,
        Err(crate::vfs::VfsError::NoSpace) => Errno::ENOSPC.raw() as _,
        Err(crate::vfs::VfsError::ReadOnly) => Errno::EACCES.raw() as _,
        Err(_) => Errno::EIO.raw() as _,
    }
}

pub fn file_stat_path(path: &[u8], out_type: &mut u8, out_size: &mut u32) -> c_int {
    if let Ok((kind, size)) = vfs_stat(path) {
        *out_type = kind;
        *out_size = size;
        return 0;
    }
    Errno::ENOENT.raw() as _
}

pub fn file_list_path(path: &[u8], entries: &mut [UserFsEntry], out_count: &mut u32) -> c_int {
    if entries.is_empty() {
        return Errno::EINVAL.raw() as _;
    }
    match vfs_list(path, entries) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => Errno::ENOENT.raw() as _,
    }
}

pub fn file_is_console_fd(process_id: u32, fd: c_int) -> bool {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return false;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return false,
        }
    };
    snap.ops.map(|ops| kind_is_tty(ops.kind())).unwrap_or(false)
}

pub fn file_get_tty_index(process_id: u32, fd: c_int) -> Option<TtyIndex> {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return None;
        };
        snapshot_fd(&inner, fd)?
    };
    let ops = snap.ops?;
    if ops.kind() == FileKind::Tty {
        Some(TtyIndex(snap.handle as u8))
    } else {
        None
    }
}

/// Open a file descriptor for a TTY device.
pub fn file_open_tty_fd(process_id: u32, tty_idx: TtyIndex, posix_flags: u32) -> c_int {
    let tty_ops = current_tty_ops();
    let base = OpenMode::READ | OpenMode::WRITE;
    let kept = posix_flags & (O_CLOEXEC as u32 | O_NOCTTY as u32 | O_NONBLOCK as u32);
    let flags = if kept != 0 { base.with_raw(kept) } else { base };
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

    let pipe_handle = match pipe::alloc_slot() {
        Some(h) => h,
        None => return Errno::ENOMEM.raw() as _,
    };

    let Some(mut inner) = pick_pid_slot_locked(process_id) else {
        pipe::free_slot(pipe_handle);
        return Errno::ESRCH.raw() as _;
    };

    let nonblock = (flags & O_NONBLOCK as u32) != 0;
    let cloexec = (flags & O_CLOEXEC as u32) != 0;
    let read_flags = if nonblock {
        OpenMode::READ.with_raw(O_NONBLOCK as u32)
    } else {
        OpenMode::READ
    };
    let write_flags = if nonblock {
        OpenMode::WRITE.with_raw(O_NONBLOCK as u32)
    } else {
        OpenMode::WRITE
    };

    let result = with_open_files(|state| {
        let Some(read_idx) = find_free_slot(&inner) else {
            return Err(Errno::EMFILE);
        };
        inner.descriptors[read_idx].valid = true;

        let Some(write_idx) = find_free_slot(&inner) else {
            reset_fd_entry(&mut inner.descriptors[read_idx]);
            return Err(Errno::EMFILE);
        };

        let Some(read_open_idx) = alloc_open_file_entry(
            &mut state.open_files,
            &PIPE_READ_OPS,
            pipe_handle.as_usize(),
            read_flags,
            0,
        ) else {
            reset_fd_entry(&mut inner.descriptors[read_idx]);
            return Err(Errno::ENFILE);
        };
        let Some(write_open_idx) = alloc_open_file_entry(
            &mut state.open_files,
            &PIPE_WRITE_OPS,
            pipe_handle.as_usize(),
            write_flags,
            0,
        ) else {
            release_open_file(&mut state.open_files, read_open_idx);
            reset_fd_entry(&mut inner.descriptors[read_idx]);
            return Err(Errno::ENFILE);
        };

        let primed = pipe::with_pipe_mut(pipe_handle, |slot| {
            slot.readers = 1;
            slot.writers = 1;
        });
        if primed.is_none() {
            release_open_file(&mut state.open_files, read_open_idx);
            release_open_file(&mut state.open_files, write_open_idx);
            reset_fd_entry(&mut inner.descriptors[read_idx]);
            return Err(Errno::ENOMEM);
        }

        inner.descriptors[read_idx] = FdEntry {
            open_file: read_open_idx,
            cloexec,
            valid: true,
        };
        inner.descriptors[write_idx] = FdEntry {
            open_file: write_open_idx,
            cloexec,
            valid: true,
        };

        Ok((read_idx as c_int, write_idx as c_int))
    });

    drop(inner);

    match result {
        Ok((r, w)) => {
            *out_read_fd = r;
            *out_write_fd = w;
            0
        }
        Err(e) => {
            pipe::free_slot(pipe_handle);
            e.raw() as _
        }
    }
}

pub fn file_dup_fd(process_id: u32, old_fd: c_int) -> c_int {
    file_dup_fd_min(process_id, old_fd, 0)
}

fn file_dup_fd_min(process_id: u32, old_fd: c_int, min_fd: usize) -> c_int {
    with_pid_slot(process_id, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Errno::EBADF.raw() as _;
        };
        let src_open_idx = src.open_file;

        let increfed =
            with_open_files(|state| incref_open_file(&mut state.open_files, src_open_idx));
        if !increfed {
            return Errno::EBADF.raw() as _;
        }

        let Some(new_idx) = find_free_slot_from(inner, min_fd) else {
            with_open_files(|state| release_open_file(&mut state.open_files, src_open_idx));
            return Errno::EMFILE.raw() as _;
        };

        inner.descriptors[new_idx] = FdEntry {
            open_file: src_open_idx,
            cloexec: false,
            valid: true,
        };
        new_idx as c_int
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    if old_fd == new_fd {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        let valid = snapshot_fd(&inner, old_fd).is_some();
        return if valid {
            new_fd
        } else {
            Errno::EBADF.raw() as _
        };
    }

    with_pid_slot(process_id, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Errno::EBADF.raw() as _;
        };
        let src_open_idx = src.open_file;
        let increfed =
            with_open_files(|state| incref_open_file(&mut state.open_files, src_open_idx));
        if !increfed {
            return Errno::EBADF.raw() as _;
        }

        if inner.descriptors[new_fd as usize].valid {
            let old_open_idx = inner.descriptors[new_fd as usize].open_file;
            with_open_files(|state| release_open_file(&mut state.open_files, old_open_idx));
        }
        inner.descriptors[new_fd as usize] = FdEntry {
            open_file: src_open_idx,
            cloexec: false,
            valid: true,
        };
        new_fd
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return Errno::EINVAL.raw() as _;
    }
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }

    with_pid_slot(process_id, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Errno::EBADF.raw() as _;
        };
        let src_open_idx = src.open_file;
        let increfed =
            with_open_files(|state| incref_open_file(&mut state.open_files, src_open_idx));
        if !increfed {
            return Errno::EBADF.raw() as _;
        }

        if inner.descriptors[new_fd as usize].valid {
            let old_open_idx = inner.descriptors[new_fd as usize].open_file;
            with_open_files(|state| release_open_file(&mut state.open_files, old_open_idx));
        }
        inner.descriptors[new_fd as usize] = FdEntry {
            open_file: src_open_idx,
            cloexec: (flags & FD_CLOEXEC as u32) != 0,
            valid: true,
        };
        new_fd
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => {
            let Some(inner) = lock_pid_slot(process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
                return Errno::EBADF.raw() as i64;
            }
            let entry = &inner.descriptors[fd as usize];
            if !entry.valid {
                return Errno::EBADF.raw() as i64;
            }
            if entry.cloexec { FD_CLOEXEC as i64 } else { 0 }
        }
        F_SETFD => {
            let Some(mut inner) = lock_pid_slot(process_id) else {
                return Errno::ESRCH.raw() as i64;
            };
            if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
                return Errno::EBADF.raw() as i64;
            }
            let entry = &mut inner.descriptors[fd as usize];
            if !entry.valid {
                return Errno::EBADF.raw() as i64;
            }
            entry.cloexec = (arg & FD_CLOEXEC) != 0;
            0
        }
        F_GETFL => {
            let snap = {
                let Some(inner) = lock_pid_slot(process_id) else {
                    return Errno::ESRCH.raw() as i64;
                };
                match snapshot_fd(&inner, fd) {
                    Some(s) => s,
                    None => return Errno::EBADF.raw() as i64,
                }
            };
            openmode_to_posix_bits(snap.status_flags) as i64
        }
        F_SETFL => with_pid_slot(process_id, |inner| {
            let Some(fd_entry) = get_fd_entry(inner, fd) else {
                return Errno::EBADF.raw() as i64;
            };
            let ofi = fd_entry.open_file;
            with_open_files(|state| {
                let Some(open_file) = get_open_file_mut(&mut state.open_files, ofi) else {
                    return Errno::EBADF.raw() as i64;
                };
                let posix_arg = arg as u32;
                let mode_bits = open_file.status_flags & (OpenMode::READ | OpenMode::WRITE);
                let mut next_flags = mode_bits;
                if posix_arg & slopos_abi::fs::O_APPEND != 0 {
                    next_flags |= OpenMode::APPEND;
                }
                let mut raw = open_file.status_flags.bits() & (O_NOCTTY as u32);
                if posix_arg & O_NONBLOCK as u32 != 0 {
                    raw |= O_NONBLOCK as u32;
                }
                let next_flags = next_flags.with_raw(raw);
                open_file.status_flags = next_flags;
                if let Some(ops) = open_file.ops {
                    let _ =
                        ops.set_status_flags(open_file.handle, openmode_to_posix_bits(next_flags));
                }
                0
            })
        })
        .unwrap_or(Errno::ESRCH.raw() as i64),
        _ => Errno::EINVAL.raw() as i64,
    }
}

pub fn file_fstat_fd(
    process_id: u32,
    fd: c_int,
    out_stat: &mut slopos_abi::fs::UserFsStat,
) -> c_int {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as _,
        }
    };
    match snap.ops {
        Some(ops) => ops.stat(snap.handle, out_stat),
        None => Errno::EBADF.raw() as _,
    }
}

pub fn fileio_open_socket_fd(process_id: u32, socket_idx: u32) -> i32 {
    let Some(socket_ops) = current_socket_ops() else {
        return Errno::ENOTSOCK.raw() as _;
    };
    install_fd_entry(
        process_id,
        socket_ops,
        socket_idx as usize,
        OpenMode::READ | OpenMode::WRITE,
        None,
    )
}

/// Open an FD using caller-supplied FileOps and handle.
pub fn fileio_open_fd_with_ops(process_id: u32, ops: &'static dyn FileOps, handle: usize) -> i32 {
    install_fd_entry(
        process_id,
        ops,
        handle,
        OpenMode::READ | OpenMode::WRITE,
        None,
    )
}

pub fn fileio_get_open_file_handle(process_id: u32, fd: i32) -> Option<(FileKind, usize)> {
    let snap = {
        let inner = lock_pid_slot(process_id)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.ops?.kind(), snap.handle))
}

/// Get the handle AND FileOps for an open fd.
pub fn fileio_get_handle_and_ops(
    process_id: u32,
    fd: i32,
) -> Option<(usize, &'static dyn FileOps)> {
    let snap = {
        let inner = lock_pid_slot(process_id)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.handle, snap.ops?))
}
