use core::ffi::c_int;
use core::sync::atomic::Ordering;

use super::*;

use slopos_abi::Errno;
use slopos_abi::file_ops::file_kind_transferable;
use slopos_abi::fs::UserFsEntry;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_CLOEXEC, O_NOCTTY, O_NONBLOCK,
    SEEK_CUR, SEEK_END, SEEK_SET,
};

use crate::pipe;
use crate::pipe_file_ops::{PIPE_READ_OPS, PIPE_WRITE_OPS, pipe_backings};
use crate::vfs::{vfs_list, vfs_mkdir, vfs_stat, vfs_unlink};
use crate::vfs_file_ops::{VFS_FILE_OPS, vfs_open_handle_flags, vnode_backing};
use slopos_abi::tty_error::TtyError;
use slopos_ostd::process::quota::FileBacking;

#[allow(non_camel_case_types)]
type ssize_t = isize;

/// Consumes `backing`, dropping it on every error path, so callers must not
/// tear the subsystem object down on their own error arm.
fn install_fd_entry(
    table: FdTable,
    ops: &'static dyn FileOps,
    handle: usize,
    mut flags: OpenMode,
    fd_flags: FdFlags,
    call_tty_policy: Option<TtyIndex>,
    backing: Option<KArc<dyn FileBacking>>,
) -> c_int {
    let mut position = 0u64;
    if flags.contains(OpenMode::APPEND) {
        match ops.size(handle) {
            Some(size) => position = size,
            None => return Errno::ENXIO.raw(),
        }
    }

    if ops.kind() == FileKind::Socket {
        let mode_bits = flags & (OpenMode::READ | OpenMode::WRITE);
        flags = mode_bits;
        let _ = ops.set_status_flags(handle, flags.bits());
    }

    let cloexec = fd_flags.cloexec || (flags.bits() & O_CLOEXEC as u32) != 0;
    let close_on_fork = fd_flags.close_on_fork;

    let Some(open_file) = new_open_file(ops, handle, flags, position, backing) else {
        return Errno::ENFILE.raw();
    };

    // Charged before the table lock is taken, so a refusal never unwinds under it.
    let Ok(reservation) = try_charge::<FdSlot>(table.account(), 1) else {
        drop(open_file);
        return Errno::EMFILE.raw();
    };

    let Some(mut inner) = lock_table_slot(table) else {
        return Errno::ESRCH.raw();
    };

    let slot_result = {
        match find_free_slot(&inner) {
            Some(slot_idx) => {
                inner.descriptors[slot_idx] = Some(FdEntry::new(
                    open_file,
                    FdFlags {
                        cloexec,
                        close_on_fork,
                    },
                    reservation,
                ));
                Ok(slot_idx as c_int)
            }
            None => Err(open_file),
        }
    };

    drop(inner);

    match slot_result {
        Ok(fd) => {
            if let Some(tty_idx) = call_tty_policy {
                maybe_acquire_controlling_tty_on_open(tty_idx, flags.bits());
            }
            fd
        }
        Err(open_file) => {
            // Detach-then-drop: teardown runs only after the slot lock is released.
            drop(open_file);
            Errno::EMFILE.raw()
        }
    }
}

fn current_tty_ops() -> &'static dyn FileOps {
    with_open_files(|state| effective_tty_ops(&state.external_ops))
}

fn current_socket_ops() -> Option<&'static dyn FileOps> {
    with_open_files(|state| external_socket_ops(&state.external_ops))
}

