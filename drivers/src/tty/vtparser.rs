//! VT100/ANSI escape sequence state machine.
//!
//! Pure `no_std`, no-alloc parser that produces typed `VtAction` values for
//! the virtual console renderer.  Parsing is fully separated from rendering
//! so that the state machine can be tested independently.
//!
//! UTF-8 decoding in ground state, 256-color/truecolor SGR,
//! bracketed paste mode, additional DEC private modes (DECCKM, DECOM, DECAWM).

const MAX_PARAMS: usize = 16;

/// Unicode replacement character — emitted for invalid UTF-8 sequences.
const REPLACEMENT_CHAR: u32 = 0xFFFD;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
    Forward,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EraseMode {
    ToEnd,
    ToStart,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SgrAttr {
    Reset,
    Bold,
    NoBold,
    Underline,
    NoUnderline,
    Inverse,
    NoInverse,
    ForegroundColor(u8),
    BackgroundColor(u8),
    BrightForeground(u8),
    BrightBackground(u8),
    DefaultForeground,
    DefaultBackground,
    /// 256-color foreground (index 0–255).
    Foreground256(u8),
    /// 256-color background (index 0–255).
    Background256(u8),
    /// Truecolor (24-bit) foreground.
    ForegroundRgb(u8, u8, u8),
    /// Truecolor (24-bit) background.
    BackgroundRgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VtAction {
    /// Printable character (Unicode codepoint).
    Print(u32),
    /// Control character (CR, LF, BS, TAB, BEL, VT, FF).
    Execute(u8),
    /// Cursor movement (CUU/CUD/CUF/CUB).
    MoveCursor { direction: Direction, count: u16 },
    /// Absolute cursor positioning (CUP) — 0-based row/col.
    SetCursorPos { row: u16, col: u16 },
    /// Erase display (ED).
    EraseDisplay(EraseMode),
    /// Erase line (EL).
    EraseLine(EraseMode),
    /// Scroll up by N lines (SU).
    ScrollUp(u16),
    /// Scroll down by N lines (SD).
    ScrollDown(u16),
    /// Set graphic rendition attribute (SGR).
    SetAttribute(SgrAttr),
    /// Save cursor position and attributes (DECSC / ESC 7).
    SaveCursor,
    /// Restore cursor position and attributes (DECRC / ESC 8).
    RestoreCursor,
    /// Set scroll region (DECSTBM) — 0-based top/bottom rows.
    SetScrollRegion { top: u16, bottom: u16 },
    /// Insert N blank lines at cursor (CSI L / IL).
    InsertLines(u16),
    /// Delete N lines at cursor (CSI M / DL).
    DeleteLines(u16),
    /// Delete N characters at cursor, shifting remainder left (CSI P / DCH).
    DeleteChars(u16),
    /// Insert N blank characters at cursor, shifting remainder right (CSI @ / ICH).
    InsertChars(u16),
    /// Erase N characters at cursor without shifting (CSI X / ECH).
    EraseChars(u16),
    /// DEC private set mode (CSI ? N h).
    SetMode(u16),
    /// DEC private reset mode (CSI ? N l).
    ResetMode(u16),
    /// No-op — unrecognized or malformed sequence, silently discarded.
    Nop,
}

// ---------------------------------------------------------------------------
// Parser state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    OscString,
    /// Sub-state for detecting ESC \ (ST) inside an OSC string.
    OscEscape,
    /// Accumulating a multi-byte UTF-8 sequence in ground state.
    Utf8,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VtParser {
    state: State,
    // CSI parameter accumulation
    params: [u16; MAX_PARAMS],
    param_count: usize,
    current_param: u16,
    has_digit: bool,
    private_mode: bool,
    // Pending action queue (for multi-param SGR)
    pending: [VtAction; MAX_PARAMS],
    pending_count: usize,
    pending_idx: usize,
    // UTF-8 accumulator
    utf8_buf: [u8; 4],
    utf8_len: u8,
    utf8_expected: u8,
    // DEC mode state
    pub(crate) bracketed_paste: bool,
    pub(crate) cursor_key_mode: bool,
    pub(crate) origin_mode: bool,
    pub(crate) auto_wrap: bool,
}

impl VtParser {
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            params: [0; MAX_PARAMS],
            param_count: 0,
            current_param: 0,
            has_digit: false,
            private_mode: false,
            pending: [VtAction::Nop; MAX_PARAMS],
            pending_count: 0,
            pending_idx: 0,
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
            bracketed_paste: false,
            cursor_key_mode: false,
            origin_mode: false,
            auto_wrap: true,
        }
    }

    /// Feed one byte into the parser.  Returns a single `VtAction`.
    ///
    /// When a CSI `m` (SGR) sequence carries multiple parameters, the first
    /// action is returned immediately and the rest are queued internally.
    /// Subsequent calls to `advance` drain the queue before processing the
    /// new byte.
    pub fn advance(&mut self, byte: u8) -> VtAction {
        // Drain pending queue first (multi-param SGR).
        if self.pending_idx < self.pending_count {
            let action = self.pending[self.pending_idx];
            self.pending_idx += 1;
            if self.pending_idx >= self.pending_count {
                self.pending_count = 0;
                self.pending_idx = 0;
            }
            return action;
        }

        match self.state {
            State::Ground => self.ground(byte),
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => self.escape_intermediate(byte),
            State::CsiEntry => self.csi_entry(byte),
            State::CsiParam => self.csi_param(byte),
            State::CsiIntermediate => self.csi_intermediate(byte),
            State::OscString => self.osc_string(byte),
            State::OscEscape => self.osc_escape(byte),
            State::Utf8 => self.utf8_continue(byte),
        }
    }

    // -- State handlers -----------------------------------------------------

    fn ground(&mut self, byte: u8) -> VtAction {
        match byte {
            0x1B => {
                self.state = State::Escape;
                VtAction::Nop
            }
            // Recognised control characters → Execute.
            0x07 | 0x08 | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D => VtAction::Execute(byte),
            // Other C0 controls → silently ignored.
            0x00..=0x1F => VtAction::Nop,
            // Printable ASCII.
            0x20..=0x7E => VtAction::Print(byte as u32),
            // DEL → ignored.
            0x7F => VtAction::Nop,
            // UTF-8 lead bytes → start accumulation.
            0xC2..=0xDF => {
                // 2-byte sequence (U+0080..U+07FF)
                self.utf8_buf[0] = byte;
                self.utf8_len = 1;
                self.utf8_expected = 2;
                self.state = State::Utf8;
                VtAction::Nop
            }
            0xE0..=0xEF => {
                // 3-byte sequence (U+0800..U+FFFF)
                self.utf8_buf[0] = byte;
                self.utf8_len = 1;
                self.utf8_expected = 3;
                self.state = State::Utf8;
                VtAction::Nop
            }
            0xF0..=0xF4 => {
                // 4-byte sequence (U+10000..U+10FFFF)
                self.utf8_buf[0] = byte;
                self.utf8_len = 1;
                self.utf8_expected = 4;
                self.state = State::Utf8;
                VtAction::Nop
            }
            // Invalid lead bytes (0x80..=0xBF are continuation bytes without
            // a lead, 0xC0..=0xC1 are overlong, 0xF5..=0xFF are invalid).
            _ => VtAction::Print(REPLACEMENT_CHAR),
        }
    }

    /// Continue accumulating a UTF-8 multi-byte sequence.
    fn utf8_continue(&mut self, byte: u8) -> VtAction {
        // Check for valid continuation byte (10xxxxxx).
        if byte & 0xC0 != 0x80 {
            // Invalid continuation — emit replacement and re-process this byte
            // from ground state.
            self.state = State::Ground;
            self.utf8_len = 0;
            // The current byte might be a new lead byte or control char.
            // Queue a replacement char and re-process.
            if self.pending_count == 0 {
                self.pending[0] = self.ground(byte);
                if self.pending[0] != VtAction::Nop {
                    self.pending_count = 1;
                    self.pending_idx = 0;
                }
            }
            return VtAction::Print(REPLACEMENT_CHAR);
        }

        self.utf8_buf[self.utf8_len as usize] = byte;
        self.utf8_len += 1;

        if self.utf8_len < self.utf8_expected {
            // Need more bytes.
            return VtAction::Nop;
        }

        // Sequence complete — decode the codepoint.
        self.state = State::Ground;
        let cp = match self.utf8_expected {
            2 => {
                let b0 = self.utf8_buf[0] as u32;
                let b1 = self.utf8_buf[1] as u32;
                ((b0 & 0x1F) << 6) | (b1 & 0x3F)
            }
            3 => {
                let b0 = self.utf8_buf[0] as u32;
                let b1 = self.utf8_buf[1] as u32;
                let b2 = self.utf8_buf[2] as u32;
                ((b0 & 0x0F) << 12) | ((b1 & 0x3F) << 6) | (b2 & 0x3F)
            }
            4 => {
                let b0 = self.utf8_buf[0] as u32;
                let b1 = self.utf8_buf[1] as u32;
                let b2 = self.utf8_buf[2] as u32;
                let b3 = self.utf8_buf[3] as u32;
                ((b0 & 0x07) << 18) | ((b1 & 0x3F) << 12) | ((b2 & 0x3F) << 6) | (b3 & 0x3F)
            }
            _ => REPLACEMENT_CHAR,
        };
        self.utf8_len = 0;

        // Validate: reject overlong encodings and surrogates.
        if cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
            return VtAction::Print(REPLACEMENT_CHAR);
        }
        // Reject overlong 2-byte (< U+0080).
        if self.utf8_expected == 2 && cp < 0x80 {
            return VtAction::Print(REPLACEMENT_CHAR);
        }
        // Reject overlong 3-byte (< U+0800).
        if self.utf8_expected == 3 && cp < 0x800 {
            return VtAction::Print(REPLACEMENT_CHAR);
        }
        // Reject overlong 4-byte (< U+10000).
        if self.utf8_expected == 4 && cp < 0x10000 {
            return VtAction::Print(REPLACEMENT_CHAR);
        }

        VtAction::Print(cp)
    }

    fn escape(&mut self, byte: u8) -> VtAction {
        match byte {
            b'[' => {
                self.reset_params();
                self.state = State::CsiEntry;
                VtAction::Nop
            }
            b'7' => {
                self.state = State::Ground;
                VtAction::SaveCursor
            }
            b'8' => {
                self.state = State::Ground;
                VtAction::RestoreCursor
            }
            b']' => {
                self.state = State::OscString;
                VtAction::Nop
            }
            0x20..=0x2F => {
                self.state = State::EscapeIntermediate;
                VtAction::Nop
            }
            _ => {
                // Unrecognised ESC sequence → back to ground.
                self.state = State::Ground;
                VtAction::Nop
            }
        }
    }

    fn escape_intermediate(&mut self, byte: u8) -> VtAction {
        match byte {
            0x20..=0x2F => VtAction::Nop, // collect intermediates
            0x30..=0x7E => {
                // Final byte → discard the sequence.
                self.state = State::Ground;
                VtAction::Nop
            }
            _ => {
                self.state = State::Ground;
                VtAction::Nop
            }
        }
    }

    fn csi_entry(&mut self, byte: u8) -> VtAction {
        match byte {
            b'?' => {
                self.private_mode = true;
                self.state = State::CsiParam;
                VtAction::Nop
            }
            b'0'..=b'9' => {
                self.current_param = (byte - b'0') as u16;
                self.has_digit = true;
                self.state = State::CsiParam;
                VtAction::Nop
            }
            b';' => {
                self.push_param();
                self.state = State::CsiParam;
                VtAction::Nop
            }
            0x20..=0x2F => {
                self.state = State::CsiIntermediate;
                VtAction::Nop
            }
            0x40..=0x7E => {
                // Final byte with no parameters.
                self.push_param_if_digit();
                self.state = State::Ground;
                self.dispatch_csi(byte)
            }
            _ => {
                self.state = State::Ground;
                VtAction::Nop
            }
        }
    }

    fn csi_param(&mut self, byte: u8) -> VtAction {
        match byte {
            b'0'..=b'9' => {
                self.current_param = self
                    .current_param
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16);
                self.has_digit = true;
                VtAction::Nop
            }
            b';' => {
                self.push_param();
                VtAction::Nop
            }
            0x20..=0x2F => {
                self.push_param_if_digit();
                self.state = State::CsiIntermediate;
                VtAction::Nop
            }
            0x40..=0x7E => {
                // Final byte.
                self.push_param_if_digit();
                self.state = State::Ground;
                self.dispatch_csi(byte)
            }
            _ => {
                // Malformed → abort.
                self.state = State::Ground;
                VtAction::Nop
            }
        }
    }

    fn csi_intermediate(&mut self, byte: u8) -> VtAction {
        match byte {
            0x20..=0x2F => VtAction::Nop, // collect
            0x40..=0x7E => {
                // Final byte — we don't support any CSI with intermediates
                // yet, so just discard.
                self.state = State::Ground;
                VtAction::Nop
            }
            _ => {
                self.state = State::Ground;
                VtAction::Nop
            }
        }
    }

    fn osc_string(&mut self, byte: u8) -> VtAction {
        match byte {
            0x07 => {
                // BEL terminates OSC.
                self.state = State::Ground;
                VtAction::Nop
            }
            0x1B => {
                // Might be start of ST (ESC \).
                self.state = State::OscEscape;
                VtAction::Nop
            }
            _ => VtAction::Nop, // consume
        }
    }

    fn osc_escape(&mut self, byte: u8) -> VtAction {
        if byte == b'\\' {
            // ST terminates OSC.
            self.state = State::Ground;
        } else {
            // Not ST — false alarm; go back to OscString.
            self.state = State::OscString;
        }
        VtAction::Nop
    }

    // -- Parameter helpers --------------------------------------------------

    fn reset_params(&mut self) {
        self.params = [0; MAX_PARAMS];
        self.param_count = 0;
        self.current_param = 0;
        self.has_digit = false;
        self.private_mode = false;
    }

    fn push_param(&mut self) {
        if self.param_count < MAX_PARAMS {
            self.params[self.param_count] = self.current_param;
            self.param_count += 1;
        }
        self.current_param = 0;
        self.has_digit = false;
    }

    fn push_param_if_digit(&mut self) {
        if self.has_digit {
            self.push_param();
        }
    }

    fn param(&self, idx: usize, default: u16) -> u16 {
        if idx < self.param_count {
            let v = self.params[idx];
            if v == 0 { default } else { v }
        } else {
            default
        }
    }

    #[expect(
        dead_code,
        reason = "API completeness — used by future CSI dispatch extensions"
    )]
    fn param_raw(&self, idx: usize) -> u16 {
        if idx < self.param_count {
            self.params[idx]
        } else {
            0
        }
    }

    // -- CSI dispatch -------------------------------------------------------

    fn dispatch_csi(&mut self, final_byte: u8) -> VtAction {
        match final_byte {
            b'A' => VtAction::MoveCursor {
                direction: Direction::Up,
                count: self.param(0, 1),
            },
            b'B' => VtAction::MoveCursor {
                direction: Direction::Down,
                count: self.param(0, 1),
            },
            b'C' => VtAction::MoveCursor {
                direction: Direction::Forward,
                count: self.param(0, 1),
            },
            b'D' => VtAction::MoveCursor {
                direction: Direction::Back,
                count: self.param(0, 1),
            },
            b'H' | b'f' => {
                let row = self.param(0, 1).saturating_sub(1);
                let col = self.param(1, 1).saturating_sub(1);
                VtAction::SetCursorPos { row, col }
            }
            b'J' => {
                let mode = match self.param(0, 0) {
                    0 => EraseMode::ToEnd,
                    1 => EraseMode::ToStart,
                    _ => EraseMode::All,
                };
                VtAction::EraseDisplay(mode)
            }
            b'K' => {
                let mode = match self.param(0, 0) {
                    0 => EraseMode::ToEnd,
                    1 => EraseMode::ToStart,
                    _ => EraseMode::All,
                };
                VtAction::EraseLine(mode)
            }
            b'L' => VtAction::InsertLines(self.param(0, 1)),
            b'M' => VtAction::DeleteLines(self.param(0, 1)),
            b'P' => VtAction::DeleteChars(self.param(0, 1)),
            b'@' => VtAction::InsertChars(self.param(0, 1)),
            b'X' => VtAction::EraseChars(self.param(0, 1)),
            b'S' => VtAction::ScrollUp(self.param(0, 1)),
            b'T' => VtAction::ScrollDown(self.param(0, 1)),
            b'm' => self.dispatch_sgr(),
            b'r' => {
                let top = self.param(0, 1).saturating_sub(1);
                let bottom = self.param(1, 0);
                VtAction::SetScrollRegion { top, bottom }
            }
            b'h' => {
                if self.private_mode {
                    let mode = self.param(0, 0);
                    self.handle_set_mode(mode);
                    VtAction::SetMode(mode)
                } else {
                    VtAction::Nop
                }
            }
            b'l' => {
                if self.private_mode {
                    let mode = self.param(0, 0);
                    self.handle_reset_mode(mode);
                    VtAction::ResetMode(mode)
                } else {
                    VtAction::Nop
                }
            }
            _ => VtAction::Nop,
        }
    }

    // -- DEC private mode tracking ------------------------------------------

    fn handle_set_mode(&mut self, mode: u16) {
        match mode {
            1 => self.cursor_key_mode = true,
            6 => self.origin_mode = true,
            7 => self.auto_wrap = true,
            2004 => self.bracketed_paste = true,
            _ => {}
        }
    }

    fn handle_reset_mode(&mut self, mode: u16) {
        match mode {
            1 => self.cursor_key_mode = false,
            6 => self.origin_mode = false,
            7 => self.auto_wrap = false,
            2004 => self.bracketed_paste = false,
            _ => {}
        }
    }

    // -- SGR dispatch -------------------------------------------------------

    fn dispatch_sgr(&mut self) -> VtAction {
        // Default: `ESC[m` with no params is equivalent to `ESC[0m`.
        let count = if self.param_count == 0 {
            self.params[0] = 0;
            1usize
        } else {
            self.param_count
        };

        // Build all SGR actions into the pending queue.
        let mut total = 0usize;
        let mut i = 0;
        while i < count {
            let p = self.params[i];
            let action = match p {
                0 => VtAction::SetAttribute(SgrAttr::Reset),
                1 => VtAction::SetAttribute(SgrAttr::Bold),
                4 => VtAction::SetAttribute(SgrAttr::Underline),
                7 => VtAction::SetAttribute(SgrAttr::Inverse),
                22 => VtAction::SetAttribute(SgrAttr::NoBold),
                24 => VtAction::SetAttribute(SgrAttr::NoUnderline),
                27 => VtAction::SetAttribute(SgrAttr::NoInverse),
                30..=37 => VtAction::SetAttribute(SgrAttr::ForegroundColor((p - 30) as u8)),
                38 => {
                    // 256-color foreground: 38;5;N
                    // Truecolor foreground: 38;2;R;G;B
                    if i + 2 < count && self.params[i + 1] == 5 {
                        let n = self.params[i + 2] as u8;
                        i += 3;
                        VtAction::SetAttribute(SgrAttr::Foreground256(n))
                    } else if i + 4 < count && self.params[i + 1] == 2 {
                        let r = self.params[i + 2] as u8;
                        let g = self.params[i + 3] as u8;
                        let b = self.params[i + 4] as u8;
                        i += 5;
                        VtAction::SetAttribute(SgrAttr::ForegroundRgb(r, g, b))
                    } else {
                        i += 1;
                        continue;
                    }
                }
                39 => VtAction::SetAttribute(SgrAttr::DefaultForeground),
                40..=47 => VtAction::SetAttribute(SgrAttr::BackgroundColor((p - 40) as u8)),
                48 => {
                    // 256-color background: 48;5;N
                    // Truecolor background: 48;2;R;G;B
                    if i + 2 < count && self.params[i + 1] == 5 {
                        let n = self.params[i + 2] as u8;
                        i += 3;
                        VtAction::SetAttribute(SgrAttr::Background256(n))
                    } else if i + 4 < count && self.params[i + 1] == 2 {
                        let r = self.params[i + 2] as u8;
                        let g = self.params[i + 3] as u8;
                        let b = self.params[i + 4] as u8;
                        i += 5;
                        VtAction::SetAttribute(SgrAttr::BackgroundRgb(r, g, b))
                    } else {
                        i += 1;
                        continue;
                    }
                }
                49 => VtAction::SetAttribute(SgrAttr::DefaultBackground),
                90..=97 => VtAction::SetAttribute(SgrAttr::BrightForeground((p - 90) as u8)),
                100..=107 => VtAction::SetAttribute(SgrAttr::BrightBackground((p - 100) as u8)),
                _ => {
                    i += 1;
                    continue;
                }
            };

            if total < MAX_PARAMS {
                self.pending[total] = action;
                total += 1;
            }
            // Only increment i for non-extended-color cases (38/48 already
            // advanced i inside their branches above).
            if p != 38 && p != 48 {
                i += 1;
            }
        }

        // Return the first action directly; queue the rest.
        if total == 0 {
            return VtAction::Nop;
        }

        let first = self.pending[0];
        if total > 1 {
            // Shift remaining actions to start of pending array.
            let mut j = 0;
            while j < total - 1 {
                self.pending[j] = self.pending[j + 1];
                j += 1;
            }
            self.pending_count = total - 1;
            self.pending_idx = 0;
        }
        first
    }
}
