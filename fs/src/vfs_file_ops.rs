use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLIN, POLLOUT};
use slopos_sync::{IrqMutex, LOCK_LEVEL_REGISTRY};

use crate::vfs::{FileSystem, InodeId};

const MAX_OPEN_VNODES: usize = 256;

/// Bits used for the slot index in the handle encoding.
const SLOT_BITS: u32 = 10;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1; // 0x3FF — supports up to 1024 slots

// ---------------------------------------------------------------------------
// VnodeHandle — type-safe handle for VFS open vnode objects
// ---------------------------------------------------------------------------

/// Opaque handle identifying an open vnode slot.
///
/// Encodes a slot index and a generation counter so that stale handles
/// (from a closed file whose slot was recycled) are reliably rejected.
/// The encoding is `(generation << SLOT_BITS) | slot_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct VnodeHandle(u32);

impl VnodeHandle {
    pub(crate) fn new(slot: usize, generation: u32) -> Self {
        Self(((generation as usize) << SLOT_BITS | (slot & SLOT_MASK)) as u32)
    }

    pub(crate) fn slot(self) -> usize {
        (self.0 as usize) & SLOT_MASK
    }

    pub(crate) fn generation(self) -> u32 {
        ((self.0 as usize) >> SLOT_BITS) as u32
    }

    /// Convert to usize for storage in OpenFileEntry.handle.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from usize stored in OpenFileEntry.handle.
    pub fn from_usize(v: usize) -> Self {
        Self(v as u32)
    }
}

/// Global generation counter for vnode slot allocation.
static VNODE_GENERATION: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy)]
struct OpenVnodeSlot {
    fs: Option<&'static dyn FileSystem>,
    inode: InodeId,
    refcount: u16,
    generation: u32,
    valid: bool,
}

impl OpenVnodeSlot {
    const fn new() -> Self {
        Self {
            fs: None,
            inode: 0,
            refcount: 0,
            generation: 0,
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

static OPEN_VNODES: IrqMutex<OpenVnodeTable> =
    IrqMutex::new(OpenVnodeTable::new(), LOCK_LEVEL_REGISTRY);

/// Validate a vnode handle against the table, returning the slot index.
fn validate_vnode(table: &OpenVnodeTable, handle: VnodeHandle) -> Option<usize> {
    let slot_idx = handle.slot();
    if slot_idx >= MAX_OPEN_VNODES {
        return None;
    }
    let slot = &table.slots[slot_idx];
    if !slot.valid || slot.generation != handle.generation() {
        return None;
    }
    Some(slot_idx)
}

pub struct VfsFileOps;

pub static VFS_FILE_OPS: VfsFileOps = VfsFileOps;

pub fn vfs_open_handle_flags(
    path: &[u8],
    flags: crate::vfs::ops::VfsOpenFlags,
) -> Result<usize, slopos_abi::Errno> {
    let opened = crate::vfs::ops::vfs_open_flags(path, flags).map_err(|e| e.to_errno())?;
    let gn = VNODE_GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut table = OPEN_VNODES.lock();
    for (idx, slot) in table.slots.iter_mut().enumerate() {
        if !slot.valid {
            *slot = OpenVnodeSlot {
                fs: Some(opened.fs),
                inode: opened.inode,
                refcount: 1,
                generation: gn,
                valid: true,
            };
            return Ok(VnodeHandle::new(idx, gn).as_usize());
        }
    }
    Err(slopos_abi::Errno::ENFILE)
}

impl FileOps for VfsFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Regular
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, offset: u64, _flags: u32) -> isize {
        let vh = VnodeHandle::from_usize(handle);
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(idx) = validate_vnode(&table, vh) else {
                return Errno::EBADF.as_isize();
            };
            let slot = &table.slots[idx];
            let Some(fs) = slot.fs else {
                return Errno::EBADF.as_isize();
            };
            (fs, slot.inode)
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
        let vh = VnodeHandle::from_usize(handle);
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(idx) = validate_vnode(&table, vh) else {
                return Errno::EBADF.as_isize();
            };
            let slot = &table.slots[idx];
            let Some(fs) = slot.fs else {
                return Errno::EBADF.as_isize();
            };
            (fs, slot.inode)
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
        let vh = VnodeHandle::from_usize(handle);
        let mut table = OPEN_VNODES.lock();
        let Some(idx) = validate_vnode(&table, vh) else {
            return;
        };
        let slot = &mut table.slots[idx];
        if slot.refcount > 1 {
            slot.refcount -= 1;
            return;
        }
        *slot = OpenVnodeSlot::new();
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        let vh = VnodeHandle::from_usize(handle);
        let mut table = OPEN_VNODES.lock();
        let idx = validate_vnode(&table, vh)?;
        table.slots[idx].refcount = table.slots[idx].refcount.saturating_add(1);
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
        let vh = VnodeHandle::from_usize(handle);
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let Some(idx) = validate_vnode(&table, vh) else {
                return Errno::EBADF.raw();
            };
            let slot = &table.slots[idx];
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
        let vh = VnodeHandle::from_usize(handle);
        let (fs, inode) = {
            let table = OPEN_VNODES.lock();
            let idx = validate_vnode(&table, vh)?;
            let slot = &table.slots[idx];
            (slot.fs?, slot.inode)
        };
        fs.stat(inode).ok().map(|s| s.size)
    }
}
