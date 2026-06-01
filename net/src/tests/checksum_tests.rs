//! Checksum-offload seed tests for the zero-copy (`OP_SEND_ZC`) send path.

use slopos_testing::{TestResult, stest};

/// The virtio `NEEDS_CSUM` pseudo-header seed is correct: a device that sums the
/// frame from `csum_start` (the UDP header — including the seeded checksum field
/// — plus the DMA'd payload) and complements the fold must reproduce the full
/// software UDP checksum. We seed the field with `pseudo_header_seed`, run the
/// plain internet checksum over `[udp header || payload]` (the device's job),
/// and require it to equal `udp_checksum` (the all-software result).
fn udp_csum_offload_seed_matches_full() -> TestResult {
    use crate::checksum::{internet_checksum, pseudo_header_seed, udp_checksum};
    let src = [10u8, 0, 2, 15];
    let dst = [10u8, 0, 2, 2];
    let sport = 12345u16;
    let dport = 9999u16;
    let payload: &[u8] = b"zero-copy-checksum-offload-seed";
    let udp_len = 8 + payload.len();

    // UDP header with the pseudo-header partial sum seeded into the csum field.
    let mut frame = [0u8; 8 + 64];
    frame[0..2].copy_from_slice(&sport.to_be_bytes());
    frame[2..4].copy_from_slice(&dport.to_be_bytes());
    frame[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    let seed = pseudo_header_seed(src, dst, crate::types::IpProtocol::Udp.as_u8(), udp_len);
    frame[6..8].copy_from_slice(&seed.to_be_bytes());
    frame[8..8 + payload.len()].copy_from_slice(payload);

    // The device sums [udp header (seeded) || payload] and complements.
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
