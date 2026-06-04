use core::ffi::c_int;
use core::sync::atomic::Ordering;

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

/// Install an FD entry in `process_id`'s table.
///
/// **Ownership contract:** the caller mints the backing reference (e.g.
/// `tty::open_ref`, an allocated socket/pipe handle) and hands it here as
/// `(ops, handle)`. This function constructs the single owning
/// [`KArc<OpenFile>`] for it, so the backing reference is now owned by
/// that `OpenFile` and released exactly once when the last alias drops.
/// On any error path the freshly-built `OpenFile` is dropped (running the
/// backing release once); if the `OpenFile` allocation itself fails the
/// caller's minted reference is released explicitly. Callers therefore
/// must **not** also release the backing reference on their own error arm
/// — that would be a double release.
fn install_fd_entry(
    process_id: u32,
    ops: &'static dyn FileOps,
    handle: usize,
    mut flags: OpenMode,
    call_tty_policy: Option<TtyIndex>,
) -> c_int {
    let mut position = 0u64;
    if flags.contains(OpenMode::APPEND) {
        match ops.size(handle) {
            Some(size) => position = size,
            None => {
                ops.release(handle);
                return Errno::ENXIO.raw();
            }
        }
    }

    if ops.kind() == FileKind::Socket {
        let mode_bits = flags & (OpenMode::READ | OpenMode::WRITE);
        flags = mode_bits;
        let _ = ops.set_status_flags(handle, flags.bits());
    }

    let cloexec = (flags.bits() & O_CLOEXEC as u32) != 0;

    // Build the single owner up front; from here the backing reference is
    // owned by `open_file` and released exactly once on its drop.
    let Some(open_file) = new_open_file(ops, handle, flags, position) else {
        ops.release(handle);
        return Errno::ENFILE.raw();
    };

    let Some(mut inner) = pick_pid_slot_locked(process_id) else {
        // `open_file` drops here → backing released once.
        return Errno::ESRCH.raw();
    };

    let slot_result = {
        match find_free_slot(&inner) {
            Some(slot_idx) => {
                inner.descriptors[slot_idx] = Some(FdEntry { open_file, cloexec });
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
            // Detach-then-drop: the slot lock is released above, so the
            // backing teardown runs lock-free here.
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

pub fn file_read_fd(process_id: u32, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    file_read_fd_inner(process_id, fd, buf, false)
}

/// SlopRing non-blocking probe variant of [`file_read_fd`] (SLOPRING
/// § 12). Runs the **exact same** `FileOps::read` path — so observable
/// results match the blocking syscall (R12 parity) — but forces the
/// `O_NONBLOCK` flag so the path returns `-EAGAIN` instead of parking on
/// a `WaitQueue`. Used by `OP_READ` / consuming reads; the ring's
/// harvest phase (not this call) is what blocks.
pub fn file_read_fd_nonblock(process_id: u32, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    file_read_fd_inner(process_id, fd, buf, true)
}

fn file_read_fd_inner(
    process_id: u32,
    fd: c_int,
    buf: &mut dyn IoBufWrite,
    force_nonblock: bool,
) -> ssize_t {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) if s.status_flags().contains(OpenMode::READ) => s,
            _ => return Errno::EBADF.raw() as _,
        }
    };

    let ops = snap.ops();

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { snap.position() } else { 0 };
    let mut flag_bits = snap.status_flags().bits();
    let mut socket_guard = None;
    if force_nonblock {
        flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
        // Sockets ignore the per-call flag and consult their *stored*
        // nonblocking state (SLOPRING § 12 reality 1), so toggle it
        // across the probe and restore it on drop.
        socket_guard = ForcedNonblockGuard::engage(ops, snap.handle(), snap.status_flags().bits());
    }
    let rc = ops.read(snap.handle(), buf, used_offset, flag_bits);
    drop(socket_guard);
    if rc > 0 && seekable {
        // The held `KArc<OpenFile>` is the very object the fd points at,
        // so advancing its shared offset is correct even if a concurrent
        // close dropped the fd alias mid-read.
        snap.open_file
            .position
            .fetch_add(rc as u64, Ordering::AcqRel);
    }
    rc
}

pub fn file_write_fd(process_id: u32, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    file_write_fd_inner(process_id, fd, buf, false)
}

/// SlopRing non-blocking probe variant of [`file_write_fd`] — see
/// [`file_read_fd_nonblock`].
pub fn file_write_fd_nonblock(process_id: u32, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    file_write_fd_inner(process_id, fd, buf, true)
}

fn file_write_fd_inner(
    process_id: u32,
    fd: c_int,
    buf: &dyn IoBufRead,
    force_nonblock: bool,
) -> ssize_t {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) if s.status_flags().contains(OpenMode::WRITE) => s,
            _ => return Errno::EBADF.raw() as _,
        }
    };

    let ops = snap.ops();

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { snap.position() } else { 0 };
    let mut flag_bits = snap.status_flags().bits();
    let mut socket_guard = None;
    if force_nonblock {
        flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
        socket_guard = ForcedNonblockGuard::engage(ops, snap.handle(), snap.status_flags().bits());
    }
    let rc = ops.write(snap.handle(), buf, used_offset, flag_bits);
    drop(socket_guard);
    if rc > 0 && seekable {
        snap.open_file
            .position
            .fetch_add(rc as u64, Ordering::AcqRel);
    }
    rc
}

