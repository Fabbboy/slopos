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

use crate::fileio::OpenMode;

use crate::vfs::traits::same_filesystem;
use crate::vfs::{FileSystem, InodeId};

/// Open vnodes system-wide — this kernel's `fs.file-max`.
///
/// Must stay well above the per-process descriptor limit: at parity, one
/// process opening its full allowance exhausts the table for every other
/// process on the machine.
const MAX_OPEN_VNODES: usize = 1024;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]).
const SLOT_BITS: u32 = 10;

const _: () = assert!(
    MAX_OPEN_VNODES <= 1 << SLOT_BITS,
    "MAX_OPEN_VNODES exceeds what SLOT_BITS can address in a packed handle"
);

/// One open vnode. The slot index and generation are owned by the
/// [`HandleTable`], so a handle whose slot has been recycled resolves to a
/// typed miss.
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

/// The capacity of the filesystem behind an open vnode handle — `fstatfs(2)`.
/// `None` when the handle names no live vnode, which the caller reports as
/// `EBADF`.
pub fn vfs_file_statfs(handle: usize) -> Option<crate::vfs::VfsResult<crate::vfs::FsStats>> {
    let (fs, _) = resolve(handle)?;
    Some(fs.statfs())
}

/// The `(filesystem, inode)` an open vnode handle names, for `mmap(2)`: a
/// file mapping is keyed on the inode, not on the descriptor that opened it.
pub fn vfs_file_inode(handle: usize) -> Option<(&'static dyn FileSystem, InodeId)> {
    resolve(handle)
}

pub struct VfsFileOps;

pub static VFS_FILE_OPS: VfsFileOps = VfsFileOps;

pub fn vfs_open_handle_flags(
    path: &[u8],
    flags: crate::vfs::ops::VfsOpenFlags,
) -> Result<usize, slopos_abi::Errno> {
    // A create may have *just* made this name, and re-resolving it is then a
    // second lookup of something this call is responsible for rather than a
    // check on something it merely found.
    let created = flags.create;
    let opened = crate::vfs::ops::vfs_open_flags(path, flags).map_err(|e| e.to_errno())?;
    // Before the table row exists, and a failure fails the open: an untracked
    // descriptor reads to `unlink` as an unreferenced inode, which is the one
    // error that frees a file somebody is reading.
    //
    // A refusal because the inode is detached is a name that is gone, which
    // `open(2)` reports as `ENOENT`; only a full table is `ENFILE`.
    if let Err(e) = crate::vfs::orphan::open_ref(opened.fs, opened.inode) {
        return Err(match e {
            crate::vfs::orphan::OpenRefError::Detached => slopos_abi::Errno::ENOENT,
            crate::vfs::orphan::OpenRefError::TableFull => slopos_abi::Errno::ENFILE,
        });
    }
    // Between the walk above and `open_ref` nothing held the inode: an
    // `unlink` landing in that window frees it, and ext2's `InodeId` is a bare
    // inode number with no generation, so the number is reallocated and this
    // descriptor would name whatever file got it. Ramfs is immune — its ids
    // carry a generation — but the rule has to hold for the filesystem that
    // does not.
    //
    // The re-check is what makes the window harmless rather than closed: an
    // inode reallocated *and* re-bound to this same path in between still
    // passes, which needs an inode cache and a parent lock held across the
    // walk. What it does close is the case that matters, where the name is
    // gone or now denotes something else.
    if !created && !still_resolves(path, opened.fs, opened.inode) {
        crate::vfs::orphan::close_ref(opened.fs, opened.inode);
        return Err(slopos_abi::Errno::ENOENT);
    }

    let inserted = with_table(|t| {
        t.insert(OpenVnode {
            fs: opened.fs,
            inode: opened.inode,
        })
        .map(|h| h.pack(SLOT_BITS))
        .map_err(|_| slopos_abi::Errno::ENFILE)
    });
    if inserted.is_err() {
        crate::vfs::orphan::close_ref(opened.fs, opened.inode);
    }
    inserted
}

