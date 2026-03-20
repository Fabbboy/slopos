// =============================================================================
// TTY types
// =============================================================================

/// Strongly-typed index into the global TTY table.
///
/// Wrapping the raw `u8` slot number prevents accidental mix-ups with other
/// small integer types (task IDs, pgrp IDs, etc.) at API boundaries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct TtyIndex(pub u8);

pub const TCGETS: u64 = 0x5401;
pub const TCSETS: u64 = 0x5402;
pub const TCSETSW: u64 = 0x5403;
pub const TCSETSF: u64 = 0x5404;
// Missing ioctls.
pub const TCSBRK: u64 = 0x5409;
pub const TCXONC: u64 = 0x540A;
pub const TCFLSH: u64 = 0x540B;
// tcflush() queue selectors.
pub const TCIFLUSH: i32 = 0;
pub const TCOFLUSH: i32 = 1;
pub const TCIOFLUSH: i32 = 2;
// tcflow() action selectors.
pub const TCOOFF: i32 = 0;
pub const TCOON: i32 = 1;
pub const TCIOFF: i32 = 2;
pub const TCION: i32 = 3;
pub const TIOCGPGRP: u64 = 0x540F;
pub const TIOCSPGRP: u64 = 0x5410;
pub const TIOCGPTN: u64 = 0x8004_5430;
pub const TIOCSETD: u64 = 0x5423;
pub const TIOCGETD: u64 = 0x5424;
pub const TIOCGSID: u64 = 0x5429;
pub const TIOCGWINSZ: u64 = 0x5413;
pub const TIOCSWINSZ: u64 = 0x5414;
pub const TIOCSCTTY: u64 = 0x540E;
/// Detach the calling process from its controlling terminal.
/// Linux value: 0x5422.
pub const TIOCNOTTY: u64 = 0x5422;
/// Get the number of bytes available for reading.
/// Linux value: 0x541B (same as TIOCINQ).
pub const FIONREAD: u64 = 0x541B;
/// Get the number of bytes in the output queue.
/// Linux value: 0x5411.
pub const TIOCOUTQ: u64 = 0x5411;
/// Set PTY slave lock state (0=unlock, 1=lock). Master FD only.
/// Linux value: 0x40045431.
pub const TIOCSPTLCK: u64 = 0x4004_5431;
/// Get PTY slave lock state. Returns 0 (unlocked) or 1 (locked).
/// Linux value: 0x80045439.
pub const TIOCGPTLCK: u64 = 0x8004_5439;
/// Enable/disable PTY packet mode on a master FD.
/// Linux value: 0x5420.
pub const TIOCPKT: u64 = 0x5420;

/// Packet mode control byte constants.
/// Normal data follows — no special event.
pub const TIOCPKT_DATA: u8 = 0x00;
/// Slave input queue was flushed.
pub const TIOCPKT_FLUSHREAD: u8 = 0x01;
/// Slave output queue was flushed.
pub const TIOCPKT_FLUSHWRITE: u8 = 0x02;
/// Slave output stopped (XOFF received).
pub const TIOCPKT_STOP: u8 = 0x04;
/// Slave output started (XON received).
pub const TIOCPKT_START: u8 = 0x08;
/// `IXON` cleared on slave termios.
pub const TIOCPKT_NOSTOP: u8 = 0x10;
/// `IXON` set on slave termios.
pub const TIOCPKT_DOSTOP: u8 = 0x20;

/// Open PTY slave from master fd — race-free, namespace-safe (Linux 4.13+).
pub const TIOCGPTPEER: u64 = 0x5441;

/// Set exclusive mode on a TTY — prevents other opens.
/// Linux value: 0x540C.
pub const TIOCEXCL: u64 = 0x540C;
/// Clear exclusive mode on a TTY — allows other opens.
/// Linux value: 0x540D.
pub const TIOCNXCL: u64 = 0x540D;
/// Get exclusive mode state (0 or 1).
/// Linux value: 0x80045440.
pub const TIOCGEXCL: u64 = 0x8004_5440;

