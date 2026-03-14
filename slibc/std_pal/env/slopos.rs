#![forbid(unsafe_op_in_unsafe_fn)]

use crate::ffi::{OsStr, OsString};
use crate::{fmt, io};

mod ffi {
    unsafe extern "C" {
        pub fn getenv(name: *const u8) -> *mut u8;
        pub fn setenv(name: *const u8, value: *const u8, overwrite: i32) -> i32;
        pub fn unsetenv(name: *const u8) -> i32;
        pub static mut environ: *mut *mut u8;
    }
}

pub struct Env {
    entries: Vec<(OsString, OsString)>,
    index: usize,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for (k, v) in &self.entries {
            list.entry(&(k, v));
        }
        list.finish()
    }
}

impl Iterator for Env {
    type Item = (OsString, OsString);

    fn next(&mut self) -> Option<(OsString, OsString)> {
        if self.index < self.entries.len() {
            let entry = self.entries[self.index].clone();
            self.index += 1;
            Some(entry)
        } else {
            None
        }
    }
}

pub fn env() -> Env {
    let mut entries = Vec::new();
    unsafe {
        let env_ptr = ffi::environ;
        if !env_ptr.is_null() {
            let mut i = 0;
            loop {
                let entry = *env_ptr.add(i);
                if entry.is_null() {
                    break;
                }
                let mut len = 0;
                while *entry.add(len) != 0 {
                    len += 1;
                }
                let bytes = core::slice::from_raw_parts(entry, len);
                if let Some(eq_pos) = bytes.iter().position(|&b| b == b'=') {
                    let key = core::str::from_utf8(&bytes[..eq_pos]).unwrap_or("");
                    let val = core::str::from_utf8(&bytes[eq_pos + 1..]).unwrap_or("");
                    entries.push((OsString::from(key), OsString::from(val)));
                }
                i += 1;
            }
        }
    }
    Env { entries, index: 0 }
}

fn osstr_to_cstring(s: &OsStr) -> Option<Vec<u8>> {
    let s_str = s.to_str()?;
    let bytes = s_str.as_bytes();
    if bytes.contains(&0) {
        return None;
    }
    let mut buf = Vec::with_capacity(bytes.len() + 1);
    buf.extend_from_slice(bytes);
    buf.push(0);
    Some(buf)
}

pub fn getenv(key: &OsStr) -> Option<OsString> {
    let key_c = osstr_to_cstring(key)?;
    let val = unsafe { ffi::getenv(key_c.as_ptr()) };
    if val.is_null() {
        return None;
    }
    let mut len = 0;
    unsafe {
        while *val.add(len) != 0 {
            len += 1;
        }
    }
    let bytes = unsafe { core::slice::from_raw_parts(val, len) };
    let s = core::str::from_utf8(bytes).unwrap_or("");
    Some(OsString::from(s))
}

pub unsafe fn setenv(key: &OsStr, value: &OsStr) -> io::Result<()> {
    let key_c = osstr_to_cstring(key)
        .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "key contains null byte"))?;
    let val_c = osstr_to_cstring(value)
        .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "value contains null byte"))?;
    let ret = unsafe { ffi::setenv(key_c.as_ptr(), val_c.as_ptr(), 1) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok(())
    }
}

pub unsafe fn unsetenv(key: &OsStr) -> io::Result<()> {
    let key_c = osstr_to_cstring(key)
        .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "key contains null byte"))?;
    let ret = unsafe { ffi::unsetenv(key_c.as_ptr()) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok(())
    }
}
