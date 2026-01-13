use core::ffi::{c_char, c_int};
use core::{ptr, slice};

use slopos_lib::IrqMutex;

use crate::blockdev::{CallbackBlockDevice, CapacityFn, MemoryBlockDevice, ReadFn, WriteFn};
use crate::ext2::{Ext2Error, Ext2Fs};
use slopos_abi::fs::{
    FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_UNKNOWN, USER_FS_OPEN_CREAT, USER_PATH_MAX,
    UserFsEntry,
};

enum BlockDeviceType {
    None,
    Memory(MemoryBlockDevice),
    Callback(CallbackBlockDevice),
}

struct Ext2State {
    device: BlockDeviceType,
}

impl Ext2State {
    const fn new() -> Self {
        Self {
            device: BlockDeviceType::None,
        }
    }

    fn is_available(&self) -> bool {
        !matches!(self.device, BlockDeviceType::None)
    }
}

static EXT2_STATE: IrqMutex<Ext2State> = IrqMutex::new(Ext2State::new());

const MAX_PATH: usize = USER_PATH_MAX;

pub fn ext2_is_available() -> bool {
    let state = EXT2_STATE.lock();
    state.is_available()
}

pub fn ext2_init_with_image(image: &[u8]) -> c_int {
    if image.is_empty() {
        return -1;
    }
    let mut state = EXT2_STATE.lock();
    if state.is_available() {
        return 0;
    }
    let mut device = match MemoryBlockDevice::allocate(image.len()) {
        Some(dev) => dev,
        None => return -1,
    };
    unsafe {
        ptr::copy_nonoverlapping(image.as_ptr(), device.as_mut_ptr(), image.len());
    }
    if Ext2Fs::init_internal(&mut device).is_err() {
        return -1;
    }
    state.device = BlockDeviceType::Memory(device);
    0
}

pub fn ext2_init_with_callbacks(
    read_fn: ReadFn,
    write_fn: WriteFn,
    capacity_fn: CapacityFn,
) -> c_int {
    let mut state = EXT2_STATE.lock();
    if state.is_available() {
        return 0;
    }
    let mut device = CallbackBlockDevice::new(read_fn, write_fn, capacity_fn);
    if Ext2Fs::init_internal(&mut device).is_err() {
        return -1;
    }
    state.device = BlockDeviceType::Callback(device);
    0
}

pub fn ext2_open(path: *const c_char, flags: u32) -> Result<u32, Ext2Error> {
    let bytes = unsafe { path_bytes(path) }.ok_or(Ext2Error::PathNotFound)?;
    with_fs(|fs| {
        if let Ok(inode) = fs.resolve_path(bytes) {
            let inode_data = fs.read_inode(inode)?;
            if !inode_data.is_regular_file() {
                return Err(Ext2Error::NotFile);
            }
            return Ok(inode);
        }
        if (flags & USER_FS_OPEN_CREAT) == 0 {
            return Err(Ext2Error::PathNotFound);
        }
        let (parent, name) = split_parent(bytes).ok_or(Ext2Error::PathNotFound)?;
        let parent_inode = fs.resolve_path(parent)?;
        fs.create_file(parent_inode, name)
    })
}

pub fn ext2_read(inode: u32, offset: u32, buffer: &mut [u8]) -> Result<usize, Ext2Error> {
    with_fs(|fs| fs.read_file(inode, offset, buffer))
}

pub fn ext2_write(inode: u32, offset: u32, buffer: &[u8]) -> Result<usize, Ext2Error> {
    with_fs(|fs| fs.write_file(inode, offset, buffer))
}

pub fn ext2_get_size(inode: u32) -> Result<u32, Ext2Error> {
    with_fs(|fs| Ok(fs.read_inode(inode)?.size))
}

pub fn ext2_stat(path: *const c_char) -> Result<(u8, u32), Ext2Error> {
    let bytes = unsafe { path_bytes(path) }.ok_or(Ext2Error::PathNotFound)?;
    with_fs(|fs| {
        let inode = fs.resolve_path(bytes)?;
        let inode_data = fs.read_inode(inode)?;
        let kind = if inode_data.is_directory() {
            FS_TYPE_DIRECTORY
        } else if inode_data.is_regular_file() {
            FS_TYPE_FILE
        } else {
            FS_TYPE_UNKNOWN
        };
        Ok((kind, inode_data.size))
    })
}

pub fn ext2_list(path: *const c_char, entries: &mut [UserFsEntry]) -> Result<usize, Ext2Error> {
    let bytes = unsafe { path_bytes(path) }.ok_or(Ext2Error::PathNotFound)?;
    with_fs(|fs| {
        let inode = fs.resolve_path(bytes)?;
        let mut count = 0usize;
        let mut inodes = [0u32; 64];
        let cap = entries.len().min(inodes.len());
        fs.for_each_dir_entry(inode, |entry| {
            if count >= cap {
                return false;
            }
            let mut out = UserFsEntry::new();
            let nlen = entry.name.len().min(out.name.len() - 1);
            out.name[..nlen].copy_from_slice(&entry.name[..nlen]);
            out.name[nlen] = 0;
            inodes[count] = entry.inode;
            entries[count] = out;
            count += 1;
            true
        })?;
        for idx in 0..count {
            let inode_data = match fs.read_inode(inodes[idx]) {
                Ok(data) => data,
                Err(_) => continue,
            };
            entries[idx].type_ = if inode_data.is_directory() {
                FS_TYPE_DIRECTORY
            } else if inode_data.is_regular_file() {
                FS_TYPE_FILE
            } else {
                FS_TYPE_UNKNOWN
            };
            entries[idx].size = inode_data.size;
        }
        Ok(count)
    })
}

pub fn ext2_mkdir(path: *const c_char) -> Result<(), Ext2Error> {
    let bytes = unsafe { path_bytes(path) }.ok_or(Ext2Error::PathNotFound)?;
    with_fs(|fs| {
        let (parent, name) = split_parent(bytes).ok_or(Ext2Error::PathNotFound)?;
        let parent_inode = fs.resolve_path(parent)?;
        fs.create_directory(parent_inode, name)?;
        Ok(())
    })
}

pub fn ext2_unlink(path: *const c_char) -> Result<(), Ext2Error> {
    let bytes = unsafe { path_bytes(path) }.ok_or(Ext2Error::PathNotFound)?;
    with_fs(|fs| fs.remove_path(bytes))
}

fn with_fs<R>(f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> Result<R, Ext2Error> {
    let mut state = EXT2_STATE.lock();
    match &mut state.device {
        BlockDeviceType::None => Err(Ext2Error::InvalidSuperblock),
        BlockDeviceType::Memory(device) => {
            let mut fs = Ext2Fs::init_internal(device)?;
            f(&mut fs)
        }
        BlockDeviceType::Callback(device) => {
            let mut fs = Ext2Fs::init_internal(device)?;
            f(&mut fs)
        }
    }
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
        Some(slice::from_raw_parts(path as *const u8, len.min(MAX_PATH)))
    }
}

fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return None;
    }
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 1 {
        return None;
    }
    let trimmed = &path[..end];
    let mut idx = trimmed.len();
    while idx > 0 && trimmed[idx - 1] != b'/' {
        idx -= 1;
    }
    if idx == 0 {
        return None;
    }
    let parent = if idx == 1 {
        &trimmed[..1]
    } else {
        &trimmed[..idx - 1]
    };
    let name = &trimmed[idx..];
    if name.is_empty() {
        return None;
    }
    Some((parent, name))
}
