use core::ffi::{c_char, c_int};
use core::slice;

use slopos_lib::{InitFlag, IrqMutex};

use slopos_abi::fs::{
    FS_TYPE_FILE, O_ACCMODE, O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_WRONLY, UserFsEntry, UserFsStat,
};
use slopos_abi::net::INVALID_SOCKET_IDX;
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_CLOEXEC, O_NOCTTY, O_NONBLOCK,
    POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, SEEK_CUR, SEEK_END, SEEK_SET, TtyIndex,
};

use slopos_lib::kernel_services::driver_runtime::{
    block_current_task, current_task_controlling_tty, current_task_id, current_task_pgid,
    current_task_sid, finish_wait, prepare_to_wait, scheduler_is_enabled,
    set_current_task_controlling_tty,
};
use slopos_lib::kernel_services::syscall_services::socket;
use slopos_lib::kernel_services::syscall_services::tty;

use crate::pipe;
use crate::vfs::{FileSystem, InodeId, vfs_list, vfs_mkdir, vfs_open, vfs_stat, vfs_unlink};

#[allow(non_camel_case_types)]
pub(super) type ssize_t = isize;

pub(super) const FILE_OPEN_READ: u32 = 1 << 0;
pub(super) const FILE_OPEN_WRITE: u32 = 1 << 1;
pub(super) const FILE_OPEN_CREAT: u32 = 1 << 2;
pub(super) const FILE_OPEN_APPEND: u32 = 1 << 3;

pub(super) fn posix_to_internal_flags(posix: u32) -> u32 {
    let mut f = 0u32;
    match posix & O_ACCMODE {
        O_RDONLY => f |= FILE_OPEN_READ,
        O_WRONLY => f |= FILE_OPEN_WRITE,
        O_RDWR => f |= FILE_OPEN_READ | FILE_OPEN_WRITE,
        _ => {}
    }
    if posix & O_CREAT != 0 {
        f |= FILE_OPEN_CREAT;
    }
    if posix & O_APPEND != 0 {
        f |= FILE_OPEN_APPEND;
    }
    f | (posix & !(O_ACCMODE | O_CREAT | O_APPEND))
}

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

use crate::MAX_PATH_LEN;

pub(super) const FILEIO_MAX_OPEN_FILES: usize = 32;

#[derive(Clone, Copy)]
pub struct PollRegInfo {
    pub kind: PollRegKind,
    pub registered: bool,
}

#[derive(Clone, Copy)]
pub enum PollRegKind {
    None,
    Tty(TtyIndex),
    Socket(u32),
}

impl PollRegInfo {
    pub const NONE: Self = Self {
        kind: PollRegKind::None,
        registered: false,
    };
}

#[derive(Clone, Copy)]
pub(super) struct FileDescriptor {
    pub(super) inode: InodeId,
    pub(super) fs: Option<&'static dyn FileSystem>,
    pub(super) position: usize,
    pub(super) flags: u32,
    pub(super) valid: bool,
    pub(super) cloexec: bool,
    pub(super) tty_index: Option<TtyIndex>,
    pub(super) pipe_id: u32,
    pub(super) socket_idx: u32,
    pub(super) pipe_read_end: bool,
    pub(super) pipe_write_end: bool,
}

impl FileDescriptor {
    const fn new() -> Self {
        Self {
            inode: 0,
            fs: None,
            position: 0,
            flags: 0,
            valid: false,
            cloexec: false,
            tty_index: None,
            pipe_id: pipe::INVALID_PIPE_ID,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: false,
            pipe_write_end: false,
        }
    }
}

unsafe impl Send for FileDescriptor {}

pub(super) struct FileTableSlot {
    pub(super) process_id: u32,
    pub(super) in_use: bool,
    pub(super) lock: IrqMutex<()>,
    pub(super) descriptors: [FileDescriptor; FILEIO_MAX_OPEN_FILES],
}

impl FileTableSlot {
    const fn new(in_use: bool) -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            in_use,
            lock: IrqMutex::new(()),
            descriptors: [FileDescriptor::new(); FILEIO_MAX_OPEN_FILES],
        }
    }
}