pub const N_TTY: u32 = 0;
pub const N_RAW: u32 = 1;

pub const NCCS: usize = 19;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserTermios {
    pub c_iflag: InputFlags,
    pub c_oflag: OutputFlags,
    pub c_cflag: ControlFlags,
    pub c_lflag: LocalFlags,
    pub c_line: u8,
    pub c_cc: [u8; NCCS],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

pub const CBAUD: u32 = 0o010017;
pub const B0: u32 = 0o000000;
pub const B50: u32 = 0o000001;
pub const B75: u32 = 0o000002;
pub const B110: u32 = 0o000003;
pub const B134: u32 = 0o000004;
pub const B150: u32 = 0o000005;
pub const B200: u32 = 0o000006;
pub const B300: u32 = 0o000007;
pub const B600: u32 = 0o000010;
pub const B1200: u32 = 0o000011;
pub const B1800: u32 = 0o000012;
pub const B2400: u32 = 0o000013;
pub const B4800: u32 = 0o000014;
pub const B9600: u32 = 0o000015;
pub const B19200: u32 = 0o000016;
pub const B38400: u32 = 0o000017;
pub const CBAUDEX: u32 = 0o010000;
pub const B57600: u32 = 0o010001;
pub const B115200: u32 = 0o010002;
pub const B230400: u32 = 0o010003;
pub const B460800: u32 = 0o010004;
pub const B500000: u32 = 0o010005;
pub const B576000: u32 = 0o010006;
pub const B921600: u32 = 0o010007;
pub const B1000000: u32 = 0o010010;
pub const B1152000: u32 = 0o010011;
pub const B1500000: u32 = 0o010012;
pub const B2000000: u32 = 0o010013;
pub const B2500000: u32 = 0o010014;
pub const B3000000: u32 = 0o010015;
pub const B3500000: u32 = 0o010016;
pub const B4000000: u32 = 0o010017;
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VEOL: usize = 11;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VREPRINT: usize = 12;
pub const VWERASE: usize = 14;
pub const VDISCARD: usize = 13;
pub const VLNEXT: usize = 15;
pub const VEOL2: usize = 16;

// =============================================================================
// Type-safe termios flag types
// =============================================================================

bitflags::bitflags! {
    /// Type-safe wrapper for `c_iflag` — input processing flags.
    ///
    /// Constructed from the raw `u32` via `InputFlags::from_bits_truncate()`.
    /// Convert back with `.bits()`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct InputFlags: u32 {
        const IGNBRK = 0x001;
        const BRKINT = 0x002;
        const IGNPAR = 0x004;
        const PARMRK = 0x008;
        const INPCK  = 0x010;
        const ISTRIP = 0x020;
        const INLCR  = 0x040;
        const IGNCR  = 0x080;
        const ICRNL  = 0x100;
        const IXON   = 0x400;
        const IXANY  = 0x800;
        const IXOFF  = 0x1000;
        const IUTF8  = 0x4000;
        const IMAXBEL = 0x2000;
        const IUCLC  = 0x200;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_oflag` — output processing flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OutputFlags: u32 {
        const OPOST  = 0x01;
        const ONLCR  = 0x04;
        const OCRNL  = 0x08;
        const ONOCR  = 0x10;
        const ONLRET = 0x20;
        const OLCUC  = 0x02;

        // Tab delay flags (TABDLY/XTABS).
        // TABDLY is a 2-bit mask; only TAB0 (no expansion) and
        // TAB3/XTABS (expand to spaces) are implemented.
        const TABDLY = 0x1800;
        const TAB0   = 0x0000;
        const TAB3   = 0x1800;
        const XTABS  = 0x1800;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_lflag` — local (line discipline) flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LocalFlags: u32 {
        const ISIG    = 0x01;
        const ICANON  = 0x02;
        const ECHO    = 0x08;
        const ECHOE   = 0x10;
        const ECHOK   = 0x20;
        const ECHONL  = 0x40;
        const NOFLSH  = 0x80;
        const TOSTOP  = 0x100;
        const ECHOCTL = 0x200;
        const ECHOPRT = 0x400;
        const ECHOKE  = 0x800;
        const FLUSHO  = 0x1000;
        const PENDIN  = 0x4000;
        const IEXTEN  = 0x8000;
        const EXTPROC = 0x10000;
    }
}

bitflags::bitflags! {
    /// Type-safe wrapper for `c_cflag` — control (hardware) flags.
    ///
    /// Full c_cflag ABI with character size, parity,
    /// stop bits, modem control, baud rates, and hardware flow control.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ControlFlags: u32 {
        const CSIZE   = 0o000060;
        const CS5     = 0o000000;
        const CS6     = 0o000020;
        const CS7     = 0o000040;
        const CS8     = 0o000060;
        const CSTOPB  = 0o000100;
        const CREAD   = 0o000200;
        const PARENB  = 0o000400;
        const PARODD  = 0o001000;
        const HUPCL   = 0o002000;
        const CLOCAL  = 0o004000;
        const CRTSCTS = 0o020000000;
    }
}

