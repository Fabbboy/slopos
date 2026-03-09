//! Enhanced line discipline for the TTY subsystem (Phase 2+).
//!
//! This module implements a simplified but fairly complete N_TTY-style line
//! discipline.  Compared to the Phase 1 stub it adds:
//!
//! - **Input flag processing** (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP,
//!   IGNBRK, BRKINT, PARMRK
//! - **Output flag processing** (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET
//! - **Additional echo modes**: ECHOCTL (^X for control chars), ECHOKE (kill
//!   via backspace sequence — deterministic visual erase)
//! - **Signal generation**: SIGINT (VINTR), SIGQUIT (VQUIT), SIGTSTP (VSUSP)
//! - **Flow control**: IXON with VSTOP/VSTART (Ctrl+S / Ctrl+Q)
//! - **Canonical editing**: VWERASE (Ctrl+W word erase), VREPRINT (Ctrl+R
//!   redisplay), VLNEXT (Ctrl+V literal next)
//! - **Non-canonical mode**: VMIN/VTIME parsed (timing not yet enforced)
//! - **Column tracking** for proper backspace/kill echo
//!
//! The line discipline never touches the hardware directly — it returns
//! [`InputAction`] / [`OutputAction`] values that the caller (the TTY core in
//! `mod.rs`) translates into driver writes.

use slopos_abi::signal::{SIGINT, SIGQUIT, SIGTSTP};
use slopos_abi::syscall::{
    BRKINT,
    ECHO,
    // c_lflag (additional)
    ECHOCTL,
    ECHOE,
    ECHOK,
    ECHOKE,
    ECHONL,
    ICANON,
    // c_iflag
    ICRNL,
    IEXTEN,
    IGNBRK,
    IGNCR,
    INLCR,
    ISIG,
    ISTRIP,
    IXON,
    N_RAW,
    N_TTY,
    NCCS,
    NOFLSH,
    // c_oflag
    OCRNL,
    ONLCR,
    ONLRET,
    ONOCR,
    OPOST,
    PARMRK,
    UserTermios,
    // c_cc indices
    VEOF,
    VERASE,
    VINTR,
    VKILL,
    VLNEXT,
    VMIN,
    VQUIT,
    VREPRINT,
    VSTART,
    VSTOP,
    VSUSP,
    VTIME,
    VWERASE,
};

const EDIT_BUF_SIZE: usize = 1024;
const COOKED_BUF_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Action enums returned to the caller
// ---------------------------------------------------------------------------

/// Actions returned by the line discipline after processing an input byte.
#[derive(Debug)]
pub enum InputAction {
    /// No action needed.
    None,
    /// Echo bytes back to the terminal.  Up to 4 bytes (e.g. BS-SPACE-BS or ^X).
    Echo { buf: [u8; 4], len: u8 },
    /// Deliver a signal to the foreground process group.
    Signal(u8),
    /// Redisplay the current edit line (VREPRINT / Ctrl+R).
    ///
    /// The caller should write a newline followed by the contents returned
    /// by [`LineDisc::edit_content()`].
    ReprintLine,
    /// Phase 27: Kill-line visual erase (ECHOKE).
    ///
    /// The caller should emit `columns` BS-SPACE-BS triples to erase the
    /// line visually.  This replaces the old pragmatic newline-only echo.
    KillLineEcho { columns: u16 },
}

/// Actions returned by output processing (`process_output_byte`).
pub enum OutputAction {
    /// Emit these bytes to the driver (up to 2 bytes, e.g. `\r\n`).
    Emit { buf: [u8; 2], len: u8 },
    /// Expand a tab to N spaces (tab stop expansion).
    Tab(u8),
    /// Suppress this byte entirely (don't output anything).
    Suppress,
}

// ---------------------------------------------------------------------------
// LineDisc
// ---------------------------------------------------------------------------

