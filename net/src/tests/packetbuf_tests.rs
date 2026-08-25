//! Tests for PacketBuf and PacketPool.

use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::packetbuf::{HEADROOM, PacketBuf};
use crate::pool::{PACKET_POOL, PacketPool};
use crate::types::{Ipv4Addr, NetError};

/// Ten is the most any one test holds at once; the rest is margin.
const TEST_POOL_SLOTS: usize = 16;

/// An exact free-count assertion needs a pool the live stack cannot reach; one
/// shared pool, because every `PacketPool` is another lockdep class.
pub static TEST_PACKET_POOL: PacketPool =
    PacketPool::new(lock_class!("TEST_PACKET_POOL", LOCK_LEVEL_RESOURCE));

pub fn ensure_test_pool() {
    TEST_PACKET_POOL.init_with_slots(TEST_POOL_SLOTS);
}

fn ensure_pool_init() {
    PACKET_POOL.init();
}

pub fn test_pool_alloc_and_release() -> TestResult {
    ensure_test_pool();

    let initial = TEST_PACKET_POOL.available();
    assert_test!(initial > 0, "pool should have free slots after init");

    let slot = match TEST_PACKET_POOL.alloc() {
        Some(s) => s,
        None => return slopos_testing::fail!("alloc should succeed"),
    };
    assert_eq_test!(
        TEST_PACKET_POOL.available(),
        initial - 1,
        "available decreases by 1 after alloc"
    );

    TEST_PACKET_POOL.release(slot);
    assert_eq_test!(
        TEST_PACKET_POOL.available(),
        initial,
        "available restored after release"
    );

    let slot2 = match TEST_PACKET_POOL.alloc() {
        Some(s) => s,
        None => return slopos_testing::fail!("alloc should succeed after release"),
    };
    TEST_PACKET_POOL.release(slot2);

    pass!()
}

pub fn test_pool_exhaust_and_recover() -> TestResult {
    ensure_test_pool();

    let initial = TEST_PACKET_POOL.available();
    assert_test!(initial > 0, "pool should have free slots after init");

    let mut slots = [0u16; TEST_POOL_SLOTS];
    let mut allocated = 0usize;

    for slot in &mut slots {
        match TEST_PACKET_POOL.alloc() {
            Some(s) => {
                *slot = s;
                allocated += 1;
            }
            None => break,
        }
    }

    assert_eq_test!(allocated, initial, "the drain took every free slot");
    assert_eq_test!(TEST_PACKET_POOL.available(), 0, "pool should be exhausted");

    assert_test!(
        TEST_PACKET_POOL.alloc().is_none(),
        "alloc on exhausted pool returns None"
    );

    for i in 0..allocated {
        TEST_PACKET_POOL.release(slots[i]);
    }
    assert_eq_test!(
        TEST_PACKET_POOL.available(),
        initial,
        "pool recovers after releasing all slots"
    );

    pass!()
}

pub fn test_packetbuf_alloc_empty() -> TestResult {
    ensure_pool_init();

    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_testing::fail!("PacketBuf::alloc should succeed"),
    };

    assert_eq_test!(pkt.len(), 0, "freshly allocated PacketBuf has len 0");
    assert_test!(pkt.is_empty(), "freshly allocated PacketBuf is empty");
    assert_test!(pkt.payload().is_empty(), "payload is empty");
    assert_eq_test!(
        pkt.head(),
        HEADROOM,
        "head starts at HEADROOM for TX buffers"
    );

    assert_test!(
        pkt.head() >= HEADROOM,
        "at least HEADROOM bytes of headroom available"
    );

    pass!()
}

pub fn test_push_header() -> TestResult {
    ensure_pool_init();

    let mut pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_testing::fail!("alloc failed"),
    };

    let eth = match pkt.push_header(14) {
        Ok(slice) => {
            assert_eq_test!(slice.len(), 14, "push_header returns 14 bytes");
            for (i, byte) in slice.iter_mut().enumerate() {
                *byte = i as u8;
            }
            true
        }
        Err(_) => return slopos_testing::fail!("push_header(14) should succeed"),
    };
    assert_test!(eth, "push_header succeeded");

    assert_eq_test!(pkt.len(), 14, "len is 14 after pushing 14-byte header");
    assert_eq_test!(pkt.head(), HEADROOM - 14, "head moved backward by 14");

    let data = pkt.payload();
    assert_eq_test!(data.len(), 14, "payload length matches");
    assert_eq_test!(data[0], 0, "first byte correct");
    assert_eq_test!(data[13], 13, "last byte correct");

    pass!()
}

