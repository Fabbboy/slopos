use core::ffi::{c_char, c_int};
use core::slice;

use slopos_lib::{InitFlag, IrqMutex};

use slopos_abi::fs::{FS_TYPE_FILE, USER_FS_OPEN_CREAT, UserFsEntry, UserFsStat};
use slopos_abi::net::INVALID_SOCKET_IDX;
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_CLOEXEC, O_NOCTTY, O_NONBLOCK,
    POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, POLLPRI, SEEK_CUR, SEEK_END, SEEK_SET, TtyIndex,
};

use slopos_lib::kernel_services::driver_runtime::{
    DriverTaskHandle, block_current_task, current_task, current_task_controlling_tty,
    current_task_id, current_task_pgid, current_task_sid, scheduler_is_enabled,
    set_current_task_controlling_tty, unblock_task,
};
use slopos_lib::kernel_services::syscall_services::socket;
use slopos_lib::kernel_services::syscall_services::tty;

use crate::vfs::{FileSystem, InodeId, vfs_list, vfs_mkdir, vfs_open, vfs_stat, vfs_unlink};

#[allow(non_camel_case_types)]
type ssize_t = isize;

const FILE_OPEN_READ: u32 = 1 << 0;
const FILE_OPEN_WRITE: u32 = 1 << 1;
const FILE_OPEN_APPEND: u32 = 1 << 3;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_mm::memory_layout_defs::MAX_PROCESSES;

use crate::MAX_PATH_LEN;

const FILEIO_MAX_OPEN_FILES: usize = 32;
const MAX_PIPES: usize = 64;
const PIPE_BUFFER_SIZE: usize = 4096;
const INVALID_PIPE_ID: u32 = u32::MAX;
const PIPE_MAX_WAITERS: usize = 8;

#[derive(Clone, Copy)]
struct PipeSlot {
    valid: bool,
    read_pos: usize,
    write_pos: usize,
    len: usize,
    readers: u16,
    writers: u16,
    buffer: [u8; PIPE_BUFFER_SIZE],
    reader_waiters: [DriverTaskHandle; PIPE_MAX_WAITERS],
    reader_waiter_count: usize,
    writer_waiters: [DriverTaskHandle; PIPE_MAX_WAITERS],
    writer_waiter_count: usize,
}

impl PipeSlot {
    const fn new() -> Self {
        Self {
            valid: false,
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            buffer: [0; PIPE_BUFFER_SIZE],
            reader_waiters: [core::ptr::null_mut(); PIPE_MAX_WAITERS],
            reader_waiter_count: 0,
            writer_waiters: [core::ptr::null_mut(); PIPE_MAX_WAITERS],
            writer_waiter_count: 0,
        }
    }
}

struct PipeState {
    slots: [PipeSlot; MAX_PIPES],
}

// SAFETY: PipeSlot contains DriverTaskHandle (*mut c_void) pointers managed by the
// scheduler. Access is synchronized through the PIPE_STATE IrqMutex.
unsafe impl Send for PipeState {}

impl PipeState {
    const fn new() -> Self {
        Self {
            slots: [PipeSlot::new(); MAX_PIPES],
        }
    }
}

static PIPE_STATE: IrqMutex<PipeState> = IrqMutex::new(PipeState::new());

#[derive(Clone, Copy)]
struct FileDescriptor {
    inode: InodeId,
    fs: Option<&'static dyn FileSystem>,
    position: usize,
    flags: u32,
    valid: bool,
    cloexec: bool,
    /// When `Some(idx)`, reads/writes route to TTY `idx` instead of a filesystem.
    /// When `None`, this is not a TTY descriptor.
    tty_index: Option<TtyIndex>,
    pipe_id: u32,
    socket_idx: u32,
    pipe_read_end: bool,
    pipe_write_end: bool,
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
            pipe_id: INVALID_PIPE_ID,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: false,
            pipe_write_end: false,
        }
    }
}

unsafe impl Send for FileDescriptor {}

