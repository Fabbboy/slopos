use slopos_abi::Errno;

use crate::syscall::args::{Fd, RawFd};
use crate::syscall::result::SyscallResult;

define_syscall!(syscall_brk
    (ctx, new_brk: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let result = slopos_mm::process_vm::process_vm_brk(process_id, new_brk);
    if result == 0 && new_brk != 0 {
        Err(Errno::ENOMEM)
    } else {
        Ok(result)
    }
});

define_syscall!(syscall_mmap
    (ctx, addr: u64, length: u64, prot: u64, flags: u64, fd: RawFd, offset: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    if fd.is_present() {
        let (kind, handle) =
            slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd.raw())
                .ok_or(Errno::EBADF)?;
        if kind != slopos_abi::file_ops::FileKind::Memfd {
            return Err(Errno::EINVAL);
        }
        let result = slopos_mm::process_vm::process_vm_mmap_shared(
            process_id,
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

    let result = slopos_mm::process_vm::process_vm_mmap(
        process_id,
        addr,
        length,
        prot,
        flags,
        fd.raw() as i64,
        offset,
    );
    if result == 0 {
        Err(Errno::ENOMEM)
    } else {
        Ok(result)
    }
});

define_syscall!(syscall_munmap
    (ctx, addr: u64, length: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    // Each page was invalidated locally as it went, and the freed frames cannot
    // be reallocated until every CPU has quiesced.
    let rc = slopos_mm::process_vm::process_vm_munmap(process_id, addr, length);
    if rc < 0 {
        Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL))
    } else {
        Ok(())
    }
});

define_syscall!(syscall_mprotect
    (ctx, addr: u64, length: u64, prot: u64)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let rc = slopos_mm::process_vm::process_vm_mprotect(process_id, addr, length, prot);
    if rc < 0 {
        Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL))
    } else {
        Ok(())
    }
});

define_syscall!(syscall_memfd_create
    (ctx, flags: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let (handle, ops, backing) = slopos_mm::memfd::memfd_create(flags).ok_or(Errno::ENOMEM)?;
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
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let (kind, handle) = slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd.raw())
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

// Suppress unused warning for the SyscallResult import.
#[allow(dead_code)]
type _Unused = SyscallResult;
