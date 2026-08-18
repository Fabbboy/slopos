//! Slim ANSI output emitter for the PTY-slave shell.
//!
//! The shell owns no surface or scrollback: fd 0/1/2 are the PTY slave the
//! parent terminal emulator provides, and it is the terminal that interprets
//! the SGR runs this module wraps interactive output in.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use slopos_abi::draw::Color32;

use crate::syscall::fs;

pub const SHELL_FG_COLOR: Color32 = Color32::rgb(0xE6, 0xE6, 0xE6);

pub const COLOR_DEFAULT: u8 = 0;
pub const COLOR_DIR_BLUE: u8 = 1;
pub const COLOR_EXEC_GREEN: u8 = 2;
pub const COLOR_ERROR_RED: u8 = 3;
pub const COLOR_WARN_YELLOW: u8 = 4;
pub const COLOR_PROMPT_ACCENT: u8 = 5;
pub const COLOR_COMMENT_GRAY: u8 = 6;
pub const COLOR_PATH_BLUE: u8 = 7;
pub const COLOR_SELECTION_BG: u8 = 8;

pub const PALETTE_SIZE: usize = 16;

/// Indexed foreground-color palette; the indices are the `COLOR_*` constants.
pub static PALETTE: [Color32; PALETTE_SIZE] = [
    SHELL_FG_COLOR,                 // 0: default
    Color32::rgb(0x5C, 0x9E, 0xD6), // 1: directory blue
    Color32::rgb(0x98, 0xC3, 0x79), // 2: executable green
    Color32::rgb(0xE0, 0x6C, 0x75), // 3: error red
    Color32::rgb(0xE5, 0xC0, 0x7B), // 4: warning yellow
    Color32::rgb(0xC6, 0x78, 0xDD), // 5: prompt accent
    Color32::rgb(0x5C, 0x63, 0x70), // 6: comment gray
    Color32::rgb(0x61, 0xAF, 0xEF), // 7: path blue
    Color32::rgb(0x26, 0x4F, 0x78), // 8: selection background
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
    SHELL_FG_COLOR,
];

/// When `>= 0`, output is redirected to this fd (pipe/file): raw bytes only,
/// no SGR color.  When `-1`, output is interactive on fd 1 with color.
static OUTPUT_FD: AtomicI32 = AtomicI32::new(-1);

/// When set, no SGR escape is ever emitted.
///
/// Separate from [`OUTPUT_FD`], which the redirect machinery clears after every
/// builtin `>`: a shell running a script must stay plain for its whole life.
static PLAIN: AtomicBool = AtomicBool::new(false);

/// Standard-output file descriptor (the PTY slave when interactive).
const STDOUT_FD: i32 = 1;

/// Standard-error file descriptor. Diagnostics go here in both modes.
const STDERR_FD: i32 = 2;

/// Suppress every SGR escape for the rest of this process's life.
pub fn set_plain_output(plain: bool) {
    PLAIN.store(plain, Ordering::Relaxed);
}

#[inline]
fn palette_color(color_idx: u8) -> Color32 {
    PALETTE[color_idx as usize % PALETTE_SIZE]
}

fn palette_index_for(color: Color32) -> u8 {
    let target = color.to_u32();
    for (i, c) in PALETTE.iter().enumerate() {
        if c.to_u32() == target {
            return i as u8;
        }
    }
    COLOR_DEFAULT
}

/// Write every byte of `bytes` to `fd`, or report failure.
///
/// A single `write(2)` may transfer less than it was given — a pipe write does
/// whenever the ring fills — and treating that count as success drops the tail.
fn write_all(fd: i32, bytes: &[u8]) -> bool {
    let mut written = 0usize;
    while written < bytes.len() {
        match fs::write_slice(fd, &bytes[written..]) {
            Ok(0) => return false,
            Ok(n) => written += n,
            Err(e) if e == crate::syscall::SyscallError::EINTR => continue,
            Err(_) => return false,
        }
    }
    true
}

/// Write raw bytes to fd 1. On failure (EBADF in a test binary with no wired
/// stdout) fall back to the serial console once, so the output still reaches a
/// transcript.
fn emit_stdout(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if write_all(STDOUT_FD, bytes) {
        return true;
    }
    let _ = crate::syscall::tty::write(bytes);
    false
}

/// Write a diagnostic to fd 2.
///
/// Deliberately blind to [`OUTPUT_FD`]: a builtin's `>` redirects its *output*,
/// not its complaints, so `ls nosuch > out` leaves the error on the terminal.
pub fn shell_error(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if write_all(STDERR_FD, bytes) {
        return true;
    }
    let _ = crate::syscall::tty::write(bytes);
    false
}