struct FileTableSlot {
    process_id: u32,
    in_use: bool,
    lock: IrqMutex<()>,
    descriptors: [FileDescriptor; FILEIO_MAX_OPEN_FILES],
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

struct FileioState {
    initialized: bool,
    kernel: FileTableSlot,
    processes: [FileTableSlot; MAX_PROCESSES],
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

static FILEIO_STATE: IrqMutex<FileioState> = IrqMutex::new(FileioState::uninitialized());
static FILEIO_INIT: InitFlag = InitFlag::new();

fn with_state<R>(f: impl FnOnce(&mut FileioState) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    f(&mut *guard)
}

fn with_tables<R>(
    f: impl FnOnce(&mut FileTableSlot, &mut [FileTableSlot; MAX_PROCESSES]) -> R,
) -> R {
    with_state(|state| {
        ensure_initialized(state);
        let kernel = &mut state.kernel;
        let processes = &mut state.processes;
        f(kernel, processes)
    })
}

fn reset_descriptor(desc: &mut FileDescriptor) {
    if desc.valid && desc.socket_idx != INVALID_SOCKET_IDX {
        let _ = socket::close(desc.socket_idx);
    }

    if desc.valid && desc.pipe_id != INVALID_PIPE_ID {
        let mut pipe_state = PIPE_STATE.lock();
        let idx = desc.pipe_id as usize;
        if idx < MAX_PIPES {
            let slot = &mut pipe_state.slots[idx];
            if slot.valid {
                if desc.pipe_read_end && slot.readers > 0 {
                    slot.readers -= 1;
                    if slot.readers == 0 {
                        // No more readers: wake ALL blocked writers (broken pipe)
                        pipe_wake_all_writers(slot);
                    }
                }
                if desc.pipe_write_end && slot.writers > 0 {
                    slot.writers -= 1;
                    if slot.writers == 0 {
                        // No more writers: wake ALL blocked readers (EOF)
                        pipe_wake_all_readers(slot);
                    }
                }
                if slot.readers == 0 && slot.writers == 0 {
                    *slot = PipeSlot::new();
                }
            }
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
    desc.pipe_id = INVALID_PIPE_ID;
    desc.socket_idx = INVALID_SOCKET_IDX;
    desc.pipe_read_end = false;
    desc.pipe_write_end = false;
}

fn alloc_pipe_slot() -> Option<u32> {
    let mut state = PIPE_STATE.lock();
    for (idx, slot) in state.slots.iter_mut().enumerate() {
        if !slot.valid {
            *slot = PipeSlot::new();
            slot.valid = true;
            return Some(idx as u32);
        }
    }
    None
}

fn pipe_slot_mut(state: &mut PipeState, pipe_id: u32) -> Option<&mut PipeSlot> {
    let idx = pipe_id as usize;
    if idx >= MAX_PIPES {
        return None;
    }
    let slot = &mut state.slots[idx];
    if !slot.valid {
        return None;
    }
    Some(slot)
}

fn pipe_read_into(slot: &mut PipeSlot, out: &mut [u8]) -> usize {
    let mut copied = 0usize;
    while copied < out.len() && slot.len > 0 {
        out[copied] = slot.buffer[slot.read_pos];
        slot.read_pos = (slot.read_pos + 1) % PIPE_BUFFER_SIZE;
        slot.len -= 1;
        copied += 1;
    }
    copied
}

fn pipe_write_from(slot: &mut PipeSlot, input: &[u8]) -> usize {
    let mut written = 0usize;
    while written < input.len() && slot.len < PIPE_BUFFER_SIZE {
        slot.buffer[slot.write_pos] = input[written];
        slot.write_pos = (slot.write_pos + 1) % PIPE_BUFFER_SIZE;
        slot.len += 1;
        written += 1;
    }
    written
}

fn pipe_revents(slot: &PipeSlot, desc: &FileDescriptor, events: u16) -> u16 {
    let mut revents = 0u16;

    if desc.pipe_read_end {
        if slot.len > 0 {
            revents |= events & (POLLIN | POLLPRI);
        }
        if slot.writers == 0 {
            revents |= POLLHUP;
            if (events & POLLIN) != 0 {
                revents |= POLLIN;
            }
        }
    }

    if desc.pipe_write_end {
        if slot.readers == 0 {
            revents |= POLLERR | POLLHUP;
        } else if slot.len < PIPE_BUFFER_SIZE {
            revents |= events & POLLOUT;
        }
    }

    revents
}

/// Add a task to a wait queue. Returns true if added or already present.
fn pipe_wait_queue_push(
    waiters: &mut [DriverTaskHandle; PIPE_MAX_WAITERS],
    count: &mut usize,
    task: DriverTaskHandle,
) -> bool {
    if task.is_null() {
        return false;
    }
    // Check for duplicates
    for i in 0..*count {
        if waiters[i] == task {
            return true;
        }
    }
    if *count >= PIPE_MAX_WAITERS {
        return false;
    }
    waiters[*count] = task;
    *count += 1;
    true
}

/// Remove and return the first waiter from a wait queue (FIFO).
fn pipe_wait_queue_pop(
    waiters: &mut [DriverTaskHandle; PIPE_MAX_WAITERS],
    count: &mut usize,
) -> DriverTaskHandle {
    if *count == 0 {
        return core::ptr::null_mut();
    }
    let task = waiters[0];
    // Shift remaining entries down
    for i in 1..*count {
        waiters[i - 1] = waiters[i];
    }
    *count -= 1;
    waiters[*count] = core::ptr::null_mut();
    task
}

/// Wake one blocked reader on this pipe slot.
fn pipe_wake_one_reader(slot: &mut PipeSlot) {
    let task = pipe_wait_queue_pop(&mut slot.reader_waiters, &mut slot.reader_waiter_count);
    if !task.is_null() {
        let _ = unblock_task(task);
    }
}

/// Wake one blocked writer on this pipe slot.
fn pipe_wake_one_writer(slot: &mut PipeSlot) {
    let task = pipe_wait_queue_pop(&mut slot.writer_waiters, &mut slot.writer_waiter_count);
    if !task.is_null() {
        let _ = unblock_task(task);
    }
}

/// Wake ALL blocked readers (used when writers hit 0 -- EOF).
fn pipe_wake_all_readers(slot: &mut PipeSlot) {
    while slot.reader_waiter_count > 0 {
        let task = pipe_wait_queue_pop(&mut slot.reader_waiters, &mut slot.reader_waiter_count);
        if !task.is_null() {
            let _ = unblock_task(task);
        }
    }
}

/// Wake ALL blocked writers (used when readers hit 0 -- broken pipe).
fn pipe_wake_all_writers(slot: &mut PipeSlot) {
    while slot.writer_waiter_count > 0 {
        let task = pipe_wait_queue_pop(&mut slot.writer_waiters, &mut slot.writer_waiter_count);
        if !task.is_null() {
            let _ = unblock_task(task);
        }
    }
}

fn clone_descriptor_for_dup(src: &FileDescriptor) -> Option<FileDescriptor> {
    let copy = *src;
    if let Some(idx) = copy.tty_index {
        let _ = tty::open_ref(idx);
    }
    if copy.pipe_id == INVALID_PIPE_ID {
        return Some(copy);
    }

    let mut pipe_state = PIPE_STATE.lock();
    let slot = pipe_slot_mut(&mut pipe_state, copy.pipe_id)?;
    if copy.pipe_read_end {
        slot.readers = slot.readers.saturating_add(1);
    }
    if copy.pipe_write_end {
        slot.writers = slot.writers.saturating_add(1);
    }
    Some(copy)
}

fn reset_table(table: &mut FileTableSlot) {
    for desc in table.descriptors.iter_mut() {
        reset_descriptor(desc);
    }
}

fn find_free_table(processes: &mut [FileTableSlot; MAX_PROCESSES]) -> Option<&mut FileTableSlot> {
    for slot in processes.iter_mut() {
        if !slot.in_use {
            return Some(slot);
        }
    }
    None
}

fn table_for_pid<'a>(
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

fn get_descriptor<'a>(table: &'a mut FileTableSlot, fd: c_int) -> Option<&'a mut FileDescriptor> {
    if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
        return None;
    }
    let desc = &mut table.descriptors[fd as usize];
    if !desc.valid {
        return None;
    }
    Some(desc)
}

fn find_free_slot(table: &FileTableSlot) -> Option<usize> {
    find_free_slot_from(table, 0)
}

fn find_free_slot_from(table: &FileTableSlot, min_fd: usize) -> Option<usize> {
    for idx in min_fd..FILEIO_MAX_OPEN_FILES {
        if !table.descriptors[idx].valid {
            return Some(idx);
        }
    }
    None
}

fn ensure_initialized(state: &mut FileioState) {
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

unsafe fn cstr_len(ptr_in: *const c_char) -> usize {
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

unsafe fn path_bytes<'a>(path: *const c_char) -> Option<&'a [u8]> {
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

fn parse_pts_path(path: &[u8]) -> Option<TtyIndex> {
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

/// Bootstrap FD 0 (stdin), 1 (stdout), 2 (stderr) as console descriptors.
///
/// Console descriptors are valid file descriptors that route reads/writes
/// through the platform console/TTY instead of a filesystem.  This ensures
/// every new user process satisfies the POSIX FD bootstrap contract.
fn bootstrap_console_fds(table: &mut FileTableSlot) {
    let console_tty = tty::default_console_tty();

    // FD 0 = stdin (read-only console)
    table.descriptors[0] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_READ,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };
    // FD 1 = stdout (write-only console)
    table.descriptors[1] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };
    // FD 2 = stderr (write-only console)
    table.descriptors[2] = FileDescriptor {
        inode: 0,
        fs: None,
        position: 0,
        flags: FILE_OPEN_WRITE,
        valid: true,
        cloexec: false,
        tty_index: Some(console_tty),
        pipe_id: INVALID_PIPE_ID,
        socket_idx: INVALID_SOCKET_IDX,
        pipe_read_end: false,
        pipe_write_end: false,
    };

    let _ = tty::open_ref(console_tty);
    let _ = tty::open_ref(console_tty);
    let _ = tty::open_ref(console_tty);
}

fn maybe_acquire_controlling_tty_on_open(tty_idx: TtyIndex, flags: u32) {
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

pub fn fileio_create_table_for_process(process_id: u32) -> c_int {
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    with_tables(|kernel, processes| {
        if table_for_pid(kernel, processes, process_id).is_some() {
            return 0;
        }
        let Some(slot) = find_free_table(processes) else {
            return -1;
        };
        reset_table(slot);
        slot.process_id = process_id;
        slot.in_use = true;
        bootstrap_console_fds(slot);
        0
    })
}

pub fn fileio_destroy_table_for_process(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        if let Some(table) = table_for_pid(kernel, processes, process_id) {
            let table_ptr = table as *mut FileTableSlot;
            if table_ptr == kernel_ptr {
                return;
            }
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            unsafe {
                reset_table(&mut *table_ptr);
                (*table_ptr).process_id = INVALID_PROCESS_ID;
                (*table_ptr).in_use = false;
            }
            drop(guard);
        }
    });
}

pub fn fileio_clone_table_for_process(src_process_id: u32, dst_process_id: u32) -> c_int {
    if src_process_id == INVALID_PROCESS_ID || dst_process_id == INVALID_PROCESS_ID {
        return -1;
    }
    if src_process_id == dst_process_id {
        return 0;
    }

    with_tables(|kernel, processes| {
        let src_table = match table_for_pid(kernel, processes, src_process_id) {
            Some(t) => t as *const FileTableSlot,
            None => return -1,
        };

        let dst_slot = match find_free_table(processes) {
            Some(s) => s,
            None => return -1,
        };

        reset_table(dst_slot);
        dst_slot.process_id = dst_process_id;
        dst_slot.in_use = true;

        for (i, src_desc) in unsafe { (*src_table).descriptors.iter().enumerate() } {
            if src_desc.valid {
                let Some(copy) = clone_descriptor_for_dup(src_desc) else {
                    reset_table(dst_slot);
                    dst_slot.process_id = INVALID_PROCESS_ID;
                    dst_slot.in_use = false;
                    return -1;
                };
                dst_slot.descriptors[i] = copy;
            }
        }

        0
    })
}

pub fn file_open_for_process(process_id: u32, path: *const c_char, flags: u32) -> c_int {
    if path.is_null() || (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return -1;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return -1;
    }

    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };

    if path_bytes == b"/dev/tty" {
        return with_tables(|kernel, processes| {
            let tty_idx = match current_task_controlling_tty() {
                Some(idx) => idx,
                None => return -6,
            };

            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.in_use = true;
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(tty_idx);
            desc.pipe_id = INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            if tty::open_ref(tty_idx) < 0 {
                reset_descriptor(desc);
                drop(guard);
                return -1;
            }

            drop(guard);
            slot_idx as c_int
        });
    }

    if path_bytes == b"/dev/ptmx" {
        return with_tables(|kernel, processes| {
            let master_idx_raw = tty::alloc_pty();
            if master_idx_raw < 0 {
                return -1;
            }
            let master_idx = TtyIndex(master_idx_raw as u8);

            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.in_use = true;
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags | O_NOCTTY as u32;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(master_idx);
            desc.pipe_id = INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            if tty::open_ref(master_idx) < 0 {
                reset_descriptor(desc);
                drop(guard);
                return -1;
            }

            drop(guard);
            slot_idx as c_int
        });
    }

    if let Some(slave_idx) = parse_pts_path(path_bytes) {
        // open_pty_slave() atomically validates the slot is still a live
        // PTY slave AND increments its open count, preventing races with
        // concurrent pair teardown.
        return with_tables(|kernel, processes| {
            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.in_use = true;
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            // Atomically validate + open the slave before populating the FD.
            // open_pty_slave also rejects locked slaves (EIO).
            if tty::open_pty_slave(slave_idx) < 0 {
                drop(guard);
                return -1;
            }

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(slave_idx);
            desc.pipe_id = INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            drop(guard);
            maybe_acquire_controlling_tty_on_open(slave_idx, flags);
            slot_idx as c_int
        });
    }

    let create = (flags & USER_FS_OPEN_CREAT) != 0;

    let handle = match vfs_open(path_bytes, create) {
        Ok(h) => h,
        Err(_) => return -1,
    };

    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };
        let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };

        let position = if (flags & FILE_OPEN_APPEND) != 0 {
            match handle.size() {
                Ok(size) => size as usize,
                Err(_) => {
                    drop(guard);
                    return -1;
                }
            }
        } else {
            0
        };

        desc.inode = handle.inode;
        desc.fs = Some(handle.fs);
        desc.flags = flags;
        desc.position = position;
        desc.valid = true;
        desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
        desc.tty_index = None;
        desc.pipe_id = INVALID_PIPE_ID;
        desc.socket_idx = INVALID_SOCKET_IDX;
        desc.pipe_read_end = false;
        desc.pipe_write_end = false;

        drop(guard);
        slot_idx as c_int
    })
}

