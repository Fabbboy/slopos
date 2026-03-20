//! Tests for PacketBuf and PacketPool.
//!
//! Covers: pool alloc/release lifecycle, PacketBuf constructors, push/pull
//! header operations, from_raw_copy, drop-returns-to-pool, layer offset
//! helpers, and checksum computation.

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::packetbuf::{HEADROOM, PacketBuf};
use crate::pool::{PACKET_POOL, POOL_SIZE};
use crate::types::{Ipv4Addr, NetError};

/// Ensure the global pool is initialized before each test.
/// Idempotent — safe to call multiple times.
fn ensure_pool_init() {
    PACKET_POOL.init();
}

// =============================================================================
// 1.T1 — PacketPool::alloc + release
// =============================================================================

pub fn test_pool_alloc_and_release() -> TestResult {
    ensure_pool_init();

    let initial = PACKET_POOL.available();
    assert_test!(initial > 0, "pool should have free slots after init");

    // Allocate one slot.
    let slot = match PACKET_POOL.alloc() {
        Some(s) => s,
        None => return slopos_lib::fail!("alloc should succeed"),
    };
    assert_eq_test!(
        PACKET_POOL.available(),
        initial - 1,
        "available decreases by 1 after alloc"
    );

    // Release it.
    PACKET_POOL.release(slot);
    assert_eq_test!(
        PACKET_POOL.available(),
        initial,
        "available restored after release"
    );

    // Alloc again — should still succeed.
    let slot2 = match PACKET_POOL.alloc() {
        Some(s) => s,
        None => return slopos_lib::fail!("alloc should succeed after release"),
    };
    PACKET_POOL.release(slot2);

    pass!()
}

pub fn test_pool_exhaust_and_recover() -> TestResult {
    ensure_pool_init();

    // Drain the pool completely.
    let mut slots = [0u16; POOL_SIZE];
    let mut allocated = 0usize;

    for slot in &mut slots {
        match PACKET_POOL.alloc() {
            Some(s) => {
                *slot = s;
                allocated += 1;
            }
            None => break,
        }
    }

    assert_test!(allocated > 0, "should allocate at least one slot");
    assert_eq_test!(PACKET_POOL.available(), 0, "pool should be exhausted");

    // Next alloc must fail.
    assert_test!(
        PACKET_POOL.alloc().is_none(),
        "alloc on exhausted pool returns None"
    );

    // Release all — pool recovers.
    for i in 0..allocated {
        PACKET_POOL.release(slots[i]);
    }
    assert_eq_test!(
        PACKET_POOL.available(),
        allocated,
        "pool recovers after releasing all slots"
    );

    pass!()
}

// =============================================================================
// 1.T2 — PacketBuf::alloc
// =============================================================================

pub fn test_packetbuf_alloc_empty() -> TestResult {
    ensure_pool_init();

    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_lib::fail!("PacketBuf::alloc should succeed"),
    };

    assert_eq_test!(pkt.len(), 0, "freshly allocated PacketBuf has len 0");
    assert_test!(pkt.is_empty(), "freshly allocated PacketBuf is empty");
    assert_test!(pkt.payload().is_empty(), "payload is empty");
    assert_eq_test!(
        pkt.head(),
        HEADROOM,
        "head starts at HEADROOM for TX buffers"
    );

    // Headroom is accessible: we can push at least HEADROOM bytes of headers.
    assert_test!(
        pkt.head() >= HEADROOM,
        "at least HEADROOM bytes of headroom available"
    );

    pass!()
}

// =============================================================================
// 1.T3 — push_header / pull_header
// =============================================================================

pub fn test_push_header() -> TestResult {
    ensure_pool_init();

    let mut pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_lib::fail!("alloc failed"),
    };

    // Push a 14-byte Ethernet header.
    let eth = match pkt.push_header(14) {
        Ok(slice) => {
            assert_eq_test!(slice.len(), 14, "push_header returns 14 bytes");
            // Fill with a pattern.
            for (i, byte) in slice.iter_mut().enumerate() {
                *byte = i as u8;
            }
            true
        }
        Err(_) => return slopos_lib::fail!("push_header(14) should succeed"),
    };
    assert_test!(eth, "push_header succeeded");

    assert_eq_test!(pkt.len(), 14, "len is 14 after pushing 14-byte header");
    assert_eq_test!(pkt.head(), HEADROOM - 14, "head moved backward by 14");

    // Verify the pushed data is in the payload.
    let data = pkt.payload();
    assert_eq_test!(data.len(), 14, "payload length matches");
    assert_eq_test!(data[0], 0, "first byte correct");
    assert_eq_test!(data[13], 13, "last byte correct");

    pass!()
}

