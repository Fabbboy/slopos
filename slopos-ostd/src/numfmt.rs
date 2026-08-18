//! Stack-only number formatting for `no_std` contexts.
//!
//! Every function writes into a caller-provided `&mut [u8]` and returns a
//! NUL-terminated sub-slice of it, for direct use with the bitmap font renderer.
//!
//! [`NumBuf`] wraps a correctly-sized stack buffer:
//!
//! ```ignore
//! let mut buf = NumBuf::<21>::new();
//! let text = buf.format_u64(12345);      // b"12345\0"
//! let hex  = buf.format_hex_u64(0xBEEF); // b"0x000000000000BEEF\0"
//! ```

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// Format a `u64` as a decimal string into `buf`, NUL-terminated. Returns
/// `b"\0"` if the buffer is too small (needs at least 2 bytes for `"0\0"`).
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

    let last = buf.len() - 1;
    buf[last] = 0;

    let mut pos = last;
    let mut n = value;
    while n != 0 && pos > 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
    }

    if n != 0 {
        buf[0] = b'0';
        buf[1] = 0;
        return &buf[..2];
    }

    let len = last - pos;
    buf.copy_within(pos..=last, 0);
    &buf[..len + 1]
}

/// Format an `i64` as a decimal string into `buf`, NUL-terminated. Negative
/// values are prefixed with `'-'`.
pub fn fmt_i64(value: i64, buf: &mut [u8]) -> &[u8] {
    if value >= 0 {
        return fmt_u64(value as u64, buf);
    }

    if buf.len() < 3 {
        // Needs room for at least "-0\0".
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

/// Format a `u32` as a decimal string into `buf`, NUL-terminated.
#[inline]
pub fn fmt_u32(value: u32, buf: &mut [u8]) -> &[u8] {
    fmt_u64(value as u64, buf)
}

/// Format a `u64` as `0x`-prefixed hex into `buf`, always a full 16 nibbles
/// with leading zeros. The buffer must hold at least 19 bytes.
pub fn fmt_hex_u64(value: u64, buf: &mut [u8]) -> &[u8] {
    const NEEDED: usize = 2 + 16 + 1;

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

/// Format a `u8` as two hex digits (no prefix) into `buf`, NUL-terminated.
/// Buffer needs at least 3 bytes.
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

/// Stack-allocated formatting buffer. `N` must cover the largest output plus
/// NUL: 21 for any decimal `u64`/`i64`, 19 for hex `u64`, 11 for decimal `u32`.
pub struct NumBuf<const N: usize> {
    buf: [u8; N],
}

impl<const N: usize> NumBuf<N> {
    #[inline]
    pub const fn new() -> Self {
        Self { buf: [0u8; N] }
    }

    #[inline]
    pub fn format_u64(&mut self, value: u64) -> &[u8] {
        fmt_u64(value, &mut self.buf)
    }

    #[inline]
    pub fn format_u32(&mut self, value: u32) -> &[u8] {
        fmt_u32(value, &mut self.buf)
    }

    #[inline]
    pub fn format_i64(&mut self, value: i64) -> &[u8] {
        fmt_i64(value, &mut self.buf)
    }

    #[inline]
    pub fn format_hex_u64(&mut self, value: u64) -> &[u8] {
        fmt_hex_u64(value, &mut self.buf)
    }

    #[inline]
    pub fn format_hex_u8(&mut self, value: u8) -> &[u8] {
        fmt_hex_u8(value, &mut self.buf)
    }
}
