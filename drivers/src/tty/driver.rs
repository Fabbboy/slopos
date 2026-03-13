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
//! adds `DriverId` for lock-free I/O dispatch: the TTY core copies
//! the driver identifier while holding the per-TTY lock, drops the lock, and
//! then writes the processed output via `write_driver_unlocked`.
//!
//! replaces plain `TtyIndex` peer references with generation-tagged
//! `PtyPeerHandle`s so that stale writes after rapid PTY free/reuse are
//! safely discarded.

use slopos_abi::syscall::UserTermios;
use slopos_lib::ports::COM1;

use crate::serial;
use crate::tty::pty;
use crate::tty::pty::PtyPeerHandle;

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

/// Backend driver operations for a TTY.
///
/// Implementors provide the hardware-level write and optional input polling.
/// The TTY core calls these methods — the driver never touches the line
/// discipline directly.
pub trait TtyDriver {
    /// Write processed output bytes to the terminal hardware (serial port,
    /// framebuffer, etc.).
    fn write_output(&self, buf: &[u8]);

    /// Poll for pending hardware input, returning bytes read into `out`.
    /// Called by `Tty::drain_hw_input_locked`.  May return 0 if no data is available
    /// (e.g. PS/2 input comes via interrupt, not polling).
    fn drain_input(&self, out: &mut [u8]) -> usize;

    /// Optional: called when termios changes (e.g. baud rate).
    fn set_termios(&self, _termios: &UserTermios) {}

    /// Returns `true` if the driver has output bytes that have been accepted
    /// but not yet fully transmitted to the hardware.  Synchronous (polling)
    /// drivers always return `false` because `write_output` blocks until the
    /// byte is on the wire.  Async / interrupt-driven drivers should return
    /// `true` while the TX FIFO is non-empty.
    ///
    /// Used by `wait_output_idle()` drain semantics.
    fn output_pending(&self) -> bool {
        false
    }

    /// Returns the number of bytes that have been accepted by the driver but
    /// not yet fully transmitted to the hardware.  Defaults to `0` or `1`
    /// based on [`output_pending`](TtyDriver::output_pending), since most
    /// synchronous drivers only know "pending or not".
    ///
    /// Async / interrupt-driven drivers with FIFO depth visibility should
    /// override this to return the actual byte count for accurate
    /// `TIOCOUTQ` reporting.
    ///
    /// Stronger per-driver pending-byte semantics.
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
/// PTY variants now carry [`PtyPeerHandle`] (index + generation)
/// instead of a plain `TtyIndex`, enabling stale-slot detection.
pub enum TtyDriverKind {
    /// COM1 serial console.
    SerialConsole(SerialConsoleDriver),
    /// PS/2 keyboard + framebuffer virtual console.
    VConsole(VConsoleDriver),
    /// PTY master — writes go to the slave's input buffer.
    PtyMaster { peer: PtyPeerHandle },
    /// PTY slave — writes go to the master's read buffer.
    PtySlave { peer: PtyPeerHandle },
    /// Uninitialised / empty slot.
    None,
}

impl TtyDriverKind {
    /// Delegate `write_output` to the inner driver.
    pub fn write_output(&self, buf: &[u8]) {
        match self {
            Self::SerialConsole(d) => d.write_output(buf),
            Self::VConsole(d) => d.write_output(buf),
            Self::PtyMaster { peer } => {
                pty::master_write(*peer, buf);
            }
            Self::PtySlave { peer } => {
                // Short writes flow back through write_driver_unlocked's return value.
                let _ = pty::slave_write(*peer, buf);
            }
            Self::None => {}
        }
    }

    /// Delegate `drain_input` to the inner driver.
    pub fn drain_input(&self, out: &mut [u8]) -> usize {
        match self {
            Self::SerialConsole(d) => d.drain_input(out),
            Self::VConsole(d) => d.drain_input(out),
            Self::PtyMaster { .. } | Self::PtySlave { .. } => {
                // PTY input arrives via push_input, not polling.
                0
            }
            Self::None => 0,
        }
    }

    /// Delegate `set_termios` to the inner driver.
    pub fn set_termios(&self, termios: &UserTermios) {
        match self {
            Self::SerialConsole(d) => d.set_termios(termios),
            Self::VConsole(d) => d.set_termios(termios),
            Self::PtyMaster { .. } | Self::PtySlave { .. } | Self::None => {}
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
            Self::None => false,
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
            Self::None => 0,
        }
    }

