//! TCP checksum helpers.
//!
//! Thin specialization over [`crate::checksum`] for TCP's protocol number and
//! pseudo-header format.  Separating this from [`super::header`] keeps header
//! parsing pure (no one's-complement arithmetic in the parser path) and lets
//! us swap in an incremental checksum implementation later without touching
//! the header code.

use crate::checksum;

/// IPv4 protocol number for TCP.
const IPV4_PROTO_TCP: u8 = 6;

/// Compute the TCP checksum over an IPv4 segment, including the RFC 793
/// pseudo-header derived from `src_ip`/`dst_ip` and the segment length.
///
/// The segment slice must have its own checksum field (bytes 16..18) already
/// zeroed; callers are expected to use [`super::header::write_header`] which
/// writes a zero placeholder.
pub fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> u16 {
    let mut sum = 0u32;
    checksum::add_pseudo_header(&mut sum, src_ip, dst_ip, IPV4_PROTO_TCP, tcp_segment.len());
    sum = sum.wrapping_add(checksum::ones_complement_sum(tcp_segment));
    checksum::fold(sum)
}

/// Verify a TCP segment's checksum.
///
/// Returns `true` if the one's-complement sum over the pseudo-header plus the
/// segment (with its checksum field in place) folds to zero, i.e. the
/// received segment is intact.
pub fn verify_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> bool {
    let mut sum = 0u32;
    checksum::add_pseudo_header(&mut sum, src_ip, dst_ip, IPV4_PROTO_TCP, tcp_segment.len());
    sum = sum.wrapping_add(checksum::ones_complement_sum(tcp_segment));
    checksum::fold(sum) == 0
}
