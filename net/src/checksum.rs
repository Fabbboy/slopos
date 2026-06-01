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

/// Fold a 32-bit accumulator to 16 bits **without** the final one's-complement.
///
/// This is the partial-checksum *seed* a virtio `NEEDS_CSUM` offload device
/// expects in the L4 checksum field: the device sums the frame from
/// `csum_start` (which includes this field) and complements the fold, so the
/// field must already hold the non-complemented pseudo-header partial sum.
#[inline]
pub(crate) fn fold_partial(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

/// The UDP/TCP checksum-offload **seed** for the IPv4 pseudo-header: the
/// non-complemented folded sum of (src, dst, proto, l4_len), written into the
/// L4 checksum field before a `NEEDS_CSUM` zero-copy send so the device can
/// complete the checksum over the DMA'd payload (RFC 768/793 pseudo-header).
pub(crate) fn pseudo_header_seed(src: [u8; 4], dst: [u8; 4], protocol: u8, l4_len: usize) -> u16 {
    let mut sum = 0u32;
    add_pseudo_header(&mut sum, src, dst, protocol, l4_len);
    fold_partial(sum)
}

/// One's-complement sum (RFC 1071) over `len` bytes pulled from a volatile
/// [`VmReader`] (pinned user pages), read in even-length chunks so no 16-bit
/// word straddles a chunk boundary. Returns the 32-bit accumulator; combine
/// with other regions then [`fold`]. Used by the ICMP zero-copy send, whose
/// checksum has no pseudo-header and so cannot be offloaded on QEMU virtio-net
/// — the payload is summed once (a single volatile read, no staging copy) while
/// the NIC still DMAs it straight from the pinned pages.
pub(crate) fn ones_complement_sum_reader(
    reader: &mut slopos_ostd::mm::VmReader<'_>,
    len: usize,
) -> u32 {
    let mut buf = [0u8; 512];
    let mut sum = 0u32;
    let mut left = len;
    while left > 0 {
        let want = left.min(buf.len());
        let got = reader.read(&mut buf[..want]);
        if got == 0 {
            break;
        }
        sum = sum.wrapping_add(ones_complement_sum(&buf[..got]));
        left -= got;
        // A short read of an odd count would mis-pair the next chunk's first
        // byte against this chunk's last; non-final chunks are even (512), so
        // stop on any short read (a single datagram reads fully in practice).
        if got < want {
            break;
        }
    }
    sum
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

    add_pseudo_header(
        &mut sum,
        src_ip,
        dst_ip,
        super::IpProtocol::Udp.as_u8(),
        udp_len,
    );

    sum = sum.wrapping_add(src_port as u32);
    sum = sum.wrapping_add(dst_port as u32);
    sum = sum.wrapping_add(udp_len as u32);

    sum = sum.wrapping_add(ones_complement_sum(payload));

    let csum = fold(sum);
    if csum == 0 { 0xFFFF } else { csum }
}