pub fn file_open_for_process(table: FdTable, path: &[u8], posix_flags: u32) -> c_int {
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
        let backing = match tty::open_tty(tty_idx) {
            Ok(b) => b,
            Err(e) => return tty_open_errno(e).raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            tty_idx.0 as usize,
            flags,
            FdFlags::NONE,
            None,
            Some(backing),
        );
    }

    if path == b"/dev/ptmx" {
        // The `/dev/ptmx` opener is the master, and it pays for the pair's two slots.
        let (master_idx, backing) = match tty::alloc_pty(table.account()) {
            Ok(v) => v,
            Err(_) => return Errno::ENFILE.raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            master_idx.0 as usize,
            flags.with_raw(O_NOCTTY as u32),
            FdFlags::NONE,
            None,
            Some(backing),
        );
    }

    if let Some(slave_idx) = parse_pts_path(path) {
        let backing = match tty::open_pty_slave(slave_idx) {
            Ok(b) => b,
            Err(e) => return tty_open_errno(e).raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            slave_idx.0 as usize,
            flags,
            FdFlags::NONE,
            Some(slave_idx),
            Some(backing),
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
    let Some(backing) = vnode_backing(vfs_handle, table.account()) else {
        return Errno::ENFILE.raw() as _;
    };
    install_fd_entry(
        table,
        &VFS_FILE_OPS,
        vfs_handle,
        flags,
        FdFlags::NONE,
        None,
        Some(backing),
    )
}

/// A locked PTY slave reports `EIO`, following Linux devpts behaviour.
fn tty_open_errno(e: TtyError) -> Errno {
    match e {
        TtyError::DeviceBusy => Errno::EBUSY,
        TtyError::PermissionDenied => Errno::EIO,
        TtyError::OutOfMemory => Errno::ENOMEM,
        _ => Errno::ENXIO,
    }
}

pub fn file_read_fd(table: FdTable, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    let open_file = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s.open_file,
            None => return Errno::EBADF.raw() as _,
        }
    };
    read_open_file(&open_file, buf, false)
}

/// Holding the `KArc<OpenFile>` keeps the shared-offset update correct if a
/// concurrent close drops the fd alias mid-read.
fn read_open_file(
    open_file: &KArc<OpenFile>,
    buf: &mut dyn IoBufWrite,
    force_nonblock: bool,
) -> ssize_t {
    if !open_file.status_flags().contains(OpenMode::READ) {
        return Errno::EBADF.raw() as _;
    }
    let ops = open_file.ops;

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { open_file.position() } else { 0 };
    let mut flag_bits = open_file.status_flags().bits();
    let mut socket_guard = None;
    if force_nonblock {
        flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
        socket_guard =
            ForcedNonblockGuard::engage(ops, open_file.handle, open_file.status_flags().bits());
    }
    let rc = ops.read(open_file.handle, buf, used_offset, flag_bits);
    drop(socket_guard);
    if rc > 0 && seekable {
        open_file.position.fetch_add(rc as u64, Ordering::AcqRel);
    }
    rc
}

/// The ring's own reference keeps the description addressable after userland
/// closed the fd.
pub fn file_read_ref_nonblock(file: &FileRef, buf: &mut dyn IoBufWrite) -> ssize_t {
    read_open_file(&file.open_file, buf, true)
}

pub fn file_write_fd(table: FdTable, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    let open_file = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s.open_file,
            None => return Errno::EBADF.raw() as _,
        }
    };
    write_open_file(&open_file, buf, false)
}

fn write_open_file(
    open_file: &KArc<OpenFile>,
    buf: &dyn IoBufRead,
    force_nonblock: bool,
) -> ssize_t {
    if !open_file.status_flags().contains(OpenMode::WRITE) {
        return Errno::EBADF.raw() as _;
    }
    let ops = open_file.ops;

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { open_file.position() } else { 0 };
    let mut flag_bits = open_file.status_flags().bits();
    let mut socket_guard = None;
    if force_nonblock {
        flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
        socket_guard =
            ForcedNonblockGuard::engage(ops, open_file.handle, open_file.status_flags().bits());
    }
    let rc = ops.write(open_file.handle, buf, used_offset, flag_bits);
    drop(socket_guard);
    if rc > 0 && seekable {
        open_file.position.fetch_add(rc as u64, Ordering::AcqRel);
    }
    rc
}

pub fn file_write_ref_nonblock(file: &FileRef, buf: &dyn IoBufRead) -> ssize_t {
    write_open_file(&file.open_file, buf, true)
}

/// Forces a socket fd's *stored* nonblocking flag on for a ring probe, then
/// restores it; a no-op for other fds, which honour the per-call `O_NONBLOCK`.
struct ForcedNonblockGuard {
    ops: &'static dyn FileOps,
    handle: usize,
    restore_bits: u32,
}

impl ForcedNonblockGuard {
    fn engage(ops: &'static dyn FileOps, handle: usize, orig_bits: u32) -> Option<Self> {
        if ops.kind() != FileKind::Socket {
            return None;
        }
        ops.set_status_flags(handle, slopos_abi::syscall::O_NONBLOCK as u32);
        Some(Self {
            ops,
            handle,
            restore_bits: orig_bits,
        })
    }
}

impl Drop for ForcedNonblockGuard {
    fn drop(&mut self) {
        self.ops.set_status_flags(self.handle, self.restore_bits);
    }
}

