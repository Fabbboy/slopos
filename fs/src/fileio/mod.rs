use core::ffi::{c_char, c_int};
use core::slice;

use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{
    FS_TYPE_CHARDEV, FS_TYPE_FILE, O_ACCMODE, O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_WRONLY,
    UserFsStat,
};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{O_NOCTTY, O_NONBLOCK, POLLIN, POLLNVAL, POLLOUT, TtyIndex};
use slopos_sync::{InitFlag, IrqMutex};

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::driver_runtime::{
    current_task_controlling_tty, current_task_id, current_task_pgid, current_task_sid,
    set_current_task_controlling_tty,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

use crate::MAX_PATH_LEN;

pub(super) const FILEIO_MAX_OPEN_FILES: usize = 32;
pub(super) const FILEIO_MAX_OPEN_FILE_ENTRIES: usize = 256;

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

#[derive(Clone, Copy)]
pub struct PollRegInfo {
    pub open_file_idx: u16,
    pub registered: bool,
}

impl PollRegInfo {
    pub const NONE: Self = Self {
        open_file_idx: u16::MAX,
        registered: false,
    };
}

#[derive(Clone, Copy)]
pub(super) struct OpenFileEntry {
    pub(super) ops: Option<&'static dyn FileOps>,
    pub(super) handle: usize,
    pub(super) position: u64,
    pub(super) status_flags: u32,
    pub(super) refcount: u16,
    pub(super) generation: u16,
    pub(super) valid: bool,
}

impl OpenFileEntry {
    const fn new() -> Self {
        Self {
            ops: None,
            handle: 0,
            position: 0,
            status_flags: 0,
            refcount: 0,
            generation: 0,
            valid: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct FdEntry {
    pub(super) open_file_idx: u16,
    pub(super) cloexec: bool,
    pub(super) valid: bool,
}

impl FdEntry {
    const fn new() -> Self {
        Self {
            open_file_idx: 0,
            cloexec: false,
            valid: false,
        }
    }
}

pub(super) struct FileTableSlot {
    pub(super) process_id: u32,
    pub(super) in_use: bool,
    pub(super) lock: IrqMutex<()>,
    pub(super) descriptors: [FdEntry; FILEIO_MAX_OPEN_FILES],
}

impl FileTableSlot {
    const fn new(in_use: bool) -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            in_use,
            lock: IrqMutex::new(()),
            descriptors: [FdEntry::new(); FILEIO_MAX_OPEN_FILES],
        }
    }
}

unsafe impl Send for FileTableSlot {}

#[derive(Clone, Copy)]
pub(super) struct ExternalOpsState {
    pub(super) tty_ops: Option<&'static dyn FileOps>,
    pub(super) socket_ops: Option<&'static dyn FileOps>,
}

impl ExternalOpsState {
    const fn new() -> Self {
        Self {
            tty_ops: None,
            socket_ops: None,
        }
    }
}

pub(super) struct FileioState {
    pub(super) initialized: bool,
    pub(super) kernel: FileTableSlot,
    pub(super) processes: [FileTableSlot; MAX_PROCESSES],
    pub(super) open_files: [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    pub(super) external_ops: ExternalOpsState,
}

impl FileioState {
    const fn uninitialized() -> Self {
        Self {
            initialized: false,
            kernel: FileTableSlot::new(true),
            processes: [const { FileTableSlot::new(false) }; MAX_PROCESSES],
            open_files: [const { OpenFileEntry::new() }; FILEIO_MAX_OPEN_FILE_ENTRIES],
            external_ops: ExternalOpsState::new(),
        }
    }
}

unsafe impl Send for FileioState {}

pub(super) static FILEIO_STATE: IrqMutex<FileioState> = IrqMutex::new(FileioState::uninitialized());
pub(super) static FILEIO_INIT: InitFlag = InitFlag::new();

pub fn fileio_register_tty_ops(ops: &'static dyn FileOps) {
    let mut guard = FILEIO_STATE.lock();
    guard.external_ops.tty_ops = Some(ops);
}

pub fn fileio_register_socket_ops(ops: &'static dyn FileOps) {
    let mut guard = FILEIO_STATE.lock();
    guard.external_ops.socket_ops = Some(ops);
}

pub(super) fn with_state<R>(f: impl FnOnce(&mut FileioState) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    f(&mut *guard)
}

pub(super) fn with_tables<R>(
    f: impl FnOnce(
        &mut FileTableSlot,
        &mut [FileTableSlot; MAX_PROCESSES],
        &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
        &mut ExternalOpsState,
    ) -> R,
) -> R {
    with_state(|state| {
        ensure_initialized(state);
        let kernel = &mut state.kernel;
        let processes = &mut state.processes;
        let open_files = &mut state.open_files;
        let external_ops = &mut state.external_ops;
        f(kernel, processes, open_files, external_ops)
    })
}

pub(super) fn reset_fd_entry(entry: &mut FdEntry) {
    *entry = FdEntry::new();
}

pub(super) fn reset_table(table: &mut FileTableSlot) {
    for entry in table.descriptors.iter_mut() {
        reset_fd_entry(entry);
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

pub(super) fn get_fd_entry(table: &mut FileTableSlot, fd: c_int) -> Option<&mut FdEntry> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let entry = &mut table.descriptors[fd as usize];
    if !entry.valid {
        return None;
    }
    Some(entry)
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
    for open in state.open_files.iter_mut() {
        *open = OpenFileEntry::new();
    }

    reset_table(&mut state.kernel);
    for slot in state.processes.iter_mut() {
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

    if tty::acquire_controlling_terminal(tty_idx, sid, pgid).is_ok() {
        let _ = set_current_task_controlling_tty(Some(tty_idx));
    }
}

struct LocalTtyOps;

static LOCAL_TTY_OPS: LocalTtyOps = LocalTtyOps;

impl FileOps for LocalTtyOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let tty_idx = TtyIndex(handle as u8);
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        // TTY service API uses raw pointers; use a kernel-side staging buffer.
        let buf_len = buf.len();
        let mut tmp = [0u8; IO_STAGING_SIZE];
        let read_len = buf_len.min(tmp.len());
        match tty::read_cooked(tty_idx, tmp.as_mut_ptr(), read_len, nonblock) {
            Ok(n) => match buf.copy_in(0, &tmp[..n]) {
                Ok(written) => written as isize,
                Err(e) => e.as_isize(),
            },
            Err(e) => e.to_errno() as isize,
        }
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        let tty_idx = TtyIndex(handle as u8);
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        // TTY service API uses raw pointers; use a kernel-side staging buffer.
        let buf_len = buf.len();
        let mut tmp = [0u8; IO_STAGING_SIZE];
        let write_len = buf_len.min(tmp.len());
        match buf.copy_out(0, &mut tmp[..write_len]) {
            Ok(n) => match tty::write_bytes(tty_idx, tmp.as_ptr(), n, nonblock) {
                Ok(written) => written as isize,
                Err(e) => e.to_errno() as isize,
            },
            Err(e) => e.as_isize(),
        }
    }

    fn release(&self, handle: usize) {
        let _ = tty::close_ref(TtyIndex(handle as u8));
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let tty_idx = TtyIndex(handle as u8);
        if tty::open_ref(tty_idx).is_ok() {
            Some(handle)
        } else {
            None
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        tty::poll_events(TtyIndex(handle as u8), events)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        tty::poll_enqueue(TtyIndex(handle as u8))
    }

    fn poll_unwait(&self, handle: usize) {
        tty::poll_dequeue(TtyIndex(handle as u8));
    }

    fn stat(&self, _handle: usize, out: &mut UserFsStat) -> i32 {
        out.type_ = FS_TYPE_CHARDEV;
        out.size = 0;
        0
    }
}

pub(super) fn external_tty_ops(external_ops: &ExternalOpsState) -> Option<&'static dyn FileOps> {
    external_ops.tty_ops
}

pub(super) fn effective_tty_ops(external_ops: &ExternalOpsState) -> &'static dyn FileOps {
    external_tty_ops(external_ops).unwrap_or(&LOCAL_TTY_OPS)
}

pub(super) fn external_socket_ops(external_ops: &ExternalOpsState) -> Option<&'static dyn FileOps> {
    external_ops.socket_ops
}

pub(super) fn kind_is_tty(kind: FileKind) -> bool {
    kind == FileKind::Tty
}

mod fdops;
mod fdtable;
pub mod open_file_table;
mod poll;

pub use fdops::*;
pub use fdtable::*;
pub use poll::*;