/// Whether `path` still names exactly `(fs, inode)`.
///
/// Its own frame: `resolve_path` stages a canonical path buffer, and the
/// caller already holds one.
#[inline(never)]
fn still_resolves(path: &[u8], fs: &'static dyn FileSystem, inode: InodeId) -> bool {
    match crate::vfs::resolve_path(path) {
        Ok(again) => again.inode == inode && same_filesystem(again.fs, fs),
        Err(_) => false,
    }
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
        release_vnode(self.handle);
    }
}

/// Remove a vnode table row and release the open reference it held.
///
/// Panic-free by construction, which `check_drop_panic_free.sh` scans for:
/// every step is a table removal, a bounded scan, a relaxed atomic, or a
/// filesystem's own non-blocking reclaim.
///
/// Where the deferred free runs is the filesystem's answer, not this
/// function's. A blocking one is handed to that filesystem's writeback thread,
/// because this can be a descriptor dropping on the task-exit path under a
/// preempt guard, where a sleeping mutex and a park on block I/O are not
/// available. A non-blocking one runs inline, because for an in-memory
/// filesystem nothing else would ever run it.
fn release_vnode(handle: usize) {
    let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
    let removed = with_table(|t| t.remove(h).ok().map(|v| (v.fs, v.inode)));
    let Some((fs, inode)) = removed else {
        return;
    };
    if crate::vfs::orphan::close_ref(fs, inode) {
        crate::vfs::orphan::drain_or_wake(fs);
    }
}

