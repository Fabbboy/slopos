use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLIN, POLLOUT};
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use crate::vfs::{FileSystem, InodeId};

const MAX_OPEN_VNODES: usize = 256;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]).
const SLOT_BITS: u32 = 10;

/// One open vnode: the filesystem it lives on, its inode, and a per-open
/// refcount (bumped by `dup`, dropped by `release`). The slot index and
/// generation are owned by the [`HandleTable`], so a handle left over from a
/// closed file whose slot was recycled resolves to a typed miss.
struct OpenVnode {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    refcount: u16,
}

static OPEN_VNODES: SpinLock<Option<HandleTable<OpenVnode>>> =
    SpinLock::new(None, LOCK_LEVEL_REGISTRY);

fn with_table<R>(f: impl FnOnce(&mut HandleTable<OpenVnode>) -> R) -> R {
    let mut guard = OPEN_VNODES.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_OPEN_VNODES).expect("vnode table alloc")
    });
    f(table)
}

/// Resolve a packed fd handle to its `(filesystem, inode)`, releasing the
/// table lock before the caller performs the (possibly blocking) I/O.
fn resolve(handle: usize) -> Option<(&'static dyn FileSystem, InodeId)> {
    let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
    with_table(|t| t.get(h).map(|v| (v.fs, v.inode)).ok())
}

pub struct VfsFileOps;

pub static VFS_FILE_OPS: VfsFileOps = VfsFileOps;

pub fn vfs_open_handle_flags(
    path: &[u8],
    flags: crate::vfs::ops::VfsOpenFlags,
) -> Result<usize, slopos_abi::Errno> {
    let opened = crate::vfs::ops::vfs_open_flags(path, flags).map_err(|e| e.to_errno())?;
    with_table(|t| {
        t.insert(OpenVnode {
            fs: opened.fs,
            inode: opened.inode,
            refcount: 1,
        })
        .map(|h| h.pack(SLOT_BITS))
        .map_err(|_| slopos_abi::Errno::ENFILE)
    })
}

impl FileOps for VfsFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Regular
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, _flags: u32) -> isize {
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.as_isize();
        };

        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
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
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.as_isize();
        };

        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
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
        let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
        with_table(|t| {
            // Drop the open after `get_mut`'s borrow ends so the `remove`
            // re-borrow does not overlap it.
            let drop_it = match t.get_mut(h) {
                Ok(v) if v.refcount > 1 => {
                    v.refcount -= 1;
                    false
                }
                Ok(_) => true,
                Err(_) => false,
            };
            if drop_it {
                let _ = t.remove(h);
            }
        });
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
        with_table(|t| match t.get_mut(h) {
            Ok(v) => {
                v.refcount = v.refcount.saturating_add(1);
                Some(handle)
            }
            Err(_) => None,
        })
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
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.raw();
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
        let (fs, inode) = resolve(handle)?;
        fs.stat(inode).ok().map(|s| s.size)
    }
}