pub fn test_pull_header() -> TestResult {
    ensure_pool_init();

    // Build a packet with some data via from_raw_copy.
    let raw = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22, 0x33, 0x44];
    let mut pkt = match PacketBuf::from_raw_copy(&raw) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };

    assert_eq_test!(pkt.len(), 10, "initial len is 10");

    // Pull first 4 bytes.
    {
        let hdr = match pkt.pull_header(4) {
            Ok(h) => h,
            Err(_) => return slopos_lib::fail!("pull_header(4) should succeed"),
        };
        assert_eq_test!(hdr.len(), 4, "pulled 4 bytes");
        assert_eq_test!(hdr[0], 0xAA, "first pulled byte");
        assert_eq_test!(hdr[3], 0xDD, "fourth pulled byte");
    }

    assert_eq_test!(pkt.len(), 6, "len reduced to 6 after pulling 4");
    assert_eq_test!(
        pkt.payload()[0],
        0xEE,
        "payload starts at 5th original byte"
    );

    // Pull too many bytes — should fail.
    assert_test!(
        pkt.pull_header(100).is_err(),
        "pull_header beyond len should fail"
    );

    pass!()
}

pub fn test_push_header_exhausts_headroom() -> TestResult {
    ensure_pool_init();

    let mut pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_lib::fail!("alloc failed"),
    };

    // Push exactly HEADROOM bytes — should succeed.
    assert_test!(
        pkt.push_header(HEADROOM as usize).is_ok(),
        "push_header(HEADROOM) should succeed"
    );

    // Push 1 more byte — should fail (no headroom left).
    match pkt.push_header(1) {
        Err(NetError::NoBufferSpace) => {}
        _ => return slopos_lib::fail!("push_header beyond headroom should return NoBufferSpace"),
    }

    pass!()
}

// =============================================================================
// 1.T4 — PacketBuf::from_raw_copy
// =============================================================================

pub fn test_from_raw_copy() -> TestResult {
    ensure_pool_init();

    let raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
    let pkt = match PacketBuf::from_raw_copy(&raw) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy should succeed"),
    };

    assert_eq_test!(pkt.len(), 14, "payload length matches raw data");
    assert_eq_test!(pkt.head(), 0, "head is 0 for RX packets");
    assert_eq_test!(pkt.tail(), 14, "tail equals data length");

    let payload = pkt.payload();
    for i in 0..14 {
        assert_eq_test!(payload[i], (i + 1) as u8, "byte content matches");
    }

    // Layer offsets are initially zero.
    assert_eq_test!(pkt.l2_offset(), 0, "l2_offset starts at 0");
    assert_eq_test!(pkt.l3_offset(), 0, "l3_offset starts at 0");
    assert_eq_test!(pkt.l4_offset(), 0, "l4_offset starts at 0");

    pass!()
}

pub fn test_from_raw_copy_empty() -> TestResult {
    ensure_pool_init();

    let pkt = match PacketBuf::from_raw_copy(&[]) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy empty should succeed"),
    };
    assert_eq_test!(pkt.len(), 0, "empty raw copy has len 0");
    assert_test!(pkt.is_empty(), "empty raw copy is_empty");

    pass!()
}

// =============================================================================
// 1.T5 — PacketBuf drop returns slot to pool
// =============================================================================

pub fn test_drop_returns_to_pool() -> TestResult {
    ensure_pool_init();

    let before = PACKET_POOL.available();

    // Allocate inside a block so it drops at block end.
    {
        let _pkt = match PacketBuf::alloc() {
            Some(p) => p,
            None => return slopos_lib::fail!("alloc failed"),
        };
        assert_eq_test!(
            PACKET_POOL.available(),
            before - 1,
            "available decreased while PacketBuf alive"
        );
    }
    // _pkt dropped here.

    assert_eq_test!(
        PACKET_POOL.available(),
        before,
        "available restored after PacketBuf dropped"
    );

    pass!()
}

