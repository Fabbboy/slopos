//! Environment variables — the compass of the lost wizards.
//!
//! Provides `getenv`, `setenv`, `unsetenv`, `putenv`, `getcwd`, `chdir`,
//! and the `environ` pointer that C code relies on.

use core::ptr;

use crate::errno::{self, EINVAL, ENOMEM};
use crate::mem::malloc;
use crate::pal::{Pal, Sys};
use crate::string::{u_memcpy, u_strlen};

// ---------------------------------------------------------------------------
// environ global
// ---------------------------------------------------------------------------

/// The environment pointer — a null-terminated array of `"KEY=VALUE\0"` C
/// strings.  Set during `__libc_start_main` from the kernel-prepared stack.
///
/// C programs access this as `extern char **environ;`.
#[unsafe(no_mangle)]
pub static mut environ: *mut *mut u8 = ptr::null_mut();

/// Internal copy of the environment array (heap-allocated) used when
/// `setenv` / `unsetenv` / `putenv` need to grow or shrink the array.
/// When null, `environ` still points at the original stack-provided array.
static mut ENVIRON_ALLOC: *mut *mut u8 = ptr::null_mut();
static mut ENVIRON_CAP: usize = 0;

// ---------------------------------------------------------------------------
// getenv
// ---------------------------------------------------------------------------

/// Search the environment for `name` and return a pointer to the value
/// (the part after `=`), or null if not found.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const u8) -> *mut u8 {
    if name.is_null() || *name == 0 {
        return ptr::null_mut();
    }

    let name_len = u_strlen(name);

    for i in 0..name_len {
        if *name.add(i) == b'=' {
            return ptr::null_mut();
        }
    }

    let ep = environ;
    if ep.is_null() {
        return ptr::null_mut();
    }

    let mut i = 0;
    loop {
        let entry = *ep.add(i);
        if entry.is_null() {
            break;
        }

        if matches_name(entry, name, name_len) {
            return entry.add(name_len + 1);
        }
        i += 1;
    }

    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// setenv
// ---------------------------------------------------------------------------

/// Set the environment variable `name` to `value`.  If `overwrite` is zero
/// and the variable already exists, the call is a no-op.
///
/// Returns 0 on success, -1 on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setenv(name: *const u8, value: *const u8, overwrite: i32) -> i32 {
    if name.is_null() || *name == 0 {
        errno::errno_set(EINVAL.raw());
        return -1;
    }

    let name_len = u_strlen(name);

    for i in 0..name_len {
        if *name.add(i) == b'=' {
            errno::errno_set(EINVAL.raw());
            return -1;
        }
    }

    let value_len = if value.is_null() { 0 } else { u_strlen(value) };

    if let Some(idx) = find_env_index(name, name_len) {
        if overwrite == 0 {
            return 0;
        }
        let new_entry = alloc_entry(name, name_len, value, value_len);
        if new_entry.is_null() {
            errno::errno_set(ENOMEM.raw());
            return -1;
        }
        *environ.add(idx) = new_entry;
        return 0;
    }

    let new_entry = alloc_entry(name, name_len, value, value_len);
    if new_entry.is_null() {
        errno::errno_set(ENOMEM.raw());
        return -1;
    }

    if append_env_entry(new_entry) < 0 {
        malloc::dealloc(new_entry as *mut core::ffi::c_void);
        errno::errno_set(ENOMEM.raw());
        return -1;
    }

    0
}

// ---------------------------------------------------------------------------
// unsetenv
// ---------------------------------------------------------------------------

/// Remove the variable `name` from the environment.
///
/// Returns 0 on success, -1 on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsetenv(name: *const u8) -> i32 {
    if name.is_null() || *name == 0 {
        errno::errno_set(EINVAL.raw());
        return -1;
    }

    let name_len = u_strlen(name);

    for i in 0..name_len {
        if *name.add(i) == b'=' {
            errno::errno_set(EINVAL.raw());
            return -1;
        }
    }

    let ep = environ;
    if ep.is_null() {
        return 0;
    }

    let mut read = 0usize;
    let mut write = 0usize;
    loop {
        let entry = *ep.add(read);
        if entry.is_null() {
            break;
        }
        if !matches_name(entry, name, name_len) {
            *ep.add(write) = entry;
            write += 1;
        }
        read += 1;
    }
    *ep.add(write) = ptr::null_mut();

    0
}

// ---------------------------------------------------------------------------
// putenv
// ---------------------------------------------------------------------------

