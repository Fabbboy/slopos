//! Line framing for shell script input.
//!
//! See the crate docs for why this reads one byte at a time. The short version:
//! the descriptor a script arrives on is the same descriptor its commands
//! inherit, so anything the framer buffers is either lost or stolen from a
//! child. Consuming exactly the line being returned is the whole contract.

/// Why a [`ByteSource`] read did not produce a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceError {
    /// Retryable — the read was interrupted before transferring anything.
    Interrupted,
    /// Not retryable. The source is done.
    Fatal,
}

/// A byte-granular input the framer pulls from.
///
/// `Ok(0)` means end of input. Implementations are only ever asked for one
/// byte, so a short read is not a case callers have to think about.
pub trait ByteSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError>;
}

/// The result of framing one line.
#[derive(Debug, PartialEq, Eq)]
pub enum Line<'a> {
    /// A complete command line, terminator stripped. May be empty (blank line).
    Line(&'a [u8]),
    /// Input is exhausted. Sticky: every subsequent call returns this without
    /// touching the source, so a caller looping on it cannot spin.
    Eof,
    /// The line did not fit the caller's buffer. It was consumed through its
    /// terminator and **not** returned: a truncated command line means
    /// something different from what was written (`rm -rf /home/x/tmpdir`
    /// truncating to `rm -rf /home`), so the caller must diagnose rather than
    /// execute. The next call returns a whole line, never this line's tail.
    TooLong,
    /// The source failed. Also sticky.
    Err,
}

/// Frames command lines out of a [`ByteSource`] without ever reading past the
/// terminator of the line it returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptReader {
    done: bool,
}

/// What [`ScriptReader::frame`] decided, before the caller's buffer is
/// reborrowed to build the returned [`Line`].
enum Framed {
    Line(usize),
    Eof,
    TooLong,
    Err,
}

impl ScriptReader {
    pub const fn new() -> Self {
        Self { done: false }
    }

    /// True once the source has reported end-of-input or a fatal error.
    pub const fn at_eof(&self) -> bool {
        self.done
    }

