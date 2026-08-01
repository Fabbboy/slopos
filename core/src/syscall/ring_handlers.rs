//! SlopRing syscall handlers (SLOPRING § 6).
//!
//! `ring_setup` and `ring_enter` are both **synchronous** — they thread
//! through the normal `define_syscall!` dispatch path with no executor
//! turn. The heavy lifting lives in the `slopos-ring` crate; these
//! handlers only marshal arguments and copy the `RingParams` header out
//! to userland.

use slopos_abi::Errno;
use slopos_abi::ring::{BufIovec, RegisterBufRingCmd, RingParams, SLOPRING_MAX_FIXED_BUFFERS};
use slopos_abi::syscall::numbers::{
    RING_REGISTER_BUFFERS, RING_REGISTER_PBUF_RING, RING_UNREGISTER_BUFFERS,
    RING_UNREGISTER_PBUF_RING,
};
use slopos_mm::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use slopos_mm::user_ptr::UserBytes as MmUserBytes;
use slopos_ostd::KVec;

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

    let rc = slopos_ring::ring_enter(process_id, handle, to_submit, min_complete, flags);
    if rc < 0 {
        return Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL));
    }
    Ok(rc as u64)
});

define_syscall!(syscall_ring_register
    (ctx, ring_fd: RawFd, op: u32, arg: u64, nr_args: u32)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    // Registered / provided buffer registration (SLOPRING § 13, ABI v2). This
    // handler owns user-copy: it marshals the typed argument into a private
    // kernel copy and hands plain values to the ring crate. Validate ownership
    // first so a foreign / non-ring fd fails -EBADF, not -ENOSYS.
    let (kind, handle) =
        slopos_fs::fileio::fileio_get_open_file_handle(process_id, ring_fd.raw())
            .ok_or(Errno::EBADF)?;
    if kind != slopos_abi::file_ops::FileKind::Ring {
        return Err(Errno::EBADF);
    }

    let rc = match op {
        RING_REGISTER_BUFFERS => {
            if nr_args == 0 || nr_args > SLOPRING_MAX_FIXED_BUFFERS {
                return Err(Errno::EINVAL);
            }
            if arg == 0 {
                return Err(Errno::EFAULT);
            }
            // Pre-validate the whole array range, then snapshot each iovec into
            // a kernel-owned (addr, len) list — never a &T over user memory.
            let total = nr_args as usize * BufIovec::SERIALIZED_LEN;
            let _ = MmUserBytes::try_new(arg, total).map_err(|_| Errno::EFAULT)?;
            let mut iovecs: KVec<(u64, u32)> =
                KVec::with_capacity(nr_args as usize).map_err(|_| Errno::ENOMEM)?;
            for i in 0..nr_args as u64 {
                let off = i * BufIovec::SERIALIZED_LEN as u64;
                let src = MmUserBytes::try_new(arg + off, BufIovec::SERIALIZED_LEN)
                    .map_err(|_| Errno::EFAULT)?;
                let mut bytes = [0u8; BufIovec::SERIALIZED_LEN];
                copy_bytes_from_user(src, &mut bytes).map_err(|_| Errno::EFAULT)?;
                let iov = BufIovec::from_bytes(&bytes);
                iovecs.push((iov.addr, iov.len)).map_err(|_| Errno::ENOMEM)?;
            }
            slopos_ring::ring_register_buffers(process_id, handle, iovecs.as_slice())
        }
        RING_UNREGISTER_BUFFERS => slopos_ring::ring_unregister_buffers(process_id, handle),
        RING_REGISTER_PBUF_RING => {
            if arg == 0 {
                return Err(Errno::EFAULT);
            }
            let src = MmUserBytes::try_new(arg, RegisterBufRingCmd::SERIALIZED_LEN)
                .map_err(|_| Errno::EFAULT)?;
            let mut bytes = [0u8; RegisterBufRingCmd::SERIALIZED_LEN];
            copy_bytes_from_user(src, &mut bytes).map_err(|_| Errno::EFAULT)?;
            let cmd = RegisterBufRingCmd::from_bytes(&bytes);
            slopos_ring::ring_register_pbuf_ring(process_id, handle, &cmd)
        }
        RING_UNREGISTER_PBUF_RING => {
            slopos_ring::ring_unregister_pbuf_ring(process_id, handle, arg as u16)
        }
        _ => return Err(Errno::ENOSYS),
    };

    if rc < 0 {
        return Err(Errno::from_raw(rc).unwrap_or(Errno::EINVAL));
    }
    Ok(rc as u64)
});
