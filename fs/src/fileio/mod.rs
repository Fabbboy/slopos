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

/// Internal open-mode flags for `OpenFileEntry`.
///
/// These are a DISTINCT type from POSIX `O_*` flags (`u32`).  Passing raw
/// POSIX flags where `OpenMode` is expected is a compile error — preventing
/// the class of bugs where `O_RDONLY` (0) is silently misinterpreted as
/// "no permissions" because `FILE_OPEN_READ` was 1.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct OpenMode(u32);

impl OpenMode {
    pub const EMPTY: Self = Self(0);
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const CREAT: Self = Self(1 << 2);
    pub const APPEND: Self = Self(1 << 3);

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0 && flag.0 != 0
    }

    pub const fn intersects(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Merge pass-through POSIX bits (O_NONBLOCK, O_NOCTTY, etc.) that
    /// live in the upper bits and don't collide with the internal flags.
    pub const fn with_raw(self, raw: u32) -> Self {
        Self(self.0 | raw)
    }
}

impl Default for OpenMode {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl core::ops::BitOr for OpenMode {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for OpenMode {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl core::ops::BitAnd for OpenMode {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// Convert POSIX `O_*` flags to internal `OpenMode`.
pub(crate) fn posix_to_open_mode(posix: u32) -> OpenMode {
    let mut m = OpenMode::EMPTY;
    match posix & O_ACCMODE {
        O_RDONLY => m |= OpenMode::READ,
        O_WRONLY => m |= OpenMode::WRITE,
        O_RDWR => m |= OpenMode::READ | OpenMode::WRITE,
        _ => {}
    }
    if posix & O_CREAT != 0 {
        m |= OpenMode::CREAT;
    }
    if posix & O_APPEND != 0 {
        m |= OpenMode::APPEND;
    }
    // Pass through remaining POSIX bits (O_NONBLOCK, O_NOCTTY, etc.)
    m.with_raw(posix & !(O_ACCMODE | O_CREAT | O_APPEND))
}

/// Convert internal `OpenMode` status flags back to POSIX `O_*` bits (for `F_GETFL`).
pub(crate) fn openmode_to_posix_bits(mode: OpenMode) -> u32 {
    let mut posix = match (
        mode.contains(OpenMode::READ),
        mode.contains(OpenMode::WRITE),
    ) {
        (true, true) => O_RDWR,
        (false, true) => O_WRONLY,
        _ => O_RDONLY,
    };
    if mode.contains(OpenMode::APPEND) {
        posix |= O_APPEND;
    }
    // O_NONBLOCK and O_NOCTTY are stored at their POSIX positions via with_raw().
    posix |= mode.bits() & (O_NONBLOCK as u32 | O_NOCTTY as u32);
    posix
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
    pub(super) status_flags: OpenMode,
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
            status_flags: OpenMode::EMPTY,
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

// ---------------------------------------------------------------------------
// Per-process file table access bypass.
//
// The global FILEIO_STATE lock serializes ALL file operations. But each
// FileTableSlot already has its own per-process `lock: IrqMutex<()>`.
// For hot-path operations (read, write, close, dup), we bypass the global
// lock and use only the per-process lock via `unsafe { FILEIO_STATE.as_ptr() }`.
//
// This is safe because:
// 1. Each FileTableSlot.lock is an independent IrqMutex with its own ticket.
// 2. The FileTableSlot data (descriptors[]) is only modified under that lock.
// 3. The global lock is still used for rare operations (table create/destroy,
//    initialization, registration) where structural changes happen.
// ---------------------------------------------------------------------------

use slopos_sync::IrqMutexGuard;

#[allow(dead_code)]
/// Snapshot of an FD entry's open file for use outside the lock.
#[derive(Clone, Copy)]
pub(super) struct FdSnapshot {
    pub(super) ops: Option<&'static dyn FileOps>,
    pub(super) handle: usize,
    pub(super) position: u64,
    pub(super) status_flags: OpenMode,
    pub(super) open_file_idx: u16,
}

/// Find a process's file table slot by PID and lock it WITHOUT the global lock.
///
/// Returns a guard on the per-process lock + a pointer to the FileTableSlot.
/// The caller can access `descriptors[]` through the slot pointer while holding
/// the guard.
///
/// # Safety
/// Uses `FILEIO_STATE.as_ptr()` for lock-free data access. Safe because:
/// - `FileTableSlot.in_use` and `.process_id` are written only under the global
///   lock (during table create/destroy), and reads of these fields are naturally
///   aligned — no torn reads on x86_64.
/// - `FileTableSlot.lock` is an independent IrqMutex that provides its own
///   synchronization for the `descriptors[]` array.
#[allow(dead_code)]
pub(super) fn lock_process_table(
    pid: u32,
) -> Option<(IrqMutexGuard<'static, ()>, *mut FileTableSlot)> {
    if !FILEIO_INIT.is_set() {
        return None;
    }

    // SAFETY: as_ptr() gives us a raw pointer to FileioState. The fields we
    // read (in_use, process_id) are naturally aligned and written atomically
    // on x86_64. The per-process lock is an independent IrqMutex.
    let state = unsafe { &*FILEIO_STATE.as_ptr() };

    if pid == INVALID_PROCESS_ID {
        let table = &state.kernel as *const FileTableSlot as *mut FileTableSlot;
        // SAFETY: table is a valid static pointer; lock() is always safe.
        let guard = unsafe { &(*table).lock }.lock();
        return Some((guard, table));
    }

    for slot in state.processes.iter() {
        if slot.in_use && slot.process_id == pid {
            let table = slot as *const FileTableSlot as *mut FileTableSlot;
            let guard = unsafe { &(*table).lock }.lock();
            return Some((guard, table));
        }
    }

    None
}

/// Get a reference to the open files array without the global lock.
///
/// # Safety
/// The caller must ensure no structural changes to the open files array are
/// happening concurrently (no file open/close on the same entry). For read-only
/// snapshots of immutable fields (ops, handle, kind), this is safe.
#[allow(dead_code)]
pub(super) unsafe fn open_files_ptr() -> &'static [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES] {
    unsafe { &(*FILEIO_STATE.as_ptr()).open_files }
}

#[allow(dead_code)]
pub(super) unsafe fn open_files_mut_ptr()
-> &'static mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES] {
    unsafe { &mut (*(FILEIO_STATE.as_ptr() as *mut FileioState)).open_files }
}

#[allow(dead_code)]
pub(super) fn snapshot_fd(table: *mut FileTableSlot, fd: c_int) -> Option<FdSnapshot> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let table_ref = unsafe { &*table };
    let entry = &table_ref.descriptors[fd as usize];
    if !entry.valid {
        return None;
    }
    let open_files = unsafe { open_files_ptr() };
    let ofe = &open_files[entry.open_file_idx as usize];
    if !ofe.valid {
        return None;
    }
    Some(FdSnapshot {
        ops: ofe.ops,
        handle: ofe.handle,
        position: ofe.position,
        status_flags: ofe.status_flags,
        open_file_idx: entry.open_file_idx,
    })
}

#[allow(dead_code)]
pub(super) fn external_ops_fast() -> ExternalOpsState {
    unsafe { (*FILEIO_STATE.as_ptr()).external_ops }
}

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
        let mut staging = [0u8; IO_STAGING_SIZE];
        let buf_len = buf.len();
        let mut total = 0usize;

        while total < buf_len {
            let chunk = (buf_len - total).min(staging.len());
            let n = match buf.copy_out(total, &mut staging[..chunk]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.as_isize()
                    };
                }
            };
            match tty::write_bytes(tty_idx, staging.as_ptr(), n, nonblock) {
                Ok(written) => {
                    total += written;
                    if written < n {
                        break;
                    }
                }
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.to_errno() as isize
                    };
                }
            }
        }
        total as isize
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

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        // Register FIRST, then check readiness (Linux pattern).
        let registered = tty::poll_enqueue(TtyIndex(handle as u8));
        let revents = tty::poll_events(TtyIndex(handle as u8), events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
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