/// Emit a POSIX-shaped diagnostic: `sh: NAME: MSG\n`.
pub fn shell_error_named(name: &[u8], msg: &[u8]) {
    let mut buf = [0u8; 320];
    let mut len = 0usize;
    for part in [b"sh: ".as_slice(), name, b": ", msg, b"\n"] {
        let n = part.len().min(buf.len() - len);
        buf[len..len + n].copy_from_slice(&part[..n]);
        len += n;
    }
    shell_error(&buf[..len]);
}

/// Format an SGR truecolor foreground-set sequence for `color` into `buf`,
/// which must hold at least 19 bytes (`\x1b[38;2;255;255;255m`).
fn write_sgr_set(buf: &mut [u8], color: Color32) -> usize {
    let mut pos = 0usize;
    let prefix = b"\x1b[38;2;";
    buf[..prefix.len()].copy_from_slice(prefix);
    pos += prefix.len();
    pos += write_u8_decimal(&mut buf[pos..], color.red());
    buf[pos] = b';';
    pos += 1;
    pos += write_u8_decimal(&mut buf[pos..], color.green());
    buf[pos] = b';';
    pos += 1;
    pos += write_u8_decimal(&mut buf[pos..], color.blue());
    buf[pos] = b'm';
    pos += 1;
    pos
}

fn write_u8_decimal(buf: &mut [u8], value: u8) -> usize {
    if value >= 100 {
        buf[0] = b'0' + value / 100;
        buf[1] = b'0' + (value / 10) % 10;
        buf[2] = b'0' + value % 10;
        3
    } else if value >= 10 {
        buf[0] = b'0' + value / 10;
        buf[1] = b'0' + value % 10;
        2
    } else {
        buf[0] = b'0' + value;
        1
    }
}

/// Emit `bytes` to fd 1 wrapped in a truecolor SGR run for `color_idx`, then
/// reset.  COLOR_DEFAULT emits no SGR (terminal's default foreground).
///
/// A broken fd 1 (EBADF in a test binary) falls back to the serial console
/// exactly once per call, carrying the payload — never the SGR escapes.
fn emit_colored(bytes: &[u8], color_idx: u8) -> bool {
    if color_idx == COLOR_DEFAULT || PLAIN.load(Ordering::Relaxed) {
        return emit_stdout(bytes);
    }
    let mut sgr = [0u8; 24];
    let len = write_sgr_set(&mut sgr, palette_color(color_idx));
    if fs::write_slice(STDOUT_FD, &sgr[..len]).is_err() {
        let _ = crate::syscall::tty::write(bytes);
        return false;
    }
    let ok = emit_stdout(bytes);
    if ok {
        let _ = fs::write_slice(STDOUT_FD, b"\x1b[0m");
    }
    ok
}

// =============================================================================
// Public API
// =============================================================================

/// Write text to the current output destination (redirect fd or fd 1).
///
/// When redirected, raw bytes are written with no color.  Returns `true` on
/// success, `false` when the write fails (e.g. broken pipe).  Callers in tight
/// loops (`yes`, `seq`) check the return value and exit early on `false`.
pub fn shell_write(buf: &[u8]) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return write_all(redirected_fd, buf);
    }
    emit_stdout(buf)
}

/// Write colored text to the current output destination.
///
/// When redirected (pipe/file), color is stripped and raw text is written.
/// Otherwise the text is wrapped in a truecolor SGR run derived from `fg`.
pub fn shell_write_colored(buf: &[u8], fg: Color32) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return write_all(redirected_fd, buf);
    }
    emit_colored(buf, palette_index_for(fg))
}

/// Write text with a palette color index to the current output destination.
///
/// Convenience wrapper that avoids a palette lookup when the caller already
/// has an index.
pub fn shell_write_idx(buf: &[u8], color_idx: u8) -> bool {
    let redirected_fd = OUTPUT_FD.load(Ordering::Relaxed);
    if redirected_fd >= 0 {
        return write_all(redirected_fd, buf);
    }
    emit_colored(buf, color_idx)
}

pub fn shell_set_output_fd(fd: i32) {
    OUTPUT_FD.store(fd, Ordering::Relaxed);
}

pub fn shell_clear_output_fd() {
    OUTPUT_FD.store(-1, Ordering::Relaxed);
}

/// Echo a single character to fd 1 (line-editor local echo).
pub fn shell_echo_char(c: u8) {
    let buf = [c];
    emit_stdout(&buf);
}
