//! Process management syscall the async `Child::wait` needs: `waitpid`
//! (reap an exited child task).

use slopos_abi::syscall::SYSCALL_WAITPID;
use slopos_slibc::pal::raw::syscall2;

#[inline(always)]
pub fn waitpid(task_id: u32) -> i32 {
    unsafe { syscall2(SYSCALL_WAITPID, task_id as u64, 0) as i32 }
}
