//! Safe legacy Intel 8254 PIT port window. The PIT serves **only** as a polled
//! calibration reference for the LAPIC timer: no IRQ routing, no frequency
//! configuration.

use crate::io::port::IoPort;

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
    /// Callers must serialise with interrupts disabled: the latch and the two
    /// byte reads must not be interleaved with another reader's.
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
