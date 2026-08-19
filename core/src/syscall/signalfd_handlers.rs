//! signalfd syscall handler (`SYSCALL_SIGNALFD`); the work lives in the
//! `slopos-signalfd` crate.

use slopos_abi::Errno;

define_syscall!(syscall_signalfd
    (ctx, mask: u64, flags: u32)
    cap(NoneSelf)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    let _ = flags; // reserved (no SFD_* flags yet)
    let fd = slopos_signalfd::signalfd_create(process_id, task_id, mask);
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});
