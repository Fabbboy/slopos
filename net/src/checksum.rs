//! Internet checksum (RFC 1071) primitives for the network subsystem.
//!
//! All checksum computation flows through this module — IPv4 headers, ICMP,
//! TCP, and UDP use the same underlying one's-complement sum with different
//! inputs.

/// Accumulate the one's-complement sum over a byte slice (RFC 1071).
///
/// Returns a 32-bit accumulator; fold via [`fold`] after combining all
/// data regions.  Pads an odd trailing byte with zero per RFC 1071 §4.1.
#[inline]
pub(crate) fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        sum = sum.wrapping_add(word);
        i += 2;
    }
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }
    sum
}

/// Fold a 32-bit accumulator into a 16-bit one's-complement checksum.
#[inline]
pub(crate) fn fold(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Accumulate the IPv4 pseudo-header (RFC 793 §3.1) into `sum`.
///
/// Used by TCP and UDP checksum computation.  The pseudo-header consists of:
/// source IP (4), destination IP (4), zero (1), protocol (1), L4 length (2).
#[inline]
pub(crate) fn add_pseudo_header(
    sum: &mut u32,
    src: [u8; 4],
    dst: [u8; 4],
    protocol: u8,
    l4_len: usize,
) {
    *sum = sum.wrapping_add(u16::from_be_bytes([src[0], src[1]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([src[2], src[3]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([dst[0], dst[1]]) as u32);
    *sum = sum.wrapping_add(u16::from_be_bytes([dst[2], dst[3]]) as u32);
    *sum = sum.wrapping_add(protocol as u32);
    *sum = sum.wrapping_add(l4_len as u32);
}

/// Compute the standard internet checksum (RFC 1071) over a contiguous
/// byte slice.
///
/// Suitable for IPv4 headers, ICMP messages, and any other protocol that
/// uses the one's-complement checksum.
pub fn internet_checksum(data: &[u8]) -> u16 {
    fold(ones_complement_sum(data))
}

/// Compute the UDP checksum from discrete fields (standalone version).
///
/// This is the flat-buffer counterpart of [`PacketBuf::compute_udp_checksum`]
/// for callers that construct frames outside of the `PacketBuf` pipeline
/// (e.g., the VirtIO-net driver's early-boot DHCP path).
///
/// Per RFC 768, a computed checksum of zero is transmitted as `0xFFFF`.
pub fn udp_checksum(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> u16 {
    let udp_len = 8 + payload.len();
    let mut sum = 0u32;

    add_pseudo_header(&mut sum, src_ip, dst_ip, super::IPPROTO_UDP, udp_len);

    sum = sum.wrapping_add(src_port as u32);
    sum = sum.wrapping_add(dst_port as u32);
    sum = sum.wrapping_add(udp_len as u32);

    sum = sum.wrapping_add(ones_complement_sum(payload));

    let csum = fold(sum);
    if csum == 0 { 0xFFFF } else { csum }
}
