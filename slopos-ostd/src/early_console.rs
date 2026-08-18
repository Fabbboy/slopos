//! Pre-OSTD-init serial output primitive.
//!
//! Three safe `pub fn`s that write bytes to COM1 (`0x3F8`) without any prior
//! OSTD initialization — in particular without
//! [`IoPortRegistry`](crate::io::port::IoPortRegistry) being installed, which
//! is why the registry-gated `IoPort<T>` cannot serve the early-boot panic
//! logger.
//!
//! No internal locking; callers serialise externally.
//!
//! On non-`target_os = "none"` builds the port I/O is replaced with a `static`
//! byte ring so host tests can observe the emitted bytes. Drain via
//! [`take_recorded_bytes_for_tests`].

#[cfg(target_os = "none")]
mod imp {
    use crate::io::port::PortAccessible;

    const COM1_BASE: u16 = 0x3F8;
    const UART_REG_THR: u16 = 0;
    const UART_REG_LSR: u16 = 5;
    const UART_LSR_TX_EMPTY: u8 = 0x20;

    #[inline(always)]
    pub fn write_byte(b: u8) {
        // SAFETY: COM1 (`0x3F8`) is a well-known 8250/16550-compatible
        // UART base address on every supported platform. Reading the
        // LSR is side-effect-free; writing the THR transmits one byte.
        unsafe {
            while (u8::read_from_port(COM1_BASE + UART_REG_LSR) & UART_LSR_TX_EMPTY) == 0 {
                core::hint::spin_loop();
            }
            u8::write_to_port(COM1_BASE + UART_REG_THR, b);
        }
    }

    #[inline]
    pub fn flush() {
        // SAFETY: see `write_byte`.
        unsafe {
            while (u8::read_from_port(COM1_BASE + UART_REG_LSR) & UART_LSR_TX_EMPTY) == 0 {
                core::hint::spin_loop();
            }
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    pub(super) const MOCK_CAP: usize = 4096;
    pub(super) static MOCK_BUFFER: [AtomicU8; MOCK_CAP] =
        { [const { AtomicU8::new(0) }; MOCK_CAP] };
    pub(super) static MOCK_LEN: AtomicUsize = AtomicUsize::new(0);

    pub fn write_byte(b: u8) {
        let i = MOCK_LEN.fetch_add(1, Ordering::AcqRel);
        if i < MOCK_CAP {
            MOCK_BUFFER[i].store(b, Ordering::Release);
        }
    }

    pub fn flush() {
        // No-op on host — the mock buffer has no pending transmit state.
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Write one byte to COM1. Polls the UART LSR until the transmit
/// holding register is empty, then writes the byte.
#[inline]
pub fn write_byte(b: u8) {
    imp::write_byte(b);
}

/// Write a byte slice to COM1, converting lone `\n` into `\r\n`.
///
/// An existing `\r\n` pair passes through unchanged (the `\n` after a
/// `\r` is preceded by the existing `\r`, not by a duplicate).
#[inline]
pub fn write_bytes(slice: &[u8]) {
    // Mirror the full serial stream into the framebuffer-log capture ring.
    // This is the single sink all serial output funnels through (kernel klog
    // and userland TTY alike), so the on-screen log matches the wire.
    crate::fblog::capture(slice);

    let mut last_was_cr = false;
    for &b in slice {
        if b == b'\n' && !last_was_cr {
            imp::write_byte(b'\r');
        }
        imp::write_byte(b);
        last_was_cr = b == b'\r';
    }
}

/// Wait for the UART transmit FIFO to drain. Returns when the
/// transmit holding register is empty.
#[inline]
pub fn flush() {
    imp::flush();
}

// ---------------------------------------------------------------------------
// Host-side test helpers.
// ---------------------------------------------------------------------------

/// Drain the recorded mock buffer and return its contents.
///
/// Used by host-side tests to observe the bytes that `write_byte` /
/// `write_bytes` would have transmitted on a real UART. Gated behind
/// `feature = "test-helpers"` (auto-enabled by the dev-dependency
/// shim in slopos-ostd's Cargo.toml) or `cfg(test)` for internal
/// crate tests. Not exposed in production builds.
#[cfg(all(not(target_os = "none"), any(test, feature = "test-helpers")))]
pub fn take_recorded_bytes_for_tests() -> alloc::vec::Vec<u8> {
    use core::sync::atomic::Ordering;
    let len = imp::MOCK_LEN.swap(0, Ordering::AcqRel).min(imp::MOCK_CAP);
    let mut out = alloc::vec::Vec::with_capacity(len);
    for i in 0..len {
        out.push(imp::MOCK_BUFFER[i].swap(0, Ordering::AcqRel));
    }
    out
}
