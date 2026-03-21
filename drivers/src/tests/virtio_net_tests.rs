use slopos_abi::net::{USER_NET_MAX_MEMBERS, UserNetMember};
use slopos_testing::TestResult;
use slopos_testing::assert_test;
use slopos_utils::klog_info;

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

pub fn test_virtio_net_scan_discovers_network_members() -> TestResult {
    let mut members = [UserNetMember::default(); USER_NET_MAX_MEMBERS];
    let mut count = 0usize;

    for _ in 0..6 {
        count = virtio_net::virtio_net_scan_members(members.as_mut_ptr(), members.len(), true);
        if count > 0 {
            break;
        }
    }

    if count == 0 {
        klog_info!("virtio-net-test: no members discovered after active probing");
        return TestResult::Fail;
    }

    let found_nonzero_ip = members[..count].iter().any(|m| m.ipv4 != [0; 4]);

    assert_test!(
        found_nonzero_ip,
        "virtio-net scan should return at least one IPv4 member"
    );

    TestResult::Pass
}

slopos_testing::define_test_suite!(
    virtio_net,
    [
        test_virtio_net_ready_and_link_up,
        test_virtio_net_scan_discovers_network_members,
    ]
);
