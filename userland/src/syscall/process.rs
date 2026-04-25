//! Process management syscalls: spawn, exec, fork, halt, reboot.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall6};
use slopos_abi::signal::{SIG_IGN, SigSet, UserSigaction};
use slopos_abi::task::TaskPriority;

/// Signal restorer trampoline — called when a signal handler returns.
///
/// The restorer address is pushed as a separate stack word before the
/// `SignalFrame`.  After the handler's `ret` pops the restorer, RSP points
/// directly at the `SignalFrame`, so `rt_sigreturn` (syscall 105) reads
/// the frame from the correct address — no stack adjustment needed.
#[unsafe(naked)]
extern "C" fn signal_restorer() {
    core::arch::naked_asm!("mov eax, 105", "syscall", "ud2");
}
use slopos_slibc::pal::{Pal, Sys};

#[inline(always)]
pub fn getpid() -> u32 {
    Sys::getpid() as u32
}

#[inline(always)]
pub fn getuid() -> u32 {
    Sys::getuid()
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
pub fn spawn_path(path: impl AsRef<[u8]>) -> i32 {
    spawn_path_with_attrs(path, TaskPriority::Normal, 0)
}

#[inline(always)]
pub fn spawn_path_with_attrs(path: impl AsRef<[u8]>, priority: TaskPriority, flags: u16) -> i32 {
    let path = path.as_ref();
    unsafe {
        syscall6(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            priority.as_u8() as u64,
            flags as u64,
            0,
            0,
        ) as i32
    }
}

#[inline(always)]
pub fn spawn_path_with_argv(
    path: &[u8],
    argv: &[*const u8],
    priority: TaskPriority,
    flags: u16,
) -> i32 {
    unsafe {
        syscall6(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            priority.as_u8() as u64,
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
pub fn openpty() -> Result<(u32, u32), i64> {
    let mut master: u32 = 0;
    let mut slave: u32 = 0;
    let ret = unsafe {
        syscall2(
            SYSCALL_OPENPTY,
            (&mut master as *mut u32) as u64,
            (&mut slave as *mut u32) as u64,
        )
    } as i64;
    if ret < 0 {
        Err(ret)
    } else {
        Ok((master, slave))
    }
}

/// Open a file descriptor for a TTY by its kernel index.
#[inline(always)]
pub fn open_tty_fd(tty_idx: u32) -> Result<super::OwnedFd, i64> {
    let ret = unsafe { syscall1(SYSCALL_OPEN_TTY_FD, tty_idx as u64) as i64 };
    if ret < 0 {
        Err(ret)
    } else {
        // SAFETY: ret is a valid fd just returned by the kernel.
        Ok(unsafe { super::OwnedFd::from_raw(ret as i32) })
    }
}

#[inline(always)]
pub fn tty_read(idx: u32, buf: &mut [u8]) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_TTY_READ,
            idx as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn tty_write(idx: u32, buf: &[u8]) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_TTY_WRITE,
            idx as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
        ) as i64
    }
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

/// Install a custom signal handler with proper restorer trampoline.
///
/// The handler receives the signal number as its argument.  When it returns,
/// the restorer automatically invokes `rt_sigreturn` to resume the
/// interrupted context.  `SA_RESTART` is deliberately omitted so that
/// blocking syscalls (e.g. `poll`) return early after the handler runs.
#[inline(always)]
pub fn set_signal_handler(signum: u8, handler: extern "C" fn(i32)) -> i32 {
    let action = UserSigaction {
        sa_handler: handler as *const () as u64,
        sa_flags: 0,
        sa_restorer: signal_restorer as *const () as u64,
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
    Sys::halt()
}

#[inline(always)]
pub fn reboot() -> ! {
    Sys::reboot()
}
