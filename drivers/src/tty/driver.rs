//! TTY driver abstraction — backend hardware operations for each terminal.
//!
//! `TtyDriver` is the trait that abstracts over different terminal backends.
//! `TtyDriverKind` is an enum dispatch so we avoid trait objects in `no_std`.
//!
//! Implementations:
//! - `SerialConsoleDriver` — wraps COM1 UART (polling-based)
//! - `VConsoleDriver`      — wraps PS/2 keyboard + framebuffer text output
//! - `PtyMaster` / `PtySlave` — pseudo-terminal pair endpoints
//!
//! `DriverId` is the lock-free dispatch handle: the TTY core clones the
//! driver identifier while holding the per-TTY lock, drops the lock, and hands
//! the id to `super::output`, the only module that emits.  Neither
//! `TtyDriverKind` nor the `TtyDriver` trait exposes a write, so a frame
//! holding a slot guard cannot reach a driver.
//!
//! PTY peer references are `KWeak<TtyBacking>` links: the write site
//! upgrades the link, which pins the peer's slot for the duration of the
//! write — a failed upgrade means the peer is gone and the write is
//! discarded.

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
    /// Poll for pending hardware input, returning bytes read into `out`.
    /// Called by `Tty::drain_hw_input_locked`.  May return 0 if no data is available
    /// (e.g. PS/2 input comes via interrupt, not polling).
    fn drain_input(&self, out: &mut [u8]) -> usize;

    /// Optional: called when termios changes (e.g. baud rate).
    fn set_termios(&self, _termios: &UserTermios) {}

    /// Returns `true` if the driver has output bytes that have been accepted
    /// but not yet fully transmitted to the hardware.  Synchronous (polling)
    /// drivers always return `false` because their emission blocks until the
    /// byte is on the wire.  Async / interrupt-driven drivers should return
    /// `true` while the TX FIFO is non-empty.
    ///
    /// Used by `wait_output_idle()` drain semantics.
    fn output_pending(&self) -> bool {
        false
    }

    /// Bytes accepted by the driver but not yet transmitted.  Defaults to
    /// `0`/`1` from [`output_pending`](TtyDriver::output_pending), since most
    /// synchronous drivers only know "pending or not"; a driver with FIFO
    /// depth visibility overrides it so `TIOCOUTQ` reports the real count.
    fn output_pending_bytes(&self) -> usize {
        if self.output_pending() { 1 } else { 0 }
    }
}

// ---------------------------------------------------------------------------
// Enum dispatch — avoids `dyn TtyDriver` in no_std
// ---------------------------------------------------------------------------

/// Concrete driver backend for a `Tty`.
///
/// We use an enum rather than a trait object so that `Tty` can live in a
/// `const`-initialised static array without requiring `alloc`.
///
/// PTY variants carry a `KWeak<TtyBacking>` peer link; upgrading it pins
/// the peer's slot for the duration of a cross-end operation.
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
    /// Delegate `drain_input` to the inner driver.
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

    /// Delegate `set_termios` to the inner driver.
    pub fn set_termios(&self, termios: &UserTermios) {
        match self {
            Self::SerialConsole(d) => d.set_termios(termios),
            Self::VConsole(d) => d.set_termios(termios),
            Self::PtyMaster { .. } | Self::PtySlave { .. } => {}
        }
    }

    /// Returns `true` if the driver backend reports pending (un-transmitted)
    /// output.  Used by drain semantics (`TCSETSW` / `TCSETSF`).
    pub fn output_pending(&self) -> bool {
        match self {
            Self::SerialConsole(d) => d.output_pending(),
            Self::VConsole(d) => d.output_pending(),
            // PTY output is immediately buffered in the peer — no hardware
            // transmission latency.  POSIX considers output "sent" once it
            // reaches the kernel buffer destined for the peer.
            Self::PtyMaster { .. } | Self::PtySlave { .. } => false,
        }
    }

    /// Returns the number of bytes pending in the driver's output queue.
    ///
    /// Finer-grained queue depth for `TIOCOUTQ`.  Falls back to
    /// `output_pending()` for bool-only drivers (returns 0 or 1).
    pub fn output_pending_bytes(&self) -> usize {
        match self {
            Self::SerialConsole(d) => d.output_pending_bytes(),
            Self::VConsole(d) => d.output_pending_bytes(),
            // PTY output is immediately buffered in the peer — zero
            // hardware-level pending bytes.
            Self::PtyMaster { .. } | Self::PtySlave { .. } => 0,
        }
    }

    /// POSIX: only real terminals and PTY slaves may become a controlling
    /// terminal.  PTY masters are excluded — `TIOCSCTTY` on a master FD
    /// would break shell session management.
    pub fn can_be_controlling_terminal(&self) -> bool {
        !matches!(self, Self::PtyMaster { .. })
    }

    /// A clonable identifier for this variant: the caller clones it while
    /// holding the per-TTY lock, drops the lock, and hands it to
    /// `super::output`.  PTY variants carry the `KWeak<TtyBacking>` peer link
    /// through, so the write site pins the peer before touching it.
    pub fn id(&self) -> DriverId {
        match self {
            Self::SerialConsole(_) => DriverId::SerialConsole,
            Self::VConsole(_) => DriverId::VConsole,
            Self::PtyMaster { peer } => DriverId::PtyMaster { peer: peer.clone() },
            Self::PtySlave { peer } => DriverId::PtySlave { peer: peer.clone() },
        }
    }
}