pub fn file_read_fd(process_id: u32, fd: c_int, buffer: *mut c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }

    // First, check if this FD is a pipe read end by peeking under the file table lock.
    let pipe_info: Option<(u32, bool)> = with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return None;
        };
        if (desc.flags & FILE_OPEN_READ) == 0 {
            drop(guard);
            return None;
        }
        let is_pipe = desc.pipe_id != INVALID_PIPE_ID;
        let pipe_id = desc.pipe_id;
        let is_read_end = desc.pipe_read_end;
        let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
        drop(guard);
        if is_pipe {
            if !is_read_end {
                return None; // writing end, can't read
            }
            Some((pipe_id, is_nonblock))
        } else if desc.socket_idx != INVALID_SOCKET_IDX {
            Some((INVALID_PIPE_ID, is_nonblock))
        } else {
            // Signal "not a pipe" -- fall through to normal path below
            Some((INVALID_PIPE_ID, false))
        }
    });

    let Some((pipe_id, is_nonblock)) = pipe_info else {
        return -1; // Invalid FD or not readable
    };

    // Non-pipe path: handle inside with_tables as before
    if pipe_id == INVALID_PIPE_ID {
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };

            if let Some(tty_idx) = desc.tty_index {
                let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
                drop(guard);
                return tty::read_cooked(tty_idx, buffer as *mut u8, count, is_nonblock);
            }

            if desc.socket_idx != INVALID_SOCKET_IDX {
                let socket_idx = desc.socket_idx;
                drop(guard);
                return socket::recv(socket_idx, buffer as *mut u8, count) as ssize_t;
            }

            let fs = match desc.fs {
                Some(fs) => fs,
                None => {
                    drop(guard);
                    return -1;
                }
            };

            let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, count) };
            let rc = fs.read(desc.inode, desc.position as u64, buf);
            if let Ok(read_len) = rc {
                desc.position = desc.position.saturating_add(read_len);
                drop(guard);
                return read_len as ssize_t;
            }
            drop(guard);
            -1
        });
    }

    // Pipe read path -- operates on PIPE_STATE only (no file table lock held while blocking).
    let mut local = [0u8; 512];
    let mut total = 0usize;
    let mut remaining = count;

    loop {
        let mut need_block = false;

        {
            let mut pipe_state = PIPE_STATE.lock();
            let Some(slot) = pipe_slot_mut(&mut pipe_state, pipe_id) else {
                return if total > 0 { total as ssize_t } else { -1 };
            };

            // Try to read as much as we can
            while remaining > 0 && slot.len > 0 {
                let chunk = remaining.min(local.len());
                let copied = pipe_read_into(slot, &mut local[..chunk]);
                if copied == 0 {
                    break;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        local.as_ptr(),
                        (buffer as *mut u8).add(total),
                        copied,
                    );
                }
                total += copied;
                remaining -= copied;

                // Wake one blocked writer since we freed buffer space
                pipe_wake_one_writer(slot);
            }

            // If we got some data, return it
            if total > 0 {
                return total as ssize_t;
            }

            // Pipe is empty
            if slot.writers == 0 {
                // EOF: no writers left, return 0
                return 0;
            }

            // Non-blocking mode: return EAGAIN (-11)
            if is_nonblock {
                return -11;
            }

            // Need to block: add to wait queue, then block after dropping lock
            if scheduler_is_enabled() != 0 {
                let task = current_task();
                if pipe_wait_queue_push(
                    &mut slot.reader_waiters,
                    &mut slot.reader_waiter_count,
                    task,
                ) {
                    need_block = true;
                }
            }
            // PIPE_STATE lock is dropped here
        }

        if need_block {
            block_current_task();
            // After waking: loop back and re-check pipe state.
            // We might have been woken by: data arrival, writer close (EOF), or signal.
            continue;
        }

        // Scheduler not enabled or wait queue full -- spin-yield fallback
        // (should not happen in normal operation)
        return -1;
    }
}