pub fn file_close_fd(table: FdTable, fd: c_int) -> c_int {
    let taken = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return Err(Errno::EBADF);
        }
        match inner.descriptors[fd as usize].take() {
            Some(entry) => Ok(entry),
            None => Err(Errno::EBADF),
        }
    });
    match taken {
        Some(Ok(entry)) => {
            drop(entry);
            0
        }
        Some(Err(e)) => e.raw() as _,
        None => Errno::ESRCH.raw() as _,
    }
}

pub fn file_seek_fd(table: FdTable, fd: c_int, offset: i64, whence: u32) -> i64 {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as i64;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as i64,
        }
    };

    let ops = snap.ops();
    if !ops.seekable() {
        return Errno::ESPIPE.raw() as i64;
    }

    let size = match ops.size(snap.handle()) {
        Some(v) => v as i64,
        None => return Errno::EBADF.raw() as i64,
    };

    let new_pos = match whence as u64 {
        SEEK_SET => offset,
        SEEK_CUR => (snap.position() as i64).saturating_add(offset),
        SEEK_END => size.saturating_add(offset),
        _ => return Errno::EINVAL.raw() as i64,
    };
    if new_pos < 0 {
        return Errno::EINVAL.raw() as i64;
    }

    snap.open_file
        .position
        .store(new_pos as u64, Ordering::Release);
    new_pos
}

pub fn file_get_size_fd(table: FdTable, fd: c_int) -> usize {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return usize::MAX;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return usize::MAX,
        }
    };
    snap.ops()
        .size(snap.handle())
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

pub fn file_is_console_fd(table: FdTable, fd: c_int) -> bool {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return false;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return false,
        }
    };
    kind_is_tty(snap.ops().kind())
}

pub fn file_get_tty_index(table: FdTable, fd: c_int) -> Option<TtyIndex> {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return None;
        };
        snapshot_fd(&inner, fd)?
    };
    if snap.ops().kind() == FileKind::Tty {
        Some(TtyIndex(snap.handle() as u8))
    } else {
        None
    }
}

/// Consumes the caller's owning TTY backing, so a failed open is undone by
/// that backing's drop.
pub fn file_open_tty_fd(
    table: FdTable,
    tty_idx: TtyIndex,
    posix_flags: u32,
    backing: KArc<dyn FileBacking>,
) -> c_int {
    let tty_ops = current_tty_ops();
    let base = OpenMode::READ | OpenMode::WRITE;
    let kept = posix_flags & (O_CLOEXEC as u32 | O_NOCTTY as u32 | O_NONBLOCK as u32);
    let flags = if kept != 0 { base.with_raw(kept) } else { base };
    install_fd_entry(
        table,
        tty_ops,
        tty_idx.0 as usize,
        flags,
        FdFlags::NONE,
        Some(tty_idx),
        Some(backing),
    )
}