/// RAII guard that forces a socket fd's *stored* nonblocking flag on for
/// the duration of a SlopRing probe, then restores it. For non-socket
/// fds (pipes/ttys/regular) it is a no-op — those honour the per-call
/// `O_NONBLOCK` flag directly. (SLOPRING § 12 reality 1.)
struct ForcedNonblockGuard {
    ops: &'static dyn FileOps,
    handle: usize,
    /// The status-flag bits to restore on drop.
    restore_bits: u32,
}

impl ForcedNonblockGuard {
    fn engage(ops: &'static dyn FileOps, handle: usize, orig_bits: u32) -> Option<Self> {
        if ops.kind() != FileKind::Socket {
            return None;
        }
        // Set nonblocking for the probe; restore the *original* status
        // bits on drop. The socket subsystem owns the bit; we round-trip
        // it through `set_status_flags` (the same path `fcntl(F_SETFL)`
        // uses), so no socket-internal invariant is bypassed.
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

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    // Detach-then-drop: take the entry out of the slot under the table
    // lock, release the lock, then drop the entry so the `OpenFile`
    // teardown (last alias → backing release) runs lock-free.
    let taken = with_pid_slot(process_id, |inner| {
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
    kind_is_tty(snap.ops().kind())
}

pub fn file_get_tty_index(process_id: u32, fd: c_int) -> Option<TtyIndex> {
    let snap = {
        let Some(inner) = lock_pid_slot(process_id) else {
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

/// Open a file descriptor for a TTY device.
///
/// **Ownership contract:** the caller mints the `tty::open_ref` (or
/// equivalent peer-open) for `tty_idx` and this consumes it via
/// [`install_fd_entry`]. The caller must not release that reference on
/// its own error arm (the failed install already released it once).
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

    // Build the two owning `OpenFile`s up front. Each owns one of the
    // pipe's two backing references (released by the respective `Drop`);
    // priming the reader/writer counts ties them to this pair.
    let Some(read_of) = new_open_file(&PIPE_READ_OPS, pipe_handle.as_usize(), read_flags, 0) else {
        pipe::free_slot(pipe_handle);
        return Errno::ENFILE.raw() as _;
    };
    let Some(write_of) = new_open_file(&PIPE_WRITE_OPS, pipe_handle.as_usize(), write_flags, 0)
    else {
        // `read_of` drops here; its `PIPE_READ_OPS::release` runs against
        // an unprimed slot (readers/writers still 0) — a no-op teardown —
        // then free the slot explicitly.
        drop(read_of);
        pipe::free_slot(pipe_handle);
        return Errno::ENFILE.raw() as _;
    };

    let Some(mut inner) = pick_pid_slot_locked(process_id) else {
        drop(read_of);
        drop(write_of);
        pipe::free_slot(pipe_handle);
        return Errno::ESRCH.raw() as _;
    };

    // Find two free slots and prime the pipe, all under the table lock.
    // On any failure return `Err(errno)` carrying the still-owned
    // `OpenFile`s out so they (and the slot) are torn down *after* the
    // lock drops (detach-then-drop). On success install both entries.
    let result: Result<(c_int, c_int), Errno> = (|| {
        let read_idx = find_free_slot(&inner).ok_or(Errno::EMFILE)?;
        inner.descriptors[read_idx] = Some(FdEntry {
            open_file: read_of.clone(),
            cloexec,
        });
        let write_idx = match find_free_slot(&inner) {
            Some(idx) => idx,
            None => {
                inner.descriptors[read_idx] = None;
                return Err(Errno::EMFILE);
            }
        };
        let primed = pipe::with_pipe_mut(pipe_handle, |slot| {
            slot.readers = 1;
            slot.writers = 1;
        });
        if primed.is_none() {
            inner.descriptors[read_idx] = None;
            return Err(Errno::ENOMEM);
        }
        inner.descriptors[write_idx] = Some(FdEntry {
            open_file: write_of.clone(),
            cloexec,
        });
        Ok((read_idx as c_int, write_idx as c_int))
    })();

    drop(inner);

    match result {
        Ok((r, w)) => {
            *out_read_fd = r;
            *out_write_fd = w;
            // The slots hold clones; drop the originals (decrement only).
            drop(read_of);
            drop(write_of);
            0
        }
        Err(e) => {
            // The pipe was never primed on the failure paths, so dropping
            // the two owning `OpenFile`s runs no-op backing teardowns; then
            // reclaim the pipe slot.
            drop(read_of);
            drop(write_of);
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
        // Clone the shared open file (strong++); cloexec is per-fd and
        // never copied by plain dup.
        let open_file = src.open_file.clone();

        let Some(new_idx) = find_free_slot_from(inner, min_fd) else {
            // `open_file` drops here under the lock — decrement only, no
            // teardown (the source alias keeps it alive).
            return Errno::EMFILE.raw() as _;
        };

        inner.descriptors[new_idx] = Some(FdEntry {
            open_file,
            cloexec: false,
        });
        new_idx as c_int
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    dup_into(process_id, old_fd, new_fd, false, false)
}

pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return Errno::EINVAL.raw() as _;
    }
    dup_into(
        process_id,
        old_fd,
        new_fd,
        (flags & FD_CLOEXEC as u32) != 0,
        true,
    )
}

/// Shared implementation of `dup2`/`dup3` into an explicit target fd.
/// dup2 with `old_fd == new_fd` is a validity check (no-op success);
/// dup3 forbids it (handled by the caller). `cloexec` is the bit to
/// install on the new fd. Any pre-existing entry at `new_fd` is detached
/// under the lock and dropped *after* the lock is released.
fn dup_into(process_id: u32, old_fd: c_int, new_fd: c_int, cloexec: bool, is_dup3: bool) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    if old_fd == new_fd && !is_dup3 {
        let Some(inner) = lock_pid_slot(process_id) else {
            return Errno::ESRCH.raw() as _;
        };
        return if get_fd_entry(&inner, old_fd).is_some() {
            new_fd
        } else {
            Errno::EBADF.raw() as _
        };
    }

    let outcome = with_pid_slot(process_id, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Err(Errno::EBADF);
        };
        let open_file = src.open_file.clone();
        // Detach any occupant of the target slot; it is dropped by the
        // caller after the lock is released.
        let displaced = inner.descriptors[new_fd as usize].take();
        inner.descriptors[new_fd as usize] = Some(FdEntry { open_file, cloexec });
        Ok(displaced)
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

pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => {
            let Some(inner) = lock_pid_slot(process_id) else {
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
            let Some(mut inner) = lock_pid_slot(process_id) else {
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
                let Some(inner) = lock_pid_slot(process_id) else {
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
                let Some(inner) = lock_pid_slot(process_id) else {
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
    snap.ops().stat(snap.handle(), out_stat)
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
    Some((snap.ops().kind(), snap.handle()))
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
    Some((snap.handle(), snap.ops()))
}