pub fn file_write_fd(process_id: u32, fd: c_int, buffer: *const c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }

    // Peek at the FD under the file table lock to determine if it's a pipe.
    let pipe_info: Option<(u32, bool)> = with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return None;
        };
        if (desc.flags & FILE_OPEN_WRITE) == 0 {
            drop(guard);
            return None;
        }
        let is_pipe = desc.pipe_id != INVALID_PIPE_ID;
        let pipe_id = desc.pipe_id;
        let is_write_end = desc.pipe_write_end;
        let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
        drop(guard);
        if is_pipe {
            if !is_write_end {
                return None;
            }
            Some((pipe_id, is_nonblock))
        } else if desc.socket_idx != INVALID_SOCKET_IDX {
            Some((INVALID_PIPE_ID, is_nonblock))
        } else {
            Some((INVALID_PIPE_ID, false))
        }
    });

    let Some((pipe_id, is_nonblock)) = pipe_info else {
        return -1;
    };

    // Non-pipe path: handle inside with_tables as before
    if pipe_id == INVALID_PIPE_ID {
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };

            // TTY descriptors: route through the TTY subsystem (output processing).
            if let Some(tty_idx) = desc.tty_index {
                let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
                drop(guard);
                let result = tty::write_bytes(tty_idx, buffer as *const u8, count, is_nonblock);
                return result as ssize_t;
            }

            if desc.socket_idx != INVALID_SOCKET_IDX {
                let socket_idx = desc.socket_idx;
                drop(guard);
                return socket::send(socket_idx, buffer as *const u8, count) as ssize_t;
            }

            let fs = match desc.fs {
                Some(fs) => fs,
                None => {
                    drop(guard);
                    return -1;
                }
            };

            let buf = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
            let rc = fs.write(desc.inode, desc.position as u64, buf);
            if let Ok(written) = rc {
                desc.position = desc.position.saturating_add(written);
                drop(guard);
                return written as ssize_t;
            }
            drop(guard);
            -1
        });
    }

    // Pipe write path -- operates on PIPE_STATE only (no file table lock held while blocking).
    let input = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
    let mut total = 0usize;

    loop {
        let mut need_block = false;

        {
            let mut pipe_state = PIPE_STATE.lock();
            let Some(slot) = pipe_slot_mut(&mut pipe_state, pipe_id) else {
                return if total > 0 { total as ssize_t } else { -1 };
            };

            // No readers: broken pipe
            if slot.readers == 0 {
                return if total > 0 { total as ssize_t } else { -1 };
            }

            // Try to write as much as we can
            if total < count && slot.len < PIPE_BUFFER_SIZE {
                let written = pipe_write_from(slot, &input[total..]);
                if written > 0 {
                    total += written;
                    // Wake one blocked reader since we added data
                    pipe_wake_one_reader(slot);
                }
            }

            // If we wrote everything, return
            if total >= count {
                return total as ssize_t;
            }

            // If we wrote something and pipe is full, return partial write
            // (POSIX: writes <= PIPE_BUF are atomic, but we return what we can)
            if total > 0 {
                return total as ssize_t;
            }

            // Pipe is full and we wrote nothing yet
            if is_nonblock {
                return -11; // EAGAIN
            }

            // Need to block: add to writer wait queue
            if scheduler_is_enabled() != 0 {
                let task = current_task();
                if pipe_wait_queue_push(
                    &mut slot.writer_waiters,
                    &mut slot.writer_waiter_count,
                    task,
                ) {
                    need_block = true;
                }
            }
            // PIPE_STATE lock dropped here
        }

        if need_block {
            block_current_task();
            // After waking: loop back and re-check (data consumed? readers gone?)
            continue;
        }

        return -1;
    }
}

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if desc.socket_idx != INVALID_SOCKET_IDX {
            let _ = socket::close(desc.socket_idx);
            desc.socket_idx = INVALID_SOCKET_IDX;
        }
        reset_descriptor(desc);
        drop(guard);
        0
    })
}

