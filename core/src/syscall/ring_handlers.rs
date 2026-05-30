//! SlopRing syscall handlers (SLOPRING § 6).
//!
//! `ring_setup` and `ring_enter` are both **synchronous** — they thread
//! through the normal `define_syscall!` dispatch path with no executor
//! turn. The heavy lifting lives in the `slopos-ring` crate; these
//! handlers only marshal arguments and copy the `RingParams` header out
//! to userland.

use slopos_abi::Errno;
use slopos_abi::ring::RingParams;
use slopos_mm::user_copy::copy_bytes_to_user;
use slopos_mm::user_ptr::UserBytes as MmUserBytes;

use crate::syscall::args::RawFd;

define_syscall!(syscall_ring_setup
    (ctx, entries: u32, params_ptr: u64)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    if params_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    // Pre-validate the out-pointer range so a bogus pointer fails before
    // any kernel state is built (the ring_setup closure also re-checks
    // on the actual copy).
    let _ = MmUserBytes::try_new(params_ptr, core::mem::size_of::<RingParams>())
        .map_err(|_| Errno::EFAULT)?;

    let fd = slopos_ring::ring_setup(process_id, entries, |params| {
        let bytes = params.to_bytes();
        let dst = MmUserBytes::try_new(params_ptr, bytes.len()).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(dst, &bytes).map_err(|_| Errno::EFAULT)?;
        Ok(())
    });
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});

define_syscall!(syscall_ring_enter
    (ctx, ring_fd: RawFd, to_submit: u32, min_complete: u32, flags: u32)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    // Resolve the ring fd → packed registry handle via the fd table.
    let (kind, handle) =
        slopos_fs::fileio::fileio_get_open_file_handle(process_id, ring_fd.raw())
            .ok_or(Errno::EBADF)?;
    if kind != slopos_abi::file_ops::FileKind::Ring {
        return Err(Errno::EBADF);
    }

    let rc = slopos_ring::ring_enter(
        process_id,
        task_id,
        handle,
        to_submit,
        min_complete,
        flags,
    );
    if rc < 0 {
        return Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL));
    }
    Ok(rc as u64)
});