pub fn test_pull_header() -> TestResult {
    ensure_pool_init();

    let raw = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22, 0x33, 0x44];
    let mut pkt = match PacketBuf::from_raw_copy(&raw) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy failed"),
    };

    assert_eq_test!(pkt.len(), 10, "initial len is 10");

    {
        let hdr = match pkt.pull_header(4) {
            Ok(h) => h,
            Err(_) => return slopos_testing::fail!("pull_header(4) should succeed"),
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
        None => return slopos_testing::fail!("alloc failed"),
    };

    assert_test!(
        pkt.push_header(HEADROOM as usize).is_ok(),
        "push_header(HEADROOM) should succeed"
    );

    match pkt.push_header(1) {
        Err(NetError::NoBufferSpace) => {}
        _ => {
            return slopos_testing::fail!(
                "push_header beyond headroom should return NoBufferSpace"
            );
        }
    }

    pass!()
}

pub fn test_from_raw_copy() -> TestResult {
    ensure_pool_init();

    let raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];
    let pkt = match PacketBuf::from_raw_copy(&raw) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    assert_eq_test!(pkt.len(), 14, "payload length matches raw data");
    assert_eq_test!(pkt.head(), 0, "head is 0 for RX packets");
    assert_eq_test!(pkt.tail(), 14, "tail equals data length");

    let payload = pkt.payload();
    for i in 0..14 {
        assert_eq_test!(payload[i], (i + 1) as u8, "byte content matches");
    }

    assert_eq_test!(pkt.l2_offset(), 0, "l2_offset starts at 0");
    assert_eq_test!(pkt.l3_offset(), 0, "l3_offset starts at 0");
    assert_eq_test!(pkt.l4_offset(), 0, "l4_offset starts at 0");

    pass!()
}

pub fn test_from_raw_copy_empty() -> TestResult {
    ensure_pool_init();

    let pkt = match PacketBuf::from_raw_copy(&[]) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy empty should succeed"),
    };
    assert_eq_test!(pkt.len(), 0, "empty raw copy has len 0");
    assert_test!(pkt.is_empty(), "empty raw copy is_empty");

    pass!()
}

pub fn test_drop_returns_to_pool() -> TestResult {
    ensure_test_pool();

    let before = TEST_PACKET_POOL.available();

    {
        let _pkt = match PacketBuf::alloc_in(&TEST_PACKET_POOL) {
            Some(p) => p,
            None => return slopos_testing::fail!("alloc failed"),
        };
        assert_eq_test!(
            TEST_PACKET_POOL.available(),
            before - 1,
            "available decreased while PacketBuf alive"
        );
    }

    assert_eq_test!(
        TEST_PACKET_POOL.available(),
        before,
        "available restored after PacketBuf dropped"
    );

    pass!()
}

pub fn test_drop_multiple() -> TestResult {
    ensure_test_pool();

    let before = TEST_PACKET_POOL.available();
    assert_test!(before >= 3, "pool has room for the 3 buffers this takes");

    {
        let (Some(_p1), Some(_p2), Some(_p3)) = (
            PacketBuf::alloc_in(&TEST_PACKET_POOL),
            PacketBuf::alloc_in(&TEST_PACKET_POOL),
            PacketBuf::alloc_in(&TEST_PACKET_POOL),
        ) else {
            return slopos_testing::fail!("alloc failed");
        };
        assert_eq_test!(
            TEST_PACKET_POOL.available(),
            before - 3,
            "3 buffers allocated"
        );
    }

    assert_eq_test!(
        TEST_PACKET_POOL.available(),
        before,
        "all 3 slots returned after drop"
    );

    pass!()
}

pub fn test_layer_offsets() -> TestResult {
    ensure_pool_init();

    // Ethernet: 14 bytes, IP: 20 bytes, UDP: 8+payload
    let mut raw = [0u8; 50];
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
        None => return slopos_testing::fail!("from_raw_copy failed"),
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
        None => return slopos_testing::fail!("from_raw_copy failed"),
    };

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

