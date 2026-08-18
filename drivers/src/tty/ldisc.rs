//! N_TTY-style line discipline for the TTY subsystem.
//!
//! The discipline never touches hardware — it returns [`InputAction`] /
//! [`OutputAction`] values the TTY core translates into driver writes.

use slopos_abi::signal::{SIGINT, SIGQUIT, SIGTSTP};
use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, N_RAW, N_TTY, NCCS, OutputFlags, POSIX_VDISABLE,
    UserTermios,
};
use slopos_ostd::ring_buffer::RingBuffer;
use slopos_ostd::{AllocError, PinBox, Zeroable};

use super::driver::{InputEvent, InputStatus};

const EDIT_BUF_SIZE: usize = 4096;
const COOKED_BUF_SIZE: usize = 8192;

// Cooked-buffer occupancy at which the slave sets and clears `throttled`,
// which is what tells the master to stop and resume writing.
pub(crate) const THROTTLE_HIGH_WATER: usize = COOKED_BUF_SIZE * 3 / 4;
pub(crate) const THROTTLE_LOW_WATER: usize = COOKED_BUF_SIZE / 4;

const IXOFF_TOTAL_CAPACITY: usize = EDIT_BUF_SIZE + COOKED_BUF_SIZE;
const IXOFF_HIGH_WATER: usize = (IXOFF_TOTAL_CAPACITY * 4) / 5;
const IXOFF_LOW_WATER: usize = IXOFF_TOTAL_CAPACITY / 5;

// Non-canonical wake batching: readers are woken once this many bytes have
// accumulated, or on a near-full / hangup / signal condition.
pub(crate) const WAKEUP_CHARS: usize = 256;

/// Actions returned by the line discipline after processing an input byte.
#[derive(Debug)]
pub enum InputAction {
    None,
    /// Echo bytes back to the terminal; 8 covers an ECHOPRT prefix plus a
    /// multi-byte UTF-8 representative.
    Echo {
        buf: [u8; 8],
        len: u8,
    },
    /// Deliver a signal to the foreground process group.
    Signal(u8),
    /// Redisplay the current edit line (VREPRINT): the caller writes a newline
    /// followed by [`LineDisc::edit_content()`].
    ReprintLine,
    /// Kill-line visual erase (ECHOKE): the caller emits `columns`
    /// BS-SPACE-BS triples.
    KillLineEcho {
        columns: u16,
    },
    /// Ring the terminal bell — IMAXBEL is set and the input buffer is full.
    Bell,
}

impl InputAction {
    /// Construct an `Echo` action from a byte slice, truncating past 8 bytes.
    #[inline]
    pub fn echo(bytes: &[u8]) -> Self {
        let mut buf = [0u8; 8];
        let len = bytes.len().min(8);
        buf[..len].copy_from_slice(&bytes[..len]);
        Self::Echo {
            buf,
            len: len as u8,
        }
    }

    #[inline]
    pub const fn echo1(b: u8) -> Self {
        Self::Echo {
            buf: [b, 0, 0, 0, 0, 0, 0, 0],
            len: 1,
        }
    }

    #[inline]
    pub const fn echo2(a: u8, b: u8) -> Self {
        Self::Echo {
            buf: [a, b, 0, 0, 0, 0, 0, 0],
            len: 2,
        }
    }

    #[inline]
    pub const fn echo3(a: u8, b: u8, c: u8) -> Self {
        Self::Echo {
            buf: [a, b, c, 0, 0, 0, 0, 0],
            len: 3,
        }
    }
}

/// Actions returned by output processing (`process_output_byte`).
pub enum OutputAction {
    /// Emit these bytes to the driver (up to 2, e.g. `\r\n`).
    Emit {
        buf: [u8; 2],
        len: u8,
    },
    /// Expand a tab to this many spaces.
    Tab(u8),
    Suppress,
}

/// Sized to hold one full `VREPRINT` redisplay — a newline plus the whole edit
/// buffer — so a redisplay of a full line is never clipped.
const ECHO_QUEUE_CAP: usize = EDIT_BUF_SIZE + 512;

/// Bytes the discipline has echoed but not yet handed to a driver.
///
/// Echo is produced under the per-TTY slot lock and emitted after it drops,
/// because a driver write for a PTY end delivers into the peer's slot.
#[derive(Zeroable)]
#[repr(C)]
pub struct EchoQueue {
    ring: [u8; ECHO_QUEUE_CAP],
    head: usize,
    len: usize,
    /// Bytes refused for want of room. Diagnostic only.
    dropped: u32,
    /// Set while one CPU is draining, so a second cannot interleave chunks.
    draining: bool,
}

impl EchoQueue {
    #[inline]
    fn room(&self) -> usize {
        ECHO_QUEUE_CAP - self.len
    }

    /// Append `bytes`, returning how many were accepted.
    fn extend(&mut self, bytes: &[u8]) -> usize {
        let n = core::cmp::min(self.room(), bytes.len());
        for (i, &b) in bytes[..n].iter().enumerate() {
            self.ring[(self.head + self.len + i) % ECHO_QUEUE_CAP] = b;
        }
        self.len += n;
        self.dropped = self.dropped.saturating_add((bytes.len() - n) as u32);
        n
    }

    #[inline]
    fn push(&mut self, byte: u8) -> bool {
        self.extend(&[byte]) == 1
    }

    /// Move up to `out.len()` staged bytes into `out`, oldest first.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = core::cmp::min(out.len(), self.len);
        for (i, slot) in out[..n].iter_mut().enumerate() {
            *slot = self.ring[(self.head + i) % ECHO_QUEUE_CAP];
        }
        self.head = (self.head + n) % ECHO_QUEUE_CAP;
        self.len -= n;
        n
    }

    /// Put bytes a short driver write did not accept back at the front.
    ///
    /// Prepends rather than rewinding `head`: a producer may have appended
    /// into the space this chunk vacated while the slot lock was released.
    fn unread(&mut self, bytes: &[u8]) {
        let n = core::cmp::min(self.room(), bytes.len());
        self.dropped = self.dropped.saturating_add((bytes.len() - n) as u32);
        for &b in bytes[..n].iter().rev() {
            self.head = (self.head + ECHO_QUEUE_CAP - 1) % ECHO_QUEUE_CAP;
            self.ring[self.head] = b;
        }
        self.len += n;
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Staged bytes; they count as pending output, being queued for a driver
    /// they have not reached.
    #[inline]
    fn staged(&self) -> usize {
        self.len
    }

    #[inline]
    fn discard(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// `false` means another CPU is already draining.
    #[inline]
    fn claim_drain(&mut self) -> bool {
        if self.draining {
            false
        } else {
            self.draining = true;
            true
        }
    }

    #[inline]
    fn release_drain(&mut self) {
        self.draining = false;
    }
}

pub struct BatchResult {
    pub signal: Option<(u8, bool)>,
    pub should_wake: bool,
    pub throttle_check: bool,
}

impl BatchResult {
    const fn new() -> Self {
        Self {
            signal: None,
            should_wake: false,
            throttle_check: false,
        }
    }
}

#[inline]
const fn utf8_is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// UTF-8 sequence length from its leading byte; 1 for ASCII and for any byte
/// that cannot lead a sequence.
#[inline]
const fn utf8_byte_count(leading: u8) -> usize {
    if leading < 0x80 {
        1
    } else if leading < 0xC0 {
        1
    } else if leading < 0xE0 {
        2
    } else if leading < 0xF0 {
        3
    } else if leading < 0xF8 {
        4
    } else {
        1
    }
}

/// Decode a UTF-8 codepoint from a complete sequence; U+FFFD for an empty or
/// oversized slice.
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

/// Display width of a codepoint: 2 for CJK, fullwidth forms, Hangul and common
/// emoji, 1 for everything else. A range approximation, not a Unicode table.
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
        _ => 1,
    }
}

