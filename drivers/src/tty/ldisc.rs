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
//! - **Flow control**: IXON with VSTOP/VSTART (Ctrl+S / Ctrl+Q), IXOFF
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
    CcIndex, ControlFlags, InputFlags, LocalFlags, N_RAW, N_TTY, NCCS, OutputFlags, POSIX_VDISABLE,
    UserTermios,
};

// Finishing Phase 6: Expanded from 1024 to 4096 to match Linux/RedoxOS.
// Handles long pastes, history expansion, and heredoc input gracefully.
const EDIT_BUF_SIZE: usize = 4096;
const COOKED_BUF_SIZE: usize = 4096;

// Finishing Phase 2: PTY throttle water marks for back-pressure.
// When the cooked buffer occupancy hits high-water, the slave sets
// `throttled = true` on the TTY, signalling the master to stop writing.
// When a read drains occupancy to the low-water mark, the slave clears
// `throttled` and wakes the master so it can resume.
pub(crate) const THROTTLE_HIGH_WATER: usize = COOKED_BUF_SIZE * 3 / 4; // 3072
pub(crate) const THROTTLE_LOW_WATER: usize = COOKED_BUF_SIZE / 4; // 1024

// Phase 36: IXOFF flow-control water marks.
// High-water: send XOFF when combined pending input exceeds this.
// Low-water: send XON when combined pending input drops below this.
const IXOFF_TOTAL_CAPACITY: usize = EDIT_BUF_SIZE + COOKED_BUF_SIZE;
const IXOFF_HIGH_WATER: usize = (IXOFF_TOTAL_CAPACITY * 4) / 5; // 80%
const IXOFF_LOW_WATER: usize = IXOFF_TOTAL_CAPACITY / 5; // 20%

// Finishing Phase 10: Input wake batching threshold.
// In non-canonical mode, wake readers only when this many bytes have
// accumulated since the last wakeup (or a near-full / hangup / signal
// condition occurs).  Canonical mode always wakes immediately on line
// completion.  Matches Linux's WAKEUP_CHARS semantics from n_tty.c.
pub(crate) const WAKEUP_CHARS: usize = 256;

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
    /// Phase 36: Ring the terminal bell (BEL, `\x07`).
    ///
    /// Returned when IMAXBEL is set and the input buffer is full.
    /// The caller should send BEL to the output driver.
    Bell,
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
// LdiscOps — shared contract for line discipline variants (Phase 29)
// ---------------------------------------------------------------------------

/// Trait formalising the shared API surface of every line-discipline variant.
///
/// `LdiscKind` delegates to this trait via the `dispatch_ldisc!` macro so that
/// adding a new method only requires one signature change instead of a manual
/// match arm in every wrapper.
///
/// **Crate-internal only** — external API surface is unchanged.
#[allow(dead_code)]
pub(crate) trait LdiscOps {
    fn termios(&self) -> &UserTermios;
    fn vmin_vtime(&self) -> (u8, u8);
    fn is_canonical(&self) -> bool;
    fn set_termios(&mut self, t: &UserTermios);
    fn has_data(&self) -> bool;
    fn bytes_available(&self) -> usize;
    fn read(&mut self, out: &mut [u8]) -> usize;
    fn flush_all(&mut self);
    fn flush_input(&mut self);
    fn edit_content(&self) -> &[u8];
    fn is_stopped(&self) -> bool;
    fn input_char(&mut self, c: u8) -> InputAction;
    fn process_output_byte(&mut self, c: u8) -> OutputAction;
    fn ixoff_check_xoff(&mut self) -> Option<u8>;
    fn ixoff_check_xon(&mut self) -> Option<u8>;
    fn input_full(&self) -> bool;
    fn should_wake_reader(&mut self) -> bool;
    fn no_room(&self) -> bool;
    fn overflow_count(&self) -> u32;
    fn check_no_room_recovery(&mut self) -> bool;
}

/// Generates delegating `impl LdiscKind` methods that forward to the inner
/// variant via the `LdiscOps` trait.  Supports `&self` and `&mut self` receivers.
///
/// Usage:
/// ```ignore
/// dispatch_ldisc! {
///     fn termios(&self) -> &UserTermios;
///     fn set_termios(&mut self, t: &UserTermios);
/// }
/// ```
macro_rules! dispatch_ldisc {
    // &self, with return type
    (@one &, $name:ident, ($($arg:ident : $argty:ty),*), $ret:ty) => {
        #[inline]
        pub fn $name(&self $(, $arg: $argty)*) -> $ret {
            match self {
                LdiscKind::NTty(inner) => inner.$name($($arg),*),
                LdiscKind::Raw(inner) => inner.$name($($arg),*),
            }
        }
    };
    // &self, no return type
    (@one &, $name:ident, ($($arg:ident : $argty:ty),*), ()) => {
        #[inline]
        pub fn $name(&self $(, $arg: $argty)*) {
            match self {
                LdiscKind::NTty(inner) => inner.$name($($arg),*),
                LdiscKind::Raw(inner) => inner.$name($($arg),*),
            }
        }
    };
    // &mut self, with return type
    (@one &mut, $name:ident, ($($arg:ident : $argty:ty),*), $ret:ty) => {
        #[inline]
        pub fn $name(&mut self $(, $arg: $argty)*) -> $ret {
            match self {
                LdiscKind::NTty(inner) => inner.$name($($arg),*),
                LdiscKind::Raw(inner) => inner.$name($($arg),*),
            }
        }
    };
    // &mut self, no return type
    (@one &mut, $name:ident, ($($arg:ident : $argty:ty),*), ()) => {
        #[inline]
        pub fn $name(&mut self $(, $arg: $argty)*) {
            match self {
                LdiscKind::NTty(inner) => inner.$name($($arg),*),
                LdiscKind::Raw(inner) => inner.$name($($arg),*),
            }
        }
    };
    // ── Entry arms ──────────────────────────────────────────────────────
    // &self with return type
    (fn $name:ident(&self $(, $arg:ident : $argty:ty)*) -> $ret:ty; $($tail:tt)*) => {
        dispatch_ldisc!(@one &, $name, ($($arg : $argty),*), $ret);
        dispatch_ldisc!($($tail)*);
    };
    // &self no return type
    (fn $name:ident(&self $(, $arg:ident : $argty:ty)*); $($tail:tt)*) => {
        dispatch_ldisc!(@one &, $name, ($($arg : $argty),*), ());
        dispatch_ldisc!($($tail)*);
    };
    // &mut self with return type
    (fn $name:ident(&mut self $(, $arg:ident : $argty:ty)*) -> $ret:ty; $($tail:tt)*) => {
        dispatch_ldisc!(@one &mut, $name, ($($arg : $argty),*), $ret);
        dispatch_ldisc!($($tail)*);
    };
    // &mut self no return type
    (fn $name:ident(&mut self $(, $arg:ident : $argty:ty)*); $($tail:tt)*) => {
        dispatch_ldisc!(@one &mut, $name, ($($arg : $argty),*), ());
        dispatch_ldisc!($($tail)*);
    };
    // base case: empty
    () => {};
}

