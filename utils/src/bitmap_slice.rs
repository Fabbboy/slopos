//! Bit operations on `&[u8]` slices for on-disk bitmap formats (ext2).
//!
//! Bit 0 is the LSB of `data[0]` (little-endian bit order).
//! These must not depend on host word size — the byte layout is an on-disk ABI.

#[inline]
pub fn test_bit(data: &[u8], bit: usize) -> bool {
    let byte = bit / 8;
    let mask = 1u8 << (bit % 8);
    byte < data.len() && (data[byte] & mask) != 0
}

#[inline]
pub fn set_bit(data: &mut [u8], bit: usize) {
    let byte = bit / 8;
    let mask = 1u8 << (bit % 8);
    if let Some(b) = data.get_mut(byte) {
        *b |= mask;
    }
}

#[inline]
pub fn clear_bit(data: &mut [u8], bit: usize) {
    let byte = bit / 8;
    let mask = 1u8 << (bit % 8);
    if let Some(b) = data.get_mut(byte) {
        *b &= !mask;
    }
}

pub fn find_first_zero(data: &[u8], nbits: usize, start: usize) -> Option<usize> {
    let start_byte = start / 8;
    let start_offset = start % 8;
    for (byte_idx, &byte) in data.iter().enumerate().skip(start_byte) {
        if byte == 0xFF {
            continue;
        }
        let bit_start = if byte_idx == start_byte {
            start_offset
        } else {
            0
        };
        for bit in bit_start..8 {
            let abs = byte_idx * 8 + bit;
            if abs >= nbits {
                return None;
            }
            if (byte & (1 << bit)) == 0 {
                return Some(abs);
            }
        }
    }
    None
}