/// Byte count of the trailing UTF-8 codepoint in `buf`; 1 for an orphan
/// continuation byte or an invalid sequence, 0 when `buf` is empty.
fn utf8_trailing_codepoint_len(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let last = buf.len() - 1;

    if !utf8_is_continuation(buf[last]) {
        return 1;
    }

    let mut cont_bytes: usize = 0;
    let mut pos = last;
    while pos > 0 && utf8_is_continuation(buf[pos]) && cont_bytes < 3 {
        cont_bytes += 1;
        pos -= 1;
    }

    // Still a continuation byte: the scan ran off the start of the buffer.
    if utf8_is_continuation(buf[pos]) {
        return 1;
    }

    let expected = utf8_byte_count(buf[pos]);
    let actual = cont_bytes + 1;

    if actual == expected {
        return actual;
    }

    // More continuation bytes than expected are orphans; fewer are an
    // incomplete sequence, erased as the partial group.
    if actual > expected { 1 } else { actual }
}

/// ASCII alphanumeric or `_`. Non-ASCII counts as non-word, which is what
/// POSIX word boundaries call for.
#[inline]
fn is_word_codepoint(cp: u32) -> bool {
    if cp <= 0x7F {
        let c = cp as u8;
        c.is_ascii_alphanumeric() || c == b'_'
    } else {
        false
    }
}

/// The line discipline state machine: one per `Tty`, owning the canonical-mode
/// edit buffer and the cooked ring buffer userland `read()`s from.
#[derive(Zeroable)]
#[repr(C)]
pub struct LineDisc {
    termios: UserTermios,

    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,

    cooked: RingBuffer<u8, COOKED_BUF_SIZE>,

    /// Complete lines in the cooked buffer. Canonical `has_data()` gates on
    /// this rather than the byte count, so `read()` never returns a partial line.
    line_count: usize,

    /// Output stopped via XOFF (Ctrl+S / VSTOP).
    stopped: bool,
    /// Next input character is literal (Ctrl+V / VLNEXT was pressed).
    literal_next: bool,

    column: usize,

    /// Continuation bytes still expected for the character being inserted.
    /// Only meaningful under IUTF8.
    utf8_remaining: u8,

    /// An XOFF has been sent; a low-water condition now triggers the XON.
    xoff_sent: bool,

    /// Reprint the edit buffer before the next input byte. Set by
    /// `set_termios()` when echo-affecting flags change.
    pending_reprint: bool,

    /// Inside an ECHOPRT `\...` erase sequence; the next non-erase input
    /// closes it by emitting `/`.
    in_erase_seq: bool,

    /// Bytes pushed to the cooked buffer since readers were last woken.
    wake_chars_pending: usize,

    /// Sticky cooked-buffer overflow. Cleared once a read drains below
    /// `THROTTLE_LOW_WATER`, or on flush, so blocked producers get a wakeup.
    no_room: bool,

    /// Bytes dropped to cooked-buffer overflow. Diagnostic only; reset on flush.
    overflow_count: u32,

    /// FLUSHO — output is discarded until another VDISCARD press.
    flushing_output: bool,

    /// Echo waiting for the TTY core to hand it to the driver once the slot
    /// lock has dropped.
    echo: EchoQueue,
}

impl LineDisc {
    /// Default termios for a fresh `LineDisc`: canonical, echo, signals.
    pub const fn default_termios() -> UserTermios {
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
            0x0F, // VDISCARD = Ctrl+O
            0x17, // VWERASE = Ctrl+W
            0x16, // VLNEXT  = Ctrl+V
            0,    // VEOL2 (disabled)
            0, 0,
        ];
        UserTermios {
            c_iflag: InputFlags::ICRNL,
            c_oflag: OutputFlags::from_bits_retain(
                OutputFlags::OPOST.bits() | OutputFlags::ONLCR.bits() | OutputFlags::XTABS.bits(),
            ),
            c_cflag: ControlFlags::from_bits_retain(
                ControlFlags::CS8.bits()
                    | ControlFlags::CREAD.bits()
                    | ControlFlags::HUPCL.bits()
                    | slopos_abi::syscall::B38400,
            ),
            c_lflag: LocalFlags::from_bits_retain(
                LocalFlags::ISIG.bits()
                    | LocalFlags::ICANON.bits()
                    | LocalFlags::ECHO.bits()
                    | LocalFlags::ECHOE.bits()
                    | LocalFlags::ECHOK.bits()
                    | LocalFlags::ECHOCTL.bits()
                    | LocalFlags::ECHOKE.bits(),
            ),
            c_line: N_TTY as u8,
            c_cc: cc,
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }

    /// Allocate a default `LineDisc` straight onto the heap. The ≈12 KiB
    /// struct must never materialise on the caller's stack: doing so costs
    /// ~20 KiB and overflows the 32 KiB kernel stack during PTY pair setup.
    pub fn new_pinned() -> Result<PinBox<Self>, AllocError> {
        let mut pb = PinBox::<Self>::zeroed()?;
        pb.termios = Self::default_termios();
        Ok(pb)
    }

    /// Panic-on-OOM convenience for callers that cannot recover from it.
    pub fn new() -> PinBox<Self> {
        Self::new_pinned().expect("kernel OOM during LineDisc allocation")
    }

    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    /// `(vmin, vtime)` for non-canonical reads; vtime is in POSIX deciseconds.
    pub fn vmin_vtime(&self) -> (u8, u8) {
        let vtime = self.termios.c_cc[CcIndex::Vtime.as_usize()];
        let vmin = self.termios.c_cc[CcIndex::Vmin.as_usize()];
        (vmin, vtime)
    }

    pub fn is_canonical(&self) -> bool {
        self.termios.local_flags().contains(LocalFlags::ICANON)
    }

    /// Update termios. Toggling canonical mode off flushes the edit buffer so
    /// pending characters become available to raw reads.
    pub fn set_termios(&mut self, t: &UserTermios) {
        let old_lflag = self.termios.local_flags();
        let was_canon = old_lflag.contains(LocalFlags::ICANON);
        let new_lflag = t.c_lflag;
        let is_canon = new_lflag.contains(LocalFlags::ICANON);

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

    /// Whether the cooked ring has readable data. In canonical mode that means
    /// a committed line — a newline or an EOF flush — never a partial one.
    pub fn has_data(&self) -> bool {
        // EXTPROC bypasses canonical line buffering.
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);
        if canonical {
            self.line_count > 0
        } else {
            self.cooked.count() > 0
        }
    }

