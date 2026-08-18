//! Safe PS/2 controller register window: data (0x60) and status/command
//! (0x64), with the per-byte port I/O gated by
//! [`IoPortRegistry`](crate::io::port::IoPortRegistry).

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

    /// Read one byte from the controller's data port (port 0x60),
    /// consuming it from the output buffer.
    #[inline]
    pub fn read_data(&self) -> u8 {
        // SAFETY: consuming the next output-buffer byte is the intended
        // side effect for every caller.
        unsafe { self.data.read() }
    }

    /// Write a controller command byte (port 0x64).
    #[inline]
    pub fn write_command(&self, cmd: u8) {
        // SAFETY: dropping one command on the controller is the intended
        // side effect.
        unsafe { self.command.write(cmd) }
    }

    /// Write a data byte (port 0x60). Used both for device commands
    /// and for command-byte arguments.
    #[inline]
    pub fn write_data(&self, data: u8) {
        // SAFETY: forwarding a byte to the controller is the intended
        // side effect.
        unsafe { self.data.write(data) }
    }
}