unsafe impl Send for FileTableSlot {}

pub(super) struct FileioState {
    pub(super) initialized: bool,
    pub(super) kernel: FileTableSlot,
    pub(super) processes: [FileTableSlot; MAX_PROCESSES],
}

impl FileioState {
    const fn uninitialized() -> Self {
        Self {
            initialized: false,
            kernel: FileTableSlot::new(true),
            processes: [const { FileTableSlot::new(false) }; MAX_PROCESSES],
        }
    }
}

unsafe impl Send for FileioState {}

pub(super) static FILEIO_STATE: IrqMutex<FileioState> = IrqMutex::new(FileioState::uninitialized());
pub(super) static FILEIO_INIT: InitFlag = InitFlag::new();

pub(super) fn with_state<R>(f: impl FnOnce(&mut FileioState) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    f(&mut *guard)
}

pub(super) fn with_tables<R>(
    f: impl FnOnce(&mut FileTableSlot, &mut [FileTableSlot; MAX_PROCESSES]) -> R,
) -> R {
    with_state(|state| {
        ensure_initialized(state);
        let kernel = &mut state.kernel;
        let processes = &mut state.processes;
        f(kernel, processes)
    })
}

pub(super) fn reset_descriptor(desc: &mut FileDescriptor) {
    if desc.valid && desc.socket_idx != INVALID_SOCKET_IDX {
        let _ = socket::close(desc.socket_idx);
    }

    if desc.valid && desc.pipe_id != pipe::INVALID_PIPE_ID {
        let pipe_id = desc.pipe_id;
        let mut wake_readers = false;
        let mut wake_writers = false;
        {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            if let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) {
                if desc.pipe_read_end && slot.readers > 0 {
                    slot.readers -= 1;
                    if slot.readers == 0 {
                        wake_writers = true;
                    }
                }
                if desc.pipe_write_end && slot.writers > 0 {
                    slot.writers -= 1;
                    if slot.writers == 0 {
                        wake_readers = true;
                    }
                }
                if slot.readers == 0 && slot.writers == 0 {
                    *slot = pipe::PipeSlot::new();
                }
            }
        }
        if wake_readers {
            pipe::reader_wq(pipe_id).wake_all();
        }
        if wake_writers {
            pipe::writer_wq(pipe_id).wake_all();
        }
    }

    if desc.valid {
        if let Some(idx) = desc.tty_index {
            let _ = tty::close_ref(idx);
        }
    }

    desc.inode = 0;
    desc.fs = None;
    desc.position = 0;
    desc.flags = 0;
    desc.valid = false;
    desc.cloexec = false;
    desc.tty_index = None;
    desc.pipe_id = pipe::INVALID_PIPE_ID;
    desc.socket_idx = INVALID_SOCKET_IDX;
    desc.pipe_read_end = false;
    desc.pipe_write_end = false;
}

pub(super) fn clone_descriptor_for_dup(src: &FileDescriptor) -> Option<FileDescriptor> {
    let copy = *src;
    if let Some(idx) = copy.tty_index {
        let _ = tty::open_ref(idx);
    }
    if copy.pipe_id == pipe::INVALID_PIPE_ID {
        return Some(copy);
    }

    let mut pipe_state = pipe::PIPE_STATE.lock();
    let slot = pipe::slot_mut(&mut pipe_state, copy.pipe_id)?;
    if copy.pipe_read_end {
        slot.readers = slot.readers.saturating_add(1);
    }
    if copy.pipe_write_end {
        slot.writers = slot.writers.saturating_add(1);
    }
    Some(copy)
}

pub(super) fn reset_table(table: &mut FileTableSlot) {
    for desc in table.descriptors.iter_mut() {
        reset_descriptor(desc);
    }
}

pub(super) fn find_free_table(
    processes: &mut [FileTableSlot; MAX_PROCESSES],
) -> Option<&mut FileTableSlot> {
    for slot in processes.iter_mut() {
        if !slot.in_use {
            return Some(slot);
        }
    }
    None
}