    /// Read cooked bytes into `out`, returning the number of bytes copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        // EXTPROC bypasses canonical line buffering.
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);

        // VEOF on an empty edit buffer bumps `line_count` without pushing a
        // byte; consume that zero-byte line or `has_data()` stays true forever.
        if canonical && self.line_count > 0 && self.cooked.peek_at_one(0).is_none() {
            self.line_count -= 1;
            return 0;
        }

        let mut copied = 0usize;
        while copied < out.len() {
            let Some(byte) = self.cooked.peek().copied() else {
                break;
            };
            let _ = self.cooked.try_pop();
            out[copied] = byte;
            copied += 1;

            if canonical && (byte == b'\n' || self.is_veol(byte)) {
                self.line_count = self.line_count.saturating_sub(1);
                return copied;
            }
        }
        // Draining the last cooked byte without hitting a newline means an
        // EOF-flushed chunk; a merely-full caller buffer is mid-line and must
        // not decrement.
        if canonical && copied > 0 && self.cooked.count() == 0 && self.line_count > 0 {
            self.line_count -= 1;
        }

        copied
    }

    /// Bytes available to read; backs the FIONREAD / TIOCINQ ioctl.
    pub fn bytes_available(&self) -> usize {
        self.cooked.count()
    }

    /// Both buffers full — PTY slave writes read this as back-pressure rather
    /// than pushing bytes that would be silently dropped.
    pub fn input_full(&self) -> bool {
        self.cooked.is_full() && self.edit_len >= EDIT_BUF_SIZE
    }

    /// Whether the cooked buffer has dropped at least one byte.
    pub fn no_room(&self) -> bool {
        self.no_room
    }

    /// Whether `byte` would be consumed as terminal control input before
    /// ordinary buffering. A throttled PTY slave still admits one such byte, so
    /// a signal or IXON state change is never lost to back-pressure.
    pub(crate) fn priority_control_input(&self, byte: u8) -> bool {
        if !self.termios.control_flags().contains(ControlFlags::CREAD) {
            return false;
        }
        if self.pending_reprint || self.literal_next {
            return false;
        }

        let iflag = self.termios.input_flags();
        let lflag = self.termios.local_flags();
        let c = if byte == 0x00 && iflag.contains(InputFlags::BRKINT) {
            if iflag.contains(InputFlags::IGNBRK) {
                return false;
            }
            self.cc(CcIndex::Vintr)
        } else {
            let Some(c) = self.process_iflag(byte, iflag) else {
                return false;
            };
            c
        };

        if lflag.contains(LocalFlags::ISIG)
            && (self.cc_matches(CcIndex::Vintr, c)
                || self.cc_matches(CcIndex::Vquit, c)
                || self.cc_matches(CcIndex::Vsusp, c))
        {
            return true;
        }

        if iflag.contains(InputFlags::IXON)
            && (self.cc_matches(CcIndex::Vstart, c) || self.cc_matches(CcIndex::Vstop, c))
        {
            return true;
        }

        false
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_count
    }

    /// Clears `no_room` once occupancy drops below `THROTTLE_LOW_WATER`;
    /// `true` means the caller should wake waiters to re-arm the producer path.
    pub fn check_no_room_recovery(&mut self) -> bool {
        if self.no_room && self.cooked.count() <= THROTTLE_LOW_WATER {
            self.no_room = false;
            true
        } else {
            false
        }
    }

    /// Whether the caller should wake readers: a complete line in canonical
    /// mode, any cooked byte otherwise. Resets `wake_chars_pending` on `true`.
    pub fn should_wake_reader(&mut self) -> bool {
        let canonical =
            self.is_canonical() && !self.termios.local_flags().contains(LocalFlags::EXTPROC);
        if canonical {
            if self.line_count > 0 {
                self.wake_chars_pending = 0;
                return true;
            }
            return false;
        }

        if self.cooked.count() > 0 {
            self.wake_chars_pending = 0;
            return true;
        }

        false
    }

    pub fn flush_all(&mut self) {
        self.edit_len = 0;
        self.cooked.flush();
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
        self.flushing_output = false;
        self.echo.discard();
    }

    /// Stage `bytes` for emission. What does not fit is dropped: a terminal
    /// whose echo outruns its own driver has already lost the display, and
    /// stalling input to preserve it would be worse.
    #[inline]
    pub fn echo_stage(&mut self, bytes: &[u8]) {
        self.echo.extend(bytes);
    }

    #[inline]
    pub fn echo_take(&mut self, out: &mut [u8]) -> usize {
        self.echo.take(out)
    }

    #[inline]
    pub fn echo_unread(&mut self, bytes: &[u8]) {
        self.echo.unread(bytes);
    }

    #[inline]
    pub fn echo_is_empty(&self) -> bool {
        self.echo.is_empty()
    }

    /// Staged bytes, for the output-completion queries.
    #[inline]
    pub fn echo_staged(&self) -> usize {
        self.echo.staged()
    }

    #[inline]
    pub fn echo_discard(&mut self) {
        self.echo.discard();
    }

    #[inline]
    pub fn echo_claim_drain(&mut self) -> bool {
        self.echo.claim_drain()
    }

    #[inline]
    pub fn echo_release_drain(&mut self) {
        self.echo.release_drain();
    }

    #[inline]
    pub fn echo_dropped(&self) -> u32 {
        self.echo.dropped
    }

    pub fn flush_input(&mut self) {
        self.edit_len = 0;
        self.cooked.flush();
        self.line_count = 0;
        self.literal_next = false;
        self.utf8_remaining = 0;
        self.xoff_sent = false;
        self.pending_reprint = false;
        self.in_erase_seq = false;
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
        self.flushing_output = false;
    }

    pub fn edit_content(&self) -> &[u8] {
        &self.edit_buf[..self.edit_len]
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Clear FLUSHO. A userspace `write()` does this, per POSIX.
    pub fn clear_flusho(&mut self) {
        self.flushing_output = false;
    }

    /// Process one raw input byte, returning what the caller should do.
    pub fn input_char<E: Into<InputEvent>>(&mut self, event: E) -> InputAction {
        let event = event.into();
        if !self.termios.control_flags().contains(ControlFlags::CREAD) {
            return InputAction::None;
        }

        // PENDIN: reprint before this byte so echo reflects the new flags.
        if self.pending_reprint {
            self.pending_reprint = false;
            return InputAction::ReprintLine;
        }

        let iflag = self.termios.input_flags();
        let lflag = self.termios.local_flags();

        let mut c = event.byte;
        let mut apply_break_heuristic = true;

        match event.status {
            InputStatus::Normal => {}
            InputStatus::Break => {
                if iflag.contains(InputFlags::IGNBRK) {
                    return InputAction::None;
                }
                if iflag.contains(InputFlags::BRKINT) {
                    self.flush_input();
                    return InputAction::Signal(SIGINT);
                }
                if iflag.contains(InputFlags::PARMRK) {
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
                c = 0x00;
                apply_break_heuristic = false;
            }
            InputStatus::ParityError | InputStatus::FrameError => {
                if iflag.contains(InputFlags::IGNPAR) {
                    return InputAction::None;
                }
                if iflag.contains(InputFlags::INPCK) {
                    if iflag.contains(InputFlags::PARMRK) {
                        if self.cooked_free() < 2 {
                            if iflag.contains(InputFlags::IMAXBEL) {
                                return InputAction::Bell;
                            }
                            return InputAction::None;
                        }
                        self.push_cooked(0xFF);
                        self.push_cooked(0x00);
                    } else {
                        c = 0x00;
                    }
                }
                apply_break_heuristic = false;
            }
            InputStatus::Overrun => {
                return InputAction::None;
            }
        }

        // A NUL is taken as a break condition when any break flag is set.
        if apply_break_heuristic
            && c == 0x00
            && iflag.intersects(InputFlags::IGNBRK | InputFlags::BRKINT | InputFlags::PARMRK)
        {
            return self.handle_break(iflag);
        }

        let c = self.process_iflag(c, iflag);

        let c = match c {
            Some(c) => c,
            None => return InputAction::None,
        };

        if self.literal_next {
            self.literal_next = false;
            let close_erase = self.in_erase_seq;
            if close_erase {
                self.in_erase_seq = false;
            }
            return self.insert_char(c, lflag, close_erase);
        }

        if lflag.contains(LocalFlags::ISIG) {
            let sig = if self.cc_matches(CcIndex::Vintr, c) {
                Some(SIGINT)
            } else if self.cc_matches(CcIndex::Vquit, c) {
                Some(SIGQUIT)
            } else if self.cc_matches(CcIndex::Vsusp, c) {
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

        if iflag.contains(InputFlags::IXON) {
            if self.cc_matches(CcIndex::Vstop, c) {
                self.stopped = true;
                return InputAction::None;
            }
            if self.cc_matches(CcIndex::Vstart, c) {
                self.stopped = false;
                return InputAction::None;
            }
            // IXANY: any character resumes stopped output, not just VSTART.
            if self.stopped && iflag.contains(InputFlags::IXANY) {
                self.stopped = false;
            }
        }

        if lflag.contains(LocalFlags::EXTPROC) {
            return self.extproc_input(c);
        }

        if lflag.contains(LocalFlags::IEXTEN) {
            if c == self.cc(CcIndex::Vlnext) {
                self.literal_next = true;
                if lflag.contains(LocalFlags::ECHOCTL | LocalFlags::ECHO) {
                    return InputAction::echo2(b'^', b'V');
                }
                return InputAction::None;
            }
            if c == self.cc(CcIndex::Vdiscard) && c != POSIX_VDISABLE {
                self.flushing_output = !self.flushing_output;
                return InputAction::None;
            }
            if lflag.contains(LocalFlags::ICANON) {
                if c == self.cc(CcIndex::Vwerase) {
                    return self.word_erase(lflag);
                }
                if c == self.cc(CcIndex::Vreprint) {
                    // An explicit VREPRINT clears the deferred one, else we
                    // would echo the line twice.
                    self.pending_reprint = false;
                    return InputAction::ReprintLine;
                }
            }
        }

        if lflag.contains(LocalFlags::ICANON) {
            self.canonical_input(c, lflag)
        } else {
            self.raw_input(c, lflag)
        }
    }

    /// Process one byte through `c_oflag` before it reaches the driver.
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        if self.flushing_output {
            return OutputAction::Suppress;
        }
        let oflag = self.termios.output_flags();
        if !oflag.contains(OutputFlags::OPOST) {
            self.update_column_raw(c);
            return OutputAction::Emit {
                buf: [c, 0],
                len: 1,
            };
        }
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
                // ONLRET: NL performs the CR function.
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
                // POSIX: a plain NL without ONLCR/ONLRET does not reset column.
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\t' => {
                // TAB3/XTABS expands to spaces; TAB0 passes the literal tab on.
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
                // Non-printable control char: column is deliberately unchanged.
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
        }
    }

    /// Apply `c_iflag` to a raw input byte. `None` means discard (IGNCR).
    /// The caller must have handled break conditions first — see `input_char`.
    fn process_iflag(&self, c: u8, iflag: InputFlags) -> Option<u8> {
        let mut c = c;

        if iflag.contains(InputFlags::ISTRIP) {
            c &= 0x7F;
        }

        if c == b'\r' {
            if iflag.contains(InputFlags::IGNCR) {
                return None;
            }
            if iflag.contains(InputFlags::ICRNL) {
                c = b'\n';
            }
        } else if c == b'\n' && iflag.contains(InputFlags::INLCR) {
            c = b'\r';
        }

        if iflag.contains(InputFlags::IUCLC) && c.is_ascii_uppercase() {
            c = c.to_ascii_lowercase();
        }

        Some(c)
    }

    /// Handle a break condition, which serial hardware signals by holding the
    /// line low for longer than a frame and the driver delivers as a NUL.
    fn handle_break(&mut self, iflag: InputFlags) -> InputAction {
        if iflag.contains(InputFlags::IGNBRK) {
            return InputAction::None;
        }
        if iflag.contains(InputFlags::BRKINT) {
            self.flush_input();
            return InputAction::Signal(SIGINT);
        }
        if iflag.contains(InputFlags::PARMRK) {
            // POSIX encodes a break as \xff \x00 \x00, and all three bytes must
            // land atomically: a partial sequence reads to userland as a stray
            // 0xFF or a different PARMRK encoding. No room means drop it whole.
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
        InputAction::None
    }

    fn canonical_input(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        if c == self.cc(CcIndex::Verase) || c == 0x08 {
            return self.erase_char(lflag);
        }

        // Any non-erase input closes an ECHOPRT `\...` sequence with `/`.
        let close_erase = self.in_erase_seq;
        if close_erase {
            self.in_erase_seq = false;
        }

        if c == self.cc(CcIndex::Vkill) {
            return self.kill_line(lflag);
        }

        // VEOF flushes without appending a newline.
        if c == self.cc(CcIndex::Veof) {
            self.flush_edit_to_cooked();
            if close_erase {
                return InputAction::echo1(b'/');
            }
            return InputAction::None;
        }

        // Unlike VEOF, a VEOL character joins the line; echoed without ECHOCTL.
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
                    return InputAction::echo2(b'/', c);
                }
                return InputAction::echo1(c);
            }
            if close_erase {
                return InputAction::echo1(b'/');
            }
            return InputAction::None;
        }
        if c == b'\n' || c == b'\r' {
            if self.edit_len < EDIT_BUF_SIZE {
                self.edit_buf[self.edit_len] = b'\n';
                self.edit_len += 1;
            }
            self.flush_edit_to_cooked();
            self.column = 0;
            if lflag.intersects(LocalFlags::ECHO | LocalFlags::ECHONL) {
                if close_erase {
                    return InputAction::echo2(b'/', b'\n');
                }
                return InputAction::echo1(b'\n');
            }
            if close_erase {
                return InputAction::echo1(b'/');
            }
            return InputAction::None;
        }

        self.insert_char(c, lflag, close_erase)
    }

    /// Insert a character into the edit buffer and produce an echo action.
    /// `close_erase` prepends `/` to close an ECHOPRT `\...` erase sequence.
    fn insert_char(&mut self, c: u8, lflag: LocalFlags, close_erase: bool) -> InputAction {
        if self.edit_len >= EDIT_BUF_SIZE {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }

        self.edit_buf[self.edit_len] = c;
        self.edit_len += 1;

        let iutf8 = self.termios.input_flags().contains(InputFlags::IUTF8);
        if iutf8 && c >= 0x80 {
            return self.insert_char_utf8(c, lflag);
        }
        if iutf8 {
            self.utf8_remaining = 0;
        }

        if !lflag.contains(LocalFlags::ECHO) {
            if close_erase {
                return InputAction::echo1(b'/');
            }
            return InputAction::None;
        }

        // ECHOCTL echoes control characters other than TAB and NL as ^X.
        if lflag.contains(LocalFlags::ECHOCTL) && c < 0x20 && c != b'\t' && c != b'\n' {
            self.column += 2;
            if close_erase {
                return InputAction::echo3(b'/', b'^', c + 0x40);
            }
            return InputAction::echo2(b'^', c + 0x40);
        }

        if self.is_printable(c) {
            self.column += 1;
            if close_erase {
                return InputAction::echo2(b'/', c);
            }
            return InputAction::echo1(c);
        }

        if close_erase {
            return InputAction::echo1(b'/');
        }
        InputAction::None
    }

    /// Insert one byte of a multi-byte UTF-8 character. The column advances by
    /// the codepoint's display width only once the sequence completes.
    fn insert_char_utf8(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        if utf8_is_continuation(c) {
            if self.utf8_remaining > 0 {
                self.utf8_remaining -= 1;
                if self.utf8_remaining == 0 {
                    let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
                    let cp = utf8_decode(
                        &self.edit_buf[self.edit_len.saturating_sub(cp_len)..self.edit_len],
                    );
                    self.column += utf8_char_width(cp) as usize;
                }
            } else {
                // Orphan continuation byte — width 1.
                self.column += 1;
            }
        } else {
            let total = utf8_byte_count(c);
            if total > 1 {
                self.utf8_remaining = (total - 1) as u8;
            } else {
                // Invalid leading byte — width 1.
                self.column += 1;
            }
        }

        // The raw byte always echoes so the terminal can reconstruct the
        // multi-byte character.
        if lflag.contains(LocalFlags::ECHO) {
            InputAction::echo1(c)
        } else {
            InputAction::None
        }
    }

    /// EXTPROC: push input straight to the cooked buffer, no canonical editing
    /// or echo — the remote side of a network terminal does the line editing.
    /// ISIG and IXON are already handled by `input_char()`.
    fn extproc_input(&mut self, c: u8) -> InputAction {
        if c == 0xFF && self.termios.input_flags().contains(InputFlags::PARMRK) {
            if self.cooked_free() < 2 {
                if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                    return InputAction::Bell;
                }
                return InputAction::None;
            }
            self.push_cooked(0xFF);
            self.push_cooked(0xFF);
            return InputAction::None;
        }

        if !self.push_cooked(c) {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }
        InputAction::None
    }

    /// Non-canonical (raw) mode: push directly to cooked buffer.
    fn raw_input(&mut self, c: u8, lflag: LocalFlags) -> InputAction {
        // Under PARMRK a literal 0xFF is doubled so userland can tell it from
        // the \xff \x00 \x00 break prefix.
        if c == 0xFF && self.termios.input_flags().contains(InputFlags::PARMRK) {
            if self.cooked_free() < 2 {
                if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                    return InputAction::Bell;
                }
                return InputAction::None;
            }
            self.push_cooked(0xFF);
            self.push_cooked(0xFF);
            if lflag.contains(LocalFlags::ECHO) {
                return InputAction::echo1(0xFF);
            }
            return InputAction::None;
        }

        if !self.push_cooked(c) {
            if self.termios.input_flags().contains(InputFlags::IMAXBEL) {
                return InputAction::Bell;
            }
            return InputAction::None;
        }
        if lflag.contains(LocalFlags::ECHO) {
            if lflag.contains(LocalFlags::ECHOCTL) && c < 0x20 && c != b'\t' && c != b'\n' {
                return InputAction::echo2(b'^', c + 0x40);
            }
            return InputAction::echo1(c);
        }
        InputAction::None
    }

    /// Erase one character (VERASE). Under IUTF8 this erases the whole trailing
    /// codepoint and echoes by its display width; otherwise, exactly one byte.
    fn erase_char(&mut self, lflag: LocalFlags) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        if self.termios.input_flags().contains(InputFlags::IUTF8) {
            return self.erase_char_utf8(lflag);
        }

        let erased = self.edit_buf[self.edit_len - 1];
        self.edit_len -= 1;

        // ECHOPRT hardcopy erase (\chars/) takes priority over ECHOE.
        if lflag.contains(LocalFlags::ECHOPRT | LocalFlags::ECHO) {
            if self.in_erase_seq {
                return InputAction::echo1(erased);
            } else {
                self.in_erase_seq = true;
                return InputAction::echo2(b'\\', erased);
            }
        }

        if lflag.contains(LocalFlags::ECHOE) {
            // A control char echoed as ^X under ECHOCTL occupies two columns.
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
            return InputAction::echo3(0x08, 0x20, 0x08);
        }
        InputAction::None
    }

    /// UTF-8 aware backspace — erases the full trailing codepoint.
    fn erase_char_utf8(&mut self, lflag: LocalFlags) -> InputAction {
        let cp_len = utf8_trailing_codepoint_len(&self.edit_buf[..self.edit_len]);
        if cp_len == 0 {
            return InputAction::None;
        }

        let cp_start = self.edit_len - cp_len;
        let codepoint = utf8_decode(&self.edit_buf[cp_start..self.edit_len]);
        let width = utf8_char_width(codepoint) as usize;

        self.edit_len = cp_start;
        self.utf8_remaining = 0;

        // ECHOPRT echoes the codepoint's first byte as a representative.
        if lflag.contains(LocalFlags::ECHOPRT | LocalFlags::ECHO) {
            let representative = self.edit_buf[cp_start];
            if self.in_erase_seq {
                return InputAction::echo1(representative);
            } else {
                self.in_erase_seq = true;
                return InputAction::echo2(b'\\', representative);
            }
        }

        if !lflag.contains(LocalFlags::ECHOE) {
            return InputAction::None;
        }

        self.column = self.column.saturating_sub(width);

        if width <= 1 {
            return InputAction::echo3(0x08, 0x20, 0x08);
        }
        InputAction::KillLineEcho {
            columns: width as u16,
        }
    }

    /// Kill the entire line (VKILL).
    fn kill_line(&mut self, lflag: LocalFlags) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        let cols_to_erase = self.column as u16;
        self.edit_len = 0;
        self.column = 0;

        if lflag.contains(LocalFlags::ECHOKE | LocalFlags::ECHO) {
            return InputAction::KillLineEcho {
                columns: cols_to_erase,
            };
        }

        if lflag.contains(LocalFlags::ECHOK) {
            return InputAction::echo1(b'\n');
        }
        InputAction::None
    }

    fn is_word_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    /// Word erase (VWERASE): erase back over one word. Boundaries are
    /// alphanumeric + underscore rather than whitespace, so `/usr/local/bin`
    /// erases one path component at a time as on most POSIX terminals.
    fn word_erase(&mut self, lflag: LocalFlags) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        if self.termios.input_flags().contains(InputFlags::IUTF8) {
            return self.word_erase_utf8(lflag);
        }

        let mut erased = 0usize;

        while self.edit_len > 0 && !Self::is_word_char(self.edit_buf[self.edit_len - 1]) {
            self.edit_len -= 1;
            erased += 1;
        }
        while self.edit_len > 0 && Self::is_word_char(self.edit_buf[self.edit_len - 1]) {
            self.edit_len -= 1;
            erased += 1;
        }

        self.column = self.column.saturating_sub(erased);

        // One action carries at most three echo bytes, so anything longer than
        // a single erase redraws the line instead.
        if erased <= 1 && lflag.contains(LocalFlags::ECHOE) {
            return InputAction::echo3(0x08, 0x20, 0x08);
        }

        if lflag.contains(LocalFlags::ECHO) {
            return InputAction::ReprintLine;
        }

        InputAction::None
    }

    /// UTF-8 word erase — whole codepoints, column width tracked per codepoint.
    fn word_erase_utf8(&mut self, lflag: LocalFlags) -> InputAction {
        let mut columns_erased: usize = 0;

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
            return InputAction::echo3(0x08, 0x20, 0x08);
        }

        if lflag.contains(LocalFlags::ECHO) {
            return InputAction::ReprintLine;
        }

        InputAction::None
    }

    fn cc(&self, idx: CcIndex) -> u8 {
        self.termios.c_cc[idx.as_usize()]
    }

    /// Returns true when an enabled control-character slot matches `c`.
    fn cc_matches(&self, idx: CcIndex, c: u8) -> bool {
        let cc = self.cc(idx);
        cc != POSIX_VDISABLE && c == cc
    }

    /// Matches an enabled VEOL or VEOL2; `POSIX_VDISABLE` means disabled.
    fn is_veol(&self, c: u8) -> bool {
        let veol = self.cc(CcIndex::Veol);
        if veol != POSIX_VDISABLE && c == veol {
            return true;
        }
        let veol2 = self.cc(CcIndex::Veol2);
        veol2 != POSIX_VDISABLE && c == veol2
    }

    fn is_printable(&self, c: u8) -> bool {
        (0x20..=0x7E).contains(&c) || c == b'\t'
    }

    /// Track the column for a byte that bypassed OPOST, so echo stays aligned.
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

    /// Push one byte into the cooked ring. `false` means the ring was full: the
    /// byte is dropped, `no_room` set and `overflow_count` bumped.
    pub(crate) fn push_cooked(&mut self, c: u8) -> bool {
        if self.cooked.is_full() {
            self.no_room = true;
            self.overflow_count = self.overflow_count.saturating_add(1);
            return false;
        }
        let _ = self.cooked.try_push(c);
        self.wake_chars_pending += 1;
        true
    }

    /// Free bytes in the cooked ring — checked before a multi-byte sequence
    /// such as PARMRK's break encoding, which must go in whole or not at all.
    fn cooked_free(&self) -> usize {
        self.cooked.free()
    }

    /// The XOFF byte, once IXOFF is on and pending input crosses high water.
    pub fn ixoff_check_xoff(&mut self) -> Option<u8> {
        if !self.termios.input_flags().contains(InputFlags::IXOFF) {
            return None;
        }
        if self.xoff_sent {
            return None;
        }
        let pending = self.edit_len + self.cooked.count();
        if pending >= IXOFF_HIGH_WATER {
            self.xoff_sent = true;
            Some(self.cc(CcIndex::Vstop))
        } else {
            None
        }
    }

    /// Allow the IXOFF stop to be generated again. `ixoff_check_xoff` latches
    /// at generation time, so a stop discarded before it reached the peer would
    /// otherwise never be re-sent. The queue is still over the water mark, so
    /// the stop is re-armed rather than cancelled.
    #[inline]
    pub fn ixoff_rearm(&mut self) {
        self.xoff_sent = false;
    }

    /// The XON byte, once IXOFF is on and pending input drops below low water.
    pub fn ixoff_check_xon(&mut self) -> Option<u8> {
        if !self.termios.input_flags().contains(InputFlags::IXOFF) {
            return None;
        }
        if !self.xoff_sent {
            return None;
        }
        let pending = self.edit_len + self.cooked.count();
        if pending < IXOFF_LOW_WATER {
            self.xoff_sent = false;
            Some(self.cc(CcIndex::Vstart))
        } else {
            None
        }
    }

    fn flush_edit_to_cooked(&mut self) {
        let parmrk = self.termios.input_flags().contains(InputFlags::PARMRK);
        let mut i = 0usize;
        while i < self.edit_len {
            let byte = self.edit_buf[i];
            // Doubling 0xFF at flush time keeps the edit buffer itself clean
            // for display and backspace.
            if parmrk && byte == 0xFF {
                if self.cooked_free() < 2 {
                    break;
                }
                self.push_cooked(0xFF);
                self.push_cooked(0xFF);
            } else if !self.push_cooked(byte) {
                break;
            }
            i += 1;
        }
        if i < self.edit_len {
            let remaining = self.edit_len - i;
            self.edit_buf.copy_within(i..self.edit_len, 0);
            self.edit_len = remaining;
        } else {
            self.edit_len = 0;
        }
        self.line_count += 1;
    }

    /// Process a batch of input events, staging echo and capturing at most one
    /// signal — processing stops at the first signal-generating character.
    ///
    /// Echo goes into this discipline's own queue, not to a driver: the caller
    /// holds the slot lock, and emission has to wait until it drops.
    pub fn receive_buf(&mut self, events: &[InputEvent]) -> BatchResult {
        let mut result = BatchResult::new();
        for &event in events {
            match self.input_char(event) {
                InputAction::Echo { buf, len } => {
                    self.echo.extend(&buf[..len as usize]);
                }
                InputAction::Bell => {
                    self.echo.push(0x07);
                }
                InputAction::Signal(sig) => {
                    let lflag = self.termios.local_flags();
                    // A typed signal char echoes its caret form before the
                    // signal is delivered: the core drains echo ahead of
                    // dispatching `result.signal`. A break carries no keypress.
                    if lflag.contains(LocalFlags::ECHO | LocalFlags::ECHOCTL)
                        && event.status == InputStatus::Normal
                    {
                        let c = event.byte;
                        if c < 0x20 && c != b'\t' && c != b'\n' {
                            self.echo.extend(&[b'^', c | 0x40]);
                        }
                    }
                    result.signal = Some((sig, !lflag.contains(LocalFlags::NOFLSH)));
                    break;
                }
                InputAction::ReprintLine => {
                    self.echo.push(b'\n');
                    // Split borrow: the redisplay reads a field of the same
                    // struct as the queue it feeds.
                    let Self {
                        edit_buf,
                        edit_len,
                        echo,
                        ..
                    } = self;
                    echo.extend(&edit_buf[..*edit_len]);
                }
                InputAction::KillLineEcho { columns } => {
                    for _ in 0..columns {
                        self.echo.extend(&[0x08, 0x20, 0x08]);
                    }
                }
                InputAction::None => {}
            }
        }
        result.should_wake = self.should_wake_reader();
        result.throttle_check = true;
        result
    }
}