// ---------------------------------------------------------------------------
// UTF-8 helpers (Phase 35: IUTF8)
// ---------------------------------------------------------------------------

/// Returns `true` if `b` is a UTF-8 continuation byte (`10xxxxxx`).
#[inline]
const fn utf8_is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Returns the total number of bytes in a UTF-8 sequence given the leading byte.
/// Returns 1 for ASCII (< 0x80) and invalid leading bytes as a fallback.
#[inline]
const fn utf8_byte_count(leading: u8) -> usize {
    if leading < 0x80 {
        1
    } else if leading < 0xC0 {
        1 // continuation byte used as leading — fallback
    } else if leading < 0xE0 {
        2
    } else if leading < 0xF0 {
        3
    } else if leading < 0xF8 {
        4
    } else {
        1 // invalid — fallback
    }
}

/// Decode a UTF-8 codepoint from a complete byte sequence.
/// Returns U+FFFD (replacement character) for empty or oversized slices.
fn utf8_decode(buf: &[u8]) -> u32 {
    match buf.len() {
        1 => buf[0] as u32,
        2 => ((buf[0] as u32 & 0x1F) << 6) | (buf[1] as u32 & 0x3F),
        3 => {
            ((buf[0] as u32 & 0x0F) << 12) | ((buf[1] as u32 & 0x3F) << 6) | (buf[2] as u32 & 0x3F)
        }
        4 => {
            ((buf[0] as u32 & 0x07) << 18)
                | ((buf[1] as u32 & 0x3F) << 12)
                | ((buf[2] as u32 & 0x3F) << 6)
                | (buf[3] as u32 & 0x3F)
        }
        _ => 0xFFFD,
    }
}

/// Returns the display width of a Unicode codepoint.
///
/// Returns 2 for CJK Unified Ideographs, fullwidth forms, Hangul syllables,
/// and common emoji ranges.  Returns 1 for everything else.
///
/// This is a range-based approximation — not a full Unicode database — but
/// covers the vast majority of wide characters encountered in practice.
pub(crate) fn utf8_char_width(codepoint: u32) -> u8 {
    match codepoint {
        // Hangul Jamo
        0x1100..=0x115F => 2,
        // Hangul Jamo Extended-A
        0xA960..=0xA97C => 2,
        // CJK Radicals Supplement .. Yi Radicals
        0x2E80..=0xA4CF => 2,
        // Hangul Syllables
        0xAC00..=0xD7AF => 2,
        // CJK Compatibility Ideographs
        0xF900..=0xFAFF => 2,
        // CJK Compatibility Forms
        0xFE30..=0xFE6F => 2,
        // Fullwidth Forms (excluding halfwidth)
        0xFF01..=0xFF60 => 2,
        // Fullwidth Signs
        0xFFE0..=0xFFE6 => 2,
        // CJK Unified Ideographs Extension B .. Kangxi Radicals Supplement
        0x20000..=0x2FA1F => 2,
        // Miscellaneous Symbols and Pictographs, Emoticons, etc. (common emoji)
        0x1F300..=0x1F9FF => 2,
        // Supplemental Symbols and Pictographs
        0x1FA00..=0x1FA6F => 2,
        // Symbols and Pictographs Extended-A
        0x1FA70..=0x1FAFF => 2,
        // Everything else — single width
        _ => 1,
    }
}

/// Count the bytes of the trailing UTF-8 codepoint at the end of `buf`.
///
/// Scans backward from the last byte through continuation bytes to find the
/// leading byte.  Returns the total byte count (1–4) of the trailing codepoint.
/// Returns 1 for orphan continuation bytes or invalid sequences.
fn utf8_trailing_codepoint_len(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let last = buf.len() - 1;

    // If the last byte is not a continuation byte it is either ASCII or a
    // (possibly invalid) leading byte — either way, it is a single codepoint.
    if !utf8_is_continuation(buf[last]) {
        return 1;
    }

    // Scan backward through continuation bytes (at most 3).
    let mut cont_bytes: usize = 0;
    let mut pos = last;
    while pos > 0 && utf8_is_continuation(buf[pos]) && cont_bytes < 3 {
        cont_bytes += 1;
        pos -= 1;
    }

    // `buf[pos]` should be the leading byte.  If it is still a continuation
    // byte we ran into the start of the buffer — treat as orphan, erase 1.
    if utf8_is_continuation(buf[pos]) {
        return 1;
    }

    let expected = utf8_byte_count(buf[pos]);
    let actual = cont_bytes + 1;

    // Valid sequence: expected byte count matches what we found.
    if actual == expected {
        return actual;
    }

    // More continuation bytes than expected → orphans; erase just 1.
    // Fewer → incomplete sequence; erase the partial group.
    if actual > expected { 1 } else { actual }
}

