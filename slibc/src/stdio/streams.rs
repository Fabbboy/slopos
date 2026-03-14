//! Standard streams — stdin, stdout, stderr.
//!
//! Three portals into the Wheel of Fate, each carrying bytes to or from
//! the kernel's divine judgement.

use super::{BufferMode, FILE, FILE_FLAG_READABLE, FILE_FLAG_WRITABLE};

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

/// Reset the standard streams to a clean state.
///
/// Called from `__libc_start_main` (Phase 3A) to ensure buffer positions
/// are zeroed and error flags are clear. Currently a no-op for TTY
/// detection (stdout stays line-buffered unconditionally).
pub fn stdio_init() {
    unsafe {
        // Reset stdin
        STDIN_FILE.buf_pos = 0;
        STDIN_FILE.buf_len = 0;
        STDIN_FILE.flags = FILE_FLAG_READABLE;
        STDIN_FILE.ungot = -1;

        // Reset stdout
        STDOUT_FILE.buf_pos = 0;
        STDOUT_FILE.buf_len = 0;
        STDOUT_FILE.flags = FILE_FLAG_WRITABLE;
        STDOUT_FILE.ungot = -1;

        // Reset stderr (always unbuffered)
        STDERR_FILE.buf_pos = 0;
        STDERR_FILE.buf_len = 0;
        STDERR_FILE.flags = FILE_FLAG_WRITABLE;
        STDERR_FILE.ungot = -1;
    }
}

// ---------------------------------------------------------------------------
// Internal stream access helpers
// ---------------------------------------------------------------------------

/// Get a mutable reference to the static stdout FILE object.
///
/// # Safety
/// Caller must ensure no concurrent access to stdout.
#[inline]
pub(crate) unsafe fn stdout_file() -> *mut FILE {
    &raw mut STDOUT_FILE
}

/// Get a mutable reference to the static stderr FILE object.
///
/// # Safety
/// Caller must ensure no concurrent access to stderr.
#[inline]
pub(crate) unsafe fn stderr_file() -> *mut FILE {
    &raw mut STDERR_FILE
}

/// Get a mutable reference to the static stdin FILE object.
///
/// # Safety
/// Caller must ensure no concurrent access to stdin.
#[inline]
pub(crate) unsafe fn stdin_file() -> *mut FILE {
    &raw mut STDIN_FILE
}