const RAW_BUF_SIZE: usize = 4096;

/// Minimal passthrough discipline for PTY masters and raw I/O paths: no input
/// processing, echo, signals or canonical editing in either direction.
#[derive(Zeroable)]
#[repr(C)]
pub struct RawDisc {
    termios: UserTermios,
    buf: RingBuffer<u8, RAW_BUF_SIZE>,
    wake_chars_pending: usize,

    /// Sticky overflow flag; see `LineDisc::no_room`.
    no_room: bool,
    /// Cumulative overflow byte count; see `LineDisc::overflow_count`.
    overflow_count: u32,
}

impl RawDisc {
    /// Default termios for a fresh `RawDisc`: no echo, no canonical processing.
    pub const fn default_termios() -> UserTermios {
        UserTermios {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::from_bits_retain(
                ControlFlags::CS8.bits()
                    | ControlFlags::CREAD.bits()
                    | ControlFlags::HUPCL.bits()
                    | slopos_abi::syscall::B38400,
            ),
            c_lflag: LocalFlags::empty(),
            c_line: N_RAW as u8,
            c_cc: {
                // VMIN=1 so a raw read blocks rather than returning the VMIN=0
                // empty `Ok(0)`: a PTY master's `read() == 0` must mean exactly
                // "peer hung up". tcsetattr can still opt in to VMIN=0 polling.
                let mut cc = [0u8; NCCS];
                cc[CcIndex::Vmin as usize] = 1;
                cc
            },
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }

    /// Allocate a default `RawDisc` straight onto the heap, avoiding the ~4 KiB
    /// stack temporary a `Self`-returning constructor produces. See
    /// [`LineDisc::new_pinned`].
    pub fn new_pinned() -> Result<PinBox<Self>, AllocError> {
        let mut pb = PinBox::<Self>::zeroed()?;
        pb.termios = Self::default_termios();
        Ok(pb)
    }

    /// Panic-on-OOM convenience for callers that cannot recover from it.
    pub fn new() -> PinBox<Self> {
        Self::new_pinned().expect("kernel OOM during RawDisc allocation")
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
        !self.buf.is_empty()
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        self.buf.read(out)
    }

    pub fn bytes_available(&self) -> usize {
        self.buf.count()
    }

    pub fn input_full(&self) -> bool {
        self.buf.is_full()
    }

    pub fn no_room(&self) -> bool {
        self.no_room
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_count
    }

    /// Raw disciplines never consume terminal-generated control input.
    pub(crate) fn priority_control_input(&self, _byte: u8) -> bool {
        false
    }

    pub fn check_no_room_recovery(&mut self) -> bool {
        if self.no_room && self.buf.count() <= THROTTLE_LOW_WATER {
            self.no_room = false;
            true
        } else {
            false
        }
    }

    /// Always non-canonical, so wakeups batch: `true` only once
    /// `WAKEUP_CHARS` have accumulated or the buffer is nearly full.
    pub fn should_wake_reader(&mut self) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        let near_full = self.buf.count() >= self.buf.capacity().saturating_sub(64);
        if self.wake_chars_pending >= WAKEUP_CHARS || near_full {
            self.wake_chars_pending = 0;
            return true;
        }
        false
    }

