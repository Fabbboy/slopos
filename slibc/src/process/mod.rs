//! Process lifecycle — the Rites of Birth and Death.

pub mod atexit;
pub mod tests;
pub mod wait;

use core::ptr;

use crate::env::environ;
use crate::errno::{self, EINVAL, ENOENT};
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

pub use wait::{WEXITSTATUS, WIFEXITED, WIFSIGNALED, WTERMSIG};

// ---------------------------------------------------------------------------
// Process creation and lifecycle
// ---------------------------------------------------------------------------

/// Create a child process.
///
/// Returns the child PID to the parent, 0 to the child, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fork() -> i32 {
    match Sys::fork() {
        Ok(pid) => pid,
        Err(_) => -1,
    }
}

/// Replace the current process image with a new program.
///
/// Only returns on error (-1, sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    if path.is_null() {
        errno::errno_set(EINVAL.raw());
        return -1;
    }
    match Sys::exec(path, argv, envp) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// `execv` — exec with the current environment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execv(path: *const u8, argv: *const *const u8) -> i32 {
    execve(path, argv, environ as *const *const u8)
}

/// `execvp` — search PATH for `file`, then exec.
///
/// If `file` contains a `/`, it is used as-is. Otherwise each directory in
/// the `PATH` environment variable is tried in order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvp(file: *const u8, argv: *const *const u8) -> i32 {
    if file.is_null() {
        errno::errno_set(EINVAL.raw());
        return -1;
    }

    let file_len = u_strlen(file);

    let has_slash = {
        let mut found = false;
        for i in 0..file_len {
            if *file.add(i) == b'/' {
                found = true;
                break;
            }
        }
        found
    };

    if has_slash {
        return execv(file, argv);
    }

    let path_val = crate::env::getenv(b"PATH\0".as_ptr());
    if path_val.is_null() {
        errno::errno_set(ENOENT.raw());
        return -1;
    }

    let path_len = u_strlen(path_val);
    let mut buf = [0u8; 4096];

    let mut seg_start = 0usize;
    while seg_start < path_len {
        let mut seg_end = seg_start;
        while seg_end < path_len && *path_val.add(seg_end) != b':' {
            seg_end += 1;
        }

        let dir_len = seg_end - seg_start;
        let total = dir_len + 1 + file_len + 1;

        if total <= buf.len() {
            ptr::copy_nonoverlapping(path_val.add(seg_start), buf.as_mut_ptr(), dir_len);
            buf[dir_len] = b'/';
            ptr::copy_nonoverlapping(file, buf.as_mut_ptr().add(dir_len + 1), file_len);
            buf[dir_len + 1 + file_len] = 0;

            let ret = execv(buf.as_ptr(), argv);
            let _ = ret;
        }

        seg_start = seg_end + 1;
    }

    errno::errno_set(ENOENT.raw());
    -1
}

/// Wait for a specific child process.
///
/// Returns the child PID on success, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32 {
    match Sys::waitpid(pid, status, options) {
        Ok(ret) => ret,
        Err(_) => -1,
    }
}

/// Wait for any child process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wait(status: *mut i32) -> i32 {
    waitpid(-1, status, 0)
}

/// Immediately terminate without cleanup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _exit(status: i32) -> ! {
    Sys::exit(status)
}

/// Clean exit — flushes stdio, runs atexit handlers, then terminates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn exit(status: i32) -> ! {
    unsafe extern "C" {
        fn fflush(stream: *mut crate::stdio::FILE) -> i32;
    }
    fflush(ptr::null_mut());
    atexit::run_atexit_handlers();
    _exit(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpid() -> i32 {
    Sys::getpid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getppid() -> i32 {
    Sys::getppid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getuid() -> u32 {
    Sys::getuid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgid() -> u32 {
    Sys::getgid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn geteuid() -> u32 {
    Sys::geteuid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getegid() -> u32 {
    Sys::getegid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setpgid(pid: i32, pgid: i32) -> i32 {
    match Sys::setpgid(pid, pgid) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpgid(pid: i32) -> i32 {
    match Sys::getpgid(pid) {
        Ok(pgid) => pgid,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setsid() -> i32 {
    match Sys::setsid() {
        Ok(sid) => sid,
        Err(_) => -1,
    }
}
