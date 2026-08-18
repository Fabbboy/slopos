//! [`crate::checksum`] specialized to TCP's protocol number and pseudo-header
//! format, kept out of [`super::header`] so header parsing stays arithmetic-free.

use crate::checksum;

const IPV4_PROTO_TCP: u8 = 6;

/// Compute the TCP checksum over an IPv4 segment, including the RFC 793
/// pseudo-header derived from `src_ip`/`dst_ip` and the segment length.
///
/// The segment's own checksum field (bytes 16..18) must already be zeroed.
pub fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> u16 {
    let mut sum = 0u32;
    checksum::add_pseudo_header(&mut sum, src_ip, dst_ip, IPV4_PROTO_TCP, tcp_segment.len());
    sum = sum.wrapping_add(checksum::ones_complement_sum(tcp_segment));
    checksum::fold(sum)
}

/// Returns `true` if the pseudo-header plus the segment — with its checksum
/// field left in place — folds to zero.
pub fn verify_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> bool {
    let mut sum = 0u32;
    checksum::add_pseudo_header(&mut sum, src_ip, dst_ip, IPV4_PROTO_TCP, tcp_segment.len());
    sum = sum.wrapping_add(checksum::ones_complement_sum(tcp_segment));
    checksum::fold(sum) == 0
}