pub fn file_pipe_create(
    table: FdTable,
    flags: u32,
    out_read_fd: &mut c_int,
    out_write_fd: &mut c_int,
) -> c_int {
    if flags & !(O_NONBLOCK as u32 | O_CLOEXEC as u32) != 0 {
        return Errno::EINVAL.raw() as _;
    }

    let pipe_handle = match pipe::alloc_slot(table.account()) {
        Some(h) => h,
        None => return Errno::ENOMEM.raw() as _,
    };

    // Prime both ends before wrapping them, so every error path below is a
    // plain drop rather than an explicit free.
    if pipe::with_pipe_mut(pipe_handle, |slot| {
        slot.readers = 1;
        slot.writers = 1;
    })
    .is_none()
    {
        pipe::free_slot(pipe_handle);
        return Errno::ENOMEM.raw() as _;
    }
    let Some((read_backing, write_backing)) = pipe_backings(pipe_handle) else {
        return Errno::ENFILE.raw() as _;
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

    let Some(read_of) = new_open_file(
        &PIPE_READ_OPS,
        pipe_handle.as_usize(),
        read_flags,
        0,
        Some(read_backing),
    ) else {
        return Errno::ENFILE.raw() as _;
    };
    let Some(write_of) = new_open_file(
        &PIPE_WRITE_OPS,
        pipe_handle.as_usize(),
        write_flags,
        0,
        Some(write_backing),
    ) else {
        drop(read_of);
        return Errno::ENFILE.raw() as _;
    };

    let account = table.account();
    let Some(mut inner) = lock_table_slot(table) else {
        drop(read_of);
        drop(write_of);
        return Errno::ESRCH.raw() as _;
    };

    let result: Result<(c_int, c_int), Errno> = (|| {
        let read_res = try_charge::<FdSlot>(account, 1).map_err(|_| Errno::EMFILE)?;
        let write_res = try_charge::<FdSlot>(account, 1).map_err(|_| Errno::EMFILE)?;
        let read_idx = find_free_slot(&inner).ok_or(Errno::EMFILE)?;
        inner.descriptors[read_idx] = Some(FdEntry::new(
            read_of.clone(),
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            read_res,
        ));
        let write_idx = match find_free_slot(&inner) {
            Some(idx) => idx,
            None => {
                inner.descriptors[read_idx] = None;
                return Err(Errno::EMFILE);
            }
        };
        inner.descriptors[write_idx] = Some(FdEntry::new(
            write_of.clone(),
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            write_res,
        ));
        Ok((read_idx as c_int, write_idx as c_int))
    })();

    drop(inner);

    match result {
        Ok((r, w)) => {
            *out_read_fd = r;
            *out_write_fd = w;
            drop(read_of);
            drop(write_of);
            0
        }
        Err(e) => {
            drop(read_of);
            drop(write_of);
            e.raw() as _
        }
    }
}

pub fn file_dup_fd(table: FdTable, old_fd: c_int) -> c_int {
    file_dup_fd_min(table, old_fd, 0)
}

fn file_dup_fd_min(table: FdTable, old_fd: c_int, min_fd: usize) -> c_int {
    let account = table.account();
    with_table_slot(table, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Errno::EBADF.raw() as _;
        };
        if !file_kind_transferable(src.open_file.ops.kind()) {
            return Errno::EINVAL.raw() as _;
        }
        let Some(mut alias) = src.try_alias(account) else {
            return Errno::EMFILE.raw() as _;
        };
        // `cloexec` is a preference on the fd number, so a dup starts it clear;
        // `close_on_fork` names the description and carries over.
        alias.cloexec = false;

        let Some(new_idx) = find_free_slot_from(inner, min_fd) else {
            return Errno::EMFILE.raw() as _;
        };

        inner.descriptors[new_idx] = Some(alias);
        new_idx as c_int
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_dup2_fd(table: FdTable, old_fd: c_int, new_fd: c_int) -> c_int {
    dup_into(table, old_fd, new_fd, false, false)
}

pub fn file_dup3_fd(table: FdTable, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return Errno::EINVAL.raw() as _;
    }
    dup_into(
        table,
        old_fd,
        new_fd,
        (flags & FD_CLOEXEC as u32) != 0,
        true,
    )
}

/// dup2 with `old_fd == new_fd` is a validity check (no-op success); dup3
/// forbids it (handled by the caller).
fn dup_into(table: FdTable, old_fd: c_int, new_fd: c_int, cloexec: bool, is_dup3: bool) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    if old_fd == new_fd && !is_dup3 {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as _;
        };
        return if get_fd_entry(&inner, old_fd).is_some() {
            new_fd
        } else {
            Errno::EBADF.raw() as _
        };
    }

    let account = table.account();
    let outcome = with_table_slot(table, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Err(Errno::EBADF);
        };
        if !file_kind_transferable(src.open_file.ops.kind()) {
            return Err(Errno::EINVAL);
        }
        // Only a *free* target needs a fresh charge; an occupied one reuses the
        // displaced entry's, keeping exactly one charge per number at all times.
        let occupied = inner.descriptors[new_fd as usize].is_some();
        let alias = if occupied {
            None
        } else {
            match src.try_alias(account) {
                Some(alias) => Some(alias),
                None => return Err(Errno::EMFILE),
            }
        };
        let open_file = src.open_file.clone();
        let close_on_fork = src.close_on_fork;

        let displaced = inner.descriptors[new_fd as usize].take();
        let (mut entry, released) = match (alias, displaced) {
            (Some(alias), _) => (alias, None),
            (None, Some(previous)) => {
                let (entry, released) = previous.replacing(open_file, close_on_fork);
                (entry, Some(released))
            }
            (None, None) => return Err(Errno::EMFILE),
        };
        entry.cloexec = cloexec;
        inner.descriptors[new_fd as usize] = Some(entry);
        Ok(released)
    });

    match outcome {
        Some(Ok(displaced)) => {
            drop(displaced);
            new_fd
        }
        Some(Err(e)) => e.raw() as _,
        None => Errno::ESRCH.raw() as _,
    }
}

