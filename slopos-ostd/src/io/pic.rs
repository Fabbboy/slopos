//! Safe legacy 8259 PIC port window.
//!
//! SlopOS does not route IRQs through the PIC (IOAPIC mandatory) — the
//! only operation we perform on the PIC is the boot-time "mask all and
//! send EOI" quiesce so the legacy chip does not deliver spurious
//! interrupts. This window exposes that single sequence as a safe
//! method.

use crate::io::port::IoPort;

const PIC_EOI: u8 = 0x20;

/// Typed handle to the master+slave 8259 PIC pair.
#[derive(Clone, Copy)]
pub struct Pic {
    pic1_cmd: IoPort<u8>,
    pic1_data: IoPort<u8>,
    pic2_cmd: IoPort<u8>,
    pic2_data: IoPort<u8>,
}

impl Pic {
    #[inline]
    pub const fn new(
        pic1_cmd: IoPort<u8>,
        pic1_data: IoPort<u8>,
        pic2_cmd: IoPort<u8>,
        pic2_data: IoPort<u8>,
    ) -> Self {
        Self {
            pic1_cmd,
            pic1_data,
            pic2_cmd,
            pic2_data,
        }
    }

    /// Mask every line on both PICs and send a non-specific EOI to
    /// each. After this call the legacy 8259 pair is silent.
    #[inline]
    pub fn quiesce_disable(&self) {
        // SAFETY: masking all lines and EOI-ing both PICs is the
        // documented "disable" sequence; no other interrupt path
        // depends on either PIC.
        unsafe {
            self.pic1_data.write(0xFF);
            self.pic2_data.write(0xFF);
            self.pic1_cmd.write(PIC_EOI);
            self.pic2_cmd.write(PIC_EOI);
        }
    }
}