/// POSIX lseek: reposition file offset.
///
/// Returns the new offset on success, or -1 on error (ESPIPE for TTY FDs).
/// The offset parameter is signed to support negative seeks with SEEK_CUR/SEEK_END.
pub fn file_seek_fd(process_id: u32, fd: c_int, offset: i64, whence: u32) -> i64 {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        // TTY descriptors are not seekable (POSIX ESPIPE).
        if desc.tty_index.is_some() {
            drop(guard);
            return -1;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        let size = match fs.stat(desc.inode) {
            Ok(stat) => stat.size as i64,
            Err(_) => {
                drop(guard);
                return -1;
            }
        };

        let new_pos = match whence as u64 {
            SEEK_SET => offset,
            SEEK_CUR => (desc.position as i64).saturating_add(offset),
            SEEK_END => size.saturating_add(offset),
            _ => {
                drop(guard);
                return -1;
            }
        };

        if new_pos < 0 {
            drop(guard);
            return -1;
        }

        desc.position = new_pos as usize;
        drop(guard);
        new_pos
    })
}

pub fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return usize::MAX;
        };
        if !table.in_use {
            return usize::MAX;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let desc = unsafe { get_descriptor(&mut *table_ptr, fd) };
        let size = if let Some(desc) = desc {
            if let Some(fs) = desc.fs {
                match fs.stat(desc.inode) {
                    Ok(stat) => stat.size as usize,
                    Err(_) => usize::MAX,
                }
            } else {
                usize::MAX
            }
        } else {
            usize::MAX
        };
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
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_unlink(path_bytes).is_ok() {
        0
    } else {
        -1
    }
}