    /// Return a lightweight, copyable identifier for this driver variant.
    ///
    /// Used by the split-write path: the caller copies the `DriverId` while
    /// holding the per-TTY lock, drops the lock, and then calls
    /// [`write_driver_unlocked`] to perform the (slow) hardware I/O without
    /// holding any TTY lock.
    ///
    /// PTY variants now carry [`PtyPeerHandle`] through the
    /// `DriverId` so generation validation happens at the write site.
    pub fn id(&self) -> DriverId {
        match self {
            Self::SerialConsole(_) => DriverId::SerialConsole,
            Self::VConsole(_) => DriverId::VConsole,
            Self::PtyMaster { peer } => DriverId::PtyMaster { peer: *peer },
            Self::PtySlave { peer } => DriverId::PtySlave { peer: *peer },
            Self::None => DriverId::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Lock-free driver I/O
// ---------------------------------------------------------------------------

/// Lightweight driver identifier — copyable across lock boundaries.
///
/// This enum carries *no mutable state* — it identifies which hardware
/// backend to use and, for PTY variants, carries a generation-tagged peer
/// handle.  The TTY core copies it out of the per-TTY lock, drops the lock,
/// and then calls [`write_driver_unlocked`] to perform the actual I/O.
///
/// PTY variants carry [`PtyPeerHandle`] for generation-safe
/// cross-end writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverId {
    /// COM1 serial console.
    SerialConsole,
    /// PS/2 + framebuffer virtual console.
    VConsole,
    /// PTY master — writes go to the slave via generation-validated handle.
    PtyMaster { peer: PtyPeerHandle },
    /// PTY slave — writes go to the master via generation-validated handle.
    PtySlave { peer: PtyPeerHandle },
    /// Empty / uninitialised slot.
    None,
}

/// Write processed output bytes to the hardware **without** holding any TTY
/// lock.
///
/// This is the second step of the split-write pattern:
///
/// 1. **Under per-TTY lock** — process output through the line discipline
///    into a local stack buffer, copy `DriverId`, drop the lock.
/// 2. **Without lock** — call this function to send the buffered bytes to the
///    hardware driver.
///
/// This separation ensures that slow serial I/O (~86 μs/byte at 115200 baud)
/// does not block operations on other TTYs.
///
/// PTY variants pass the `PtyPeerHandle` to `master_write` /
/// `slave_write`, which validate the generation before touching the peer slot.
pub fn write_driver_unlocked(driver: DriverId, data: &[u8]) -> usize {
    match driver {
        DriverId::SerialConsole => {
            for &b in data {
                serial::serial_putc_com1(b);
            }
            data.len()
        }
        DriverId::VConsole => {
            super::vconsole::write(data);
            for &b in data {
                serial::serial_putc_com1(b);
            }
            data.len()
        }
        DriverId::PtyMaster { peer } => pty::master_write(peer, data),
        DriverId::PtySlave { peer } => pty::slave_write(peer, data),
        DriverId::None => data.len(),
    }
}

// ---------------------------------------------------------------------------
// Serial console driver — wraps COM1 UART polling-based I/O
// ---------------------------------------------------------------------------

/// Driver backend for COM1 serial console (TTY 0).
///
/// Output goes through `serial_putc_com1`.  Input is polled from the serial
/// UART's `INPUT_BUFFER` ring via `serial_poll_receive` + buffer drain.
pub struct SerialConsoleDriver;

impl TtyDriver for SerialConsoleDriver {
    fn write_output(&self, buf: &[u8]) {
        for &b in buf {
            serial::serial_putc_com1(b);
        }
    }

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
/// Input arrives via interrupt (`tty::push_input`), so `drain_input` returns 0.
pub struct VConsoleDriver;

impl TtyDriver for VConsoleDriver {
    fn write_output(&self, buf: &[u8]) {
        super::vconsole::write(buf);
        for &b in buf {
            serial::serial_putc_com1(b);
        }
    }

    fn drain_input(&self, _out: &mut [u8]) -> usize {
        // PS/2 keyboard input comes via interrupt → tty::push_input.
        // No polling needed.
        0
    }
}
