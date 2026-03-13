//! Process management syscalls: spawn, exec, fork, halt, reboot.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall6};
use slopos_abi::signal::{SIG_IGN, SigSet, UserSigaction};

#[inline(always)]
pub fn getpid() -> u32 {
    unsafe { syscall0(SYSCALL_GETPID) as u32 }
}

#[inline(always)]
pub fn getuid() -> u32 {
    unsafe { syscall0(SYSCALL_GETUID) as u32 }
}

#[inline(always)]
pub fn chdir(path: *const u8) -> i64 {
    unsafe { syscall1(SYSCALL_CHDIR, path as u64) as i64 }
}

#[inline(always)]
pub fn getcwd(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn spawn_path(path: &[u8]) -> i32 {
    spawn_path_with_attrs(path, 5, 0)
}

#[inline(always)]
pub fn spawn_path_with_attrs(path: &[u8], priority: u8, flags: u16) -> i32 {
    unsafe {
        syscall6(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            priority as u64,
            flags as u64,
            0,
            0,
        ) as i32
    }
}

#[inline(always)]
pub fn spawn_path_with_argv(path: &[u8], argv: &[*const u8], priority: u8, flags: u16) -> i32 {
    unsafe {
        syscall6(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            priority as u64,
            flags as u64,
            argv.as_ptr() as u64,
            argv.len() as u64,
        ) as i32
    }
}

#[inline(always)]
pub fn waitpid(task_id: u32) -> i32 {
    unsafe { syscall2(SYSCALL_WAITPID, task_id as u64, 0) as i32 }
}

#[inline(always)]
pub fn waitpid_nohang(task_id: u32) -> Option<i32> {
    let rc = unsafe { syscall2(SYSCALL_WAITPID, task_id as u64, 1) as i64 };
    if rc == ERRNO_EAGAIN as i64 {
        None
    } else {
        Some(rc as i32)
    }
}

#[inline(always)]
pub fn terminate_task(task_id: u32) -> i32 {
    unsafe { syscall1(SYSCALL_TERMINATE_TASK, task_id as u64) as i32 }
}

#[inline(always)]
pub fn exec(path: &[u8]) -> i64 {
    unsafe { syscall1(SYSCALL_EXEC, path.as_ptr() as u64) as i64 }
}

#[inline(always)]
pub fn exec_ptr(path: *const u8) -> i64 {
    unsafe { syscall1(SYSCALL_EXEC, path as u64) as i64 }
}

#[inline(always)]
pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i64 {
    unsafe { syscall3(SYSCALL_EXEC, path as u64, argv as u64, envp as u64) as i64 }
}

#[inline(always)]
pub fn fork() -> i32 {
    unsafe { syscall0(SYSCALL_FORK) as i32 }
}

#[inline(always)]
pub fn setsid() -> i32 {
    unsafe { syscall0(SYSCALL_SETSID) as i32 }
}

#[inline(always)]
pub fn setpgid(pid: u32, pgid: u32) -> i32 {
    unsafe { syscall2(SYSCALL_SETPGID, pid as u64, pgid as u64) as i32 }
}

#[inline(always)]
pub fn getpgid(pid: u32) -> i32 {
    unsafe { syscall1(SYSCALL_GETPGID, pid as u64) as i32 }
}

#[inline(always)]
pub fn kill(pid: u32, signum: u8) -> i32 {
    kill_pid(pid as i32, signum)
}

#[inline(always)]
pub fn kill_pid(pid: i32, signum: u8) -> i32 {
    unsafe { syscall2(SYSCALL_KILL, pid as i64 as u64, signum as u64) as i32 }
}

#[inline(always)]
pub fn ignore_signal(signum: u8) -> i32 {
    let action = UserSigaction {
        sa_handler: SIG_IGN,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    unsafe {
        syscall4(
            SYSCALL_RT_SIGACTION,
            signum as u64,
            (&action as *const UserSigaction) as u64,
            0,
            core::mem::size_of::<SigSet>() as u64,
        ) as i32
    }
}

#[inline(always)]
pub fn halt() -> ! {
    unsafe {
        syscall0(SYSCALL_HALT);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[inline(always)]
pub fn reboot() -> ! {
    unsafe {
        syscall0(SYSCALL_REBOOT);
    }
    loop {
        core::hint::spin_loop();
    }
}