/// The line discipline state machine.
///
/// Each `Tty` owns one `LineDisc` instance.  It maintains an edit buffer
/// (for canonical mode line editing) and a cooked ring buffer (ready for
/// userland `read()`).
pub struct LineDisc {
    termios: UserTermios,

    // -- Canonical mode buffers --
    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,

    // -- Cooked output ring buffer (ready for userland read) --
    cooked: [u8; COOKED_BUF_SIZE],
    cooked_head: usize,
    cooked_tail: usize,
    cooked_count: usize,

    // -- Canonical line boundary tracking (Phase 15) --
    /// Number of complete lines in the cooked buffer (delimited by newline or
    /// EOF flush).  In canonical mode `has_data()` checks this instead of
    /// `cooked_count` so that `read()` blocks until a full line is available.
    line_count: usize,

    // -- Flow control --
    /// Output stopped via XOFF (Ctrl+S / VSTOP).
    stopped: bool,
    /// Next input character is literal (Ctrl+V / VLNEXT was pressed).
    literal_next: bool,

    // -- Column tracking (for ECHOKE / backspace echo) --
    column: usize,
}

impl LineDisc {
    /// Create a new `LineDisc` with default termios (canonical + echo + signals).
    pub const fn new() -> Self {
        let cc = [
            0x03, // VINTR   = Ctrl+C
            0x1C, // VQUIT   = Ctrl+backslash
            0x7F, // VERASE  = DEL
            0x15, // VKILL   = Ctrl+U
            0x04, // VEOF    = Ctrl+D
            0,    // VTIME
            1,    // VMIN
            0,    // (unused index 7)
            0x11, // VSTART  = Ctrl+Q
            0x13, // VSTOP   = Ctrl+S
            0x1A, // VSUSP   = Ctrl+Z
            0,    // VEOL
            0x12, // VREPRINT = Ctrl+R
            0,    // (unused index 13)
            0x17, // VWERASE = Ctrl+W
            0x16, // VLNEXT  = Ctrl+V
            0, 0, 0,
        ];
        Self {
            termios: UserTermios {
                c_iflag: ICRNL,
                c_oflag: OPOST | ONLCR,
                c_cflag: 0,
                c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE,
                c_line: N_TTY as u8,
                c_cc: cc,
                c_ispeed: 0,
                c_ospeed: 0,
            },
            edit_buf: [0; EDIT_BUF_SIZE],
            edit_len: 0,
            cooked: [0; COOKED_BUF_SIZE],
            cooked_head: 0,
            cooked_tail: 0,
            cooked_count: 0,
            line_count: 0,
            stopped: false,
            literal_next: false,
            column: 0,
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// Immutable reference to the current termios.
    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    /// Returns (vmin, vtime_deciseconds) for non-canonical mode reads.
    /// vtime is in deciseconds (100ms units) as per POSIX.
    pub fn vmin_vtime(&self) -> (u8, u8) {
        // c_cc layout: index 5 = VTIME, index 6 = VMIN
        let vtime = self.termios.c_cc[5];
        let vmin = self.termios.c_cc[6];
        (vmin, vtime)
    }

    /// Returns true if in canonical mode.
    pub fn is_canonical(&self) -> bool {
        (self.termios.c_lflag & ICANON) != 0
    }

    /// Update termios.  If canonical mode is toggled off, flushes the edit
    /// buffer so that any pending characters become available for raw reads.
    pub fn set_termios(&mut self, t: &UserTermios) {
        let was_canon = (self.termios.c_lflag & ICANON) != 0;
        let is_canon = (t.c_lflag & ICANON) != 0;
        self.termios = *t;
        if was_canon && !is_canon {
            self.flush_edit_to_cooked();
        }
    }

    /// Returns `true` if the cooked ring buffer has data available for reading.
    ///
    /// In canonical mode, data is only "available" when at least one complete
    /// line has been committed (newline pressed or EOF flush).  This prevents
    /// `read()` from returning partial lines.
    pub fn has_data(&self) -> bool {
        if self.is_canonical() {
            self.line_count > 0
        } else {
            self.cooked_count > 0
        }
    }

    /// Read cooked bytes into `out`, returning the number of bytes copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let canonical = self.is_canonical();

        // Phase 23: Canonical EOF on empty buffer.  When VEOF (Ctrl+D) is
        // pressed with an empty edit buffer, `flush_edit_to_cooked()` bumps
        // `line_count` but pushes zero bytes.  Without this guard, `has_data()`
        // keeps returning true (line_count > 0) and `read()` returns 0 bytes
        // without decrementing, creating a phantom-readable state.  Fix: detect
        // the zero-byte line and consume it immediately.
        if canonical && self.line_count > 0 && self.cooked_count == 0 {
            self.line_count -= 1;
            return 0;
        }

        let mut copied = 0usize;
        while copied < out.len() && self.cooked_count > 0 {
            let byte = self.cooked[self.cooked_tail];
            out[copied] = byte;
            self.cooked_tail = (self.cooked_tail + 1) % COOKED_BUF_SIZE;
            self.cooked_count -= 1;
            copied += 1;

            // In canonical mode, stop after consuming one complete line.
            // A line boundary is marked by a newline character.
            if canonical && byte == b'\n' {
                self.line_count = self.line_count.saturating_sub(1);
                return copied;
            }
        }
        // If we drained ALL remaining cooked bytes in canonical mode without
        // hitting a newline, this was an EOF-flushed chunk (Ctrl+D with no
        // trailing newline).  Decrement line_count since the full line has
        // been consumed.  If we merely filled the caller's buffer (cooked_count
        // > 0), we are mid-line and must NOT decrement.
        if canonical && copied > 0 && self.cooked_count == 0 && self.line_count > 0 {
            self.line_count -= 1;
        }

        copied
    }

    /// Phase 27: Returns the number of bytes available for reading.
    /// Used by the FIONREAD / TIOCINQ ioctl.
    pub fn bytes_available(&self) -> usize {
        self.cooked_count
    }

    pub fn flush_all(&mut self) {
        self.edit_len = 0;
        self.cooked_head = 0;
        self.cooked_tail = 0;
        self.cooked_count = 0;
        self.line_count = 0;
        self.stopped = false;
        self.literal_next = false;
        self.column = 0;
    }

    pub fn flush_input(&mut self) {
        self.edit_len = 0;
        self.cooked_head = 0;
        self.cooked_tail = 0;
        self.cooked_count = 0;
        self.line_count = 0;
        self.literal_next = false;
    }

    /// Return a slice of the current edit buffer contents (for VREPRINT echo).
    pub fn edit_content(&self) -> &[u8] {
        &self.edit_buf[..self.edit_len]
    }

    /// Whether output is currently stopped (XOFF / Ctrl+S).
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    // -- Input processing ----------------------------------------------------

    /// Process a single raw input byte through the line discipline.
    ///
    /// Returns an [`InputAction`] indicating what the caller should do (echo,
    /// signal, reprint, or nothing).
    pub fn input_char(&mut self, c: u8) -> InputAction {
        let iflag = self.termios.c_iflag;
        let lflag = self.termios.c_lflag;

        // Phase 27: Break handling.  A NUL byte (0x00) is treated as a
        // break condition when any of IGNBRK/BRKINT/PARMRK is set.
        if c == 0x00 && (iflag & (IGNBRK | BRKINT | PARMRK)) != 0 {
            return self.handle_break(iflag);
        }

        // 1. Input flag processing (c_iflag).
        let c = self.process_iflag(c, iflag);

        // A return value of None from process_iflag means "discard this byte"
        // (IGNCR ate it).
        let c = match c {
            Some(c) => c,
            None => return InputAction::None,
        };

        // 2. Literal-next mode (Ctrl+V was pressed previously).
        if self.literal_next {
            self.literal_next = false;
            return self.insert_char(c, lflag);
        }

        // 3. Signal generation (ISIG) + Phase 23 NOFLSH flush.
        if (lflag & ISIG) != 0 {
            let sig = if c == self.cc(VINTR) {
                Some(SIGINT)
            } else if c == self.cc(VQUIT) {
                Some(SIGQUIT)
            } else if c == self.cc(VSUSP) {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                // POSIX: unless NOFLSH is set, flush input queues on signal.
                if (lflag & NOFLSH) == 0 {
                    self.flush_input();
                }
                return InputAction::Signal(sig);
            }
        }

        // 4. Flow control (IXON).
        if (iflag & IXON) != 0 {
            if c == self.cc(VSTOP) {
                self.stopped = true;
                return InputAction::None;
            }
            if c == self.cc(VSTART) {
                self.stopped = false;
                return InputAction::None;
            }
            // Any character resumes output when stopped (if IXON is set).
            if self.stopped {
                self.stopped = false;
            }
        }

        // 5. Extended input processing (IEXTEN).
        if (lflag & IEXTEN) != 0 {
            if c == self.cc(VLNEXT) {
                self.literal_next = true;
                // Echo ^V if ECHOCTL is set.
                if (lflag & ECHOCTL) != 0 && (lflag & ECHO) != 0 {
                    return InputAction::Echo {
                        buf: [b'^', b'V', 0, 0],
                        len: 2,
                    };
                }
                return InputAction::None;
            }
            if (lflag & ICANON) != 0 {
                if c == self.cc(VWERASE) {
                    return self.word_erase(lflag);
                }
                if c == self.cc(VREPRINT) {
                    return InputAction::ReprintLine;
                }
            }
        }

        // 6. Canonical vs non-canonical.
        if (lflag & ICANON) != 0 {
            self.canonical_input(c, lflag)
        } else {
            self.raw_input(c, lflag)
        }
    }

    // -- Output processing ---------------------------------------------------

    /// Process a single byte through `c_oflag` before sending to the driver.
    ///
    /// Called by the TTY core's `write()` function for each output byte.
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        let oflag = self.termios.c_oflag;
        if (oflag & OPOST) == 0 {
            // No output processing — still track column for echo accuracy.
            self.update_column_raw(c);
            return OutputAction::Emit {
                buf: [c, 0],
                len: 1,
            };
        }
        match c {
            b'\n' if (oflag & ONLCR) != 0 => {
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\r', b'\n'],
                    len: 2,
                }
            }
            b'\r' if (oflag & OCRNL) != 0 => {
                // OCRNL: convert CR to NL.  If ONLRET is also set, reset column.
                if (oflag & ONLRET) != 0 {
                    self.column = 0;
                }
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\r' if (oflag & ONOCR) != 0 && self.column == 0 => OutputAction::Suppress,
            b'\n' if (oflag & ONLRET) != 0 => {
                // ONLRET: NL performs CR function — reset column.
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\r' => {
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\r', 0],
                    len: 1,
                }
            }
            b'\n' => {
                // Plain NL without ONLCR/ONLRET — no column reset per POSIX.
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\t' => {
                let spaces = 8 - (self.column % 8);
                self.column += spaces;
                OutputAction::Tab(spaces as u8)
            }
            0x08 => {
                // Backspace — decrement column if possible.
                if self.column > 0 {
                    self.column -= 1;
                }
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
            c if c >= 0x20 && c < 0x7F => {
                self.column += 1;
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
            _ => {
                // Non-printable control char — no column change.
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
        }
    }

    // -- Private helpers -----------------------------------------------------

    /// Apply c_iflag processing to a raw input byte.
    ///
    /// Returns `None` if the byte should be discarded (IGNCR, IGNBRK).
    /// Returns `Some(InputAction::Signal(SIGINT))` indirectly via break
    /// handling when BRKINT is set (the caller must check for break
    /// conditions before calling this method — see `input_char`).
    fn process_iflag(&self, c: u8, iflag: u32) -> Option<u8> {
        let mut c = c;

        // ISTRIP: strip bit 7.
        if (iflag & ISTRIP) != 0 {
            c &= 0x7F;
        }

        // CR/NL mapping.
        if c == b'\r' {
            if (iflag & IGNCR) != 0 {
                return None; // Discard CR entirely.
            }
            if (iflag & ICRNL) != 0 {
                c = b'\n'; // Map CR → NL.
            }
        } else if c == b'\n' && (iflag & INLCR) != 0 {
            c = b'\r'; // Map NL → CR.
        }

        Some(c)
    }

    /// Phase 27: Handle a NUL byte (break condition).
    ///
    /// In real serial hardware, a break is signalled by holding the line low
    /// for longer than a frame.  The driver delivers it as a NUL (0x00).
    /// POSIX break handling:
    ///   - IGNBRK set → discard silently.
    ///   - BRKINT set → flush I/O, send SIGINT to foreground pgrp.
    ///   - PARMRK set → insert 3-byte sequence `\xff \x00 \x00`.
    ///   - Otherwise  → pass the NUL through unchanged.
    fn handle_break(&mut self, iflag: u32) -> InputAction {
        if (iflag & IGNBRK) != 0 {
            return InputAction::None;
        }
        if (iflag & BRKINT) != 0 {
            self.flush_input();
            return InputAction::Signal(SIGINT);
        }
        if (iflag & PARMRK) != 0 {
            // POSIX: a break is encoded as \xff \x00 \x00 in the input stream.
            self.push_cooked(0xFF);
            self.push_cooked(0x00);
            self.push_cooked(0x00);
            return InputAction::None;
        }
        // No break flags set — pass the NUL through as a regular byte.
        InputAction::None
    }

    /// Canonical mode input processing.
    fn canonical_input(&mut self, c: u8, lflag: u32) -> InputAction {
        // VERASE (backspace).
        if c == self.cc(VERASE) || c == 0x08 {
            return self.erase_char(lflag);
        }

        // VKILL (kill line).
        if c == self.cc(VKILL) {
            return self.kill_line(lflag);
        }

        // VEOF (Ctrl+D) — flush without adding a newline.
        if c == self.cc(VEOF) {
            self.flush_edit_to_cooked();
            return InputAction::None;
        }

        // Newline / carriage return — flush with newline appended.
        if c == b'\n' || c == b'\r' {
            if self.edit_len < EDIT_BUF_SIZE {
                self.edit_buf[self.edit_len] = b'\n';
                self.edit_len += 1;
            }
            self.flush_edit_to_cooked();
            self.column = 0;
            if (lflag & (ECHO | ECHONL)) != 0 {
                return InputAction::Echo {
                    buf: [b'\n', 0, 0, 0],
                    len: 1,
                };
            }
            return InputAction::None;
        }

        // Regular character — insert into edit buffer.
        self.insert_char(c, lflag)
    }

    /// Insert a character into the edit buffer and produce an echo action.
    fn insert_char(&mut self, c: u8, lflag: u32) -> InputAction {
        if self.edit_len < EDIT_BUF_SIZE {
            self.edit_buf[self.edit_len] = c;
            self.edit_len += 1;
        }

        if (lflag & ECHO) == 0 {
            return InputAction::None;
        }

        // ECHOCTL: control characters (except TAB, NL) are echoed as ^X.
        if (lflag & ECHOCTL) != 0 && c < 0x20 && c != b'\t' && c != b'\n' {
            self.column += 2;
            return InputAction::Echo {
                buf: [b'^', c + 0x40, 0, 0],
                len: 2,
            };
        }

        if self.is_printable(c) {
            self.column += 1;
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }

        // Non-printable, no ECHOCTL — no echo.
        InputAction::None
    }

    /// Non-canonical (raw) mode: push directly to cooked buffer.
    fn raw_input(&mut self, c: u8, lflag: u32) -> InputAction {
        self.push_cooked(c);
        if (lflag & ECHO) != 0 {
            // ECHOCTL in raw mode.
            if (lflag & ECHOCTL) != 0 && c < 0x20 && c != b'\t' && c != b'\n' {
                return InputAction::Echo {
                    buf: [b'^', c + 0x40, 0, 0],
                    len: 2,
                };
            }
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Erase one character (VERASE / backspace).
    fn erase_char(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        let erased = self.edit_buf[self.edit_len - 1];
        self.edit_len -= 1;

        if (lflag & ECHOE) != 0 {
            // Phase 27: If the erased character was a control char echoed as
            // ^X via ECHOCTL, erase two columns with two BS-SP-BS triples.
            // Use `KillLineEcho` to emit the correct number of columns.
            if (lflag & ECHOCTL) != 0 && erased < 0x20 && erased != b'\t' && erased != b'\n' {
                self.column = self.column.saturating_sub(2);
                return InputAction::KillLineEcho { columns: 2 };
            }
            if self.column > 0 {
                self.column -= 1;
            }
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }
        InputAction::None
    }

    /// Kill the entire line (VKILL).
    fn kill_line(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        // Phase 27: ECHOKE — erase the line visually by backspacing over
        // every character.  We compute the total column width to erase and
        // return a `KillLineEcho` action so the caller can emit the
        // appropriate number of BS-SP-BS triples.
        let cols_to_erase = self.column as u16;
        self.edit_len = 0;
        self.column = 0;

        if (lflag & ECHOKE) != 0 && (lflag & ECHO) != 0 {
            return InputAction::KillLineEcho {
                columns: cols_to_erase,
            };
        }

        if (lflag & ECHOK) != 0 {
            return InputAction::Echo {
                buf: [b'\n', 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Returns `true` if `c` is a word character (alphanumeric or underscore).
    /// Used by `word_erase()` for proper POSIX word boundaries.
    fn is_word_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    /// Word erase (VWERASE / Ctrl+W): erase backward to start of previous word.
    ///
    /// Uses proper word boundaries (alphanumeric + underscore) instead of just
    /// spaces, matching the behavior of most POSIX terminals.  This correctly
    /// handles paths like `/usr/local/bin` -- Ctrl+W erases `bin`, then `local`,
    /// etc.
    fn word_erase(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        let mut erased = 0usize;

        // Phase 1: skip trailing non-word characters (whitespace, punctuation).
        while self.edit_len > 0 && !Self::is_word_char(self.edit_buf[self.edit_len - 1]) {
            self.edit_len -= 1;
            erased += 1;
        }
        // Phase 2: delete word characters (alphanumeric + underscore).
        while self.edit_len > 0 && Self::is_word_char(self.edit_buf[self.edit_len - 1]) {
            self.edit_len -= 1;
            erased += 1;
        }

        self.column = self.column.saturating_sub(erased);

        // Echo backspace-space-backspace for each erased character.
        // We can only return 4 bytes, so for longer erases we just echo
        // a newline + the remaining edit content (like a simplified reprint).
        // Most terminals handle this gracefully.
        if erased <= 1 && (lflag & ECHOE) != 0 {
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }

        // For multi-char erases, request a reprint so the line is redrawn.
        if (lflag & ECHO) != 0 {
            return InputAction::ReprintLine;
        }

        InputAction::None
    }

    /// Look up a control character from the c_cc array.
    fn cc(&self, idx: usize) -> u8 {
        if idx < NCCS {
            self.termios.c_cc[idx]
        } else {
            0
        }
    }

    /// Returns `true` if `c` is a printable ASCII character or tab.
    fn is_printable(&self, c: u8) -> bool {
        (0x20..=0x7E).contains(&c) || c == b'\t'
    }

    /// Track column position for a raw byte (no OPOST processing).
    /// Used when OPOST is disabled so echo column tracking stays accurate.
    fn update_column_raw(&mut self, c: u8) {
        match c {
            b'\n' | b'\r' => self.column = 0,
            b'\t' => self.column += 8 - (self.column % 8),
            0x08 => {
                if self.column > 0 {
                    self.column -= 1;
                }
            }
            c if c >= 0x20 && c < 0x7F => self.column += 1,
            _ => {}
        }
    }

    /// Push a single byte into the cooked ring buffer.
    pub(crate) fn push_cooked(&mut self, c: u8) {
        if self.cooked_count >= COOKED_BUF_SIZE {
            return;
        }
        self.cooked[self.cooked_head] = c;
        self.cooked_head = (self.cooked_head + 1) % COOKED_BUF_SIZE;
        self.cooked_count += 1;
    }

    /// Move everything in the edit buffer into the cooked ring buffer.
    fn flush_edit_to_cooked(&mut self) {
        let mut i = 0usize;
        while i < self.edit_len {
            self.push_cooked(self.edit_buf[i]);
            i += 1;
        }
        self.edit_len = 0;
        self.line_count += 1;
    }
}

// ---------------------------------------------------------------------------
// RawDisc — minimal passthrough line discipline (Phase 14)
// ---------------------------------------------------------------------------

const RAW_BUF_SIZE: usize = 4096;

/// Minimal passthrough line discipline for PTY masters and raw I/O paths.
///
/// No input processing, no echo, no signals, no canonical editing.
/// Bytes pushed via `input_char` go directly to the cooked ring buffer;
/// output bytes pass through without `c_oflag` processing.
pub struct RawDisc {
    termios: UserTermios,
    buf: [u8; RAW_BUF_SIZE],
    head: usize,
    tail: usize,
    count: usize,
}

impl RawDisc {
    /// Create a new `RawDisc` with default raw-mode termios.
    pub const fn new() -> Self {
        Self {
            termios: UserTermios {
                c_iflag: 0,
                c_oflag: 0,
                c_cflag: 0,
                c_lflag: 0,
                c_line: N_RAW as u8,
                c_cc: [0; NCCS],
                c_ispeed: 0,
                c_ospeed: 0,
            },
            buf: [0; RAW_BUF_SIZE],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    pub fn set_termios(&mut self, t: &UserTermios) {
        self.termios = *t;
    }

    pub fn vmin_vtime(&self) -> (u8, u8) {
        (self.termios.c_cc[VMIN], self.termios.c_cc[VTIME])
    }

    pub fn is_canonical(&self) -> bool {
        false
    }

    pub fn has_data(&self) -> bool {
        self.count > 0
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() && self.count > 0 {
            out[copied] = self.buf[self.tail];
            self.tail = (self.tail + 1) % RAW_BUF_SIZE;
            self.count -= 1;
            copied += 1;
        }
        copied
    }

    /// Phase 27: Returns the number of bytes available for reading.
    pub fn bytes_available(&self) -> usize {
        self.count
    }

    pub fn flush_all(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }

    pub fn flush_input(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
    }

    pub fn edit_content(&self) -> &[u8] {
        // Raw mode has no edit buffer.
        &[]
    }

    pub fn is_stopped(&self) -> bool {
        false
    }

    /// Raw input: push byte directly to buffer, no processing.
    pub fn input_char(&mut self, c: u8) -> InputAction {
        if self.count < RAW_BUF_SIZE {
            self.buf[self.head] = c;
            self.head = (self.head + 1) % RAW_BUF_SIZE;
            self.count += 1;
        }
        InputAction::None
    }

    /// Raw output: emit byte directly, no `c_oflag` processing.
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        OutputAction::Emit {
            buf: [c, 0],
            len: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// LdiscKind — swappable line discipline abstraction (Phase 14)
// ---------------------------------------------------------------------------

/// Swappable line discipline — each TTY owns one `LdiscKind`.
///
/// This allows PTY masters (and future SLIP/PPP) to use raw passthrough
/// while normal terminals use full N_TTY processing.
pub enum LdiscKind {
    /// Full N_TTY processing (canonical, echo, signals, etc.)
    NTty(LineDisc),
    /// Raw passthrough (for PTY master, future SLIP/PPP).
    Raw(RawDisc),
}

impl LdiscKind {
    pub fn id(&self) -> u32 {
        match self {
            LdiscKind::NTty(_) => N_TTY,
            LdiscKind::Raw(_) => N_RAW,
        }
    }

    pub fn from_id(ldisc_id: u32, termios: UserTermios) -> Option<Self> {
        match ldisc_id {
            N_TTY => {
                let mut ld = LineDisc::new();
                ld.set_termios(&termios);
                Some(LdiscKind::NTty(ld))
            }
            N_RAW => {
                let mut rd = RawDisc::new();
                rd.set_termios(&termios);
                Some(LdiscKind::Raw(rd))
            }
            _ => None,
        }
    }

    /// Immutable reference to the current termios.
    pub fn termios(&self) -> &UserTermios {
        match self {
            LdiscKind::NTty(ld) => ld.termios(),
            LdiscKind::Raw(rd) => rd.termios(),
        }
    }

    /// Returns (vmin, vtime_deciseconds) for non-canonical mode reads.
    pub fn vmin_vtime(&self) -> (u8, u8) {
        match self {
            LdiscKind::NTty(ld) => ld.vmin_vtime(),
            LdiscKind::Raw(rd) => rd.vmin_vtime(),
        }
    }

    /// Returns true if in canonical mode.
    pub fn is_canonical(&self) -> bool {
        match self {
            LdiscKind::NTty(ld) => ld.is_canonical(),
            LdiscKind::Raw(rd) => rd.is_canonical(),
        }
    }

    /// Update termios.
    pub fn set_termios(&mut self, t: &UserTermios) {
        match self {
            LdiscKind::NTty(ld) => ld.set_termios(t),
            LdiscKind::Raw(rd) => rd.set_termios(t),
        }
    }

    /// Returns `true` if cooked data is available for reading.
    pub fn has_data(&self) -> bool {
        match self {
            LdiscKind::NTty(ld) => ld.has_data(),
            LdiscKind::Raw(rd) => rd.has_data(),
        }
    }

    /// Phase 27: Returns the number of bytes available for reading.
    pub fn bytes_available(&self) -> usize {
        match self {
            LdiscKind::NTty(ld) => ld.bytes_available(),
            LdiscKind::Raw(rd) => rd.bytes_available(),
        }
    }

    /// Read cooked bytes into `out`, returning the number of bytes copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        match self {
            LdiscKind::NTty(ld) => ld.read(out),
            LdiscKind::Raw(rd) => rd.read(out),
        }
    }

    /// Flush all buffers.
    pub fn flush_all(&mut self) {
        match self {
            LdiscKind::NTty(ld) => ld.flush_all(),
            LdiscKind::Raw(rd) => rd.flush_all(),
        }
    }

    pub fn flush_input(&mut self) {
        match self {
            LdiscKind::NTty(ld) => ld.flush_input(),
            LdiscKind::Raw(rd) => rd.flush_input(),
        }
    }

    /// Return a slice of the current edit buffer contents (for VREPRINT echo).
    pub fn edit_content(&self) -> &[u8] {
        match self {
            LdiscKind::NTty(ld) => ld.edit_content(),
            LdiscKind::Raw(rd) => rd.edit_content(),
        }
    }

    /// Whether output is currently stopped (XOFF / Ctrl+S).
    pub fn is_stopped(&self) -> bool {
        match self {
            LdiscKind::NTty(ld) => ld.is_stopped(),
            LdiscKind::Raw(rd) => rd.is_stopped(),
        }
    }

    /// Process a single raw input byte through the line discipline.
    pub fn input_char(&mut self, c: u8) -> InputAction {
        match self {
            LdiscKind::NTty(ld) => ld.input_char(c),
            LdiscKind::Raw(rd) => rd.input_char(c),
        }
    }

    /// Process a single byte through output processing before sending to the driver.
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        match self {
            LdiscKind::NTty(ld) => ld.process_output_byte(c),
            LdiscKind::Raw(rd) => rd.process_output_byte(c),
        }
    }
}
