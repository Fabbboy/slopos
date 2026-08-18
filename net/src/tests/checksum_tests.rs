//! Checksum-offload seed tests for the zero-copy (`OP_SEND_ZC`) send path.

use slopos_testing::{TestResult, stest};

/// A virtio device summing from `csum_start` over the seeded UDP header plus the
/// DMA'd payload must reproduce the all-software `udp_checksum`.
fn udp_csum_offload_seed_matches_full() -> TestResult {
    use crate::checksum::{internet_checksum, pseudo_header_seed, udp_checksum};
    let src = [10u8, 0, 2, 15];
    let dst = [10u8, 0, 2, 2];
    let sport = 12345u16;
    let dport = 9999u16;
    let payload: &[u8] = b"zero-copy-checksum-offload-seed";
    let udp_len = 8 + payload.len();

    let mut frame = [0u8; 8 + 64];
    frame[0..2].copy_from_slice(&sport.to_be_bytes());
    frame[2..4].copy_from_slice(&dport.to_be_bytes());
    frame[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    let seed = pseudo_header_seed(src, dst, crate::types::IpProtocol::Udp.as_u8(), udp_len);
    frame[6..8].copy_from_slice(&seed.to_be_bytes());
    frame[8..8 + payload.len()].copy_from_slice(payload);

    let device_csum = internet_checksum(&frame[..8 + payload.len()]);
    let full = udp_checksum(src, dst, sport, dport, payload);
    if device_csum == full {
        TestResult::Pass
    } else {
        slopos_testing::fail!(
            "csum-offload seed mismatch: device={:#06x} full={:#06x}",
            device_csum,
            full
        )
    }
}
stest!(name = udp_csum_offload_seed_matches_full, suite = checksum);

/// The same seed identity for TCP `MSG_ZEROCOPY` (`csum_start` = TCP header,
/// `csum_offset` = 16). Both header lengths are checked because the offloaded
/// span covers TCP options too.
fn tcp_csum_offload_seed_matches_full() -> TestResult {
    use crate::checksum::{internet_checksum, pseudo_header_seed};
    use crate::tcp::tcp_checksum;
    let src = [10u8, 0, 2, 15];
    let dst = [10u8, 0, 2, 2];
    let sport = 40000u16;
    let dport = 80u16;
    let payload: &[u8] = b"tcp-zero-copy-checksum-offload";

    for hdr_len in [20usize, 32usize] {
        let tcp_total = hdr_len + payload.len();
        let mut seg = [0u8; 32 + 64];
        seg[0..2].copy_from_slice(&sport.to_be_bytes());
        seg[2..4].copy_from_slice(&dport.to_be_bytes());
        seg[4..8].copy_from_slice(&1u32.to_be_bytes()); // seq
        seg[8..12].copy_from_slice(&0u32.to_be_bytes()); // ack
        seg[12] = ((hdr_len / 4) as u8) << 4; // data offset in 32-bit words
        seg[13] = 0x10; // ACK flag
        seg[14..16].copy_from_slice(&64240u16.to_be_bytes()); // window
        // csum field (16..18) stays zero for the all-software reference.
        seg[hdr_len..hdr_len + payload.len()].copy_from_slice(payload);

        let full = tcp_checksum(src, dst, &seg[..tcp_total]);

        let seed = pseudo_header_seed(src, dst, crate::types::IpProtocol::Tcp.as_u8(), tcp_total);
        seg[16..18].copy_from_slice(&seed.to_be_bytes());
        let device = internet_checksum(&seg[..tcp_total]);

        if device != full {
            return slopos_testing::fail!(
                "tcp csum-offload seed mismatch (hdr_len={}): device={:#06x} full={:#06x}",
                hdr_len,
                device,
                full
            );
        }
    }
    TestResult::Pass
}
stest!(name = tcp_csum_offload_seed_matches_full, suite = checksum);
