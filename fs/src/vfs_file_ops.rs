use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLIN, POLLOUT};
use slopos_sync::IrqMutex;

use crate::vfs::{FileSystem, InodeId};

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

pub fn vfs_open_handle_flags(
    path: &[u8],
    flags: crate::vfs::ops::VfsOpenFlags,
) -> Result<usize, slopos_abi::Errno> {
    let opened = crate::vfs::ops::vfs_open_flags(path, flags).map_err(|e| e.to_errno())?;
    let mut table = OPEN_VNODES.lock();
    for (idx, slot) in table.slots.iter_mut().enumerate() {
        if !slot.valid {
            *slot = OpenVnodeSlot {
                fs: Some(opened.fs),
                inode: opened.inode,
                refcount: 1,
                valid: true,
            };
            return Ok(idx);
        }
    }
    Err(slopos_abi::Errno::ENFILE)
}

impl FileOps for VfsFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Regular
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, _flags: u32) -> isize {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(slot) = table.slots.get(handle) else {
                return Errno::EBADF.as_isize();
            };
            if !slot.valid {
                return Errno::EBADF.as_isize();
            }
            let Some(fs) = slot.fs else {
                return Errno::EBADF.as_isize();
            };
            (fs, slot.inode)
        };

        let mut staging = [0u8; IO_STAGING_SIZE];
        let mut total = 0usize;
        let want = buf.len();

        while total < want {
            let chunk = (want - total).min(IO_STAGING_SIZE);
            match fs.read(inode, offset + total as u64, &mut staging[..chunk]) {
                Ok(0) => break,
                Ok(n) => match buf.copy_in(total, &staging[..n]) {
                    Ok(w) => {
                        total += w;
                        if w < n {
                            break;
                        }
                    }
                    Err(e) => {
                        return if total > 0 {
                            total as isize
                        } else {
                            e.as_isize()
                        };
                    }
                },
                Err(_) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EIO.as_isize()
                    };
                }
            }
        }
        total as isize
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, offset: u64, _flags: u32) -> isize {
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(slot) = table.slots.get(handle) else {
                return Errno::EBADF.as_isize();
            };
            if !slot.valid {
                return Errno::EBADF.as_isize();
            }
            let Some(fs) = slot.fs else {
                return Errno::EBADF.as_isize();
            };
            (fs, slot.inode)
        };

        let mut staging = [0u8; IO_STAGING_SIZE];
        let mut total = 0usize;
        let want = buf.len();

        while total < want {
            let chunk = (want - total).min(IO_STAGING_SIZE);
            let n = match buf.copy_out(total, &mut staging[..chunk]) {
                Ok(n) if n > 0 => n,
                Ok(_) => break,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.as_isize()
                    };
                }
            };
            match fs.write(inode, offset + total as u64, &staging[..n]) {
                Ok(w) => {
                    total += w;
                    if w < n {
                        break;
                    }
                }
                Err(_) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EIO.as_isize()
                    };
                }
            }
        }
        total as isize
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
                return Errno::EBADF.raw();
            };
            if !slot.valid {
                return Errno::EBADF.raw();
            }
            let Some(fs) = slot.fs else {
                return Errno::EBADF.raw();
            };
            (fs, slot.inode)
        };
        match fs.stat(inode) {
            Ok(stat) => {
                out.type_ = stat.file_type as u8;
                out.size = stat.size as u32;
                0
            }
            Err(_) => Errno::EPERM.raw(),
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
