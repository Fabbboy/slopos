#![forbid(unsafe_op_in_unsafe_fn)]

use crate::ffi::OsString;
use crate::fmt;
use crate::os::raw::c_char;
use crate::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};

static ARGC: AtomicIsize = AtomicIsize::new(0);
static ARGV: AtomicPtr<*const c_char> = AtomicPtr::new(core::ptr::null_mut());

pub unsafe fn init(argc: isize, argv: *const *const u8) {
    ARGC.store(argc, Ordering::Relaxed);
    ARGV.store(argv as *mut *const c_char, Ordering::Relaxed);
}

pub struct Args {
    index: usize,
    count: usize,
}

pub fn args() -> Args {
    Args {
        index: 0,
        count: ARGC.load(Ordering::Relaxed) as usize,
    }
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for arg in args() {
            list.entry(&arg);
        }
        list.finish()
    }
}

impl Iterator for Args {
    type Item = OsString;

    fn next(&mut self) -> Option<OsString> {
        if self.index >= self.count {
            return None;
        }
        let argv = ARGV.load(Ordering::Relaxed);
        if argv.is_null() {
            return None;
        }
        let arg_ptr = unsafe { *argv.add(self.index) };
        self.index += 1;
        if arg_ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        unsafe {
            while *arg_ptr.add(len) != 0 {
                len += 1;
            }
        }
        let bytes = unsafe { core::slice::from_raw_parts(arg_ptr as *const u8, len) };
        let s = core::str::from_utf8(bytes).unwrap_or("");
        Some(OsString::from(s))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count.saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl DoubleEndedIterator for Args {
    fn next_back(&mut self) -> Option<OsString> {
        if self.index >= self.count {
            return None;
        }
        self.count -= 1;
        let argv = ARGV.load(Ordering::Relaxed);
        if argv.is_null() {
            return None;
        }
        let arg_ptr = unsafe { *argv.add(self.count) };
        if arg_ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        unsafe {
            while *arg_ptr.add(len) != 0 {
                len += 1;
            }
        }
        let bytes = unsafe { core::slice::from_raw_parts(arg_ptr as *const u8, len) };
        let s = core::str::from_utf8(bytes).unwrap_or("");
        Some(OsString::from(s))
    }
}

impl ExactSizeIterator for Args {
    fn len(&self) -> usize {
        self.count.saturating_sub(self.index)
    }
}
