//! Wait status decoding — POSIX-compatible macros for interpreting the
//! status value returned by `waitpid`.

#![allow(non_snake_case)]

/// The low 7 bits of the status encode the termination signal.
/// If zero, the process exited normally.
pub const WAIT_STATUS_SIG_MASK: i32 = 0x7F;

/// Options for `waitpid`.
pub const WNOHANG: i32 = 1;
pub const WUNTRACED: i32 = 2;

/// True if the child terminated normally (via `exit()` or returning from
/// `main`).
#[inline]
pub const fn WIFEXITED(status: i32) -> bool {
    (status & WAIT_STATUS_SIG_MASK) == 0
}

/// If `WIFEXITED` is true, returns the exit code passed to `exit()`.
#[inline]
pub const fn WEXITSTATUS(status: i32) -> i32 {
    (status >> 8) & 0xFF
}

/// True if the child was terminated by a signal.
#[inline]
pub const fn WIFSIGNALED(status: i32) -> bool {
    let sig = status & WAIT_STATUS_SIG_MASK;
    sig != 0 && sig != 0x7F
}

/// If `WIFSIGNALED` is true, returns the signal number that caused
/// termination.
#[inline]
pub const fn WTERMSIG(status: i32) -> i32 {
    status & WAIT_STATUS_SIG_MASK
}

/// True if the child is currently stopped.
#[inline]
pub const fn WIFSTOPPED(status: i32) -> bool {
    (status & 0xFF) == 0x7F
}

/// If `WIFSTOPPED` is true, returns the signal that caused the stop.
#[inline]
pub const fn WSTOPSIG(status: i32) -> i32 {
    (status >> 8) & 0xFF
}