    /// Read the next command line into `out`.
    ///
    /// Consumes exactly the returned line plus its terminator — never one byte
    /// more. `\n` and `\r\n` both terminate; a bare `\r` is kept literally,
    /// because this frames bytes rather than keystrokes. Interior NUL bytes are
    /// dropped as they are read: NUL terminates the shell's line and token
    /// buffers, so passing one through would silently truncate the command.
    pub fn next_line<'a, S: ByteSource>(&mut self, src: &mut S, out: &'a mut [u8]) -> Line<'a> {
        match self.frame(src, out) {
            Framed::Line(len) => Line::Line(&out[..len]),
            Framed::Eof => Line::Eof,
            Framed::TooLong => Line::TooLong,
            Framed::Err => Line::Err,
        }
    }

    fn frame<S: ByteSource>(&mut self, src: &mut S, out: &mut [u8]) -> Framed {
        if self.done {
            return Framed::Eof;
        }

        let mut len = 0usize;
        let mut overflowed = false;

        loop {
            let mut byte = [0u8; 1];
            let got = match src.read(&mut byte) {
                Ok(0) => {
                    // End of input mid-line: a final line without a terminator
                    // is still a command. Report it now and EOF next call.
                    self.done = true;
                    return if overflowed {
                        Framed::TooLong
                    } else if len > 0 {
                        Framed::Line(len)
                    } else {
                        Framed::Eof
                    };
                }
                Ok(_) => byte[0],
                Err(SourceError::Interrupted) => continue,
                Err(SourceError::Fatal) => {
                    self.done = true;
                    return Framed::Err;
                }
            };

            match got {
                b'\n' => {
                    if overflowed {
                        return Framed::TooLong;
                    }
                    // A `\r` immediately before the `\n` is the other half of a
                    // CRLF terminator, not content.
                    if len > 0 && out[len - 1] == b'\r' {
                        len -= 1;
                    }
                    return Framed::Line(len);
                }
                0 => continue,
                _ if overflowed => continue,
                _ => {
                    if len == out.len() {
                        // Keep consuming to the terminator so the *next* call
                        // starts at a line boundary rather than mid-command.
                        overflowed = true;
                        continue;
                    }
                    out[len] = got;
                    len += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts every byte handed out, so a test can assert that framing a line
    /// consumed exactly that line — the property a block read would break.
    struct SliceSource<'a> {
        data: &'a [u8],
        pos: usize,
        consumed: usize,
        /// Fail the read issued once this many bytes have been consumed.
        fail_after: Option<usize>,
        fail_kind: SourceError,
        /// Number of `Interrupted` results still to be injected before each
        /// successful read.
        interrupts: usize,
    }

    impl<'a> SliceSource<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self {
                data,
                pos: 0,
                consumed: 0,
                fail_after: None,
                fail_kind: SourceError::Fatal,
                interrupts: 0,
            }
        }

        fn failing_at(data: &'a [u8], after: usize, kind: SourceError) -> Self {
            let mut s = Self::new(data);
            s.fail_after = Some(after);
            s.fail_kind = kind;
            s
        }
    }

    impl ByteSource for SliceSource<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
            assert_eq!(buf.len(), 1, "framer must issue one-byte reads");
            if self.interrupts > 0 {
                self.interrupts -= 1;
                return Err(SourceError::Interrupted);
            }
            if self.fail_after == Some(self.consumed) {
                // One-shot for Interrupted so the retry can make progress.
                if self.fail_kind == SourceError::Interrupted {
                    self.fail_after = None;
                }
                return Err(self.fail_kind);
            }
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            self.consumed += 1;
            Ok(1)
        }
    }

    fn line_of<'a>(l: &Line<'a>) -> &'a [u8] {
        match l {
            Line::Line(b) => b,
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn stops_at_first_newline() {
        let mut src = SliceSource::new(b"a\nb\nc\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"a");
        assert_eq!(src.consumed, 2, "must consume exactly `a` and its newline");
    }

    #[test]
    fn consumes_nothing_beyond_the_terminator() {
        // The reported bug in one assertion: after framing line 1, everything
        // else must still be in the source for the next reader (or a child).
        let script = b"curl http://google.com\ncurl http://google.com\n";
        let mut src = SliceSource::new(script);
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 256];
        assert_eq!(
            line_of(&r.next_line(&mut src, &mut buf)),
            b"curl http://google.com"
        );
        assert_eq!(src.consumed, 23);
        assert_eq!(src.pos, 23);
    }

    #[test]
    fn concatenation_of_all_lines_equals_input() {
        // A script far larger than any plausible read chunk: every line must
        // come back exactly once, in order, with nothing dropped or repeated.
        let mut script = [0u8; 4096];
        let mut written = 0usize;
        let mut expected_lines = 0usize;
        let mut n = 0u32;
        while written + 16 < script.len() {
            let line = [
                b'e',
                b'c',
                b'h',
                b'o',
                b' ',
                b'0' + (n / 100 % 10) as u8,
                b'0' + (n / 10 % 10) as u8,
                b'0' + (n % 10) as u8,
                b'\n',
            ];
            script[written..written + line.len()].copy_from_slice(&line);
            written += line.len();
            expected_lines += 1;
            n += 1;
        }

        let mut src = SliceSource::new(&script[..written]);
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 256];
        let mut seen = 0usize;
        let mut rebuilt = [0u8; 4096];
        let mut rebuilt_len = 0usize;
        loop {
            match r.next_line(&mut src, &mut buf) {
                Line::Line(l) => {
                    rebuilt[rebuilt_len..rebuilt_len + l.len()].copy_from_slice(l);
                    rebuilt_len += l.len();
                    rebuilt[rebuilt_len] = b'\n';
                    rebuilt_len += 1;
                    seen += 1;
                }
                Line::Eof => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, expected_lines);
        assert_eq!(&rebuilt[..rebuilt_len], &script[..written]);
        assert_eq!(src.consumed, written);
    }

    #[test]
    fn crlf_terminator() {
        let mut src = SliceSource::new(b"echo one\r\necho two\r\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo one");
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo two");
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
    }

    #[test]
    fn bare_cr_is_literal() {
        // This frames bytes, not keystrokes: only `\n` ends a line.
        let mut src = SliceSource::new(b"a\rb\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"a\rb");
    }

    #[test]
    fn nul_bytes_discarded() {
        let mut src = SliceSource::new(b"ec\0ho ok\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo ok");
        assert_eq!(src.consumed, 9, "the NUL is consumed, just not stored");
    }

    #[test]
    fn final_line_without_newline() {
        let mut src = SliceSource::new(b"echo a\necho b");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo a");
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo b");
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
    }

    #[test]
    fn eof_is_sticky_and_does_not_spin() {
        let mut src = SliceSource::new(b"");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
        let after_first = src.consumed;
        for _ in 0..10 {
            assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
        }
        assert!(r.at_eof());
        assert_eq!(src.consumed, after_first, "EOF must not re-read the source");
    }

    #[test]
    fn blank_line_is_empty_not_eof() {
        let mut src = SliceSource::new(b"\n\n  \nx\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"");
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"");
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"  ");
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"x");
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
    }

    #[test]
    fn over_long_line_reports_too_long_then_next_line_is_whole() {
        let mut script = [b'x'; 64];
        script[0] = b'e';
        script[1] = b'c';
        script[2] = b'h';
        script[3] = b'o';
        script[4] = b' ';
        let mut full = [0u8; 96];
        full[..64].copy_from_slice(&script);
        full[64] = b'\n';
        full[65..75].copy_from_slice(b"echo after");
        full[75] = b'\n';

        let mut src = SliceSource::new(&full[..76]);
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 16];
        assert_eq!(r.next_line(&mut src, &mut buf), Line::TooLong);
        // The whole over-long line, terminator included, was consumed — so the
        // next call starts at a line boundary, never mid-command.
        assert_eq!(src.consumed, 65);
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo after");
    }

    #[test]
    fn over_long_final_line_without_newline() {
        let mut src = SliceSource::new(b"aaaaaaaaaaaaaaaaaaaa");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 4];
        assert_eq!(r.next_line(&mut src, &mut buf), Line::TooLong);
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Eof);
    }

    #[test]
    fn interrupted_is_retried_fatal_is_err() {
        let mut src = SliceSource::failing_at(b"echo ok\n", 4, SourceError::Interrupted);
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo ok");

        let mut src = SliceSource::failing_at(b"echo ok\n", 4, SourceError::Fatal);
        let mut r = ScriptReader::new();
        assert_eq!(r.next_line(&mut src, &mut buf), Line::Err);
        assert_eq!(
            r.next_line(&mut src, &mut buf),
            Line::Eof,
            "a fatal source error must terminate the caller's loop"
        );
    }

    #[test]
    fn repeated_interrupts_before_every_byte_still_frame() {
        let mut src = SliceSource::new(b"echo ok\n");
        src.interrupts = 3;
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 64];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"echo ok");
    }

    #[test]
    fn zero_length_buffer_reports_too_long_but_frames_blank_lines() {
        let mut src = SliceSource::new(b"\nx\n");
        let mut r = ScriptReader::new();
        let mut buf = [0u8; 0];
        assert_eq!(line_of(&r.next_line(&mut src, &mut buf)), b"");
        assert_eq!(r.next_line(&mut src, &mut buf), Line::TooLong);
    }
}