pub fn file_mkdir_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_mkdir(path_bytes).is_ok() { 0 } else { -1 }
}

pub fn file_stat_path(path: *const c_char, out_type: &mut u8, out_size: &mut u32) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if let Ok((kind, size)) = vfs_stat(path_bytes) {
        *out_type = kind;
        *out_size = size;
        return 0;
    }
    -1
}

pub fn file_list_path(
    path: *const c_char,
    entries: *mut UserFsEntry,
    max: u32,
    out_count: &mut u32,
) -> c_int {
    if path.is_null() || entries.is_null() || max == 0 {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    let cap = max as usize;
    let out_slice = unsafe { slice::from_raw_parts_mut(entries, cap) };
    match vfs_list(path_bytes, out_slice) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => -1,
    }
}

pub fn file_is_console_fd(process_id: u32, fd: c_int) -> bool {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return false;
        };
        if !table.in_use {
            return false;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let is_console = unsafe { get_descriptor(&mut *table_ptr, fd) }
            .map(|d| d.tty_index.is_some())
            .unwrap_or(false);
        drop(guard);
        is_console
    })
}

/// Return the TTY index for an open file descriptor, or `None` if the FD is
/// not a TTY.  Used by the ioctl dispatcher to route TTY ioctls to the
/// correct per-TTY instance.
pub fn file_get_tty_index(process_id: u32, fd: c_int) -> Option<TtyIndex> {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let tty_idx = unsafe { get_descriptor(&mut *table_ptr, fd) }.and_then(|d| d.tty_index);
        drop(guard);
        tty_idx
    })
}

