//! Safe PS/2 controller register window.
//!
//! The 8042 PS/2 controller exposes two ports: data (0x60) and
//! status/command (0x64). The per-byte port I/O is gated by the
//! [`IoPortRegistry`](crate::io::port::IoPortRegistry); this wrapper
//! folds the `unsafe { port.read() }` / `port.write()` calls behind
//! safe methods because every PS/2 access has the same well-understood
//! side-effect contract (status read is side-effect-free; data read
//! consumes one byte from the controller's output buffer; command
//! write drops one command on the controller).

use crate::io::port::IoPort;

/// Typed handle to the 8042-class PS/2 controller's two-port register
/// window.
#[derive(Clone, Copy)]
pub struct Ps2Regs {
    data: IoPort<u8>,
    status: IoPort<u8>,
    command: IoPort<u8>,
}

impl Ps2Regs {
    #[inline]
    pub const fn new(data: IoPort<u8>, status: IoPort<u8>, command: IoPort<u8>) -> Self {
        Self {
            data,
            status,
            command,
        }
    }

    /// Read the controller status register (port 0x64).
    #[inline]
    pub fn read_status(&self) -> u8 {
        // SAFETY: PS/2 status read is side-effect-free.
        unsafe { self.status.read() }
    }

    /// Read one byte from the controller's data port (port 0x60).
    /// Consumes one byte from the output buffer.
    #[inline]
    pub fn read_data(&self) -> u8 {
        // SAFETY: PS/2 data read consumes the next byte from the
        // controller's output buffer — the intended side effect for
        // every caller in this driver.
        unsafe { self.data.read() }
    }

    /// Write a controller command byte (port 0x64).
    #[inline]
    pub fn write_command(&self, cmd: u8) {
        // SAFETY: writing port 0x64 drops one command on the PS/2
        // controller — the intended side effect.
        unsafe { self.command.write(cmd) }
    }

    /// Write a data byte (port 0x60). Used both for device commands
    /// and for command-byte arguments.
    #[inline]
    pub fn write_data(&self, data: u8) {
        // SAFETY: writing port 0x60 forwards a byte to the controller
        // — the intended side effect.
        unsafe { self.data.write(data) }
    }
}