pub fn file_fcntl_fd(table: FdTable, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(table, fd, arg as usize) as i64,
        F_GETFD => {
            let Some(inner) = lock_table_slot(table) else {
                return Errno::ESRCH.raw() as i64;
            };
            match get_fd_entry(&inner, fd) {
                Some(entry) => {
                    if entry.cloexec {
                        FD_CLOEXEC as i64
                    } else {
                        0
                    }
                }
                None => Errno::EBADF.raw() as i64,
            }
        }
        F_SETFD => {
            let Some(mut inner) = lock_table_slot(table) else {
                return Errno::ESRCH.raw() as i64;
            };
            match get_fd_entry_mut(&mut inner, fd) {
                Some(entry) => {
                    entry.cloexec = (arg & FD_CLOEXEC) != 0;
                    0
                }
                None => Errno::EBADF.raw() as i64,
            }
        }
        F_GETFL => {
            let snap = {
                let Some(inner) = lock_table_slot(table) else {
                    return Errno::ESRCH.raw() as i64;
                };
                match snapshot_fd(&inner, fd) {
                    Some(s) => s,
                    None => return Errno::EBADF.raw() as i64,
                }
            };
            openmode_to_posix_bits(snap.status_flags()) as i64
        }
        F_SETFL => {
            let snap = {
                let Some(inner) = lock_table_slot(table) else {
                    return Errno::ESRCH.raw() as i64;
                };
                match snapshot_fd(&inner, fd) {
                    Some(s) => s,
                    None => return Errno::EBADF.raw() as i64,
                }
            };
            let posix_arg = arg as u32;
            let current = snap.status_flags();
            let mode_bits = current & (OpenMode::READ | OpenMode::WRITE);
            let mut next_flags = mode_bits;
            if posix_arg & slopos_abi::fs::O_APPEND != 0 {
                next_flags |= OpenMode::APPEND;
            }
            let mut raw = current.bits() & (O_NOCTTY as u32);
            if posix_arg & O_NONBLOCK as u32 != 0 {
                raw |= O_NONBLOCK as u32;
            }
            let next_flags = next_flags.with_raw(raw);
            snap.open_file.set_status_flags(next_flags);
            let _ = snap
                .ops()
                .set_status_flags(snap.handle(), openmode_to_posix_bits(next_flags));
            0
        }
        _ => Errno::EINVAL.raw() as i64,
    }
}

pub fn file_fstat_fd(
    table: FdTable,
    fd: c_int,
    out_stat: &mut slopos_abi::fs::UserFsStat,
) -> c_int {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as _,
        }
    };
    snap.ops().stat(snap.handle(), out_stat)
}

pub fn fileio_open_socket_fd(
    table: FdTable,
    socket_idx: u32,
    backing: Option<KArc<dyn FileBacking>>,
) -> i32 {
    let Some(socket_ops) = current_socket_ops() else {
        return Errno::ENOTSOCK.raw() as _;
    };
    install_fd_entry(
        table,
        socket_ops,
        socket_idx as usize,
        OpenMode::READ | OpenMode::WRITE,
        FdFlags::NONE,
        None,
        backing,
    )
}

/// `fd_flags` is mandatory: an fd minted from kernel-side ops carries no
/// POSIX open flags, so its inheritance policy has no other source.
pub fn fileio_open_fd_with_ops(
    table: FdTable,
    ops: &'static dyn FileOps,
    handle: usize,
    backing: Option<KArc<dyn FileBacking>>,
    fd_flags: FdFlags,
) -> i32 {
    install_fd_entry(
        table,
        ops,
        handle,
        OpenMode::READ | OpenMode::WRITE,
        fd_flags,
        None,
        backing,
    )
}

pub fn fileio_get_open_file_handle(table: FdTable, fd: i32) -> Option<(FileKind, usize)> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.ops().kind(), snap.handle()))
}

/// Confers no ownership: the caller's own fd keeps the file alive for the
/// operation.
pub fn fileio_get_handle_and_ops(table: FdTable, fd: i32) -> Option<(usize, &'static dyn FileOps)> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.handle(), snap.ops()))
}

pub fn fileio_handle_and_ops_from_ref(file: &FileRef) -> (usize, &'static dyn FileOps) {
    (file.open_file.handle, file.open_file.ops)
}