/// Returns `true` if `cp` is an ASCII word character (alphanumeric or `_`).
/// Non-ASCII codepoints are treated as non-word for POSIX word boundaries.
#[inline]
fn is_word_codepoint(cp: u32) -> bool {
    if cp <= 0x7F {
        let c = cp as u8;
        c.is_ascii_alphanumeric() || c == b'_'
    } else {
        false
    }
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

    // -- UTF-8 multi-byte tracking (Phase 35: IUTF8) --
    /// Number of UTF-8 continuation bytes still expected for the current
    /// multi-byte character being inserted.  0 = not in a multi-byte sequence.
    /// Only meaningful when IUTF8 is set in `c_iflag`.
    utf8_remaining: u8,

    // -- Phase 36: IXOFF flow-control state --
    /// Whether an XOFF has been sent to the terminal (via IXOFF).
    /// When `true`, a subsequent low-water condition triggers XON.
    xoff_sent: bool,

    // -- Phase 37: Deferred reprint (PENDIN) --
    /// When `true`, the next `input_char()` call triggers an automatic
    /// reprint of the edit buffer before processing the new byte.  Set
    /// by `set_termios()` when echo-affecting flags change.
    pending_reprint: bool,

    // -- Phase 42: ECHOPRT hardcopy erase sequence --
    /// When `true`, we are inside a `\...` erase sequence (ECHOPRT).  The
    /// next non-erase input closes the sequence by emitting `/`.
    in_erase_seq: bool,

    // -- Finishing Phase 10: Input wake batching --
    /// Number of bytes pushed to the cooked buffer since the last time
    /// we woke readers.  In non-canonical mode, we suppress wakeups until
    /// this counter crosses `WAKEUP_CHARS` (or an immediate-wake condition
    /// such as buffer-near-full or canonical line completion applies).
    wake_chars_pending: usize,

    // -- Finishing Phase 12: no_room-style overflow recovery --
    /// Sticky overflow flag.  Set when `push_cooked()` fails because the
    /// cooked buffer is completely full.  Cleared when a read drains
    /// occupancy below `THROTTLE_LOW_WATER` (via `check_no_room_recovery()`)
    /// or on `flush_input()`/`flush_all()`.  Allows deterministic recovery
    /// wakeups so blocked producers retry after the reader drains.
    no_room: bool,

    /// Cumulative count of bytes dropped due to cooked buffer overflow.
    /// Incremented by `push_cooked()` on each failed push.  Reset on
    /// `flush_input()`/`flush_all()`.  Diagnostic-only.
    overflow_count: u32,
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
            0,    // VEOL2 (disabled)
            0, 0,
        ];
        Self {
            termios: UserTermios {
                c_iflag: slopos_abi::syscall::ICRNL,
                c_oflag: slopos_abi::syscall::OPOST
                    | slopos_abi::syscall::ONLCR
                    | slopos_abi::syscall::XTABS,
                c_cflag: slopos_abi::syscall::CS8
                    | slopos_abi::syscall::CREAD
                    | slopos_abi::syscall::HUPCL
                    | slopos_abi::syscall::B38400,
                c_lflag: slopos_abi::syscall::ISIG
                    | slopos_abi::syscall::ICANON
                    | slopos_abi::syscall::ECHO
                    | slopos_abi::syscall::ECHOE
                    | slopos_abi::syscall::ECHOK
                    | slopos_abi::syscall::ECHOCTL
                    | slopos_abi::syscall::ECHOKE,
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
            utf8_remaining: 0,
            xoff_sent: false,
            pending_reprint: false,
            in_erase_seq: false,
            wake_chars_pending: 0,
            no_room: false,
            overflow_count: 0,
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
        let vtime = self.termios.c_cc[CcIndex::Vtime.as_usize()];
        let vmin = self.termios.c_cc[CcIndex::Vmin.as_usize()];
        (vmin, vtime)
    }

    /// Returns true if in canonical mode.
    pub fn is_canonical(&self) -> bool {
        self.termios.local_flags().contains(LocalFlags::ICANON)
    }

    /// Update termios.  If canonical mode is toggled off, flushes the edit
    /// buffer so that any pending characters become available for raw reads.
    pub fn set_termios(&mut self, t: &UserTermios) {
        let old_lflag = self.termios.local_flags();
        let was_canon = old_lflag.contains(LocalFlags::ICANON);
        let new_lflag = LocalFlags::from_bits_truncate(t.c_lflag);
        let is_canon = new_lflag.contains(LocalFlags::ICANON);

        // Phase 37: Detect echo-affecting flag changes and set PENDIN so
        // the next input_char() reprints the edit buffer under the new
        // settings.  Only meaningful when there is content to reprint.
        const ECHO_MASK: LocalFlags = LocalFlags::ECHO
            .union(LocalFlags::ECHOE)
            .union(LocalFlags::ECHOK)
            .union(LocalFlags::ECHONL)
            .union(LocalFlags::ECHOCTL)
            .union(LocalFlags::ECHOKE)
            .union(LocalFlags::ICANON);
        if old_lflag.intersection(ECHO_MASK) != new_lflag.intersection(ECHO_MASK)
            && self.edit_len > 0
        {
            self.pending_reprint = true;
        }

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
        // Phase 41: EXTPROC bypasses canonical line buffering, so treat
        // it like non-canonical mode for readiness checks.
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);
        if canonical {
            self.line_count > 0
        } else {
            self.cooked_count > 0
        }
    }

    /// Read cooked bytes into `out`, returning the number of bytes copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        // Phase 41: EXTPROC bypasses canonical line buffering.
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);

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
            // A line boundary is marked by a newline or an enabled
            // VEOL/VEOL2 character (Phase 34).
            if canonical && (byte == b'\n' || self.is_veol(byte)) {
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

    /// Returns `true` if both the edit and cooked buffers are full.
    ///
    /// Used by PTY slave write paths to detect back-pressure before
    /// pushing bytes that would otherwise be silently dropped.
    pub fn input_full(&self) -> bool {
        self.cooked_count >= COOKED_BUF_SIZE && self.edit_len >= EDIT_BUF_SIZE
    }

    /// Finishing Phase 12: Returns `true` if the cooked buffer has
    /// entered overflow state (at least one byte was dropped).
    pub fn no_room(&self) -> bool {
        self.no_room
    }

    /// Finishing Phase 12: Returns the cumulative count of bytes
    /// dropped due to cooked buffer overflow.
    pub fn overflow_count(&self) -> u32 {
        self.overflow_count
    }

    /// Finishing Phase 12: Check and clear no-room recovery condition.
    ///
    /// Returns `true` if `no_room` was set and occupancy has dropped
    /// below `THROTTLE_LOW_WATER`, clearing the flag.  The caller
    /// should wake relevant waiters to re-arm the producer path.
    pub fn check_no_room_recovery(&mut self) -> bool {
        if self.no_room && self.cooked_count <= THROTTLE_LOW_WATER {
            self.no_room = false;
            true
        } else {
            false
        }
    }

    /// Finishing Phase 10: Decide whether the caller should wake readers.
    ///
    /// In **canonical mode**, returns `true` when at least one complete line
    /// is available (`line_count > 0`) — matching existing immediate-wake
    /// semantics.
    ///
    /// In **non-canonical mode**, batches wakeups: returns `true` only when
    /// `wake_chars_pending` crosses `WAKEUP_CHARS` or the cooked buffer is
    /// nearly full (within 64 bytes of capacity).  This reduces scheduler
    /// churn on high-rate input streams while guaranteeing no starvation.
    ///
    /// Resets `wake_chars_pending` when returning `true`.
    pub fn should_wake_reader(&mut self) -> bool {
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);
        if canonical {
            // Canonical mode: wake immediately when a complete line is ready.
            if self.line_count > 0 {
                self.wake_chars_pending = 0;
                return true;
            }
            return false;
        }

        // Non-canonical mode: batch wakeups.
        if self.cooked_count == 0 {
            return false;
        }

        let near_full = self.cooked_count >= COOKED_BUF_SIZE.saturating_sub(64);
        if self.wake_chars_pending >= WAKEUP_CHARS || near_full {
            self.wake_chars_pending = 0;
            return true;
        }

        false
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
        self.utf8_remaining = 0;
        self.xoff_sent = false;
        self.pending_reprint = false;
        self.in_erase_seq = false;
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
    }

    pub fn flush_input(&mut self) {
        self.edit_len = 0;
        self.cooked_head = 0;
        self.cooked_tail = 0;
        self.cooked_count = 0;
        self.line_count = 0;
        self.literal_next = false;
        self.utf8_remaining = 0;
        self.xoff_sent = false;
        self.pending_reprint = false;
        self.in_erase_seq = false;
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
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
        // Phase 36: CREAD gate — if the receiver is disabled in c_cflag,
        // silently discard all input.
        if !self.termios.control_flags().contains(ControlFlags::CREAD) {
            return InputAction::None;
        }

        // Phase 37: Deferred reprint (PENDIN) — if echo-affecting flags
        // changed since the last input, reprint the edit buffer *before*
        // processing this byte so the user sees up-to-date echo output.
        if self.pending_reprint {
            self.pending_reprint = false;
            return InputAction::ReprintLine;
        }

        let iflag = self.termios.input_flags();
        let lflag = self.termios.local_flags();

        // Phase 27: Break handling.  A NUL byte (0x00) is treated as a
        // break condition when any of IGNBRK/BRKINT/PARMRK is set.
        if c == 0x00
            && iflag.intersects(InputFlags::IGNBRK | InputFlags::BRKINT | InputFlags::PARMRK)
        {
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
            let close_erase = self.in_erase_seq;
            if close_erase {
                self.in_erase_seq = false;
            }
            return self.insert_char(c, lflag, close_erase);
        }

        // 3. Signal generation (ISIG) + Phase 23 NOFLSH flush.
        if lflag.contains(LocalFlags::ISIG) {
            let sig = if c == self.cc(CcIndex::Vintr) {
                Some(SIGINT)
            } else if c == self.cc(CcIndex::Vquit) {
                Some(SIGQUIT)
            } else if c == self.cc(CcIndex::Vsusp) {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                // POSIX: unless NOFLSH is set, flush input queues on signal.
                if !lflag.contains(LocalFlags::NOFLSH) {
                    self.flush_input();
                }
                return InputAction::Signal(sig);
            }
        }

        // 4. Flow control (IXON).
        if iflag.contains(InputFlags::IXON) {
            if c == self.cc(CcIndex::Vstop) {
                self.stopped = true;
                return InputAction::None;
            }
            if c == self.cc(CcIndex::Vstart) {
                self.stopped = false;
                return InputAction::None;
            }
            // Any character resumes output when stopped (if IXON is set).
            if self.stopped {
                self.stopped = false;
            }
        }

        // 5. EXTPROC bypass (Phase 41): when EXTPROC is set, the line
        //    discipline passes input directly to the read buffer without
        //    canonical editing or echo.  Signal processing (ISIG, step 3)
        //    and flow control (IXON, step 4) are already handled above.
        if lflag.contains(LocalFlags::EXTPROC) {
            return self.extproc_input(c);
        }

        // 6. Extended input processing (IEXTEN).
        if lflag.contains(LocalFlags::IEXTEN) {
            if c == self.cc(CcIndex::Vlnext) {
                self.literal_next = true;
                // Echo ^V if ECHOCTL is set.
                if lflag.contains(LocalFlags::ECHOCTL | LocalFlags::ECHO) {
                    return InputAction::Echo {
                        buf: [b'^', b'V', 0, 0],
                        len: 2,
                    };
                }
                return InputAction::None;
            }
            if lflag.contains(LocalFlags::ICANON) {
                if c == self.cc(CcIndex::Vwerase) {
                    return self.word_erase(lflag);
                }
                if c == self.cc(CcIndex::Vreprint) {
                    // Phase 37: An explicit VREPRINT also clears any
                    // pending deferred-reprint so we don't double-echo.
                    self.pending_reprint = false;
                    return InputAction::ReprintLine;
                }
            }
        }

        // 7. Canonical vs non-canonical.
        if lflag.contains(LocalFlags::ICANON) {
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
        let oflag = self.termios.output_flags();
        if !oflag.contains(OutputFlags::OPOST) {
            // No output processing — still track column for echo accuracy.
            self.update_column_raw(c);
            return OutputAction::Emit {
                buf: [c, 0],
                len: 1,
            };
        }
        // Phase 42: OLCUC — map lowercase output to uppercase.
        let c = if oflag.contains(OutputFlags::OLCUC) && c.is_ascii_lowercase() {
            c.to_ascii_uppercase()
        } else {
            c
        };

        match c {
            b'\n' if oflag.contains(OutputFlags::ONLCR) => {
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\r', b'\n'],
                    len: 2,
                }
            }
            b'\r' if oflag.contains(OutputFlags::OCRNL) => {
                // OCRNL: convert CR to NL.  If ONLRET is also set, reset column.
                if oflag.contains(OutputFlags::ONLRET) {
                    self.column = 0;
                }
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\r' if oflag.contains(OutputFlags::ONOCR) && self.column == 0 => {
                OutputAction::Suppress
            }
            b'\n' if oflag.contains(OutputFlags::ONLRET) => {
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
                // Finishing Phase 11: Gate tab expansion through TABDLY/XTABS.
                // TAB3/XTABS: expand tab to spaces using column tracking.
                // TAB0 (no TABDLY bits set): pass literal tab to terminal.
                let tab_advance = 8 - (self.column % 8);
                self.column += tab_advance;
                if oflag.contains(OutputFlags::TAB3) {
                    OutputAction::Tab(tab_advance as u8)
                } else {
                    OutputAction::Emit {
                        buf: [b'\t', 0],
                        len: 1,
                    }
                }
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
    fn process_iflag(&self, c: u8, iflag: InputFlags) -> Option<u8> {
        let mut c = c;

        // ISTRIP: strip bit 7.
        if iflag.contains(InputFlags::ISTRIP) {
            c &= 0x7F;
        }

        // CR/NL mapping.
        if c == b'\r' {
            if iflag.contains(InputFlags::IGNCR) {
                return None; // Discard CR entirely.
            }
            if iflag.contains(InputFlags::ICRNL) {
                c = b'\n'; // Map CR → NL.
            }
        } else if c == b'\n' && iflag.contains(InputFlags::INLCR) {
            c = b'\r'; // Map NL → CR.
        }

        // Phase 42: IUCLC — map uppercase input to lowercase.
        if iflag.contains(InputFlags::IUCLC) && c.is_ascii_uppercase() {
            c = c.to_ascii_lowercase();
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
    fn handle_break(&mut self, iflag: InputFlags) -> InputAction {
        if iflag.contains(InputFlags::IGNBRK) {
            return InputAction::None;
        }
        if iflag.contains(InputFlags::BRKINT) {
            self.flush_input();
            return InputAction::Signal(SIGINT);
        }
        if iflag.contains(InputFlags::PARMRK) {
            // POSIX: a break is encoded as \xff \x00 \x00 in the input
            // stream.  All three bytes must be inserted atomically —
            // a partial sequence would be misparsed by userland as either
            // a stray 0xFF literal or the start of a different PARMRK
            // encoding.  If the cooked buffer does not have room for the
            // full triplet, drop it entirely (matching Linux's
            // n_tty_receive_break behaviour) and ring the bell when
            // IMAXBEL is set.
            if self.cooked_free() < 3 {
                if iflag.contains(InputFlags::IMAXBEL) {
                    return InputAction::Bell;
                }
                return InputAction::None;
            }
            self.push_cooked(0xFF);
            self.push_cooked(0x00);
            self.push_cooked(0x00);
            return InputAction::None;
        }
        // No break flags set — pass the NUL through as a regular byte.
        InputAction::None
    }

    /// Canonical mode input processing.
    fn canonical_input(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        // VERASE (backspace).
        if c == self.cc(CcIndex::Verase) || c == 0x08 {
            return self.erase_char(lflag);
        }

        // Phase 42: Close any active ECHOPRT erase sequence. Every
        // non-erase input that reaches canonical processing ends the
        // `\...` sequence by emitting `/`.
        let close_erase = self.in_erase_seq;
        if close_erase {
            self.in_erase_seq = false;
        }

        // VKILL (kill line).
        if c == self.cc(CcIndex::Vkill) {
            return self.kill_line(lflag);
        }

        // VEOF (Ctrl+D) — flush without adding a newline.
        if c == self.cc(CcIndex::Veof) {
            self.flush_edit_to_cooked();
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', 0, 0, 0],
                    len: 1,
                };
            }
            return InputAction::None;
        }

        // Phase 34: VEOL / VEOL2 — additional configurable line terminators.
        // The character is added to the edit buffer (unlike VEOF) and then
        // flushed, completing a canonical line.  Echoed normally — no ECHOCTL.
        if self.is_veol(c) {
            if self.edit_len < EDIT_BUF_SIZE {
                self.edit_buf[self.edit_len] = c;
                self.edit_len += 1;
            }
            self.flush_edit_to_cooked();
            if lflag.contains(LocalFlags::ECHO) {
                if self.is_printable(c) {
                    self.column += 1;
                }
                if close_erase {
                    return InputAction::Echo {
                        buf: [b'/', c, 0, 0],
                        len: 2,
                    };
                }
                return InputAction::Echo {
                    buf: [c, 0, 0, 0],
                    len: 1,
                };
            }
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', 0, 0, 0],
                    len: 1,
                };
            }
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
            if lflag.intersects(LocalFlags::ECHO | LocalFlags::ECHONL) {
                if close_erase {
                    return InputAction::Echo {
                        buf: [b'/', b'\n', 0, 0],
                        len: 2,
                    };
                }
                return InputAction::Echo {
                    buf: [b'\n', 0, 0, 0],
                    len: 1,
                };
            }
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', 0, 0, 0],
                    len: 1,
                };
            }
            return InputAction::None;
        }

        // Regular character — insert into edit buffer.
        self.insert_char(c, lflag, close_erase)
    }

    /// Insert a character into the edit buffer and produce an echo action.
    ///
    /// `close_erase`: when `true`, a `/` is prepended to the echo output to
    /// close an active ECHOPRT `\...` erase sequence.
    fn insert_char(&mut self, c: u8, lflag: LocalFlags, close_erase: bool) -> InputAction {
        // Phase 36: IMAXBEL — if the edit buffer is full, ring the bell
        // instead of silently discarding.  Without IMAXBEL, discard silently.
        if self.edit_len >= EDIT_BUF_SIZE {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }

        self.edit_buf[self.edit_len] = c;
        self.edit_len += 1;

        // Phase 35: IUTF8 multi-byte column tracking.
        let iutf8 = self.termios.input_flags().contains(InputFlags::IUTF8);
        if iutf8 && c >= 0x80 {
            return self.insert_char_utf8(c, lflag);
        }
        // Reset multi-byte state for ASCII bytes when IUTF8 is active.
        if iutf8 {
            self.utf8_remaining = 0;
        }

        if !lflag.contains(LocalFlags::ECHO) {
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', 0, 0, 0],
                    len: 1,
                };
            }
            return InputAction::None;
        }

        // ECHOCTL: control characters (except TAB, NL) are echoed as ^X.
        if lflag.contains(LocalFlags::ECHOCTL) && c < 0x20 && c != b'\t' && c != b'\n' {
            self.column += 2;
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', b'^', c + 0x40, 0],
                    len: 3,
                };
            }
            return InputAction::Echo {
                buf: [b'^', c + 0x40, 0, 0],
                len: 2,
            };
        }

        if self.is_printable(c) {
            self.column += 1;
            if close_erase {
                return InputAction::Echo {
                    buf: [b'/', c, 0, 0],
                    len: 2,
                };
            }
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }

        // Non-printable, no ECHOCTL — no echo.
        if close_erase {
            return InputAction::Echo {
                buf: [b'/', 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Phase 35: Insert a multi-byte UTF-8 byte (>= 0x80) with proper column
    /// tracking.  Continuation bytes do not advance the column; the column is
    /// incremented by the codepoint's display width only when the final
    /// continuation byte completes the sequence.
    fn insert_char_utf8(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        if utf8_is_continuation(c) {
            if self.utf8_remaining > 0 {
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    // Codepoint complete — decode and add display width.
                    let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
                    let cp = utf8_decode(
                        &self.edit_buf[self.edit_len.saturating_sub(cp_len)..self.edit_len],
                    );
                    self.column += utf8_char_width(cp) as usize;
                }
            } else {
                // Orphan continuation byte — treat as width 1.
                self.column += 1;
            }
        } else {
            // Leading byte of a new multi-byte sequence.
            let total = utf8_byte_count(c);
            if total > 1 {
                self.utf8_remaining = (total - 1) as u8;
            } else {
                // Shouldn't reach here (c >= 0x80 but byte_count == 1 means
                // invalid leading byte) — treat as width 1.
                self.column += 1;
            }
        }

        // Always echo the raw byte so the terminal can reconstruct the
        // multi-byte character.
        if lflag.contains(LocalFlags::ECHO) {
            InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            }
        } else {
            InputAction::None
        }
    }

    /// Phase 41: EXTPROC mode — push input directly to the cooked buffer
    /// without any canonical editing or echo.  Used by network terminal
    /// protocols (ssh, telnet) where the remote side handles line editing.
    ///
    /// ISIG signal processing and IXON flow control are already handled
    /// upstream in `input_char()` before this method is called.
    fn extproc_input(&mut self, c: u8) -> InputAction {
        // Finishing Phase 12: Overflow tracking centralised in push_cooked().
        if !self.push_cooked(c) {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }
        // No echo — external processor handles display.
        InputAction::None
    }

    /// Non-canonical (raw) mode: push directly to cooked buffer.
    fn raw_input(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        // Finishing Phase 12: Overflow tracking centralised in push_cooked().
        if !self.push_cooked(c) {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }
        if lflag.contains(LocalFlags::ECHO) {
            // ECHOCTL in raw mode.
            if lflag.contains(LocalFlags::ECHOCTL) && c < 0x20 && c != b'\t' && c != b'\n' {
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
    ///
    /// When IUTF8 is set, erases the full trailing UTF-8 codepoint (1–4 bytes)
    /// and echoes based on the codepoint's display width.  When IUTF8 is not
    /// set, erases exactly one byte (legacy behavior).
    fn erase_char(&mut self, lflag: LocalFlags) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        // Phase 35: IUTF8 multi-byte erase.
        if self.termios.input_flags().contains(InputFlags::IUTF8) {
            return self.erase_char_utf8(lflag);
        }

        // Legacy single-byte erase.
        let erased = self.edit_buf[self.edit_len - 1];
        self.edit_len -= 1;

        // Phase 42: ECHOPRT — hardcopy erase style (\chars/).
        // Takes priority over ECHOE when both ECHOPRT and ECHO are set.
        if lflag.contains(LocalFlags::ECHOPRT | LocalFlags::ECHO) {
            if self.in_erase_seq {
                // Continuing an existing erase sequence — just echo the erased char.
                return InputAction::Echo {
                    buf: [erased, 0, 0, 0],
                    len: 1,
                };
            } else {
                // First erase in a new sequence — output \ then the erased char.
                self.in_erase_seq = true;
                return InputAction::Echo {
                    buf: [b'\\', erased, 0, 0],
                    len: 2,
                };
            }
        }

        if lflag.contains(LocalFlags::ECHOE) {
            // Phase 27: If the erased character was a control char echoed as
            // ^X via ECHOCTL, erase two columns with two BS-SP-BS triples.
            if lflag.contains(LocalFlags::ECHOCTL)
                && erased < 0x20
                && erased != b'\t'
                && erased != b'\n'
            {
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

    /// Phase 35: UTF-8 aware backspace — erases the full trailing codepoint.
    fn erase_char_utf8(&mut self, lflag: LocalFlags) -> InputAction {
        let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
        if cp_len == 0 {
            return InputAction::None;
        }

        let cp_start = self.edit_len - cp_len;
        let codepoint = utf8_decode(&self.edit_buf[cp_start..self.edit_len]);
        let width = utf8_char_width(codepoint) as usize;

        // Remove all bytes of the codepoint.
        self.edit_len = cp_start;
        self.utf8_remaining = 0;

        // Phase 42: ECHOPRT — hardcopy erase style for UTF-8 codepoints.
        // Echo the first byte of the erased codepoint as a representative.
        if lflag.contains(LocalFlags::ECHOPRT | LocalFlags::ECHO) {
            let representative = self.edit_buf[cp_start];
            if self.in_erase_seq {
                return InputAction::Echo {
                    buf: [representative, 0, 0, 0],
                    len: 1,
                };
            } else {
                self.in_erase_seq = true;
                return InputAction::Echo {
                    buf: [b'\\', representative, 0, 0],
                    len: 2,
                };
            }
        }

        if !lflag.contains(LocalFlags::ECHOE) {
            return InputAction::None;
        }

        self.column = self.column.saturating_sub(width);

        if width <= 1 {
            // Single-column character — one BS-SP-BS triple.
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }
        // Multi-column character (e.g. CJK, emoji) — multiple BS-SP-BS triples.
        InputAction::KillLineEcho {
            columns: width as u16,
        }
    }

    /// Kill the entire line (VKILL).
    fn kill_line(&mut self, lflag: LocalFlags) -> InputAction {
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

        if lflag.contains(LocalFlags::ECHOKE | LocalFlags::ECHO) {
            return InputAction::KillLineEcho {
                columns: cols_to_erase,
            };
        }

        if lflag.contains(LocalFlags::ECHOK) {
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
    fn word_erase(&mut self, lflag: LocalFlags) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        // Phase 35: IUTF8 codepoint-aware word erase.
        if self.termios.input_flags().contains(InputFlags::IUTF8) {
            return self.word_erase_utf8(lflag);
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
        if erased <= 1 && lflag.contains(LocalFlags::ECHOE) {
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }

        // For multi-char erases, request a reprint so the line is redrawn.
        if lflag.contains(LocalFlags::ECHO) {
            return InputAction::ReprintLine;
        }

        InputAction::None
    }

    /// Phase 35: UTF-8 aware word erase — erases full codepoints until a word
    /// boundary, tracking column width per codepoint.
    fn word_erase_utf8(&mut self, lflag: LocalFlags) -> InputAction {
        let mut columns_erased: usize = 0;

        // Phase 1: skip trailing non-word codepoints (whitespace, punctuation,
        // CJK characters, etc.).
        while self.edit_len > 0 {
            let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
            if cp_len == 0 {
                break;
            }
            let cp_start = self.edit_len - cp_len;
            let cp = utf8_decode(&self.edit_buf[cp_start..self.edit_len]);
            if is_word_codepoint(cp) {
                break;
            }
            columns_erased += utf8_char_width(cp) as usize;
            self.edit_len = cp_start;
        }

        // Phase 2: erase word codepoints (ASCII alphanumeric + underscore).
        while self.edit_len > 0 {
            let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
            if cp_len == 0 {
                break;
            }
            let cp_start = self.edit_len - cp_len;
            let cp = utf8_decode(&self.edit_buf[cp_start..self.edit_len]);
            if !is_word_codepoint(cp) {
                break;
            }
            columns_erased += utf8_char_width(cp) as usize;
            self.edit_len = cp_start;
        }

        self.column = self.column.saturating_sub(columns_erased);
        self.utf8_remaining = 0;

        if columns_erased == 0 {
            return InputAction::None;
        }

        if columns_erased <= 1 && lflag.contains(LocalFlags::ECHOE) {
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }

        // For multi-column erases, request a reprint so the line is redrawn.
        if lflag.contains(LocalFlags::ECHO) {
            return InputAction::ReprintLine;
        }

        InputAction::None
    }

    /// Look up a control character from the c_cc array.
    fn cc(&self, idx: CcIndex) -> u8 {
        self.termios.c_cc[idx.as_usize()]
    }

    /// Phase 34: Returns `true` if `c` matches an enabled VEOL or VEOL2
    /// control character.  A value of `POSIX_VDISABLE` (0) means disabled.
    fn is_veol(&self, c: u8) -> bool {
        let veol = self.cc(CcIndex::Veol);
        if veol != POSIX_VDISABLE && c == veol {
            return true;
        }
        let veol2 = self.cc(CcIndex::Veol2);
        veol2 != POSIX_VDISABLE && c == veol2
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
    ///
    /// Returns `true` if the byte was enqueued, `false` if the buffer
    /// was full and the byte was dropped (Finishing Phase 3 hardening).
    ///
    /// Finishing Phase 12: On failure, sets the `no_room` sticky flag and
    /// increments `overflow_count` for diagnostics.
    pub(crate) fn push_cooked(&mut self, c: u8) -> bool {
        if self.cooked_count >= COOKED_BUF_SIZE {
            self.no_room = true;
            self.overflow_count = self.overflow_count.saturating_add(1);
            return false;
        }
        self.cooked[self.cooked_head] = c;
        self.cooked_head = (self.cooked_head + 1) % COOKED_BUF_SIZE;
        self.cooked_count += 1;
        self.wake_chars_pending += 1;
        true
    }

    /// Returns the number of free bytes in the cooked ring buffer.
    ///
    /// Used to check whether a multi-byte sequence (e.g. PARMRK's 3-byte
    /// break encoding) can be inserted atomically without partial pushes.
    fn cooked_free(&self) -> usize {
        COOKED_BUF_SIZE - self.cooked_count
    }

    /// Phase 36: IXOFF — check if input buffer exceeds high-water mark.
    /// Returns the XOFF byte (VSTOP) if IXOFF is enabled and we should send XOFF.
    pub fn ixoff_check_xoff(&mut self) -> Option<u8> {
        if !self.termios.input_flags().contains(InputFlags::IXOFF) {
            return None;
        }
        if self.xoff_sent {
            return None;
        }
        let pending = self.edit_len + self.cooked_count;
        if pending >= IXOFF_HIGH_WATER {
            self.xoff_sent = true;
            Some(self.cc(CcIndex::Vstop))
        } else {
            None
        }
    }

    /// Phase 36: IXOFF — check if input buffer dropped below low-water mark.
    /// Returns the XON byte (VSTART) if IXOFF is enabled and we should send XON.
    pub fn ixoff_check_xon(&mut self) -> Option<u8> {
        if !self.termios.input_flags().contains(InputFlags::IXOFF) {
            return None;
        }
        if !self.xoff_sent {
            return None;
        }
        let pending = self.edit_len + self.cooked_count;
        if pending < IXOFF_LOW_WATER {
            self.xoff_sent = false;
            Some(self.cc(CcIndex::Vstart))
        } else {
            None
        }
    }

    /// Move everything in the edit buffer into the cooked ring buffer.
    ///
    /// Uses the `bool` return from `push_cooked()` to stop early if the
    /// cooked buffer fills up.  Any bytes that could not be flushed are
    /// preserved: the unflushed tail is shifted to the front of the edit
    /// buffer and `edit_len` is set to the remainder count.  This prevents
    /// silent data loss when the cooked buffer is partially occupied.
    fn flush_edit_to_cooked(&mut self) {
        let mut i = 0usize;
        while i < self.edit_len {
            if !self.push_cooked(self.edit_buf[i]) {
                break; // cooked buffer full — stop flushing
            }
            i += 1;
        }
        // Preserve any unflushed remainder by shifting to front of edit buffer.
        if i < self.edit_len {
            let remaining = self.edit_len - i;
            self.edit_buf.copy_within(i..self.edit_len, 0);
            self.edit_len = remaining;
        } else {
            self.edit_len = 0;
        }
        self.line_count += 1;
    }
}

impl LdiscOps for LineDisc {
    #[inline]
    fn termios(&self) -> &UserTermios {
        self.termios()
    }
    #[inline]
    fn vmin_vtime(&self) -> (u8, u8) {
        self.vmin_vtime()
    }
    #[inline]
    fn is_canonical(&self) -> bool {
        self.is_canonical()
    }
    #[inline]
    fn set_termios(&mut self, t: &UserTermios) {
        self.set_termios(t)
    }
    #[inline]
    fn has_data(&self) -> bool {
        self.has_data()
    }
    #[inline]
    fn bytes_available(&self) -> usize {
        self.bytes_available()
    }
    #[inline]
    fn read(&mut self, out: &mut [u8]) -> usize {
        self.read(out)
    }
    #[inline]
    fn flush_all(&mut self) {
        self.flush_all()
    }
    #[inline]
    fn flush_input(&mut self) {
        self.flush_input()
    }
    #[inline]
    fn edit_content(&self) -> &[u8] {
        self.edit_content()
    }
    #[inline]
    fn is_stopped(&self) -> bool {
        self.is_stopped()
    }
    #[inline]
    fn input_char(&mut self, c: u8) -> InputAction {
        self.input_char(c)
    }
    #[inline]
    fn process_output_byte(&mut self, c: u8) -> OutputAction {
        self.process_output_byte(c)
    }
    #[inline]
    fn ixoff_check_xoff(&mut self) -> Option<u8> {
        self.ixoff_check_xoff()
    }
    #[inline]
    fn ixoff_check_xon(&mut self) -> Option<u8> {
        self.ixoff_check_xon()
    }
    #[inline]
    fn input_full(&self) -> bool {
        self.input_full()
    }
    #[inline]
    fn should_wake_reader(&mut self) -> bool {
        self.should_wake_reader()
    }
    #[inline]
    fn no_room(&self) -> bool {
        self.no_room()
    }
    #[inline]
    fn overflow_count(&self) -> u32 {
        self.overflow_count()
    }
    #[inline]
    fn check_no_room_recovery(&mut self) -> bool {
        self.check_no_room_recovery()
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
    // Finishing Phase 10: Input wake batching (same as LineDisc).
    wake_chars_pending: usize,

    // -- Finishing Phase 12: no_room-style overflow recovery --
    /// Sticky overflow flag (see `LineDisc::no_room` for full docs).
    no_room: bool,
    /// Cumulative overflow byte count (see `LineDisc::overflow_count`).
    overflow_count: u32,
}

impl RawDisc {
    /// Create a new `RawDisc` with default raw-mode termios.
    pub const fn new() -> Self {
        Self {
            termios: UserTermios {
                c_iflag: 0,
                c_oflag: 0,
                c_cflag: slopos_abi::syscall::CS8
                    | slopos_abi::syscall::CREAD
                    | slopos_abi::syscall::HUPCL
                    | slopos_abi::syscall::B38400,
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
            wake_chars_pending: 0,
            no_room: false,
            overflow_count: 0,
        }
    }

    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    pub fn set_termios(&mut self, t: &UserTermios) {
        self.termios = *t;
    }

    pub fn vmin_vtime(&self) -> (u8, u8) {
        (
            self.termios.c_cc[CcIndex::Vmin.as_usize()],
            self.termios.c_cc[CcIndex::Vtime.as_usize()],
        )
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

    /// Returns `true` if the raw buffer is full.
    pub fn input_full(&self) -> bool {
        self.count >= RAW_BUF_SIZE
    }

    /// Finishing Phase 12: Returns `true` if overflow state is active.
    pub fn no_room(&self) -> bool {
        self.no_room
    }

    /// Finishing Phase 12: Returns cumulative overflow byte count.
    pub fn overflow_count(&self) -> u32 {
        self.overflow_count
    }

    /// Finishing Phase 12: Check and clear no-room recovery condition.
    pub fn check_no_room_recovery(&mut self) -> bool {
        if self.no_room && self.count <= THROTTLE_LOW_WATER {
            self.no_room = false;
            true
        } else {
            false
        }
    }

    /// Finishing Phase 10: Decide whether the caller should wake readers.
    ///
    /// RawDisc is always non-canonical.  Batches wakeups: returns `true`
    /// only when `wake_chars_pending` crosses `WAKEUP_CHARS` or the buffer
    /// is nearly full.
    pub fn should_wake_reader(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        let near_full = self.count >= RAW_BUF_SIZE.saturating_sub(64);
        if self.wake_chars_pending >= WAKEUP_CHARS || near_full {
            self.wake_chars_pending = 0;
            return true;
        }
        false
    }

    pub fn flush_all(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
    }

    pub fn flush_input(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
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
        // Phase 36: CREAD gate — discard input when receiver disabled.
        if !self.termios.control_flags().contains(ControlFlags::CREAD) {
            return InputAction::None;
        }
        if self.count < RAW_BUF_SIZE {
            self.buf[self.head] = c;
            self.head = (self.head + 1) % RAW_BUF_SIZE;
            self.count += 1;
            self.wake_chars_pending += 1;
        } else {
            // Finishing Phase 12: Record overflow state.
            self.no_room = true;
            self.overflow_count = self.overflow_count.saturating_add(1);
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

    /// Phase 36: IXOFF — RawDisc does not implement IXOFF flow control.
    pub fn ixoff_check_xoff(&mut self) -> Option<u8> {
        None
    }

    /// Phase 36: IXOFF — RawDisc does not implement IXOFF flow control.
    pub fn ixoff_check_xon(&mut self) -> Option<u8> {
        None
    }
}

impl LdiscOps for RawDisc {
    #[inline]
    fn termios(&self) -> &UserTermios {
        self.termios()
    }
    #[inline]
    fn vmin_vtime(&self) -> (u8, u8) {
        self.vmin_vtime()
    }
    #[inline]
    fn is_canonical(&self) -> bool {
        self.is_canonical()
    }
    #[inline]
    fn set_termios(&mut self, t: &UserTermios) {
        self.set_termios(t)
    }
    #[inline]
    fn has_data(&self) -> bool {
        self.has_data()
    }
    #[inline]
    fn bytes_available(&self) -> usize {
        self.bytes_available()
    }
    #[inline]
    fn read(&mut self, out: &mut [u8]) -> usize {
        self.read(out)
    }
    #[inline]
    fn flush_all(&mut self) {
        self.flush_all()
    }
    #[inline]
    fn flush_input(&mut self) {
        self.flush_input()
    }
    #[inline]
    fn edit_content(&self) -> &[u8] {
        self.edit_content()
    }
    #[inline]
    fn is_stopped(&self) -> bool {
        self.is_stopped()
    }
    #[inline]
    fn input_char(&mut self, c: u8) -> InputAction {
        self.input_char(c)
    }
    #[inline]
    fn process_output_byte(&mut self, c: u8) -> OutputAction {
        self.process_output_byte(c)
    }
    #[inline]
    fn ixoff_check_xoff(&mut self) -> Option<u8> {
        self.ixoff_check_xoff()
    }
    #[inline]
    fn ixoff_check_xon(&mut self) -> Option<u8> {
        self.ixoff_check_xon()
    }
    #[inline]
    fn input_full(&self) -> bool {
        self.input_full()
    }
    #[inline]
    fn should_wake_reader(&mut self) -> bool {
        self.should_wake_reader()
    }
    #[inline]
    fn no_room(&self) -> bool {
        self.no_room()
    }
    #[inline]
    fn overflow_count(&self) -> u32 {
        self.overflow_count()
    }
    #[inline]
    fn check_no_room_recovery(&mut self) -> bool {
        self.check_no_room_recovery()
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
    /// Returns the numeric line-discipline identifier (e.g. `N_TTY`, `N_RAW`).
    pub fn id(&self) -> u32 {
        match self {
            LdiscKind::NTty(_) => N_TTY,
            LdiscKind::Raw(_) => N_RAW,
        }
    }

    /// Construct an `LdiscKind` from a numeric id, applying the given termios.
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

    // --- Dispatched methods (Phase 29) ---
    // All methods below are generated by the `dispatch_ldisc!` macro, which
    // delegates to the inner variant via matching.  Adding a new shared method
    // only requires a single signature line here and `impl LdiscOps` entries
    // for each variant.

    dispatch_ldisc! {
        fn termios(&self) -> &UserTermios;
        fn vmin_vtime(&self) -> (u8, u8);
        fn is_canonical(&self) -> bool;
        fn set_termios(&mut self, t: &UserTermios);
        fn has_data(&self) -> bool;
        fn bytes_available(&self) -> usize;
        fn read(&mut self, out: &mut [u8]) -> usize;
        fn flush_all(&mut self);
        fn flush_input(&mut self);
        fn edit_content(&self) -> &[u8];
        fn is_stopped(&self) -> bool;
        fn input_char(&mut self, c: u8) -> InputAction;
        fn process_output_byte(&mut self, c: u8) -> OutputAction;
        fn ixoff_check_xoff(&mut self) -> Option<u8>;
        fn ixoff_check_xon(&mut self) -> Option<u8>;
        fn input_full(&self) -> bool;
        fn should_wake_reader(&mut self) -> bool;
        fn no_room(&self) -> bool;
        fn overflow_count(&self) -> u32;
        fn check_no_room_recovery(&mut self) -> bool;
    }
}
