//! Standard streams — stdin, stdout, stderr.
//!
//! Three portals into the Wheel of Fate, each carrying bytes to or from
//! the kernel's divine judgement.

use super::{BufferMode, FILE, FILE_FLAG_LINKED, FILE_FLAG_READABLE, FILE_FLAG_WRITABLE, registry};

// ---------------------------------------------------------------------------
// Static FILE objects
// ---------------------------------------------------------------------------

static mut STDIN_FILE: FILE = FILE::new_const(0, BufferMode::Line, FILE_FLAG_READABLE);
static mut STDOUT_FILE: FILE = FILE::new_const(1, BufferMode::Line, FILE_FLAG_WRITABLE);
static mut STDERR_FILE: FILE = FILE::new_const(2, BufferMode::None, FILE_FLAG_WRITABLE);

/// Pointer to the standard input stream.
#[unsafe(no_mangle)]
pub static mut stdin: *mut FILE = &raw mut STDIN_FILE;

#[unsafe(no_mangle)]
pub static mut stdout: *mut FILE = &raw mut STDOUT_FILE;

#[unsafe(no_mangle)]
pub static mut stderr: *mut FILE = &raw mut STDERR_FILE;

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Clear a standard stream's buffer state and flags, preserving its place on
/// the open-stream list. Dropping `FILE_FLAG_LINKED` here would re-link an
/// already-linked node and leave the walk spinning on `f.next == f`.
///
/// # Safety
/// `stream` must point at one of the three standard `FILE` statics.
unsafe fn reset(stream: *mut FILE, flags: u32) {
    let f = &mut *stream;
    f.buf_pos = 0;
    f.buf_len = 0;
    f.flags = flags | (f.flags & FILE_FLAG_LINKED);
    f.ungot = -1;
}

/// Reset the standard streams and put them on the open-stream list.
///
/// Called from the CRT before `main`. Buffering follows C11 §7.21.3: stdout is
/// line-buffered when it refers to an interactive device and fully buffered
/// otherwise; stderr is never fully buffered.
pub fn stdio_init() {
    unsafe {
        reset(&raw mut STDIN_FILE, FILE_FLAG_READABLE);
        reset(&raw mut STDOUT_FILE, FILE_FLAG_WRITABLE);
        reset(&raw mut STDERR_FILE, FILE_FLAG_WRITABLE);

        STDOUT_FILE.mode = if crate::io::shim::isatty(1) != 0 {
            BufferMode::Line
        } else {
            BufferMode::Full
        };

        registry::link(&raw mut STDIN_FILE);
        registry::link(&raw mut STDOUT_FILE);
        registry::link(&raw mut STDERR_FILE);
    }
}

// ---------------------------------------------------------------------------
// Internal stream access helpers
// ---------------------------------------------------------------------------

/// Get a pointer to the static stdout FILE object.
#[inline]
pub fn stdout_file() -> *mut FILE {
    &raw mut STDOUT_FILE
}

/// Get a pointer to the static stderr FILE object.
#[inline]
pub fn stderr_file() -> *mut FILE {
    &raw mut STDERR_FILE
}

/// Get a pointer to the static stdin FILE object.
#[inline]
pub fn stdin_file() -> *mut FILE {
    &raw mut STDIN_FILE
}
