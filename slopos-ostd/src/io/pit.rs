//! Safe legacy Intel 8254 PIT port window.
//!
//! The PIT is used **only** as a polled calibration reference for the
//! LAPIC timer. No IRQ routing, no frequency configuration. This
//! wrapper exposes the read-channel-0 sequence as a single safe call.

use crate::io::port::IoPort;

/// Typed handle to the PIT channel-0 counter and the controller
/// command port.
#[derive(Clone, Copy)]
pub struct Pit {
    command: IoPort<u8>,
    channel0: IoPort<u8>,
}

impl Pit {
    #[inline]
    pub const fn new(command: IoPort<u8>, channel0: IoPort<u8>) -> Self {
        Self { command, channel0 }
    }

    /// Latch and read the channel-0 down-counter.
    ///
    /// Issues the latch command, then reads two bytes (low, high) from
    /// the channel-0 port and assembles the 16-bit counter value.
    /// Callers must serialise with interrupts disabled to avoid the
    /// stale two-byte read window.
    #[inline]
    pub fn read_count(&self) -> u16 {
        // SAFETY: the three port accesses below are the documented
        // "latch + 2-byte read" sequence on channel 0; together they
        // form one atomic counter snapshot.
        unsafe {
            self.command.write(0x00);
            let low = self.channel0.read();
            let high = self.channel0.read();
            ((high as u16) << 8) | (low as u16)
        }
    }
}