    pub fn flush_all(&mut self) {
        self.buf.flush();
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
    }

    pub fn flush_input(&mut self) {
        self.buf.flush();
        self.wake_chars_pending = 0;
        self.no_room = false;
        self.overflow_count = 0;
    }

    pub fn edit_content(&self) -> &[u8] {
        &[]
    }

    pub fn is_stopped(&self) -> bool {
        false
    }

    pub fn clear_flusho(&mut self) {}

    /// Raw input: push byte directly to buffer, no processing.
    pub fn input_char<E: Into<InputEvent>>(&mut self, event: E) -> InputAction {
        let event = event.into();
        if !self.termios.control_flags().contains(ControlFlags::CREAD) {
            return InputAction::None;
        }
        if self.buf.try_push(event.byte) {
            self.wake_chars_pending += 1;
        } else {
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

    /// `RawDisc` implements no IXOFF flow control.
    pub fn ixoff_check_xoff(&mut self) -> Option<u8> {
        None
    }

    /// `RawDisc` implements no IXOFF flow control.
    pub fn ixoff_check_xon(&mut self) -> Option<u8> {
        None
    }

    pub fn receive_buf(&mut self, events: &[InputEvent]) -> BatchResult {
        for &event in events {
            let _ = self.input_char(event);
        }
        let mut result = BatchResult::new();
        result.should_wake = !self.buf.is_empty();
        result.throttle_check = true;
        result
    }
}

/// Swappable line discipline — one per TTY, so PTY masters (and future
/// SLIP/PPP) get raw passthrough while terminals get full N_TTY processing.
///
/// The state (`LineDisc` ≈ 12 KiB, `RawDisc` ≈ 4 KiB) lives on the heap, so a
/// `Tty` costs a handful of stack bytes and can be built in a syscall path
/// without running the kernel stack into its guard page.
pub enum LdiscKind {
    /// Full N_TTY processing (canonical, echo, signals, etc.)
    NTty(PinBox<LineDisc>),
    /// Raw passthrough (for PTY master, future SLIP/PPP).
    Raw(PinBox<RawDisc>),
}

impl LdiscKind {
    #[inline]
    pub fn id(&self) -> u32 {
        match self {
            LdiscKind::NTty(_) => N_TTY,
            LdiscKind::Raw(_) => N_RAW,
        }
    }

    /// Construct from a numeric id, applying `termios`. `Ok(None)` for an
    /// unknown id; the ioctl path surfaces either failure as an errno.
    pub fn from_id(ldisc_id: u32, termios: UserTermios) -> Result<Option<Self>, AllocError> {
        match ldisc_id {
            N_TTY => {
                let mut ld = LineDisc::new_pinned()?;
                ld.set_termios(&termios);
                Ok(Some(LdiscKind::NTty(ld)))
            }
            N_RAW => {
                let mut rd = RawDisc::new_pinned()?;
                rd.set_termios(&termios);
                Ok(Some(LdiscKind::Raw(rd)))
            }
            _ => Ok(None),
        }
    }

    #[inline]
    pub fn input_char<E: Into<InputEvent>>(&mut self, event: E) -> InputAction {
        let event = event.into();
        match self {
            LdiscKind::NTty(inner) => inner.input_char(event),
            LdiscKind::Raw(inner) => inner.input_char(event),
        }
    }

    #[inline]
    pub fn receive_buf(&mut self, events: &[InputEvent]) -> BatchResult {
        match self {
            LdiscKind::NTty(inner) => inner.receive_buf(events),
            LdiscKind::Raw(inner) => inner.receive_buf(events),
        }
    }

    // `RawDisc` never echoes, so its echo arms are inert.
    #[inline]
    pub fn echo_stage(&mut self, bytes: &[u8]) {
        match self {
            LdiscKind::NTty(inner) => inner.echo_stage(bytes),
            LdiscKind::Raw(_) => {}
        }
    }

    #[inline]
    pub fn echo_take(&mut self, out: &mut [u8]) -> usize {
        match self {
            LdiscKind::NTty(inner) => inner.echo_take(out),
            LdiscKind::Raw(_) => 0,
        }
    }

    #[inline]
    pub fn echo_unread(&mut self, bytes: &[u8]) {
        match self {
            LdiscKind::NTty(inner) => inner.echo_unread(bytes),
            LdiscKind::Raw(_) => {}
        }
    }

    #[inline]
    pub fn echo_is_empty(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.echo_is_empty(),
            LdiscKind::Raw(_) => true,
        }
    }

    #[inline]
    pub fn echo_staged(&self) -> usize {
        match self {
            LdiscKind::NTty(inner) => inner.echo_staged(),
            LdiscKind::Raw(_) => 0,
        }
    }

    #[inline]
    pub fn echo_discard(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.echo_discard(),
            LdiscKind::Raw(_) => {}
        }
    }

