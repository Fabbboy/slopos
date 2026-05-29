//! SlopOS path/cwd operations for `std::env`.
//!
//! Lives at `std::sys::paths` (the module `std::env::{current_dir,
//! set_current_dir, temp_dir}` route through). Upstream std moved `chdir` /
//! `getcwd` here out of `sys::pal::<os>::os`, so a target without a `paths`
//! arm silently falls back to `unsupported` — which is exactly why
//! `set_current_dir` used to always fail on SlopOS. We wire `getcwd`/`chdir`
//! to the slibc C ABI (`SYSCALL_GETCWD` / `SYSCALL_CHDIR`); the remaining
//! path helpers stay on the `unsupported` stub.

#![forbid(unsafe_op_in_unsafe_fn)]

use crate::io;
use crate::path::{self, PathBuf};

mod ffi {
    unsafe extern "C" {
        pub fn getcwd(buf: *mut u8, size: usize) -> *mut u8;
        pub fn chdir(path: *const u8) -> i32;
    }
}

pub fn getcwd() -> io::Result<PathBuf> {
    let mut buf = [0u8; 4096];
    let ret = unsafe { ffi::getcwd(buf.as_mut_ptr(), buf.len()) };
    if ret.is_null() {
        Err(io::Error::last_os_error())
    } else {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = core::str::from_utf8(&buf[..len]).unwrap_or("/");
        Ok(PathBuf::from(s))
    }
}

pub fn chdir(p: &path::Path) -> io::Result<()> {
    let path_str = p.to_str().ok_or_else(|| {
        io::const_error!(io::ErrorKind::InvalidInput, "path contains invalid UTF-8")
    })?;
    let bytes = path_str.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "path too long"
        ));
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    buf[bytes.len()] = 0;
    let ret = unsafe { ffi::chdir(buf.as_ptr()) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok(())
    }
}

pub fn temp_dir() -> PathBuf {
    // `/tmp` is a ramfs mount in the SlopOS VFS (see fs/src/vfs/init.rs).
    PathBuf::from("/tmp")
}
