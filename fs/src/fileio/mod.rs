use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::KernelErrno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::{
    FS_TYPE_CHARDEV, FS_TYPE_FILE, O_ACCMODE, O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_WRONLY,
    UserFsStat,
};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{O_NOCTTY, O_NONBLOCK, POLLIN, POLLNVAL, POLLOUT, TtyIndex};
use slopos_ostd::sync::{
    InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock, SpinLockGuard,
};

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::driver_runtime::{
    current_task_controlling_tty, current_task_id, current_task_pgid, current_task_sid,
    set_current_task_controlling_tty,
};
use slopos_kernel_services::syscall_services::tty;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

pub(super) const FILEIO_MAX_OPEN_FILES: usize = 32;
pub(super) const FILEIO_MAX_OPEN_FILE_ENTRIES: usize = 256;

/// Internal open-mode flags for `OpenFileEntry`.
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
    m.with_raw(posix & !(O_ACCMODE | O_CREAT | O_APPEND))
}

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

// ---------------------------------------------------------------------------
// Per-process slot layout.
//
// Each `FileTableSlot` lives in a top-level static array. Its `process_id`
// is an `AtomicU32` outside the lock, supporting lock-free scans via
// `slot_for_pid`. The mutable per-slot state — `in_use` plus the
// `descriptors` array — is wrapped in a `SpinLock<FileTableSlotInner>` at
// `LOCK_LEVEL_REGISTRY`.
//
// Lock order: per-process `slot.inner` (REGISTRY=2) is acquired first;
// the shared `OPEN_FILES_STATE` (RESOURCE=1) is acquired second. Holders
// of two different per-process locks must snapshot one and release before
// taking the other (used by fork in fdtable.rs).
// ---------------------------------------------------------------------------

pub(super) struct FileTableSlot {
    pub(super) process_id: AtomicU32,
    pub(super) inner: SpinLock<FileTableSlotInner>,
}

pub(super) struct FileTableSlotInner {
    pub(super) in_use: bool,
    pub(super) descriptors: [FdEntry; FILEIO_MAX_OPEN_FILES],
}

impl FileTableSlot {
    pub(super) const fn new(in_use: bool) -> Self {
        Self {
            process_id: AtomicU32::new(INVALID_PROCESS_ID),
            inner: SpinLock::new(
                FileTableSlotInner {
                    in_use,
                    descriptors: [FdEntry::new(); FILEIO_MAX_OPEN_FILES],
                },
                LOCK_LEVEL_REGISTRY,
            ),
        }
    }
}

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

pub(super) struct OpenFilesState {
    pub(super) initialized: bool,
    pub(super) open_files: [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    pub(super) external_ops: ExternalOpsState,
}

impl OpenFilesState {
    const fn uninitialized() -> Self {
        Self {
            initialized: false,
            open_files: [const { OpenFileEntry::new() }; FILEIO_MAX_OPEN_FILE_ENTRIES],
            external_ops: ExternalOpsState::new(),
        }
    }
}

pub(super) static KERNEL_TABLE: FileTableSlot = FileTableSlot::new(true);
pub(super) static PROCESS_TABLES: [FileTableSlot; MAX_PROCESSES] =
    [const { FileTableSlot::new(false) }; MAX_PROCESSES];
pub(super) static OPEN_FILES_STATE: SpinLock<OpenFilesState> =
    SpinLock::new(OpenFilesState::uninitialized(), LOCK_LEVEL_RESOURCE);
pub(super) static FILEIO_INIT: InitFlag = InitFlag::new();

/// Lock-free scan: return the slot whose `process_id` matches `pid`.
///
/// `INVALID_PROCESS_ID` (the kernel pid) maps to [`KERNEL_TABLE`].
pub(super) fn slot_for_pid(pid: u32) -> Option<&'static FileTableSlot> {
    if pid == INVALID_PROCESS_ID {
        return Some(&KERNEL_TABLE);
    }
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) == pid {
            return Some(slot);
        }
    }
    None
}

/// Lazy-initialise the open-files registry and run `f` with it locked.
pub(super) fn with_open_files<R>(f: impl FnOnce(&mut OpenFilesState) -> R) -> R {
    let mut guard = OPEN_FILES_STATE.lock();
    if !guard.initialized {
        FILEIO_INIT.init_once();
        for entry in guard.open_files.iter_mut() {
            *entry = OpenFileEntry::new();
        }
        guard.initialized = true;
    }
    f(&mut *guard)
}

/// Lock the per-process slot for `pid` and run `f` with mutable access
/// to its descriptor table. Returns `None` if no slot owns `pid` or if
/// the slot was claimed but is not yet `in_use`.
pub(super) fn with_pid_slot<R>(
    pid: u32,
    f: impl FnOnce(&mut FileTableSlotInner) -> R,
) -> Option<R> {
    let slot = slot_for_pid(pid)?;
    let mut guard = slot.inner.lock();
    if !guard.in_use {
        return None;
    }
    Some(f(&mut *guard))
}

/// Acquire the per-process slot lock and return the guard. Used by hot
/// paths that snapshot a single FD then drop the lock before further
/// work.
pub(super) fn lock_pid_slot(pid: u32) -> Option<SpinLockGuard<'static, FileTableSlotInner>> {
    let slot = slot_for_pid(pid)?;
    let guard = slot.inner.lock();
    if !guard.in_use {
        return None;
    }
    Some(guard)
}