/// Add or replace the `"NAME=VALUE"` string directly in the environment.
/// The string is NOT copied — the caller must ensure it remains valid.
///
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn putenv(string: *mut u8) -> i32 {
    if string.is_null() {
        errno::errno_set(EINVAL.raw());
        return -1;
    }

    let mut eq_pos = 0usize;
    loop {
        let c = *string.add(eq_pos);
        if c == 0 {
            errno::errno_set(EINVAL.raw());
            return -1;
        }
        if c == b'=' {
            break;
        }
        eq_pos += 1;
    }

    let name_len = eq_pos;

    if let Some(idx) = find_env_index(string, name_len) {
        *environ.add(idx) = string;
        return 0;
    }

    if append_env_entry(string) < 0 {
        errno::errno_set(ENOMEM.raw());
        return -1;
    }

    0
}

// ---------------------------------------------------------------------------
// getcwd / chdir
// ---------------------------------------------------------------------------

/// Get the current working directory.
///
/// Writes the absolute path into `buf` (up to `size` bytes including NUL).
/// Returns `buf` on success, null on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getcwd(buf: *mut u8, size: usize) -> *mut u8 {
    if buf.is_null() || size == 0 {
        errno::errno_set(EINVAL.raw());
        return ptr::null_mut();
    }

    match Sys::getcwd(buf, size) {
        Ok(_) => buf,
        Err(_) => ptr::null_mut(),
    }
}

/// Change the current working directory.
///
/// Returns 0 on success, -1 on error (sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chdir(path: *const u8) -> i32 {
    if path.is_null() {
        errno::errno_set(EINVAL.raw());
        return -1;
    }

    match Sys::chdir(path) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check if an environment entry starts with `name=`.
unsafe fn matches_name(entry: *const u8, name: *const u8, name_len: usize) -> bool {
    for i in 0..name_len {
        if *entry.add(i) != *name.add(i) {
            return false;
        }
    }
    *entry.add(name_len) == b'='
}

/// Find the index of `name` in the current environ array.
unsafe fn find_env_index(name: *const u8, name_len: usize) -> Option<usize> {
    let ep = environ;
    if ep.is_null() {
        return None;
    }

    let mut i = 0;
    loop {
        let entry = *ep.add(i);
        if entry.is_null() {
            return None;
        }
        if matches_name(entry, name, name_len) {
            return Some(i);
        }
        i += 1;
    }
}

/// Count the number of entries in environ (not including the null terminator).
unsafe fn environ_count() -> usize {
    let ep = environ;
    if ep.is_null() {
        return 0;
    }
    let mut n = 0;
    while !(*ep.add(n)).is_null() {
        n += 1;
    }
    n
}

/// Allocate a new `"name=value\0"` string on the heap.
unsafe fn alloc_entry(
    name: *const u8,
    name_len: usize,
    value: *const u8,
    value_len: usize,
) -> *mut u8 {
    let total = name_len + 1 + value_len + 1;
    let ptr = malloc::alloc(total) as *mut u8;
    if ptr.is_null() {
        return ptr::null_mut();
    }
    u_memcpy(
        ptr as *mut core::ffi::c_void,
        name as *const core::ffi::c_void,
        name_len,
    );
    *ptr.add(name_len) = b'=';
    if value_len > 0 && !value.is_null() {
        u_memcpy(
            ptr.add(name_len + 1) as *mut core::ffi::c_void,
            value as *const core::ffi::c_void,
            value_len,
        );
    }
    *ptr.add(name_len + 1 + value_len) = 0;
    ptr
}

/// Append an entry to the environ array, growing it if needed.
/// Returns 0 on success, -1 on failure.
unsafe fn append_env_entry(entry: *mut u8) -> i32 {
    let count = environ_count();
    let needed = count + 2;

    if ENVIRON_ALLOC.is_null() || needed > ENVIRON_CAP {
        let new_cap = if needed < 32 { 32 } else { needed * 2 };
        let new_arr = malloc::alloc(new_cap * core::mem::size_of::<*mut u8>()) as *mut *mut u8;
        if new_arr.is_null() {
            return -1;
        }

        let ep = environ;
        if !ep.is_null() {
            for i in 0..count {
                *new_arr.add(i) = *ep.add(i);
            }
        }

        if !ENVIRON_ALLOC.is_null() {
            malloc::dealloc(ENVIRON_ALLOC as *mut core::ffi::c_void);
        }

        ENVIRON_ALLOC = new_arr;
        ENVIRON_CAP = new_cap;
        environ = new_arr;
    }

    *environ.add(count) = entry;
    *environ.add(count + 1) = ptr::null_mut();
    0
}