pub fn test_drop_multiple() -> TestResult {
    ensure_pool_init();

    let before = PACKET_POOL.available();

    {
        let _p1 = PacketBuf::alloc();
        let _p2 = PacketBuf::alloc();
        let _p3 = PacketBuf::alloc();
        assert_test!(PACKET_POOL.available() <= before - 3, "3 buffers allocated");
    }

    assert_eq_test!(
        PACKET_POOL.available(),
        before,
        "all 3 slots returned after drop"
    );

    pass!()
}

// =============================================================================
// Layer offset helpers
// =============================================================================

pub fn test_layer_offsets() -> TestResult {
    ensure_pool_init();

    // Simulate an RX Ethernet+IP+UDP packet.
    // Ethernet: 14 bytes, IP: 20 bytes, UDP: 8+payload
    let mut raw = [0u8; 50];
    // Fill with recognizable pattern per layer.
    for i in 0..14 {
        raw[i] = 0xE0 | (i as u8); // "Ethernet" marker
    }
    for i in 14..34 {
        raw[i] = 0x40 | ((i - 14) as u8); // "IPv4" marker
    }
    for i in 34..50 {
        raw[i] = 0xD0 | ((i - 34) as u8); // "UDP" marker
    }

    let mut pkt = match PacketBuf::from_raw_copy(&raw) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };

    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let l2 = pkt.l2_header();
    assert_eq_test!(l2.len(), 14, "l2_header is 14 bytes");
    assert_eq_test!(l2[0], 0xE0, "l2 first byte");
    assert_eq_test!(l2[13], 0xE0 | 13, "l2 last byte");

    let l3 = pkt.l3_header();
    assert_eq_test!(l3.len(), 20, "l3_header is 20 bytes");
    assert_eq_test!(l3[0], 0x40, "l3 first byte");

    let l4 = pkt.l4_header();
    assert_eq_test!(l4.len(), 16, "l4_header is 16 bytes");
    assert_eq_test!(l4[0], 0xD0, "l4 first byte");

    pass!()
}

pub fn test_layer_offsets_unset() -> TestResult {
    ensure_pool_init();

    let pkt = match PacketBuf::from_raw_copy(&[0u8; 100]) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };

    // No offsets set — all layer accessors return empty.
    assert_test!(
        pkt.l2_header().is_empty(),
        "l2_header empty when l3 not set"
    );
    assert_test!(
        pkt.l3_header().is_empty(),
        "l3_header empty when offsets not set"
    );
    assert_test!(
        pkt.l4_header().is_empty(),
        "l4_header empty when l4 not set"
    );

    pass!()
}

// =============================================================================
// Append
// =============================================================================

pub fn test_append() -> TestResult {
    ensure_pool_init();

    let mut pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_lib::fail!("alloc failed"),
    };

    let payload = b"Hello, SlopOS!";
    assert_test!(pkt.append(payload).is_ok(), "append should succeed");
    assert_eq_test!(pkt.len(), payload.len(), "len matches appended data");

    let data = pkt.payload();
    for i in 0..payload.len() {
        assert_eq_test!(data[i], payload[i], "appended byte matches");
    }

    pass!()
}

// =============================================================================
// Checksum helpers
// =============================================================================

pub fn test_ipv4_checksum() -> TestResult {
    ensure_pool_init();

    // Build a minimal IPv4 header (20 bytes) inside a PacketBuf.
    // Version=4, IHL=5, Total Length=20, TTL=64, Protocol=UDP(17)
    // Src=10.0.2.15, Dst=10.0.2.1, Checksum=0 (to be computed)
    #[rustfmt::skip]
    let ip_header: [u8; 20] = [
        0x45, 0x00, 0x00, 0x14,  // ver/ihl, dscp/ecn, total_len
        0x00, 0x00, 0x00, 0x00,  // id, flags/frag
        0x40, 0x11, 0x00, 0x00,  // ttl=64, proto=17(UDP), checksum=0
        0x0A, 0x00, 0x02, 0x0F,  // src=10.0.2.15
        0x0A, 0x00, 0x02, 0x01,  // dst=10.0.2.1
    ];

    // Build frame: 14-byte eth stub + 20-byte IP header
    let mut frame = [0u8; 34];
    frame[14..34].copy_from_slice(&ip_header);
    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_ipv4_checksum();
    assert_test!(csum != 0, "checksum should be non-zero");

    // Verify: setting the computed checksum and recomputing should give 0.
    // Patch the checksum into the header.
    let l3_off = pkt.l3_offset() as usize;
    pkt.payload_mut()[l3_off + 10] = (csum >> 8) as u8;
    pkt.payload_mut()[l3_off + 11] = (csum & 0xFF) as u8;

    // Recompute over the whole header (including the checksum field).
    // Use the standalone ipv4_header_checksum from mod.rs for verification.
    let hdr = &pkt.payload()[l3_off..l3_off + 20];
    let verify = crate::checksum::internet_checksum(hdr);
    assert_eq_test!(verify, 0, "checksum verifies to 0");

    pass!()
}

