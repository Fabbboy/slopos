use core::ffi::{c_char, c_int};
use core::mem::{self, MaybeUninit};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use crate::ramfs::{
    ramfs_acquire_node, ramfs_create_file, ramfs_find_node, ramfs_get_size, ramfs_node_release,
    ramfs_node_retain, ramfs_read_bytes, ramfs_remove_file, ramfs_write_bytes, ramfs_node_t,
    RAMFS_TYPE_FILE,
};

type ssize_t = isize;

const FILE_OPEN_READ: u32 = 1 << 0;
const FILE_OPEN_WRITE: u32 = 1 << 1;
const FILE_OPEN_CREAT: u32 = 1 << 2;
const FILE_OPEN_APPEND: u32 = 1 << 3;

const FILEIO_MAX_OPEN_FILES: usize = 32;
const MAX_PROCESSES: usize = 256;
const INVALID_PROCESS_ID: u32 = 0xFFFF_FFFF;

#[derive(Copy, Clone)]
struct FileDescriptor {
    node: *mut ramfs_node_t,
    position: usize,
    flags: u32,
    valid: bool,
}

impl FileDescriptor {
    const fn new() -> Self {
        Self {
            node: ptr::null_mut(),
            position: 0,
            flags: 0,
            valid: false,
        }
    }
}

struct FileTableSlot {
    process_id: u32,
    in_use: bool,
    lock: Mutex<()>,
    descriptors: [FileDescriptor; FILEIO_MAX_OPEN_FILES],
}

impl FileTableSlot {
    const fn new(in_use: bool) -> Self {
        Self {
            process_id: INVALID_PROCESS_ID,
            in_use,
            lock: Mutex::new(()),
            descriptors: [FileDescriptor::new(); FILEIO_MAX_OPEN_FILES],
        }
    }
}

struct FileioState {
    kernel: FileTableSlot,
    processes: [FileTableSlot; MAX_PROCESSES],
}

impl FileioState {
    fn new() -> Self {
        let kernel = FileTableSlot::new(true);
        let mut processes: [MaybeUninit<FileTableSlot>; MAX_PROCESSES] =
            unsafe { MaybeUninit::uninit().assume_init() };
        for slot in processes.iter_mut() {
            slot.write(FileTableSlot::new(false));
        }
        let processes = unsafe { mem::transmute::<_, [FileTableSlot; MAX_PROCESSES]>(processes) };
        Self { kernel, processes }
    }
}

static FILEIO_STATE: Mutex<Option<FileioState>> = Mutex::new(None);
static FILEIO_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn with_state<R>(f: impl FnOnce(&mut FileioState) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    if guard.is_none() {
        *guard = Some(FileioState::new());
    }
    let state = guard.as_mut().unwrap();
    f(state)
}

fn reset_descriptor(desc: &mut FileDescriptor) {
    unsafe {
        if !desc.node.is_null() {
            ramfs_node_release(desc.node);
        }
    }
    desc.node = ptr::null_mut();
    desc.position = 0;
    desc.flags = 0;
    desc.valid = false;
}

fn reset_table(table: &mut FileTableSlot) {
    for desc in table.descriptors.iter_mut() {
        reset_descriptor(desc);
    }
}

fn find_free_table(state: &mut FileioState) -> Option<&mut FileTableSlot> {
    for slot in state.processes.iter_mut() {
        if !slot.in_use {
            return Some(slot);
        }
    }
    None
}

fn table_for_pid<'a>(state: &'a mut FileioState, pid: u32) -> Option<&'a mut FileTableSlot> {
    if pid == INVALID_PROCESS_ID {
        return Some(&mut state.kernel);
    }
    for slot in state.processes.iter_mut() {
        if slot.in_use && slot.process_id == pid {
            return Some(slot);
        }
    }
    None
}

fn get_descriptor<'a>(
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

fn find_free_slot(table: &FileTableSlot) -> Option<usize> {
    for (idx, desc) in table.descriptors.iter().enumerate() {
        if !desc.valid {
            return Some(idx);
        }
    }
    None
}

fn ensure_initialized() {
    if FILEIO_INITIALIZED.swap(true, Ordering::AcqRel) {
        return;
    }
    with_state(|state| {
        reset_table(&mut state.kernel);
        for slot in state.processes.iter_mut() {
            reset_table(slot);
            slot.process_id = INVALID_PROCESS_ID;
            slot.in_use = false;
        }
    });
}

#[no_mangle]
pub extern "C" fn fileio_create_table_for_process(process_id: u32) -> c_int {
    ensure_initialized();
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    with_state(|state| {
        if table_for_pid(state, process_id).is_some() {
            return 0;
        }
        let Some(slot) = find_free_table(state) else {
            return -1;
        };
        reset_table(slot);
        slot.process_id = process_id;
        slot.in_use = true;
        0
    })
}

#[no_mangle]
pub extern "C" fn fileio_destroy_table_for_process(process_id: u32) {
    ensure_initialized();
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_state(|state| {
        if let Some(table) = table_for_pid(state, process_id) {
            if ptr::eq(table, &state.kernel) {
                return;
            }
            let guard = table.lock.lock();
            reset_table(table);
            table.process_id = INVALID_PROCESS_ID;
            table.in_use = false;
            drop(guard);
        }
    });
}

