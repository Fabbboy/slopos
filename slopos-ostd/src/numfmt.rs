//! Stack-only number formatting for `no_std` contexts.
//!
//! Every function writes into a caller-provided `&mut [u8]` buffer and returns
//! a `&[u8]` sub-slice of the formatted result (null-terminated for direct use
//! with the bitmap font renderer). No heap, no allocator, no raw pointers.
//!
//! # Typed wrappers
//!
//! The [`NumBuf`] helper encapsulates a correctly-sized stack buffer so callers
//! don't need to compute buffer lengths themselves:
//!
//! ```ignore
//! let mut buf = NumBuf::<21>::new();
//! let text = buf.format_u64(12345);      // b"12345\0"
//! let hex  = buf.format_hex_u64(0xBEEF); // b"0x000000000000BEEF\0"
//! ```

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

// ---------------------------------------------------------------------------
// Core formatting functions
// ---------------------------------------------------------------------------

/// Format a `u64` as a decimal string into `buf`.
///
/// Returns the sub-slice containing the formatted digits followed by a NUL
/// terminator. Returns `b"\0"` if the buffer is too small (needs at least 2
/// bytes for the `"0\0"` case).
pub fn fmt_u64(value: u64, buf: &mut [u8]) -> &[u8] {
    if buf.len() < 2 {
        if !buf.is_empty() {
            buf[0] = 0;
        }
        return &buf[..buf.len().min(1)];
    }

    if value == 0 {
        buf[0] = b'0';
        buf[1] = 0;
        return &buf[..2];
    }

    // Write digits in reverse, then shift to the front.
    let last = buf.len() - 1;
    buf[last] = 0; // NUL sentinel

    let mut pos = last;
    let mut n = value;
    while n != 0 && pos > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }

    // If the number didn't fit, return "0\0" as a safe fallback.
    if n != 0 {
        buf[0] = b'0';
        buf[1] = 0;
        return &buf[..2];
    }

    // Shift the formatted digits to the start of the buffer.
    let len = last - pos; // digit count (not including NUL)
    buf.copy_within(pos..=last, 0);
    &buf[..len + 1]
}

/// Format an `i64` as a decimal string into `buf`.
///
/// Negative values are prefixed with `'-'`. Returns the formatted sub-slice
/// (NUL-terminated).
pub fn fmt_i64(value: i64, buf: &mut [u8]) -> &[u8] {
    if value >= 0 {
        return fmt_u64(value as u64, buf);
    }

    if buf.len() < 3 {
        // Need at least "-0\0"
        if !buf.is_empty() {
            buf[0] = 0;
        }
        return &buf[..buf.len().min(1)];
    }

    buf[0] = b'-';
    let magnitude = if value == i64::MIN {
        (i64::MAX as u64) + 1
    } else {
        (-value) as u64
    };

    let tail = fmt_u64(magnitude, &mut buf[1..]);
    let total = 1 + tail.len();
    &buf[..total]
}

/// Format a `u32` as a decimal string into `buf`.
///
/// Convenience wrapper around [`fmt_u64`]. Returns the formatted sub-slice
/// (NUL-terminated).
#[inline]
pub fn fmt_u32(value: u32, buf: &mut [u8]) -> &[u8] {
    fmt_u64(value as u64, buf)
}

/// Format a `u64` as a hexadecimal string with `0x` prefix into `buf`.
///
/// Always produces a full 16-nibble representation (leading zeros included),
/// e.g. `"0x00000000DEADBEEF\0"`. The buffer must hold at least 19 bytes
/// (`"0x"` + 16 nibbles + NUL).
pub fn fmt_hex_u64(value: u64, buf: &mut [u8]) -> &[u8] {
    const NEEDED: usize = 2 + 16 + 1; // "0x" + 16 hex digits + NUL

    if buf.len() < NEEDED {
        if !buf.is_empty() {
            buf[0] = 0;
        }
        return &buf[..buf.len().min(1)];
    }

    buf[0] = b'0';
    buf[1] = b'x';

    let mut i = 0;
    while i < 16 {
        let nibble = ((value >> (60 - i * 4)) & 0xF) as usize;
        buf[2 + i] = HEX_DIGITS[nibble];
        i += 1;
    }

    buf[NEEDED - 1] = 0;
    &buf[..NEEDED]
}

/// Format a `u8` as a two-character hex string (no prefix) into `buf`.
///
/// Produces e.g. `"FF\0"`. Buffer needs at least 3 bytes.
pub fn fmt_hex_u8(value: u8, buf: &mut [u8]) -> &[u8] {
    if buf.len() < 3 {
        if !buf.is_empty() {
            buf[0] = 0;
        }
        return &buf[..buf.len().min(1)];
    }

    buf[0] = HEX_DIGITS[((value >> 4) & 0xF) as usize];
    buf[1] = HEX_DIGITS[(value & 0xF) as usize];
    buf[2] = 0;
    &buf[..3]
}

// ---------------------------------------------------------------------------
// NumBuf --- typed stack buffer helper
// ---------------------------------------------------------------------------

/// Stack-allocated formatting buffer.
///
/// `N` should be sized for the largest formatted output expected:
/// - Decimal `u64::MAX` needs 21 bytes (20 digits + NUL)
/// - Decimal `i64::MIN` needs 21 bytes ('-' + 19 digits + NUL)
/// - Hex `u64` needs 19 bytes ("0x" + 16 nibbles + NUL)
/// - Decimal `u32::MAX` needs 11 bytes (10 digits + NUL)
///
/// Common sizes: `NumBuf::<21>` (any integer), `NumBuf::<12>` (u32),
/// `NumBuf::<19>` (hex u64).
pub struct NumBuf<const N: usize> {
    buf: [u8; N],
}

impl<const N: usize> NumBuf<N> {
    #[inline]
    pub const fn new() -> Self {
        Self { buf: [0u8; N] }
    }

    /// Format a `u64` as decimal.
    #[inline]
    pub fn format_u64(&mut self, value: u64) -> &[u8] {
        fmt_u64(value, &mut self.buf)
    }

    /// Format a `u32` as decimal.
    #[inline]
    pub fn format_u32(&mut self, value: u32) -> &[u8] {
        fmt_u32(value, &mut self.buf)
    }

    /// Format an `i64` as decimal.
    #[inline]
    pub fn format_i64(&mut self, value: i64) -> &[u8] {
        fmt_i64(value, &mut self.buf)
    }

    /// Format a `u64` as hexadecimal with `0x` prefix.
    #[inline]
    pub fn format_hex_u64(&mut self, value: u64) -> &[u8] {
        fmt_hex_u64(value, &mut self.buf)
    }

    /// Format a `u8` as two hex digits (no prefix).
    #[inline]
    pub fn format_hex_u8(&mut self, value: u8) -> &[u8] {
        fmt_hex_u8(value, &mut self.buf)
    }
}
