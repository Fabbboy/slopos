//! Safe wrapper around the COM1 Line Status Register read, used by the
//! serial-flush shutdown test's poll loop.

#[cfg(not(miri))]
use core::arch::asm;

const COM1_BASE: u16 = 0x3F8;
const COM1_LSR_OFFSET: u16 = 5;

/// Read the COM1 Line Status Register.
///
/// Bit 0x40 is "transmit holding register empty + transmit shift register
/// empty" — the UART has drained every pending byte.
#[cfg(not(miri))]
#[inline]
pub fn read_lsr() -> u8 {
    let port = COM1_BASE + COM1_LSR_OFFSET;
    let v: u8;
    // SAFETY: 8-bit `in al, dx` from a fixed UART port; no memory
    // access; side-effect-free read on every PC-class system.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") v,
            options(nomem, nostack, preserves_flags),
        );
    }
    v
}

/// Miri stub: report TX drained so a poll loop terminates on its first pass.
#[cfg(miri)]
#[inline]
pub fn read_lsr() -> u8 {
    let _ = (COM1_BASE, COM1_LSR_OFFSET);
    0x60 // THRE | TEMT (transmit holding empty + transmit empty)
}
