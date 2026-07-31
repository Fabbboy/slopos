//! Non-interactive input: reading commands from a descriptor.
//!
//! A shell whose stdin is a pipe or a file is running a *script*, and the
//! descriptor it reads that script from is the same one it hands to every
//! command it runs.  `{ read x; cat; } < file` and `cmd | shell` both depend on
//! the shell leaving behind exactly what it did not execute, so the framing
//! lives in [`slopos_shell_core::ScriptReader`], which reads a byte at a time
//! and is therefore structurally incapable of consuming past the line it
//! returns.  This module is the thin descriptor-backed source plus the
//! read-parse-execute loop.

use slopos_shell_core::{ByteSource, Line, ScriptReader, SourceError};

use crate::syscall::{SyscallError, fs};

use super::buffers::ParsedTokens;
use super::display::{shell_error, shell_error_named};
use super::{exec, parser};

/// Longest command line a script may contain.  Beyond this the line is
/// diagnosed and skipped rather than truncated and run.
pub const SCRIPT_LINE_MAX: usize = 8192;

/// Expansion headroom: `$VAR` substitution can grow a line well past its
/// source length.
const SCRIPT_EXPAND_MAX: usize = SCRIPT_LINE_MAX * 2;

/// A [`ByteSource`] over a file descriptor.
pub struct FdSource {
    fd: i32,
}

impl FdSource {
    pub const fn new(fd: i32) -> Self {
        Self { fd }
    }
}

impl ByteSource for FdSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
        match fs::read_slice(self.fd, buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::EINTR) => Err(SourceError::Interrupted),
            Err(_) => Err(SourceError::Fatal),
        }
    }
}

/// Read commands from `src` until end of input.  Returns the exit status of
/// the last command run, which is the script's own exit status.
pub fn run_script<S: ByteSource>(src: &mut S) -> i32 {
    let mut reader = ScriptReader::new();
    let mut line = vec![0u8; SCRIPT_LINE_MAX];
    let mut expanded = vec![0u8; SCRIPT_EXPAND_MAX];
    let mut status = 0i32;
    let mut lineno = 0u32;

    loop {
        lineno += 1;
        match reader.next_line(src, &mut line) {
            Line::Line(text) => {
                let text = parser::strip_comment(text);
                let expanded_len = parser::expand_variables(text, text.len(), &mut expanded);
                let mut tokens = ParsedTokens::new();
                let count = parser::shell_parse_line(&expanded[..expanded_len], &mut tokens);
                if count <= 0 {
                    continue;
                }
                status = exec::execute_tokens(&tokens);
                super::set_last_exit_code(status);
                if let Some(requested) = super::exit_requested() {
                    return requested;
                }
            }
            Line::Eof => return status,
            Line::TooLong => {
                report_line_error(lineno, b"line too long");
                status = 2;
                super::set_last_exit_code(status);
            }
            Line::Err => {
                shell_error(b"sh: error reading input\n");
                return status;
            }
        }
    }
}

/// `sh: line N: MSG` — the shape POSIX shells use for a defect in the script
/// itself rather than in a command it ran.
fn report_line_error(lineno: u32, msg: &[u8]) {
    let mut name = [0u8; 24];
    let mut len = 0usize;
    for b in b"line " {
        name[len] = *b;
        len += 1;
    }
    len += write_u32_decimal(&mut name[len..], lineno);
    shell_error_named(&name[..len], msg);
}

fn write_u32_decimal(buf: &mut [u8], mut value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let n = n.min(buf.len());
    for i in 0..n {
        buf[i] = digits[n - 1 - i];
    }
    n
}