// ---------------------------------------------------------------------------
// Lock-free driver I/O
// ---------------------------------------------------------------------------

/// Lightweight driver identifier — clonable across lock boundaries.
///
/// This enum carries *no mutable state* — it identifies which hardware
/// backend to use and, for PTY variants, carries the weak peer link.  The
/// TTY core clones it out of the per-TTY lock, drops the lock, and then
/// calls [`write_driver_unlocked`] to perform the actual I/O.
#[derive(Clone)]
pub enum DriverId {
    /// COM1 serial console.
    SerialConsole,
    /// PS/2 + framebuffer virtual console.
    VConsole,
    /// PTY master — writes go to the slave via the pinned peer link.
    PtyMaster { peer: KWeak<TtyBacking> },
    /// PTY slave — writes go to the master via the pinned peer link.
    PtySlave { peer: KWeak<TtyBacking> },
}

// ---------------------------------------------------------------------------
// Serial console driver — wraps COM1 UART polling-based I/O
// ---------------------------------------------------------------------------

/// Driver backend for COM1 serial console (TTY 0).
///
/// Output goes through `serial::serial_locked_write_bytes`, which takes
/// the same ticket lock as the klog backend, so TTY 0 writes never
/// byte-interleave with concurrent `klog_info!` output. Input is polled
/// from the serial UART's `INPUT_BUFFER` ring via `serial_poll_receive`
/// + buffer drain.
pub struct SerialConsoleDriver;

impl TtyDriver for SerialConsoleDriver {
    fn drain_input(&self, out: &mut [u8]) -> usize {
        // Poll the UART first — moves bytes from hardware FIFO into
        // INPUT_BUFFER.
        serial::serial_poll_receive(COM1.address());

        // Drain whatever the UART deposited into our scratch buffer.
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

// ---------------------------------------------------------------------------
// Virtual console driver — PS/2 keyboard + framebuffer
// ---------------------------------------------------------------------------

/// Driver backend for a virtual console (PS/2 keyboard + framebuffer).
///
/// Input arrives via interrupt (`tty::push_input`), so `drain_input` returns
/// nothing beyond what a test has injected.
pub struct VConsoleDriver;

/// Bytes a test has queued as if the vconsole's hardware had produced them.
///
/// The polled drain — the path that stages echo under the slot lock —
/// otherwise has no in-harness driver behind it: the serial console reads a
/// real UART and both PTY ends return nothing. Injecting here lets a test walk
/// it with the serial mirror off, so the echo cannot reach the wire the
/// harness is parsing.
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
        // PS/2 keyboard input comes via interrupt → tty::push_input.
        // No polling needed.
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
