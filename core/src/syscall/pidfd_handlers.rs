//! pidfd syscall handler (`SYSCALL_PIDFD_OPEN`).
//!
//! Synchronous; threads through the normal `define_syscall!` dispatch path.
//! The work lives in the `slopos-pidfd` crate — this only marshals the
//! caller's task/process ids and maps the result to an errno.

use slopos_abi::Errno;

define_syscall!(syscall_pidfd_open
    (ctx, pid: u32)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    let fd = slopos_pidfd::pidfd_open(process_id, task_id, pid);
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});