    #[inline]
    pub fn ixoff_rearm(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.ixoff_rearm(),
            LdiscKind::Raw(_) => {}
        }
    }

    #[inline]
    pub fn echo_claim_drain(&mut self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.echo_claim_drain(),
            LdiscKind::Raw(_) => false,
        }
    }

    #[inline]
    pub fn echo_release_drain(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.echo_release_drain(),
            LdiscKind::Raw(_) => {}
        }
    }

    #[inline]
    pub fn echo_dropped(&self) -> u32 {
        match self {
            LdiscKind::NTty(inner) => inner.echo_dropped(),
            LdiscKind::Raw(_) => 0,
        }
    }

    #[inline]
    pub fn termios(&self) -> &UserTermios {
        match self {
            LdiscKind::NTty(inner) => inner.termios(),
            LdiscKind::Raw(inner) => inner.termios(),
        }
    }

    #[inline]
    pub fn vmin_vtime(&self) -> (u8, u8) {
        match self {
            LdiscKind::NTty(inner) => inner.vmin_vtime(),
            LdiscKind::Raw(inner) => inner.vmin_vtime(),
        }
    }

    #[inline]
    pub fn is_canonical(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.is_canonical(),
            LdiscKind::Raw(inner) => inner.is_canonical(),
        }
    }

    #[inline]
    pub fn set_termios(&mut self, t: &UserTermios) {
        match self {
            LdiscKind::NTty(inner) => inner.set_termios(t),
            LdiscKind::Raw(inner) => inner.set_termios(t),
        }
    }

    #[inline]
    pub fn has_data(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.has_data(),
            LdiscKind::Raw(inner) => inner.has_data(),
        }
    }

    #[inline]
    pub fn bytes_available(&self) -> usize {
        match self {
            LdiscKind::NTty(inner) => inner.bytes_available(),
            LdiscKind::Raw(inner) => inner.bytes_available(),
        }
    }

    #[inline]
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        match self {
            LdiscKind::NTty(inner) => inner.read(out),
            LdiscKind::Raw(inner) => inner.read(out),
        }
    }

    #[inline]
    pub fn flush_all(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.flush_all(),
            LdiscKind::Raw(inner) => inner.flush_all(),
        }
    }

    #[inline]
    pub fn flush_input(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.flush_input(),
            LdiscKind::Raw(inner) => inner.flush_input(),
        }
    }

    #[inline]
    pub fn edit_content(&self) -> &[u8] {
        match self {
            LdiscKind::NTty(inner) => inner.edit_content(),
            LdiscKind::Raw(inner) => inner.edit_content(),
        }
    }

    #[inline]
    pub fn is_stopped(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.is_stopped(),
            LdiscKind::Raw(inner) => inner.is_stopped(),
        }
    }

    #[inline]
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        match self {
            LdiscKind::NTty(inner) => inner.process_output_byte(c),
            LdiscKind::Raw(inner) => inner.process_output_byte(c),
        }
    }

    #[inline]
    pub fn ixoff_check_xoff(&mut self) -> Option<u8> {
        match self {
            LdiscKind::NTty(inner) => inner.ixoff_check_xoff(),
            LdiscKind::Raw(inner) => inner.ixoff_check_xoff(),
        }
    }

    #[inline]
    pub fn ixoff_check_xon(&mut self) -> Option<u8> {
        match self {
            LdiscKind::NTty(inner) => inner.ixoff_check_xon(),
            LdiscKind::Raw(inner) => inner.ixoff_check_xon(),
        }
    }

    #[inline]
    pub fn input_full(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.input_full(),
            LdiscKind::Raw(inner) => inner.input_full(),
        }
    }

    #[inline]
    pub(crate) fn priority_control_input(&self, byte: u8) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.priority_control_input(byte),
            LdiscKind::Raw(inner) => inner.priority_control_input(byte),
        }
    }

    #[inline]
    pub fn should_wake_reader(&mut self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.should_wake_reader(),
            LdiscKind::Raw(inner) => inner.should_wake_reader(),
        }
    }

    #[inline]
    pub fn no_room(&self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.no_room(),
            LdiscKind::Raw(inner) => inner.no_room(),
        }
    }

    #[inline]
    pub fn overflow_count(&self) -> u32 {
        match self {
            LdiscKind::NTty(inner) => inner.overflow_count(),
            LdiscKind::Raw(inner) => inner.overflow_count(),
        }
    }

    #[inline]
    pub fn check_no_room_recovery(&mut self) -> bool {
        match self {
            LdiscKind::NTty(inner) => inner.check_no_room_recovery(),
            LdiscKind::Raw(inner) => inner.check_no_room_recovery(),
        }
    }

    #[inline]
    pub fn clear_flusho(&mut self) {
        match self {
            LdiscKind::NTty(inner) => inner.clear_flusho(),
            LdiscKind::Raw(inner) => inner.clear_flusho(),
        }
    }
}