// =============================================================================
// Strongly-typed c_cc index enum
// =============================================================================

/// Strongly-typed index into the `c_cc` control character array.
///
/// Replaces raw `usize` constants (`VINTR`, `VQUIT`, …) with a closed enum
/// so that invalid indices are compile-time errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CcIndex {
    Vintr = 0,
    Vquit = 1,
    Verase = 2,
    Vkill = 3,
    Veof = 4,
    Vtime = 5,
    Vmin = 6,
    Vstart = 8,
    Vstop = 9,
    Vsusp = 10,
    Veol = 11,
    Vreprint = 12,
    Vdiscard = 13,
    Vwerase = 14,
    Vlnext = 15,
    Veol2 = 16,
}

impl CcIndex {
    /// Convert to the underlying `usize` for array indexing.
    #[inline]
    pub const fn as_usize(self) -> usize {
        self as usize
    }
}

/// POSIX `_POSIX_VDISABLE` — value indicating a disabled control character.
pub const POSIX_VDISABLE: u8 = 0;

// =============================================================================
// UserTermios typed accessors
// =============================================================================

impl UserTermios {
    /// Get the typed input flags.
    #[inline]
    pub fn input_flags(&self) -> InputFlags {
        self.c_iflag
    }

    /// Get the typed output flags.
    #[inline]
    pub fn output_flags(&self) -> OutputFlags {
        self.c_oflag
    }

    /// Get the typed local flags.
    #[inline]
    pub fn local_flags(&self) -> LocalFlags {
        self.c_lflag
    }

    /// Get the typed control flags.
    #[inline]
    pub fn control_flags(&self) -> ControlFlags {
        self.c_cflag
    }

    /// Look up a control character by typed index.
    #[inline]
    pub fn cc(&self, idx: CcIndex) -> u8 {
        self.c_cc[idx.as_usize()]
    }

    /// Set a control character by typed index.
    #[inline]
    pub fn set_cc(&mut self, idx: CcIndex, val: u8) {
        self.c_cc[idx.as_usize()] = val;
    }
}

impl Default for UserTermios {
    fn default() -> Self {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03; // Ctrl+C
        cc[VQUIT] = 0x1C; // Ctrl+\
        cc[VERASE] = 0x7F; // DEL
        cc[VKILL] = 0x15; // Ctrl+U
        cc[VEOF] = 0x04; // Ctrl+D
        cc[VMIN] = 1;
        cc[VSTART] = 0x11; // Ctrl+Q
        cc[VSTOP] = 0x13; // Ctrl+S
        cc[VSUSP] = 0x1A; // Ctrl+Z
        cc[VREPRINT] = 0x12; // Ctrl+R
        cc[VDISCARD] = 0x0F; // Ctrl+O
        cc[VWERASE] = 0x17; // Ctrl+W
        cc[VLNEXT] = 0x16; // Ctrl+V
        Self {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::ECHOE,
            c_line: 0,
            c_cc: cc,
            c_ispeed: 0,
            c_ospeed: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserWinsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}
