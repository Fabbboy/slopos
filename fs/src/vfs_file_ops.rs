use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::syscall::{POLLIN, POLLOUT};
use slopos_sync::IrqMutex;

use crate::vfs::{FileSystem, InodeId, vfs_open};

const MAX_OPEN_VNODES: usize = 256;

#[derive(Clone, Copy)]
struct OpenVnodeSlot {
    fs: Option<&'static dyn FileSystem>,
    inode: InodeId,
    refcount: u16,
    valid: bool,
}

impl OpenVnodeSlot {
    const fn new() -> Self {
        Self {
            fs: None,
            inode: 0,
            refcount: 0,
            valid: false,
        }
    }
}

struct OpenVnodeTable {
    slots: [OpenVnodeSlot; MAX_OPEN_VNODES],
}

impl OpenVnodeTable {
    const fn new() -> Self {
        Self {
            slots: [const { OpenVnodeSlot::new() }; MAX_OPEN_VNODES],
        }
    }
}

static OPEN_VNODES: IrqMutex<OpenVnodeTable> = IrqMutex::new(OpenVnodeTable::new());

pub struct VfsFileOps;

pub static VFS_FILE_OPS: VfsFileOps = VfsFileOps;

pub fn vfs_open_handle(path: &[u8], create: bool) -> Option<usize> {
    let opened = vfs_open(path, create).ok()?;
    let mut table = OPEN_VNODES.lock();
    for (idx, slot) in table.slots.iter_mut().enumerate() {
        if !slot.valid {
            *slot = OpenVnodeSlot {
                fs: Some(opened.fs),
                inode: opened.inode,
                refcount: 1,
                valid: true,
            };
            return Some(idx);
        }
    }
    None
}

impl FileOps for VfsFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Regular
    }

    fn read(&self, handle: usize, buf: &mut dyn slopos_abi::io::IoBuf, offset: u64, _flags: u32) -> isize {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(slot) = table.slots.get(handle) else {
                return -1;
            };
            if !slot.valid {
                return -1;
            }
            let Some(fs) = slot.fs else {
                return -1;
            };
            (fs, slot.inode)
        };
        match fs.read(inode, offset, buf) {
            Ok(n) => n as isize,
            Err(_) => -1,
        }
    }

    fn write(&self, handle: usize, buf: &mut dyn slopos_abi::io::IoBuf, offset: u64, _flags: u32) -> isize {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(slot) = table.slots.get(handle) else {
                return -1;
            };
            if !slot.valid {
                return -1;
            }
            let Some(fs) = slot.fs else {
                return -1;
            };
            (fs, slot.inode)
        };
        match fs.write(inode, offset, buf) {
            Ok(n) => n as isize,
            Err(_) => -1,
        }
    }

    fn release(&self, handle: usize) {
        let mut table = OPEN_VNODES.lock();
        let Some(slot) = table.slots.get_mut(handle) else {
            return;
        };
        if !slot.valid {
            return;
        }
        if slot.refcount > 1 {
            slot.refcount -= 1;
            return;
        }
        *slot = OpenVnodeSlot::new();
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let mut table = OPEN_VNODES.lock();
        let slot = table.slots.get_mut(handle)?;
        if !slot.valid {
            return None;
        }
        slot.refcount = slot.refcount.saturating_add(1);
        Some(handle)
    }

    fn poll_events(&self, _handle: usize, events: u16) -> u16 {
        let mut revents = 0u16;
        if (events & POLLIN) != 0 {
            revents |= POLLIN;
        }
        if (events & POLLOUT) != 0 {
            revents |= POLLOUT;
        }
        revents
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(slot) = table.slots.get(handle) else {
                return -1;
            };
            if !slot.valid {
                return -1;
            }
            let Some(fs) = slot.fs else {
                return -1;
            };
            (fs, slot.inode)
        };
        match fs.stat(inode) {
            Ok(stat) => {
                out.type_ = stat.file_type as u8;
                out.size = stat.size as u32;
                0
            }
            Err(_) => -1,
        }
    }

    fn seekable(&self) -> bool {
        true
    }

    fn size(&self, handle: usize) -> Option<u64> {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let slot = table.slots.get(handle)?;
            if !slot.valid {
                return None;
            }
            (slot.fs?, slot.inode)
        };
        fs.stat(inode).ok().map(|s| s.size)
    }
}