pub(super) fn table_for_pid<'a>(
    kernel: &'a mut FileTableSlot,
    processes: &'a mut [FileTableSlot; MAX_PROCESSES],
    pid: u32,
) -> Option<&'a mut FileTableSlot> {
    if pid == INVALID_PROCESS_ID {
        return Some(kernel);
    }
    for slot in processes.iter_mut() {
        if slot.in_use && slot.process_id == pid {
            return Some(slot);
        }
    }
    None
}

pub(super) fn get_descriptor<'a>(
    table: &'a mut FileTableSlot,
    fd: c_int,
) -> Option<&'a mut FileDescriptor> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let desc = &mut table.descriptors[fd as usize];
    if !desc.valid {
        return None;
    }
    Some(desc)
}

pub(super) fn find_free_slot(table: &FileTableSlot) -> Option<usize> {
    find_free_slot_from(table, 0)
}

pub(super) fn find_free_slot_from(table: &FileTableSlot, min_fd: usize) -> Option<usize> {
    for idx in min_fd..FILEIO_MAX_OPEN_FILES {
        if !table.descriptors[idx].valid {
            return Some(idx);
        }
    }
    None
}

pub(super) fn ensure_initialized(state: &mut FileioState) {
    if !FILEIO_INIT.init_once() {
        return;
    }

    state.kernel = FileTableSlot::new(true);
    for slot in state.processes.iter_mut() {
        *slot = FileTableSlot::new(false);
    }
    let kernel = &mut state.kernel;
    reset_table(kernel);
    let processes = &mut state.processes;
    for slot in processes.iter_mut() {
        reset_table(slot);
        slot.process_id = INVALID_PROCESS_ID;
        slot.in_use = false;
    }
    state.initialized = true;
}

pub(super) unsafe fn cstr_len(ptr_in: *const c_char) -> usize {
    if ptr_in.is_null() {
        return 0;
    }
    let mut len = 0usize;
    unsafe {
        while *ptr_in.add(len) != 0 {
            len += 1;
        }
    }
    len
}

pub(super) unsafe fn path_bytes<'a>(path: *const c_char) -> Option<&'a [u8]> {
    if path.is_null() {
        return None;
    }
    unsafe {
        let len = cstr_len(path);
        Some(slice::from_raw_parts(
            path as *const u8,
            len.min(MAX_PATH_LEN),
        ))
    }
}

pub(super) fn parse_pts_path(path: &[u8]) -> Option<TtyIndex> {
    let rest = path.strip_prefix(b"/dev/pts/")?;
    if rest.is_empty() {
        return None;
    }

    let mut value = 0u16;
    for &byte in rest {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u16)?;
    }

    let idx = value as u8;
    if value > u8::MAX as u16 || idx < 2 {
        return None;
    }

    Some(TtyIndex(idx))
}

pub(super) fn bootstrap_console_fds(table: &mut FileTableSlot) {
    let console_tty = tty::default_console_tty();

    table.descriptors[0] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_READ,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: pipe::INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };
    table.descriptors[1] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: pipe::INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };
    table.descriptors[2] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: pipe::INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };

    let _ = tty::open_ref(console_tty);
    let _ = tty::open_ref(console_tty);
    let _ = tty::open_ref(console_tty);
}

pub(super) fn maybe_acquire_controlling_tty_on_open(tty_idx: TtyIndex, flags: u32) {
    if (flags & O_NOCTTY as u32) != 0 {
        return;
    }

    let task_id = current_task_id();
    let sid = current_task_sid();
    let pgid = current_task_pgid();
    if task_id == 0 || sid == 0 || sid != task_id {
        return;
    }
    if current_task_controlling_tty().is_some() {
        return;
    }

    if tty::acquire_controlling_terminal(tty_idx, sid, pgid) == 0 {
        let _ = set_current_task_controlling_tty(Some(tty_idx));
    }
}

mod fdops;
mod fdtable;
mod poll;

pub use fdops::*;
pub use fdtable::*;
pub use poll::*;
