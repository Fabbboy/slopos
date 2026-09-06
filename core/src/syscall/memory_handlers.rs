use slopos_abi::Errno;
use slopos_abi::file_ops::FileKind;
use slopos_abi::syscall::{MAP_PRIVATE, MAP_SHARED, MS_ASYNC, MS_INVALIDATE, MS_SYNC, PROT_WRITE};
use slopos_fs::fileio::OpenMode;
use slopos_fs::filemap;
use slopos_fs::vfs::FileType;
use slopos_ostd::process::ProcessId;

use crate::syscall::args::{Fd, RawFd};
use crate::syscall::result::SyscallResult;

const PAGE_SIZE: u64 = 4096;

/// `mmap(2)` of a regular file (G14).
///
/// Both sharing modes are populated eagerly from the inode's page set: the #PF
/// handler cannot sleep, so nothing can be faulted in from the device later. A
/// mapping past EOF is therefore refused rather than deferring a `SIGBUS` this
/// kernel has no path to deliver.
///
/// A writable shared mapping publishes through the page set that every
/// `read(2)` is routed through, so it requires `OpenMode::WRITE` — the seal
/// and the read-only mount are enforced against the descriptor by `open(2)`.
#[inline(never)]
fn mmap_regular_file(
    process: ProcessId,
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    offset: u64,
    handle: usize,
    mode: OpenMode,
) -> Result<u64, Errno> {
    let shared = flags & MAP_SHARED != 0;
    let private = flags & MAP_PRIVATE != 0;
    if shared == private {
        return Err(Errno::EINVAL);
    }
    if length == 0 || offset & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    let writable = shared && prot & PROT_WRITE != 0;
    if writable && !mode.contains(OpenMode::WRITE) {
        return Err(Errno::EACCES);
    }

    let (fs, inode) = slopos_fs::vfs_file_ops::vfs_file_inode(handle).ok_or(Errno::EBADF)?;
    let stat = fs.stat(inode).map_err(|e| e.to_errno())?;
    if stat.file_type != FileType::Regular {
        return Err(Errno::ENODEV);
    }
    let end = offset.checked_add(length).ok_or(Errno::EINVAL)?;
    if end > stat.size {
        return Err(Errno::EINVAL);
    }

    let first_page = offset / PAGE_SIZE;
    let last_page = (end - 1) / PAGE_SIZE;
    let page_count = u32::try_from(last_page - first_page + 1).map_err(|_| Errno::ENOMEM)?;

    let (map, paddrs) =
        filemap::acquire(fs, inode, first_page, page_count, writable).map_err(|e| e.to_errno())?;

    let result = if shared {
        slopos_mm::process_vm::process_vm_mmap_file_shared(
            process,
            addr,
            length,
            prot,
            flags,
            map,
            paddrs.as_slice(),
        )
    } else {
        // The copy is taken here, so the region needs no file backing.
        slopos_mm::process_vm::process_vm_mmap_file_private(
            process,
            addr,
            length,
            prot,
            flags,
            paddrs.as_slice(),
        )
    };
    // Drops `acquire`'s own reference; a failed mmap must not pin the set.
    filemap::release(map, 1);

    if result == 0 {
        Err(Errno::ENOMEM)
    } else {
        Ok(result)
    }
}

/// `msync(2)`. `MS_INVALIDATE` is refused: there is one page set per inode,
/// so there is no second copy to invalidate against.
#[inline(never)]
fn msync_range(process: ProcessId, addr: u64, length: u64, flags: u64) -> Result<(), Errno> {
    if flags & !(MS_ASYNC | MS_SYNC | MS_INVALIDATE) != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & MS_INVALIDATE != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 {
        return Err(Errno::EINVAL);
    }
    if addr & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::EINVAL);
    }
    if length == 0 {
        return Ok(());
    }
    let size = length.checked_add(PAGE_SIZE - 1).ok_or(Errno::EINVAL)? & !(PAGE_SIZE - 1);
    let end = addr.checked_add(size).ok_or(Errno::EINVAL)?;

    // A hole in the range is POSIX's `ENOMEM`; a mapped range with nothing
    // file-backed in it succeeds having written nothing.
    let maps = slopos_mm::process_vm::process_vm_collect_filemaps(process, addr, end)
        .ok_or(Errno::ENOMEM)?;
    for map in maps.iter() {
        if flags & MS_SYNC != 0 {
            filemap::flush(*map).map_err(|e| e.to_errno())?;
        } else {
            filemap::queue_flush(*map);
        }
    }
    Ok(())
}

