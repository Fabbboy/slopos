//! TTY driver abstraction — backend hardware operations for each terminal.
//!
//! `DriverId` is the lock-free dispatch handle: the TTY core clones it under
//! the per-TTY lock, drops the lock, then hands it to `super::output`, the only
//! module that emits. Neither `TtyDriverKind` nor the `TtyDriver` trait exposes
//! a write, so a frame holding a slot guard cannot reach a driver.
//!
//! PTY peer references are `KWeak<TtyBacking>`: the write site upgrades one to
//! pin the peer's slot, and a failed upgrade discards the write.

use slopos_abi::syscall::UserTermios;
use slopos_ostd::KWeak;
use slopos_ostd::io::port_consts::COM1;

use crate::serial;
use crate::tty::backing::TtyBacking;

#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    pub byte: u8,
    pub status: InputStatus,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputStatus {
    Normal = 0,
    Break = 1,
    ParityError = 2,
    FrameError = 3,
    Overrun = 4,
}

impl InputEvent {
    pub const fn normal(byte: u8) -> Self {
        Self {
            byte,
            status: InputStatus::Normal,
        }
    }
}

impl From<u8> for InputEvent {
    fn from(value: u8) -> Self {
        Self::normal(value)
    }
}

/// Backend operations for a TTY, none of which emit: output goes through
/// `super::output`.  A driver never touches the line discipline directly.
pub trait TtyDriver {
    fn drain_input(&self, out: &mut [u8]) -> usize;

    fn set_termios(&self, _termios: &UserTermios) {}

    /// Whether the driver holds output accepted but not yet transmitted.
    /// Synchronous (polling) drivers return `false`: their emission blocks
    /// until the byte is on the wire.
    fn output_pending(&self) -> bool {
        false
    }

    /// Bytes accepted but not yet transmitted. Defaults to `0`/`1` from
    /// [`output_pending`](TtyDriver::output_pending); a driver with FIFO-depth
    /// visibility overrides it so `TIOCOUTQ` reports the real count.
    fn output_pending_bytes(&self) -> usize {
        if self.output_pending() { 1 } else { 0 }
    }
}

/// Concrete driver backend for a `Tty`. An enum rather than a trait object so
/// `Tty` can live in a `const`-initialised static array without `alloc`.
pub enum TtyDriverKind {
    /// COM1 serial console.
    SerialConsole(SerialConsoleDriver),
    /// PS/2 keyboard + framebuffer virtual console.
    VConsole(VConsoleDriver),
    /// PTY master — writes go to the slave's input buffer.
    PtyMaster { peer: KWeak<TtyBacking> },
    /// PTY slave — writes go to the master's read buffer.
    PtySlave { peer: KWeak<TtyBacking> },
}

impl TtyDriverKind {
    pub fn drain_input(&self, out: &mut [u8]) -> usize {
        match self {
            Self::SerialConsole(d) => d.drain_input(out),
            Self::VConsole(d) => d.drain_input(out),
            Self::PtyMaster { .. } | Self::PtySlave { .. } => {
                // PTY input arrives via push_input, not polling.
                0
            }
        }
    }

    pub fn set_termios(&self, termios: &UserTermios) {
        match self {
            Self::SerialConsole(d) => d.set_termios(termios),
            Self::VConsole(d) => d.set_termios(termios),
            Self::PtyMaster { .. } | Self::PtySlave { .. } => {}
        }
    }

    pub fn output_pending(&self) -> bool {
        match self {
            Self::SerialConsole(d) => d.output_pending(),
            Self::VConsole(d) => d.output_pending(),
            // POSIX considers PTY output sent once it reaches the peer's
            // buffer, which the write already did.
            Self::PtyMaster { .. } | Self::PtySlave { .. } => false,
        }
    }

    pub fn output_pending_bytes(&self) -> usize {
        match self {
            Self::SerialConsole(d) => d.output_pending_bytes(),
            Self::VConsole(d) => d.output_pending_bytes(),
            Self::PtyMaster { .. } | Self::PtySlave { .. } => 0,
        }
    }

    /// POSIX: only real terminals and PTY slaves may become a controlling
    /// terminal; `TIOCSCTTY` on a master FD would break session management.
    pub fn can_be_controlling_terminal(&self) -> bool {
        !matches!(self, Self::PtyMaster { .. })
    }

    pub fn id(&self) -> DriverId {
        match self {
            Self::SerialConsole(_) => DriverId::SerialConsole,
            Self::VConsole(_) => DriverId::VConsole,
            Self::PtyMaster { peer } => DriverId::PtyMaster { peer: peer.clone() },
            Self::PtySlave { peer } => DriverId::PtySlave { peer: peer.clone() },
        }
    }
}

/// Carries no mutable state, so it can be cloned out of the per-TTY lock.
#[derive(Clone)]
pub enum DriverId {
    SerialConsole,
    VConsole,
    PtyMaster { peer: KWeak<TtyBacking> },
    PtySlave { peer: KWeak<TtyBacking> },
}

/// COM1 serial console, TTY 0.
pub struct SerialConsoleDriver;

impl TtyDriver for SerialConsoleDriver {
    fn drain_input(&self, out: &mut [u8]) -> usize {
        serial::serial_poll_receive(COM1.address());

        let mut buf = serial::input_buffer_lock();
        let mut n = 0usize;
        while n < out.len() {
            match buf.try_pop() {
                Some(b) => {
                    out[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

/// Input arrives via interrupt (`tty::push_input`), so `drain_input` returns
/// nothing beyond what a test has injected.
pub struct VConsoleDriver;

/// Bytes a test has queued as if the vconsole's hardware had produced them:
/// the polled drain is the only in-harness path that stages echo under the
/// slot lock.
#[cfg(feature = "test-hooks")]
static VCONSOLE_INJECT: slopos_ostd::sync::SpinLock<slopos_ostd::ring_buffer::RingBuffer<u8, 64>> =
    slopos_ostd::sync::SpinLock::new(
        slopos_ostd::ring_buffer::RingBuffer::new_zeroed(),
        slopos_ostd::lock_class!("VCONSOLE_INJECT", slopos_ostd::sync::LOCK_LEVEL_RESOURCE),
    );

/// Queue `bytes` for the next virtual-console drain.
#[cfg(feature = "test-hooks")]
pub fn inject_vconsole_input(bytes: &[u8]) {
    let mut buf = VCONSOLE_INJECT.lock();
    for &b in bytes {
        let _ = buf.try_push(b);
    }
}

impl TtyDriver for VConsoleDriver {
    fn drain_input(&self, _out: &mut [u8]) -> usize {
        #[cfg(feature = "test-hooks")]
        {
            let mut buf = VCONSOLE_INJECT.lock();
            let mut n = 0usize;
            while n < _out.len() {
                match buf.try_pop() {
                    Some(b) => {
                        _out[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            return n;
        }
        #[cfg(not(feature = "test-hooks"))]
        0
    }
}
