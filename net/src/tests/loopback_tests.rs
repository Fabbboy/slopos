//! Tests for the loopback device (tx/poll_rx delivery without VirtIO) and for
//! route-table population and replacement.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::loopback::LoopbackDev;
use crate::netdev::{NetDevice, NetDeviceFeatures};
use crate::packetbuf::PacketBuf;
use crate::route::RouteTable;
use crate::tests::packetbuf_tests::{TEST_PACKET_POOL, ensure_test_pool};
use crate::types::{DevIndex, Ipv4Addr};

fn dummy_packet(fill: u8) -> Option<PacketBuf> {
    let data = [fill; 64];
    PacketBuf::from_raw_copy_in(&TEST_PACKET_POOL, &data)
}

pub fn test_loopback_tx_then_poll_rx() -> TestResult {
    ensure_test_pool();

    let lo = LoopbackDev::new();

    let Some(pkt) = dummy_packet(0xAB) else {
        return slopos_testing::fail!("test pool should have capacity");
    };
    let result = lo.tx(pkt);
    assert_test!(result.is_ok(), "loopback tx should succeed");

    let received = lo.poll_rx(16, &TEST_PACKET_POOL);
    assert_eq_test!(received.len(), 1, "should receive 1 packet back");
    assert_eq_test!(
        received[0].payload()[0],
        0xAB,
        "received payload should match tx payload"
    );

    pass!()
}

pub fn test_loopback_multiple_tx_poll() -> TestResult {
    ensure_test_pool();

    let lo = LoopbackDev::new();

    for i in 0..5u8 {
        let Some(pkt) = dummy_packet(i) else {
            return slopos_testing::fail!("test pool should have capacity");
        };
        assert_test!(lo.tx(pkt).is_ok(), "tx should succeed");
    }

    let batch1 = lo.poll_rx(3, &TEST_PACKET_POOL);
    assert_eq_test!(batch1.len(), 3, "first poll should return 3 packets");
    assert_eq_test!(batch1[0].payload()[0], 0, "first packet fill=0");
    assert_eq_test!(batch1[1].payload()[0], 1, "second packet fill=1");
    assert_eq_test!(batch1[2].payload()[0], 2, "third packet fill=2");

    let batch2 = lo.poll_rx(16, &TEST_PACKET_POOL);
    assert_eq_test!(batch2.len(), 2, "second poll should return 2 packets");
    assert_eq_test!(batch2[0].payload()[0], 3, "fourth packet fill=3");
    assert_eq_test!(batch2[1].payload()[0], 4, "fifth packet fill=4");

    let batch3 = lo.poll_rx(16, &TEST_PACKET_POOL);
    assert_test!(batch3.is_empty(), "third poll should be empty");

    pass!()
}

pub fn test_loopback_stats() -> TestResult {
    ensure_test_pool();

    let lo = LoopbackDev::new();

    let stats = lo.stats();
    assert_eq_test!(stats.tx_packets, 0, "initial tx_packets = 0");
    assert_eq_test!(stats.rx_packets, 0, "initial rx_packets = 0");

    let (Some(first), Some(second)) = (dummy_packet(0xAA), dummy_packet(0xBB)) else {
        return slopos_testing::fail!("test pool should have capacity");
    };
    let _ = lo.tx(first);
    let _ = lo.tx(second);

    let stats_after_tx = lo.stats();
    assert_eq_test!(stats_after_tx.tx_packets, 2, "tx_packets = 2 after tx");
    assert_eq_test!(
        stats_after_tx.rx_packets,
        0,
        "rx_packets still 0 before poll"
    );

    let _ = lo.poll_rx(1, &TEST_PACKET_POOL);

    let stats_after_poll = lo.stats();
    assert_eq_test!(
        stats_after_poll.rx_packets,
        1,
        "rx_packets = 1 after polling 1"
    );

    pass!()
}

pub fn test_loopback_properties() -> TestResult {
    let lo = LoopbackDev::new();

    assert_eq_test!(lo.mtu(), 65535, "loopback mtu should be 65535");
    assert_eq_test!(lo.mac().0, [0; 6], "loopback mac should be zero");
    assert_test!(
        lo.features()
            .contains(NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX),
        "loopback should advertise CHECKSUM_TX | CHECKSUM_RX"
    );

    pass!()
}