pub fn test_udp_checksum() -> TestResult {
    ensure_pool_init();

    let src = Ipv4Addr([10, 0, 2, 15]);
    let dst = Ipv4Addr([10, 0, 2, 1]);

    // Build a UDP datagram: 8-byte header + 5-byte payload "Hello"
    #[rustfmt::skip]
    let udp_header: [u8; 8] = [
        0x04, 0xD2, // src_port = 1234
        0x00, 0x35, // dst_port = 53
        0x00, 0x0D, // length = 13 (8 + 5)
        0x00, 0x00, // checksum = 0
    ];
    let payload = b"Hello";

    // Build frame: 14-byte eth stub + 20-byte IP stub + 8 UDP header + 5 payload
    let mut frame = [0u8; 47];
    frame[34..42].copy_from_slice(&udp_header);
    frame[42..47].copy_from_slice(payload);

    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_udp_checksum(src, dst);
    assert_test!(csum != 0, "UDP checksum should be non-zero");

    // Cross-check with the existing udp_checksum function from mod.rs.
    let expected = crate::checksum::udp_checksum(src.0, dst.0, 1234, 53, payload);
    assert_eq_test!(csum, expected, "matches existing udp_checksum function");

    pass!()
}

pub fn test_tcp_checksum() -> TestResult {
    ensure_pool_init();

    let src = Ipv4Addr([192, 168, 1, 100]);
    let dst = Ipv4Addr([93, 184, 216, 34]);

    // Build a minimal TCP SYN segment (20 bytes, no options, no payload).
    #[rustfmt::skip]
    let tcp_header: [u8; 20] = [
        0xC0, 0x00, // src_port = 49152
        0x00, 0x50, // dst_port = 80
        0x00, 0x00, 0x00, 0x01, // seq = 1
        0x00, 0x00, 0x00, 0x00, // ack = 0
        0x50, 0x02, // data_offset=5, SYN flag
        0xFF, 0xFF, // window = 65535
        0x00, 0x00, // checksum = 0
        0x00, 0x00, // urgent_ptr = 0
    ];

    // Build frame: 14 eth + 20 IP + 20 TCP
    let mut frame = [0u8; 54];
    frame[34..54].copy_from_slice(&tcp_header);

    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_lib::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_tcp_checksum(src, dst);
    assert_test!(csum != 0, "TCP checksum should be non-zero");

    // Cross-check with the existing tcp_checksum function.
    let expected = crate::tcp::tcp_checksum(src.0, dst.0, &tcp_header);
    assert_eq_test!(csum, expected, "matches existing tcp_checksum function");

    pass!()
}

// =============================================================================
// Test suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    packetbuf,
    [
        // 1.T1 — Pool alloc + release
        test_pool_alloc_and_release,
        test_pool_exhaust_and_recover,
        // 1.T2 — PacketBuf::alloc
        test_packetbuf_alloc_empty,
        // 1.T3 — push_header / pull_header
        test_push_header,
        test_pull_header,
        test_push_header_exhausts_headroom,
        // 1.T4 — from_raw_copy
        test_from_raw_copy,
        test_from_raw_copy_empty,
        // 1.T5 — Drop returns to pool
        test_drop_returns_to_pool,
        test_drop_multiple,
        // Layer offset helpers
        test_layer_offsets,
        test_layer_offsets_unset,
        // Append
        test_append,
        // Checksum helpers
        test_ipv4_checksum,
        test_udp_checksum,
        test_tcp_checksum,
    ]
);