/// Find or claim the per-process slot for `pid`, returning its lock
/// guard already held. Used by `file_open_for_process` and pipe-create
/// where the caller may need to lazily allocate a table.
///
/// Falls back to the kernel table only when no free slot is available.
pub(super) fn pick_pid_slot_locked(pid: u32) -> Option<SpinLockGuard<'static, FileTableSlotInner>> {
    if pid == INVALID_PROCESS_ID {
        return Some(KERNEL_TABLE.inner.lock());
    }
    if let Some(slot) = slot_for_pid(pid) {
        let guard = slot.inner.lock();
        if guard.in_use {
            return Some(guard);
        }
    }
    for slot in PROCESS_TABLES.iter() {
        if slot
            .process_id
            .compare_exchange(INVALID_PROCESS_ID, pid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let mut guard = slot.inner.lock();
            guard.in_use = true;
            for entry in guard.descriptors.iter_mut() {
                *entry = FdEntry::new();
            }
            return Some(guard);
        }
    }
    Some(KERNEL_TABLE.inner.lock())
}

pub(super) fn reset_fd_entry(entry: &mut FdEntry) {
    *entry = FdEntry::new();
}

/// Snapshot of a file descriptor's open-file state. Captured under
/// the per-process slot lock and the open-files lock, then used after
/// both are released so I/O proceeds without holding any lock.
#[derive(Clone, Copy)]
pub(super) struct FdSnapshot {
    pub(super) ops: Option<&'static dyn FileOps>,
    pub(super) handle: usize,
    pub(super) position: u64,
    pub(super) status_flags: OpenMode,
    pub(super) open_file_idx: u16,
}

pub(super) fn snapshot_fd(inner: &FileTableSlotInner, fd: c_int) -> Option<FdSnapshot> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let entry = &inner.descriptors[fd as usize];
    if !entry.valid {
        return None;
    }
    let ofi = entry.open_file_idx;
    with_open_files(|state| {
        let ofe = &state.open_files[ofi as usize];
        if !ofe.valid {
            return None;
        }
        Some(FdSnapshot {
            ops: ofe.ops,
            handle: ofe.handle,
            position: ofe.position,
            status_flags: ofe.status_flags,
            open_file_idx: ofi,
        })
    })
}

pub(super) fn get_fd_entry(inner: &mut FileTableSlotInner, fd: c_int) -> Option<&mut FdEntry> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let entry = &mut inner.descriptors[fd as usize];
    if !entry.valid {
        return None;
    }
    Some(entry)
}

pub(super) fn find_free_slot(inner: &FileTableSlotInner) -> Option<usize> {
    find_free_slot_from(inner, 0)
}

pub(super) fn find_free_slot_from(inner: &FileTableSlotInner, min_fd: usize) -> Option<usize> {
    for idx in min_fd..FILEIO_MAX_OPEN_FILES {
        if !inner.descriptors[idx].valid {
            return Some(idx);
        }
    }
    None
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

pub fn fileio_register_tty_ops(ops: &'static dyn FileOps) {
    with_open_files(|state| state.external_ops.tty_ops = Some(ops));
}

pub fn fileio_register_socket_ops(ops: &'static dyn FileOps) {
    with_open_files(|state| state.external_ops.socket_ops = Some(ops));
}

struct LocalTtyOps;

static LOCAL_TTY_OPS: LocalTtyOps = LocalTtyOps;

fn local_tty_index(handle: usize) -> Result<TtyIndex, slopos_abi::Errno> {
    if handle > u8::MAX as usize {
        Err(slopos_abi::Errno::EBADF)
    } else {
        Ok(TtyIndex(handle as u8))
    }
}

impl FileOps for LocalTtyOps {
    fn kind(&self) -> FileKind {
        FileKind::Tty
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let buf_len = buf.len();
        let mut tmp = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return slopos_abi::Errno::ENOMEM.as_isize(),
        };
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
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(e) => return e.as_isize(),
        };
        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return slopos_abi::Errno::ENOMEM.as_isize(),
        };
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
        if let Ok(idx) = local_tty_index(handle) {
            let _ = tty::close_ref(idx);
        }
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let tty_idx = local_tty_index(handle).ok()?;
        if tty::open_ref(tty_idx).is_ok() {
            Some(handle)
        } else {
            None
        }
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let tty_idx = match local_tty_index(handle) {
            Ok(idx) => idx,
            Err(_) => {
                return slopos_abi::file_ops::FusedPollResult {
                    revents: 0,
                    registered: false,
                    open_file_idx: 0,
                };
            }
        };
        let registered = tty::poll_enqueue(tty_idx);
        let revents = tty::poll_events(tty_idx, events);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_idx: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        match local_tty_index(handle) {
            Ok(idx) => tty::poll_events(idx, events),
            Err(_) => 0,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match local_tty_index(handle) {
            Ok(idx) => tty::poll_enqueue(idx),
            Err(_) => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Ok(idx) = local_tty_index(handle) {
            tty::poll_dequeue(idx);
        }
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
