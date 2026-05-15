//! Safe wrapper around the COM1 Line Status Register read used by the
//! `boot/src/tests/shutdown_tests.rs::test_serial_flush_terminates`
//! test loop.

#[cfg(not(miri))]
use core::arch::asm;

/// COM1 base I/O port.
const COM1_BASE: u16 = 0x3F8;
/// COM1 Line Status Register offset.
const COM1_LSR_OFFSET: u16 = 5;

/// Read the COM1 Line Status Register (offset 5) via an 8-bit port `in`.
///
/// Bit 0x40 is "transmit holding register empty + transmit shift
/// register empty", which the shutdown stress test polls to confirm
/// the UART has actually drained pending bytes.
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

/// Miri stub: report TX empty + drained immediately so any poll loop
/// terminates on the first iteration.
#[cfg(miri)]
#[inline]
pub fn read_lsr() -> u8 {
    let _ = (COM1_BASE, COM1_LSR_OFFSET);
    0x60 // THRE | TEMT (transmit holding empty + transmit empty)
}
