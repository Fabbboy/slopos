use core::ffi::{c_char, c_int};
use core::mem::{self, MaybeUninit};
use core::slice;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use slopos_abi::fs::{FS_TYPE_FILE, UserFsEntry};

use crate::ext2_state::{
    ext2_get_size, ext2_list, ext2_mkdir, ext2_open, ext2_read, ext2_stat, ext2_unlink, ext2_write,
};

#[allow(non_camel_case_types)]
type ssize_t = isize;

const FILE_OPEN_READ: u32 = 1 << 0;
const FILE_OPEN_WRITE: u32 = 1 << 1;
const FILE_OPEN_APPEND: u32 = 1 << 3;

use slopos_mm::mm_constants::{INVALID_PROCESS_ID, MAX_PROCESSES};

const FILEIO_MAX_OPEN_FILES: usize = 32;

#[derive(Copy, Clone)]
struct FileDescriptor {
    inode: u32,
    position: usize,
    flags: u32,
    valid: bool,
}

impl FileDescriptor {
    const fn new() -> Self {
        Self {
            inode: 0,
            position: 0,
            flags: 0,
            valid: false,
        }
    }
}

unsafe impl Send for FileDescriptor {}

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

unsafe impl Send for FileTableSlot {}

struct FileioStateStorage {
    initialized: bool,
    kernel: MaybeUninit<FileTableSlot>,
    processes: [MaybeUninit<FileTableSlot>; MAX_PROCESSES],
}

impl FileioStateStorage {
    const fn uninitialized() -> Self {
        let processes: [MaybeUninit<FileTableSlot>; MAX_PROCESSES] = unsafe {
            MaybeUninit::<[MaybeUninit<FileTableSlot>; MAX_PROCESSES]>::uninit().assume_init()
        };
        Self {
            initialized: false,
            kernel: MaybeUninit::uninit(),
            processes,
        }
    }
}

unsafe impl Send for FileioStateStorage {}

static FILEIO_STATE: Mutex<FileioStateStorage> = Mutex::new(FileioStateStorage::uninitialized());
static FILEIO_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn with_state<R>(f: impl FnOnce(&mut FileioStateStorage) -> R) -> R {
    let mut guard = FILEIO_STATE.lock();
    f(&mut *guard)
}

fn with_tables<R>(
    f: impl FnOnce(&mut FileTableSlot, &mut [FileTableSlot; MAX_PROCESSES]) -> R,
) -> R {
    with_state(|state| {
        ensure_initialized(state);
        let kernel = unsafe { state.kernel.assume_init_mut() };
        let processes = unsafe {
            mem::transmute::<_, &mut [FileTableSlot; MAX_PROCESSES]>(&mut state.processes)
        };
        f(kernel, processes)
    })
}

fn reset_descriptor(desc: &mut FileDescriptor) {
    desc.inode = 0;
    desc.position = 0;
    desc.flags = 0;
    desc.valid = false;
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
    for (idx, desc) in table.descriptors.iter().enumerate() {
        if !desc.valid {
            return Some(idx);
        }
    }
    None
}

fn ensure_initialized(state: &mut FileioStateStorage) {
    if FILEIO_INITIALIZED.swap(true, Ordering::AcqRel) {
        return;
    }

    state.kernel.write(FileTableSlot::new(true));
    for slot in state.processes.iter_mut() {
        slot.write(FileTableSlot::new(false));
    }
    // Now that memory is populated, clear descriptors and mark free.
    let kernel = unsafe { state.kernel.assume_init_mut() };
    reset_table(kernel);
    let processes =
        unsafe { mem::transmute::<_, &mut [FileTableSlot; MAX_PROCESSES]>(&mut state.processes) };
    for slot in processes.iter_mut() {
        reset_table(slot);
        slot.process_id = INVALID_PROCESS_ID;
        slot.in_use = false;
    }
    state.initialized = true;
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
pub fn file_open_for_process(process_id: u32, path: *const c_char, flags: u32) -> c_int {
    if path.is_null() || (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return -1;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return -1;
    }

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
        let inode = match ext2_open(path, flags) {
            Ok(inode) => inode,
            Err(_) => {
                drop(guard);
                return -1;
            }
        };
        let position = if (flags & FILE_OPEN_APPEND) != 0 {
            match ext2_get_size(inode) {
                Ok(size) => size as usize,
                Err(_) => {
                    drop(guard);
                    return -1;
                }
            }
        } else {
            0
        };
        desc.inode = inode;
        desc.flags = flags;
        desc.position = position;
        desc.valid = true;

        drop(guard);
        slot_idx as c_int
    })
}
pub fn file_read_fd(process_id: u32, fd: c_int, buffer: *mut c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
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
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_READ) == 0 {
            drop(guard);
            return -1;
        }

        let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, count) };
        let rc = ext2_read(desc.inode, desc.position as u32, buf);
        if let Ok(read_len) = rc {
            desc.position = desc.position.saturating_add(read_len);
            drop(guard);
            return read_len as ssize_t;
        }
        drop(guard);
        -1
    })
}
pub fn file_write_fd(process_id: u32, fd: c_int, buffer: *const c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
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
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if (desc.flags & FILE_OPEN_WRITE) == 0 {
            drop(guard);
            return -1;
        }

        let buf = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
        let rc = ext2_write(desc.inode, desc.position as u32, buf);
        if let Ok(written) = rc {
            desc.position = desc.position.saturating_add(written);
            drop(guard);
            return written as ssize_t;
        }
        drop(guard);
        -1
    })
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
        reset_descriptor(desc);
        drop(guard);
        0
    })
}
pub fn file_seek_fd(process_id: u32, fd: c_int, offset: u64, whence: c_int) -> c_int {
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
        let size = match ext2_get_size(desc.inode) {
            Ok(size) => size as usize,
            Err(_) => {
                drop(guard);
                return -1;
            }
        };
        let delta = offset as usize;
        let new_pos = match whence {
            0 => {
                if delta > size {
                    drop(guard);
                    return -1;
                }
                delta
            }
            1 => {
                if let Some(p) = desc.position.checked_add(delta) {
                    if p <= size {
                        p
                    } else {
                        drop(guard);
                        return -1;
                    }
                } else {
                    drop(guard);
                    return -1;
                }
            }
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
        desc.position = new_pos;
        drop(guard);
        0
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
            match ext2_get_size(desc.inode) {
                Ok(size) => size as usize,
                Err(_) => usize::MAX,
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
    let rc = ext2_stat(path);
    if let Ok((kind, _)) = rc {
        return if kind == FS_TYPE_FILE { 1 } else { 0 };
    }
    0
}
pub fn file_unlink_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    if ext2_unlink(path).is_ok() { 0 } else { -1 }
}

pub fn file_mkdir_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    if ext2_mkdir(path).is_ok() { 0 } else { -1 }
}

pub fn file_stat_path(path: *const c_char, out_type: &mut u8, out_size: &mut u32) -> c_int {
    if path.is_null() {
        return -1;
    }
    if let Ok((kind, size)) = ext2_stat(path) {
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
    let cap = max as usize;
    let out_slice = unsafe { slice::from_raw_parts_mut(entries, cap) };
    match ext2_list(path, out_slice) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => -1,
    }
}