define_syscall!(syscall_brk
    (ctx, new_brk: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let result = slopos_mm::process_vm::process_vm_brk(process_id.process().ok_or(Errno::ESRCH)?, new_brk);
    if result == 0 && new_brk != 0 {
        Err(Errno::ENOMEM)
    } else {
        Ok(result)
    }
});

define_syscall!(syscall_mmap
    (ctx, addr: u64, length: u64, prot: u64, flags: u64, fd: RawFd, offset: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    if fd.is_present() {
        let (kind, handle, mode) =
            slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd.raw())
                .ok_or(Errno::EBADF)?;
        let process = process_id.process().ok_or(Errno::ESRCH)?;
        match kind {
            FileKind::Memfd => {
                let result = slopos_mm::process_vm::process_vm_mmap_shared(
                    process,
                    addr,
                    length,
                    prot,
                    flags,
                    offset,
                    handle,
                );
                if result == 0 {
                    return Err(Errno::ENOMEM);
                }
                return Ok(result);
            }
            FileKind::Regular => {
                return mmap_regular_file(
                    process, addr, length, prot, flags, offset, handle, mode,
                );
            }
            _ => return Err(Errno::EINVAL),
        }
    }

    let process = process_id.process().ok_or(Errno::ESRCH)?;
    let mut result = slopos_mm::process_vm::process_vm_mmap(
        process, addr, length, prot, flags, fd.raw() as i64, offset,
    );
    if result == 0 {
        // Reclaim-and-retry lives here, not in `try_charge`: the account arena
        // takes no locks, and a syscall boundary is where blocking is legal.
        let want = length.div_ceil(4096).try_into().unwrap_or(u32::MAX);
        if slopos_mm::reclaim_pages(want) != 0 {
            result = slopos_mm::process_vm::process_vm_mmap(
                process, addr, length, prot, flags, fd.raw() as i64, offset,
            );
        }
    }
    if result == 0 {
        Err(Errno::ENOMEM)
    } else {
        Ok(result)
    }
});

define_syscall!(syscall_munmap
    (ctx, addr: u64, length: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    // No shootdown here: pages are invalidated locally as they go, and freed
    // frames cannot be reallocated until every CPU has quiesced.
    let rc = slopos_mm::process_vm::process_vm_munmap(process_id.process().ok_or(Errno::ESRCH)?, addr, length);
    if rc < 0 {
        Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL))
    } else {
        Ok(())
    }
});

define_syscall!(syscall_mprotect
    (ctx, addr: u64, length: u64, prot: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let rc = slopos_mm::process_vm::process_vm_mprotect(process_id.process().ok_or(Errno::ESRCH)?, addr, length, prot);
    if rc < 0 {
        Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL))
    } else {
        Ok(())
    }
});

define_syscall!(syscall_memfd_create
    (ctx, flags: u32)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let (handle, ops, backing) =
        slopos_mm::memfd::memfd_create(flags, process_id.account()).ok_or(Errno::ENFILE)?;
    let fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        process_id,
        ops,
        handle,
        Some(backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    if fd < 0 {
        // A failed install drops the backing, which runs the memfd teardown.
        return Err(Errno::from_raw(fd).unwrap_or(Errno::ENOMEM));
    }
    Ok(fd as u64)
});

define_syscall!(syscall_ftruncate
    (ctx, fd: Fd, size: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let (kind, handle, _mode) = slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd.raw())
        .ok_or(Errno::EBADF)?;
    if kind != slopos_abi::file_ops::FileKind::Memfd {
        return Err(Errno::EINVAL);
    }
    let rc = slopos_mm::memfd::memfd_ftruncate(handle, size as usize);
    if rc < 0 {
        Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL))
    } else {
        Ok(())
    }
});

define_syscall!(syscall_msync
    (ctx, addr: u64, length: u64, flags: u64)
    cap(NoneSelf)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    msync_range(pid.process().ok_or(Errno::ESRCH)?, addr, length, flags)
});

#[allow(dead_code)]
type _Unused = SyscallResult;