/// Mint a [`FileRef`] alias of an open fd — the SCM_RIGHTS send side. The
/// alias keeps the description alive until dropped or installed.
///
/// Refuses a non-transferable kind. This is the choke point every duplication
/// path funnels through — SCM_RIGHTS, the spawn `CloneFd`/`TransferFd` arms,
/// and the ring's fd resolution — so the predicate is tested once, here,
/// rather than at each caller.
pub fn fileio_clone_file_ref(table: FdTable, fd: i32) -> Option<FileRef> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    if !file_kind_transferable(snap.ops().kind()) {
        return None;
    }
    Some(FileRef {
        open_file: snap.open_file,
    })
}

/// Install a received [`FileRef`] — the SCM_RIGHTS receive side. On failure
/// the alias drops here, closing it.
pub fn fileio_install_file_ref(table: FdTable, file: FileRef) -> c_int {
    // The receiver pays for the number; the sender's in-flight custody charge
    // is released by the queue that held it.
    let Ok(reservation) = try_charge::<FdSlot>(table.account(), 1) else {
        drop(file);
        return Errno::EMFILE.raw() as _;
    };
    let Some(mut inner) = lock_table_slot(table) else {
        return Errno::ESRCH.raw() as _;
    };
    let Some(idx) = find_free_slot(&inner) else {
        drop(inner);
        drop(file);
        return Errno::EMFILE.raw() as _;
    };
    inner.descriptors[idx] = Some(FdEntry::new(file.open_file, FdFlags::NONE, reservation));
    idx as c_int
}

/// Install a [`FileRef`] at exactly `target_fd`, displacing any occupant. On
/// failure the alias drops here, closing it.
pub fn fileio_install_file_ref_at(
    table: FdTable,
    target_fd: c_int,
    file: FileRef,
    cloexec: bool,
) -> c_int {
    if target_fd < 0 || target_fd as usize >= FILEIO_MAX_OPEN_FILES {
        drop(file);
        return Errno::EBADF.raw() as _;
    }
    let account = table.account();
    let displaced = {
        let Some(mut inner) = lock_table_slot(table) else {
            drop(file);
            return Errno::ESRCH.raw() as _;
        };
        // Displace first: the charge below is then only for a new number.
        let displaced = inner.descriptors[target_fd as usize].take();
        let Ok(reservation) = try_charge::<FdSlot>(account, 1) else {
            inner.descriptors[target_fd as usize] = displaced;
            drop(inner);
            drop(file);
            return Errno::EMFILE.raw() as _;
        };
        inner.descriptors[target_fd as usize] = Some(FdEntry::new(
            file.open_file,
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            reservation,
        ));
        displaced
    };
    drop(displaced);
    target_fd
}

/// Detach the description at `fd` — the spawn `TransferFd` move.
///
/// Refuses a non-transferable kind, leaving the descriptor in place: a seat
/// moved into another process would leave the arbiter naming a task that no
/// longer holds it.
pub fn fileio_take_file_ref(table: FdTable, fd: c_int) -> Option<FileRef> {
    let entry = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return None;
        }
        let held = inner.descriptors[fd as usize].as_ref()?;
        if !file_kind_transferable(held.open_file.ops.kind()) {
            return None;
        }
        inner.descriptors[fd as usize].take()
    })??;
    Some(FileRef {
        open_file: entry.open_file,
    })
}

/// Detaches only while the slot still holds `expected`'s description; a slot
/// the owner concurrently closed or repopulated is left untouched.
pub fn fileio_take_file_ref_matching(
    table: FdTable,
    fd: c_int,
    expected: &FileRef,
) -> Option<FileRef> {
    let entry = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return None;
        }
        let held = inner.descriptors[fd as usize].as_ref()?;
        if !KArc::ptr_eq(&held.open_file, &expected.open_file) {
            return None;
        }
        inner.descriptors[fd as usize].take()
    })??;
    Some(FileRef {
        open_file: entry.open_file,
    })
}

/// Open `path` at exactly `target_fd`, displacing any occupant — the spawn
/// `Open` action. The inherited fd is never close-on-exec.
pub fn fileio_open_at_fd(table: FdTable, target_fd: c_int, path: &[u8], posix_flags: u32) -> c_int {
    if target_fd < 0 || target_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    let opened = file_open_for_process(table, path, posix_flags & !(O_CLOEXEC as u32));
    if opened < 0 {
        return opened;
    }
    if opened == target_fd {
        return target_fd;
    }
    let rc = file_dup2_fd(table, opened, target_fd);
    let _ = file_close_fd(table, opened);
    if rc < 0 { rc } else { target_fd }
}