pub fn test_append() -> TestResult {
    ensure_pool_init();

    let mut pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_testing::fail!("alloc failed"),
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

pub fn test_ipv4_checksum() -> TestResult {
    ensure_pool_init();

    #[rustfmt::skip]
    let ip_header: [u8; 20] = [
        0x45, 0x00, 0x00, 0x14,  // ver/ihl, dscp/ecn, total_len
        0x00, 0x00, 0x00, 0x00,  // id, flags/frag
        0x40, 0x11, 0x00, 0x00,  // ttl=64, proto=17(UDP), checksum=0
        0x0A, 0x00, 0x02, 0x0F,  // src=10.0.2.15
        0x0A, 0x00, 0x02, 0x01,  // dst=10.0.2.1
    ];

    let mut frame = [0u8; 34];
    frame[14..34].copy_from_slice(&ip_header);
    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_ipv4_checksum();
    assert_test!(csum != 0, "checksum should be non-zero");

    let l3_off = pkt.l3_offset() as usize;
    pkt.payload_mut()[l3_off + 10] = (csum >> 8) as u8;
    pkt.payload_mut()[l3_off + 11] = (csum & 0xFF) as u8;

    let hdr = &pkt.payload()[l3_off..l3_off + 20];
    let verify = crate::checksum::internet_checksum(hdr);
    assert_eq_test!(verify, 0, "checksum verifies to 0");

    pass!()
}

pub fn test_udp_checksum() -> TestResult {
    ensure_pool_init();

    let src = Ipv4Addr([10, 0, 2, 15]);
    let dst = Ipv4Addr([10, 0, 2, 1]);

    #[rustfmt::skip]
    let udp_header: [u8; 8] = [
        0x04, 0xD2, // src_port = 1234
        0x00, 0x35, // dst_port = 53
        0x00, 0x0D, // length = 13 (8 + 5)
        0x00, 0x00, // checksum = 0
    ];
    let payload = b"Hello";

    let mut frame = [0u8; 47];
    frame[34..42].copy_from_slice(&udp_header);
    frame[42..47].copy_from_slice(payload);

    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_udp_checksum(src, dst);
    assert_test!(csum != 0, "UDP checksum should be non-zero");

    let expected = crate::checksum::udp_checksum(src.0, dst.0, 1234, 53, payload);
    assert_eq_test!(csum, expected, "matches existing udp_checksum function");

    pass!()
}

pub fn test_tcp_checksum() -> TestResult {
    ensure_pool_init();

    let src = Ipv4Addr([192, 168, 1, 100]);
    let dst = Ipv4Addr([93, 184, 216, 34]);

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

    let mut frame = [0u8; 54];
    frame[34..54].copy_from_slice(&tcp_header);

    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy failed"),
    };
    pkt.set_l2(0);
    pkt.set_l3(14);
    pkt.set_l4(34);

    let csum = pkt.compute_tcp_checksum(src, dst);
    assert_test!(csum != 0, "TCP checksum should be non-zero");

    let expected = crate::tcp::tcp_checksum(src.0, dst.0, &tcp_header);
    assert_eq_test!(csum, expected, "matches existing tcp_checksum function");

    pass!()
}

slopos_testing::stest!(name = test_pool_alloc_and_release, suite = packetbuf);
slopos_testing::stest!(name = test_pool_exhaust_and_recover, suite = packetbuf);
slopos_testing::stest!(name = test_packetbuf_alloc_empty, suite = packetbuf);
slopos_testing::stest!(name = test_push_header, suite = packetbuf);
slopos_testing::stest!(name = test_pull_header, suite = packetbuf);
slopos_testing::stest!(name = test_push_header_exhausts_headroom, suite = packetbuf);
slopos_testing::stest!(name = test_from_raw_copy, suite = packetbuf);
slopos_testing::stest!(name = test_from_raw_copy_empty, suite = packetbuf);
slopos_testing::stest!(name = test_drop_returns_to_pool, suite = packetbuf);
slopos_testing::stest!(name = test_drop_multiple, suite = packetbuf);
slopos_testing::stest!(name = test_layer_offsets, suite = packetbuf);
slopos_testing::stest!(name = test_layer_offsets_unset, suite = packetbuf);
slopos_testing::stest!(name = test_append, suite = packetbuf);
slopos_testing::stest!(name = test_ipv4_checksum, suite = packetbuf);
slopos_testing::stest!(name = test_udp_checksum, suite = packetbuf);
slopos_testing::stest!(name = test_tcp_checksum, suite = packetbuf);
