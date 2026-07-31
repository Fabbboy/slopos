//! Safe wrappers over `process::*` for use from tests.

/// Terminate the process with `code`. Wraps `process::exit`, which is
/// `unsafe extern "C"` only because it's exported as the C `exit`
/// symbol — calling it with a plain integer is sound.
pub fn exit(code: i32) -> ! {
    // SAFETY: `process::exit` never reads memory through its argument
    // and never returns.
    unsafe { super::exit(code) }
}

/// Terminate the process without flushing stdio or running `atexit` handlers.
pub fn _exit(code: i32) -> ! {
    // SAFETY: takes no pointers and never returns.
    unsafe { super::_exit(code) }
}

/// Create a child process. Returns the child pid to the parent, 0 to the
/// child, and -1 on error.
pub fn fork() -> i32 {
    // SAFETY: `fork` takes no arguments and returns a plain integer.
    unsafe { super::fork() }
}

/// Block until `pid` terminates and return the exit code the kernel recorded
/// for it, or -1 on error.
pub fn wait_for_child(pid: i32) -> i32 {
    // SAFETY: a null status pointer is the "discard the status" request.
    unsafe { super::waitpid(pid, core::ptr::null_mut(), 0) }
}

/// Register a function to run at normal process termination. Returns 0 on
/// success, -1 if the table is full.
pub fn atexit(func: extern "C" fn()) -> i32 {
    // SAFETY: `func` is a valid function pointer with the required signature;
    // the coercion to `unsafe extern "C" fn()` adds no obligation to the
    // caller, and `atexit` only stores it.
    unsafe { super::atexit::atexit(func) }
}