/// Allocate a new file descriptor pointing to an already-opened TTY.
///
/// Used by `TIOCGPTPEER` to create a slave FD from a master FD without
/// going through the `/dev/pts/N` path.  The caller must have already
/// called `tty::open_pty_peer()` to validate and increment the open count.
pub fn file_open_tty_fd(process_id: u32, tty_idx: TtyIndex, flags: u32) -> c_int {
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };
        let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
        desc.inode = 0;
        desc.fs = None;
        desc.flags = flags;
        desc.position = 0;
        desc.valid = true;
        desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
        desc.tty_index = Some(tty_idx);
        desc.pipe_id = INVALID_PIPE_ID;
        desc.socket_idx = INVALID_SOCKET_IDX;
        desc.pipe_read_end = false;
        desc.pipe_write_end = false;

        drop(guard);
        maybe_acquire_controlling_tty_on_open(tty_idx, flags);
        slot_idx as c_int
    })
}

pub fn file_pipe_create(
    process_id: u32,
    flags: u32,
    out_read_fd: &mut c_int,
    out_write_fd: &mut c_int,
) -> c_int {
    if flags & !(O_NONBLOCK as u32 | O_CLOEXEC as u32) != 0 {
        return -1;
    }

    let pipe_id = match alloc_pipe_slot() {
        Some(id) => id,
        None => return -1,
    };

    let rc = with_tables(|kernel, processes| {
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

        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(read_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };
        table.descriptors[read_idx].valid = true;

        let Some(write_idx) = find_free_slot(table) else {
            reset_descriptor(&mut table.descriptors[read_idx]);
            drop(guard);
            return -1;
        };

        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let cloexec = (flags & O_CLOEXEC as u32) != 0;

        table.descriptors[read_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_READ | if nonblock { O_NONBLOCK as u32 } else { 0 },
            valid: true,
            cloexec,
            tty_index: None,
            pipe_id,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: true,
            pipe_write_end: false,
        };

        table.descriptors[write_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_WRITE | if nonblock { O_NONBLOCK as u32 } else { 0 },
            valid: true,
            cloexec,
            tty_index: None,
            pipe_id,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: false,
            pipe_write_end: true,
        };

        {
            let mut pipe_state = PIPE_STATE.lock();
            let Some(slot) = pipe_slot_mut(&mut pipe_state, pipe_id) else {
                reset_descriptor(&mut table.descriptors[read_idx]);
                reset_descriptor(&mut table.descriptors[write_idx]);
                drop(guard);
                return -1;
            };
            slot.readers = 1;
            slot.writers = 1;
        }

        *out_read_fd = read_idx as c_int;
        *out_write_fd = write_idx as c_int;
        drop(guard);
        0
    });

    if rc != 0 {
        let mut pipe_state = PIPE_STATE.lock();
        if let Some(slot) = pipe_slot_mut(&mut pipe_state, pipe_id) {
            *slot = PipeSlot::new();
        }
    }

    rc
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return POLLNVAL;
        };
        if !table.in_use {
            return POLLNVAL;
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return POLLNVAL;
        };

        if desc.pipe_id != INVALID_PIPE_ID {
            let mut pipe_state = PIPE_STATE.lock();
            let revents = match pipe_slot_mut(&mut pipe_state, desc.pipe_id) {
                Some(slot) => pipe_revents(slot, desc, events),
                None => POLLERR,
            };
            drop(guard);
            return revents;
        }

        if desc.socket_idx != INVALID_SOCKET_IDX {
            let socket_idx = desc.socket_idx;
            let readable = socket::poll_readable(socket_idx) as u16;
            let writable = socket::poll_writable(socket_idx) as u16;
            let mut revents = 0u16;
            if (events & POLLIN) != 0 {
                if (readable & 1) != 0 {
                    revents |= POLLIN;
                }
                revents |= readable & (POLLIN | POLLERR | POLLHUP);
            }
            if (events & POLLOUT) != 0 {
                if (writable & 1) != 0 {
                    revents |= POLLOUT;
                }
                revents |= writable & (POLLOUT | POLLERR | POLLHUP);
            }
            drop(guard);
            return revents;
        }

        if let Some(tty_idx) = desc.tty_index {
            // tty::poll_events() drains hw input, checks IXON stopped
            // state for POLLOUT, and peer/hangup for POLLHUP.
            let revents = tty::poll_events(tty_idx, events);
            drop(guard);
            return revents;
        }

        let mut revents = 0u16;
        if (events & POLLIN) != 0 {
            revents |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            revents |= POLLOUT;
        }
        drop(guard);
        revents
    })
}

// =============================================================================
// POSIX FD operations: dup, dup2, dup3, fcntl, fstat
// =============================================================================

/// Duplicate a file descriptor to the lowest available fd.
/// Returns the new fd on success, -1 on error.
pub fn file_dup_fd(process_id: u32, old_fd: c_int) -> c_int {
    file_dup_fd_min(process_id, old_fd, 0)
}

/// Duplicate a file descriptor to the lowest available fd >= min_fd.
/// Used by both dup() (min_fd=0) and fcntl F_DUPFD.
fn file_dup_fd_min(process_id: u32, old_fd: c_int, min_fd: usize) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        let Some(new_idx) = find_free_slot_from(table, min_fd) else {
            drop(guard);
            return -1;
        };

        table.descriptors[new_idx] = copy;
        // dup() clears FD_CLOEXEC on the new descriptor
        table.descriptors[new_idx].cloexec = false;
        drop(guard);
        new_idx as c_int
    })
}

