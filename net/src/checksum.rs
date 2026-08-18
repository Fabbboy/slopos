//! Internet checksum (RFC 1071) primitives shared by IPv4, ICMP, TCP and UDP.

/// Accumulate the one's-complement sum over a byte slice (RFC 1071).
///
/// Returns a 32-bit accumulator; fold via [`fold`] after combining all data
/// regions.  Pads an odd trailing byte with zero per RFC 1071 §4.1.
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
/// A virtio `NEEDS_CSUM` device sums the frame from `csum_start` — which
/// includes the L4 checksum field itself — and complements the fold, so that
/// field must be seeded with a non-complemented partial sum.
#[inline]
pub(crate) fn fold_partial(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

/// Non-complemented folded sum of the IPv4 pseudo-header (RFC 768/793),
/// written into the L4 checksum field before a `NEEDS_CSUM` zero-copy send so
/// the device can complete the checksum over the DMA'd payload.
pub(crate) fn pseudo_header_seed(src: [u8; 4], dst: [u8; 4], protocol: u8, l4_len: usize) -> u16 {
    let mut sum = 0u32;
    add_pseudo_header(&mut sum, src, dst, protocol, l4_len);
    fold_partial(sum)
}

/// One's-complement sum (RFC 1071) over `len` bytes pulled from a volatile
/// [`VmReader`] (pinned user pages), read in even-length chunks so no 16-bit
/// word straddles a chunk boundary. Returns the 32-bit accumulator; combine
/// with other regions then [`fold`]. Used by the ICMP zero-copy send, whose
/// checksum has no pseudo-header and so cannot be offloaded on QEMU virtio-net.
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
        // byte against this chunk's last.
        if got < want {
            break;
        }
    }
    sum
}

/// Accumulate the IPv4 pseudo-header (RFC 793 §3.1) into `sum`: source IP (4),
/// destination IP (4), zero (1), protocol (1), L4 length (2).
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
pub fn internet_checksum(data: &[u8]) -> u16 {
    fold(ones_complement_sum(data))
}

/// Compute the UDP checksum from discrete fields, for callers that build
/// frames outside the `PacketBuf` pipeline (the VirtIO-net early-boot DHCP
/// path).  Per RFC 768, a computed checksum of zero is transmitted as `0xFFFF`.
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