/// Wrap a handle from [`vfs_open_handle_flags`] into its owning backing,
/// charged to `account`. On allocation failure or a refused charge the vnode
/// entry is removed, so the table row never outlives the attempt to own it.
pub(crate) fn vnode_backing(handle: usize, account: AccountId) -> Option<KArc<dyn FileBacking>> {
    let release = || release_vnode(handle);
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

/// A vnode handle over `(fs, inode)` without a path walk, so a kernel test can
/// drive the [`FileOps`] loops, where the coverage-boundary rules live.
#[cfg(feature = "tests")]
pub(crate) fn vnode_handle_for_tests(fs: &'static dyn FileSystem, inode: InodeId) -> Option<usize> {
    with_table(|t| {
        t.insert(OpenVnode { fs, inode })
            .ok()
            .map(|h| h.pack(SLOT_BITS))
    })
}

#[cfg(feature = "tests")]
pub(crate) fn drop_vnode_for_tests(handle: usize) {
    let h = Handle::<OpenVnode>::unpack(handle, SLOT_BITS);
    let _ = with_table(|t| t.remove(h));
}

/// One chunk of a `read(2)`, taken from the inode's page set when it covers
/// `offset` and from the filesystem otherwise.
///
/// Both clips are load-bearing. The length stays the filesystem's answer,
/// because the set holds whole pages and the tail of its last one is zero-fill
/// past EOF; and a chunk that *starts* below the set is cut where coverage
/// begins, or the rest would be read from the filesystem while the set holds
/// newer bytes. A short answer is a coverage boundary, not EOF: only `Ok(0)`
/// ends a read.
pub(crate) fn read_chunk(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
) -> crate::vfs::VfsResult<usize> {
    match crate::filemap::coverage_at(fs, inode, offset) {
        crate::filemap::Coverage::Here => {
            let size = fs.stat(inode)?.size;
            if offset >= size {
                return Ok(0);
            }
            let want = clip(buf.len(), size - offset);
            match crate::filemap::read_through(fs, inode, offset, &mut buf[..want]) {
                Some(n) => Ok(n),
                None => fs.read(inode, offset, &mut buf[..want]),
            }
        }
        crate::filemap::Coverage::Above(boundary) => {
            let want = clip(buf.len(), boundary - offset);
            fs.read(inode, offset, &mut buf[..want])
        }
        crate::filemap::Coverage::Absent => fs.read(inode, offset, buf),
    }
}

/// One chunk of a `write(2)`. A range the page set covers is written into the
/// set, which is the authority for it; a chunk starting below the set is cut
/// at the boundary, or the bytes past it would be invisible to every mapper
/// and then overwritten by the set's own writeback.
///
/// A write reaching past the end of the file goes to *both*: the set cannot
/// lengthen a file, and writeback is clamped to the length. Both copies then
/// hold the same bytes, so the later writeback is idempotent.
pub(crate) fn write_chunk(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &[u8],
) -> crate::vfs::VfsResult<usize> {
    match crate::filemap::coverage_at(fs, inode, offset) {
        crate::filemap::Coverage::Above(boundary) => {
            let want = clip(buf.len(), boundary - offset);
            return fs.write(inode, offset, &buf[..want]);
        }
        crate::filemap::Coverage::Absent => return fs.write(inode, offset, buf),
        crate::filemap::Coverage::Here => {}
    }
    let size = fs.stat(inode)?.size;
    let written = match crate::filemap::write_through(fs, inode, offset, buf) {
        crate::filemap::WriteThrough::Served(n) => n,
        crate::filemap::WriteThrough::Interrupted => {
            return Err(crate::vfs::VfsError::Interrupted);
        }
        crate::filemap::WriteThrough::NotCovered => return fs.write(inode, offset, buf),
    };
    let end = offset + written as u64;
    if end > size {
        let tail_start = offset.max(size);
        let from = usize::try_from(tail_start - offset).unwrap_or(written);
        fs.write(inode, tail_start, &buf[from..written])?;
    }
    Ok(written)
}

/// `len` clipped to `limit`, saturating a `u64` that does not fit a `usize`.
#[inline]
fn clip(len: usize, limit: u64) -> usize {
    usize::try_from(limit).unwrap_or(usize::MAX).min(len)
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
        // Sized to the request, capped at the staging bound: a one-byte read
        // must not cost a 4 KiB kernel allocation.
        let mut staging = match slopos_ostd::KVec::<u8>::zeroed(buf.len().min(IO_STAGING_SIZE)) {
            Ok(v) => v,
            Err(_) => return Errno::ENOMEM.as_isize(),
        };
        let mut total = 0usize;
        let want = buf.len();

        while total < want {
            let chunk = (want - total).min(staging.len());
            match read_chunk(fs, inode, offset + total as u64, &mut staging[..chunk]) {
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
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.to_errno().as_isize()
                    };
                }
            }
        }
        total as isize
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, offset: u64, flags: u32) -> isize {
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.as_isize();
        };

        if buf.is_empty() {
            return 0;
        }
        // POSIX: O_APPEND resolves the offset from the current size at write
        // time, not at open. A snapshot taken at open lets two descriptions
        // overwrite each other, so an append-only log is not append-only.
        let offset = if OpenMode::from_bits(flags).contains(OpenMode::APPEND) {
            match fs.stat(inode) {
                Ok(st) => st.size,
                Err(_) => return Errno::EIO.as_isize(),
            }
        } else {
            offset
        };
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
            // A short chunk is a coverage boundary, not a full device: the
            // loop continues from there and stops only when nothing moves.
            match write_chunk(fs, inode, offset + total as u64, &staging[..n]) {
                Ok(0) => break,
                Ok(w) => total += w,
                Err(e) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        e.to_errno().as_isize()
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

    fn sync(&self, handle: usize, data_only: bool) -> i32 {
        let Some((fs, inode)) = resolve(handle) else {
            return Errno::EBADF.raw();
        };
        // Before the commit, not after: the page set holds bytes the
        // filesystem has not seen.
        if let Err(e) = crate::filemap::flush_inode(fs, inode) {
            return e.to_errno().raw();
        }
        match fs.sync_inode(inode, data_only) {
            Ok(()) => 0,
            Err(e) => e.to_errno().raw(),
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
