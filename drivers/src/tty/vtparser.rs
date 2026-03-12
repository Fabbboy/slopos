//! VT100/ANSI escape sequence state machine.
//!
//! Pure `no_std`, no-alloc parser that produces typed `VtAction` values for
//! the virtual console renderer.  Parsing is fully separated from rendering
//! so that the state machine can be tested independently.

const MAX_PARAMS: usize = 16;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VtAction {
    /// Printable ASCII character.
    Print(u8),
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
            0x20..=0x7E => VtAction::Print(byte),
            // DEL + high bytes → ignored this phase.
            _ => VtAction::Nop,
        }
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
            b'S' => VtAction::ScrollUp(self.param(0, 1)),
            b'T' => VtAction::ScrollDown(self.param(0, 1)),
            b'm' => self.dispatch_sgr(),
            b'h' => {
                if self.private_mode {
                    VtAction::SetMode(self.param(0, 0))
                } else {
                    VtAction::Nop
                }
            }
            b'l' => {
                if self.private_mode {
                    VtAction::ResetMode(self.param(0, 0))
                } else {
                    VtAction::Nop
                }
            }
            _ => VtAction::Nop,
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
                    // 256-color / truecolor — skip sub-params.
                    // Skip the entire 38;5;N or 38;2;R;G;B sub-sequence.
                    if i + 1 < count && self.params[i + 1] == 5 {
                        i += 3; // skip 38;5;N
                    } else if i + 1 < count && self.params[i + 1] == 2 {
                        i += 5; // skip 38;2;R;G;B
                    } else {
                        i += 1;
                    }
                    continue;
                }
                39 => VtAction::SetAttribute(SgrAttr::DefaultForeground),
                40..=47 => VtAction::SetAttribute(SgrAttr::BackgroundColor((p - 40) as u8)),
                48 => {
                    if i + 1 < count && self.params[i + 1] == 5 {
                        i += 3;
                    } else if i + 1 < count && self.params[i + 1] == 2 {
                        i += 5;
                    } else {
                        i += 1;
                    }
                    continue;
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
            i += 1;
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