pub fn test_loopback_queue_capacity() -> TestResult {
    ensure_test_pool();

    let lo = LoopbackDev::new();

    // The test pool holds fewer buffers than the device's 256-packet queue, so
    // this reaches only the within-capacity path.
    for _ in 0..10 {
        let Some(pkt) = dummy_packet(0xFF) else {
            return slopos_testing::fail!("test pool should have capacity");
        };
        let result = lo.tx(pkt);
        assert_test!(result.is_ok(), "should accept packets within capacity");
    }

    let stats_before = lo.stats();
    assert_eq_test!(stats_before.tx_packets, 10, "tx_packets = 10 after batch");
    assert_eq_test!(stats_before.tx_dropped, 0, "no drops within capacity");

    let drained = lo.poll_rx(256, &TEST_PACKET_POOL);
    assert_eq_test!(drained.len(), 10, "should drain all 10 packets");
    drop(drained);

    pass!()
}

pub fn test_configure_populates_route_table() -> TestResult {
    // Heap-allocate so the 33-bucket RouteTable does not materialise on the
    // test fn's stack.
    let rt: slopos_ostd::KBox<RouteTable> =
        slopos_ostd::KBox::try_init(RouteTable::init()).expect("alloc");

    // Mirrors what `iface_ctl::configure_ipv4` installs for a lease; a scratch
    // table keeps the assertions free of whatever the live boot configured.
    let dev = DevIndex(1);
    let addr = Ipv4Addr([10, 0, 0, 50]);
    let netmask = Ipv4Addr([255, 255, 255, 0]);
    let gateway = Ipv4Addr([10, 0, 0, 1]);

    let prefix_len = netmask.to_u32_be().leading_ones() as u8;
    let prefix = Ipv4Addr::from_u32_be(addr.to_u32_be() & netmask.to_u32_be());

    rt.add(crate::route::RouteEntry {
        prefix,
        prefix_len,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    });

    rt.add(crate::route::RouteEntry {
        prefix: Ipv4Addr::UNSPECIFIED,
        prefix_len: 0,
        gateway,
        dev,
        metric: 100,
    });

    assert_eq_test!(
        rt.route_count(),
        2,
        "should have 2 routes after configure simulation"
    );

    let r1 = rt.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(r1.is_some(), "local subnet address should match");
    let (r1_dev, r1_hop) = r1.unwrap();
    assert_eq_test!(r1_dev, DevIndex(1), "should route through dev 1");
    assert_eq_test!(r1_hop.0, [10, 0, 0, 42], "connected route: next_hop = dst");

    let r2 = rt.lookup(Ipv4Addr([8, 8, 8, 8]));
    assert_test!(r2.is_some(), "external address should match default route");
    let (r2_dev, r2_hop) = r2.unwrap();
    assert_eq_test!(r2_dev, DevIndex(1), "should route through dev 1");
    assert_eq_test!(r2_hop.0, [10, 0, 0, 1], "default route: next_hop = gateway");

    pass!()
}

pub fn test_reconfigure_replaces_routes() -> TestResult {
    let rt = RouteTable::new();
    let dev = DevIndex(1);

    rt.add(crate::route::RouteEntry {
        prefix: Ipv4Addr([10, 0, 0, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    });
    rt.add(crate::route::RouteEntry {
        prefix: Ipv4Addr::UNSPECIFIED,
        prefix_len: 0,
        gateway: Ipv4Addr([10, 0, 0, 1]),
        dev,
        metric: 100,
    });

    assert_eq_test!(rt.route_count(), 2, "2 routes after initial config");

    rt.remove_device_routes(dev);
    assert_eq_test!(rt.route_count(), 0, "0 routes after remove_device_routes");

    rt.add(crate::route::RouteEntry {
        prefix: Ipv4Addr([192, 168, 1, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    });
    rt.add(crate::route::RouteEntry {
        prefix: Ipv4Addr::UNSPECIFIED,
        prefix_len: 0,
        gateway: Ipv4Addr([192, 168, 1, 1]),
        dev,
        metric: 100,
    });

    assert_eq_test!(rt.route_count(), 2, "2 routes after reconfig");

    let r_old = rt.lookup(Ipv4Addr([10, 0, 0, 42]));
    assert_test!(r_old.is_some(), "should match default route");
    let (_, hop) = r_old.unwrap();
    assert_eq_test!(
        hop.0,
        [192, 168, 1, 1],
        "default route should point to new gateway"
    );

    pass!()
}

slopos_testing::stest!(name = test_loopback_tx_then_poll_rx, suite = loopback);
slopos_testing::stest!(name = test_loopback_multiple_tx_poll, suite = loopback);
slopos_testing::stest!(name = test_loopback_stats, suite = loopback);
slopos_testing::stest!(name = test_loopback_properties, suite = loopback);
slopos_testing::stest!(name = test_loopback_queue_capacity, suite = loopback);
slopos_testing::stest!(
    name = test_configure_populates_route_table,
    suite = loopback
);
slopos_testing::stest!(name = test_reconfigure_replaces_routes, suite = loopback);