/// Duplicate old_fd to exactly new_fd. If new_fd is already open it is closed first.
/// If old_fd == new_fd, return new_fd without closing.
/// Returns new_fd on success, -1 on error.
pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }
    if old_fd == new_fd {
        // Verify old_fd is valid, return new_fd if so
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let valid = unsafe { get_descriptor(&mut *table_ptr, old_fd) }.is_some();
            drop(guard);
            if valid { new_fd } else { -1 }
        });
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        // Silently close new_fd if it was open
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        // dup2 clears FD_CLOEXEC on the new descriptor
        table.descriptors[new_fd as usize].cloexec = false;
        drop(guard);
        new_fd
    })
}

/// Duplicate old_fd to exactly new_fd with flags.
/// Unlike dup2, dup3 fails if old_fd == new_fd.
/// The only supported flag is O_CLOEXEC (mapped to bit 0 of flags).
/// Returns new_fd on success, -1 on error.
pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return -1;
    }
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        // dup3 sets cloexec based on flags
        table.descriptors[new_fd as usize].cloexec = (flags & FD_CLOEXEC as u32) != 0;
        drop(guard);
        new_fd
    })
}

/// Minimal fcntl implementation.
///
/// Supported commands:
/// - F_DUPFD: duplicate fd to lowest available >= arg
/// - F_GETFD: get FD_CLOEXEC flag
/// - F_SETFD: set FD_CLOEXEC flag
/// - F_GETFL: get file status flags (open mode)
/// - F_SETFL: set file status flags (currently only APPEND)
///
/// Returns command-specific value on success, -1 on error.
pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = if desc.cloexec { FD_CLOEXEC as i64 } else { 0 };
            drop(guard);
            val
        }),
        F_SETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            desc.cloexec = (arg & FD_CLOEXEC) != 0;
            drop(guard);
            0
        }),
        F_GETFL => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = desc.flags as i64;
            drop(guard);
            val
        }),
        F_SETFL => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let mode_bits = desc.flags & (FILE_OPEN_READ | FILE_OPEN_WRITE);
            let sticky_flags = desc.flags & (O_NOCTTY as u32);
            let mut next_flags = mode_bits | sticky_flags | (arg as u32 & FILE_OPEN_APPEND);
            if (arg & O_NONBLOCK) != 0 {
                next_flags |= O_NONBLOCK as u32;
            }
            desc.flags = next_flags;
            if desc.socket_idx != INVALID_SOCKET_IDX {
                let _ = socket::set_nonblocking(desc.socket_idx, (arg & O_NONBLOCK) != 0);
            }
            drop(guard);
            0
        }),
        _ => -1,
    }
}

/// Close all file descriptors with FD_CLOEXEC set for a process.
///
/// Called during exec() to satisfy the POSIX close-on-exec contract.
/// Console FDs (0/1/2) are never marked cloexec by default, so they
/// survive exec transitions automatically.
pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let table = unsafe { &mut *table_ptr };
        for desc in table.descriptors.iter_mut() {
            if desc.valid && desc.cloexec {
                reset_descriptor(desc);
            }
        }
        drop(guard);
    });
}

/// Stat an open file descriptor.
/// Returns 0 on success and fills out_stat, -1 on error.
pub fn file_fstat_fd(process_id: u32, fd: c_int, out_stat: &mut UserFsStat) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        // TTY descriptors report as character devices with size 0.
        if desc.tty_index.is_some() {
            out_stat.type_ = slopos_abi::fs::FS_TYPE_CHARDEV;
            out_stat.size = 0;
            drop(guard);
            return 0;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        match fs.stat(desc.inode) {
            Ok(stat) => {
                out_stat.type_ = stat.file_type as u8;
                out_stat.size = stat.size as u32;
                drop(guard);
                0
            }
            Err(_) => {
                drop(guard);
                -1
            }
        }
    })
}

pub fn fileio_open_socket_fd(process_id: u32, socket_idx: u32) -> i32 {
    with_tables(|kernel, processes| {
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

        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        table.descriptors[slot_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_READ | FILE_OPEN_WRITE,
            valid: true,
            cloexec: false,
            tty_index: None,
            pipe_id: INVALID_PIPE_ID,
            socket_idx,
            pipe_read_end: false,
            pipe_write_end: false,
        };
        let _ = socket::set_nonblocking(socket_idx, false);
        drop(guard);
        slot_idx as i32
    })
}

pub fn fileio_get_socket_idx(process_id: u32, fd: i32) -> Option<u32> {
    with_tables(|kernel, processes| {
        let table = table_for_pid(kernel, processes, process_id)?;
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let out = unsafe { get_descriptor(&mut *table_ptr, fd) }
            .map(|d| d.socket_idx)
            .filter(|idx| *idx != INVALID_SOCKET_IDX);
        drop(guard);
        out
    })
}
