use slopos_testing::TestResult;
use slopos_testing::assert_test;

use crate::virtio::VIRTQ_DESC_F_NEXT;
use crate::virtio_net;

pub fn test_virtio_net_ready_and_link_up() -> TestResult {
    assert_test!(
        virtio_net::virtio_net_is_ready(),
        "virtio-net should be discovered and initialized"
    );
    assert_test!(
        virtio_net::virtio_net_link_up(),
        "virtio-net link should be reported up"
    );

    let mac = virtio_net::virtio_net_mac().unwrap_or([0; 6]);
    assert_test!(mac != [0; 6], "virtio-net MAC should not be all-zero");

    let ipv4 = virtio_net::virtio_net_ipv4_addr().unwrap_or([0; 4]);
    assert_test!(ipv4 != [0; 4], "virtio-net should acquire IPv4 via DHCP");
    TestResult::Pass
}

/// Zero-copy SG TX chain linkage, `[header] -> run0 -> run1` (SLOPRING § 13).
pub fn test_build_tx_chain_links_runs() -> TestResult {
    let slots = [3u16, 4, 5];
    let runs = [(0x0020_0000u64, 1500u32), (0x0030_0000u64, 200u32)];
    let Some(chain) = virtio_net::build_tx_chain_for_test(&slots, 0x1000, 42, &runs) else {
        return TestResult::Fail;
    };
    assert_test!(chain.len() == 3, "chain = header + 2 runs");

    let (s0, a0, l0, f0, n0) = chain[0];
    assert_test!(
        s0 == 3 && a0 == 0x1000 && l0 == 42 && (f0 & VIRTQ_DESC_F_NEXT) != 0 && n0 == 4,
        "header descriptor links to run 0"
    );
    let (s1, a1, l1, f1, n1) = chain[1];
    assert_test!(
        s1 == 4 && a1 == 0x0020_0000 && l1 == 1500 && (f1 & VIRTQ_DESC_F_NEXT) != 0 && n1 == 5,
        "run 0 links to run 1"
    );
    let (s2, a2, l2, f2, n2) = chain[2];
    assert_test!(
        s2 == 5 && a2 == 0x0030_0000 && l2 == 200 && f2 == 0 && n2 == 0,
        "run 1 terminates the chain"
    );

    // Empty datagrams take the inline single-copy path, not SG DMA.
    assert_test!(
        virtio_net::build_tx_chain_for_test(&[3, 4], 0x1000, 42, &runs).is_none(),
        "wrong slot count rejected"
    );
    assert_test!(
        virtio_net::build_tx_chain_for_test(&[3], 0x1000, 42, &[]).is_none(),
        "empty payload rejected"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_build_tx_chain_links_runs, suite = virtio_net);
slopos_testing::stest!(name = test_virtio_net_ready_and_link_up, suite = virtio_net);