#[no_mangle]
pub extern "C" fn file_open_for_process(
    process_id: u32,
    path: *const c_char,
    flags: u32,
) -> c_int {
    ensure_initialized();
    if path.is_null() || (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return -1;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return -1;
    }

    with_state(|state| {
        let table = table_for_pid(state, process_id).unwrap_or_else(|| {
            // Auto-create slot if possible.
            let slot = find_free_table(state);
            slot.unwrap_or(&mut state.kernel)
        });

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let guard = table.lock.lock();

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let mut node = unsafe { ramfs_acquire_node(path) };
        if node.is_null() && (flags & FILE_OPEN_CREAT) != 0 {
            node = unsafe { ramfs_create_file(path, ptr::null(), 0) };
            if !node.is_null() {
                unsafe { ramfs_node_retain(node) };
            }
        }

        if node.is_null() || unsafe { (*node).type_ } != RAMFS_TYPE_FILE {
            if !node.is_null() {
                unsafe { ramfs_node_release(node) };
            }
            drop(guard);
            return -1;
        }

        let desc = &mut table.descriptors[slot_idx];
        desc.node = node;
        desc.flags = flags;
        desc.position = if (flags & FILE_OPEN_APPEND) != 0 {
            unsafe { ramfs_get_size(node) }
        } else {
            0
        };
        desc.valid = true;

        drop(guard);
        slot_idx as c_int
    })
}

#[no_mangle]
pub extern "C" fn file_read_fd(
    process_id: u32,
    fd: c_int,
    buffer: *mut c_char,
    count: usize,
) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }
    ensure_initialized();

    with_state(|state| {
        let Some(table) = table_for_pid(state, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let guard = table.lock.lock();
        let Some(desc) = get_descriptor(table, fd) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_READ) == 0
            || desc.node.is_null()
            || unsafe { (*desc.node).type_ } != RAMFS_TYPE_FILE
        {
            drop(guard);
            return -1;
        }

        let mut read_len: usize = 0;
        let rc = unsafe {
            ramfs_read_bytes(
                desc.node,
                desc.position,
                buffer as *mut _,
                count,
                &mut read_len as *mut usize,
            )
        };
        if rc == 0 {
            desc.position = desc.position.saturating_add(read_len);
        }
        drop(guard);
        if rc == 0 {
            read_len as ssize_t
        } else {
            -1
        }
    })
}

#[no_mangle]
pub extern "C" fn file_write_fd(
    process_id: u32,
    fd: c_int,
    buffer: *const c_char,
    count: usize,
) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }
    ensure_initialized();

    with_state(|state| {
        let Some(table) = table_for_pid(state, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let guard = table.lock.lock();
        let Some(desc) = get_descriptor(table, fd) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_WRITE) == 0
            || desc.node.is_null()
            || unsafe { (*desc.node).type_ } != RAMFS_TYPE_FILE
        {
            drop(guard);
            return -1;
        }

        let rc =
            unsafe { ramfs_write_bytes(desc.node, desc.position, buffer as *const _, count) };
        if rc == 0 {
            desc.position = desc.position.saturating_add(count);
        }
        drop(guard);
        if rc == 0 {
            count as ssize_t
        } else {
            -1
        }
    })
}

#[no_mangle]
pub extern "C" fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    ensure_initialized();
    with_state(|state| {
        let Some(table) = table_for_pid(state, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let guard = table.lock.lock();
        let Some(desc) = get_descriptor(table, fd) else {
            drop(guard);
            return -1;
        };
        reset_descriptor(desc);
        drop(guard);
        0
    })
}

#[no_mangle]
pub extern "C" fn file_seek_fd(
    process_id: u32,
    fd: c_int,
    offset: u64,
    whence: c_int,
) -> c_int {
    ensure_initialized();
    with_state(|state| {
        let Some(table) = table_for_pid(state, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let guard = table.lock.lock();
        let Some(desc) = get_descriptor(table, fd) else {
            drop(guard);
            return -1;
        };
        if desc.node.is_null() || unsafe { (*desc.node).type_ } != RAMFS_TYPE_FILE {
            drop(guard);
            return -1;
        }
        let size = unsafe { ramfs_get_size(desc.node) };
        let delta = offset as usize;
        let new_pos = match whence {
            0 => {
                if delta > size {
                    drop(guard);
                    return -1;
                }
                delta
            }
            1 => desc
                .position
                .checked_add(delta)
                .filter(|p| *p <= size)
                .unwrap_or_else(|| {
                    drop(guard);
                    usize::MAX
                }),
            2 => {
                if delta > size {
                    drop(guard);
                    return -1;
                }
                size - delta
            }
            _ => {
                drop(guard);
                return -1;
            }
        };
        if new_pos == usize::MAX {
            return -1;
        }
        desc.position = new_pos;
        drop(guard);
        0
    })
}

#[no_mangle]
pub extern "C" fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    ensure_initialized();
    with_state(|state| {
        let Some(table) = table_for_pid(state, process_id) else {
            return usize::MAX;
        };
        if !table.in_use {
            return usize::MAX;
        }
        let guard = table.lock.lock();
        let desc = get_descriptor(table, fd);
        let size = if let Some(desc) = desc {
            if !desc.node.is_null() && unsafe { (*desc.node).type_ } == RAMFS_TYPE_FILE {
                unsafe { ramfs_get_size(desc.node) }
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

#[no_mangle]
pub extern "C" fn file_exists_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let node = unsafe { ramfs_find_node(path) };
    if node.is_null() || unsafe { (*node).type_ } != RAMFS_TYPE_FILE {
        0
    } else {
        1
    }
}

#[no_mangle]
pub extern "C" fn file_unlink_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    unsafe { ramfs_remove_file(path) }
}

