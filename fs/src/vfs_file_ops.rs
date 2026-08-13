use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLIN, POLLOUT};
use slopos_ostd::KArc;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use crate::vfs::{FileSystem, InodeId};

/// Open vnodes system-wide — this kernel's `fs.file-max`.
///
/// Must stay well above the per-process descriptor limit. At parity, one
/// process opening its full allowance exhausts the table for every other
/// process on the machine, which turns a per-process bound into a
/// system-wide denial. `SLOT_BITS` caps it at 1024.
const MAX_OPEN_VNODES: usize = 1024;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]).
const SLOT_BITS: u32 = 10;

const _: () = assert!(
    MAX_OPEN_VNODES <= 1 << SLOT_BITS,
    "MAX_OPEN_VNODES exceeds what SLOT_BITS can address in a packed handle"
);

/// One open vnode: the filesystem it lives on and its inode. The slot index
/// and generation are owned by the [`HandleTable`], so a handle left over
/// from a closed file whose slot was recycled resolves to a typed miss.
struct OpenVnode {
    fs: &'static dyn FileSystem,
    inode: InodeId,
}

static OPEN_VNODES: SpinLock<Option<HandleTable<OpenVnode>>> =
    SpinLock::new(None, lock_class!("OPEN_VNODES", LOCK_LEVEL_REGISTRY));

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
        })
        .map(|h| h.pack(SLOT_BITS))
        .map_err(|_| slopos_abi::Errno::ENFILE)
    })
}

/// Sole owner of one `OpenVnode` table entry; dropping it closes the vnode.
#[derive(slopos_ostd::Charged)]
struct VnodeBacking {
    handle: usize,
    object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(VnodeBacking);

impl FileBacking for VnodeBacking {}

impl Drop for VnodeBacking {
    fn drop(&mut self) {
        let h = Handle::<OpenVnode>::unpack(self.handle, SLOT_BITS);
        with_table(|t| {
            let _ = t.remove(h);
        });
    }
}

/// Wrap a handle from [`vfs_open_handle_flags`] into its owning backing,
/// charged to `account` — the opener.
///
/// On allocation failure or a refused charge the vnode entry is removed before
/// returning, so the table row never outlives the attempt to own it.
pub(crate) fn vnode_backing(handle: usize, account: AccountId) -> Option<KArc<dyn FileBacking>> {
    let release = || {
        let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
        with_table(|t| {
            let _ = t.remove(h);
        });
    };
    let Ok(reservation) = try_charge::<ObjectRow>(account, 1) else {
        release();
        return None;
    };
    match KArc::try_new(VnodeBacking {
        handle,
        object_charge: Charge::commit(reservation),
    }) {
        Ok(backing) => Some(backing),
        Err(_) => {
            release();
            None
        }
    }
}

impl FileOps for VfsFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Regular
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, _flags: u32) -> isize {
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.as_isize();
        };

        if buf.is_empty() {
            return 0;
        }
        // Sized to the request, capped at the staging bound: a shell reading a
        // script one byte at a time must not cost a 4 KiB kernel allocation per
        // byte. Larger requests still iterate the loop below at IO_STAGING_SIZE.
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(buf.len().min(IO_STAGING_SIZE)) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let want = buf.len();

        while total < want {
            let chunk = (want - total).min(staging.len());
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

        if buf.is_empty() {
            return 0;
        }
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(buf.len().min(IO_STAGING_SIZE)) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let want = buf.len();

        while total < want {
            let chunk = (want - total).min(staging.len());
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
