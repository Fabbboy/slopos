use core::sync::atomic::{AtomicU16, Ordering};

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::reassembly::REASSEMBLY_TABLE;
use crate::timer::NET_TIMER_WHEEL;
use crate::types::Ipv4Addr;

const MAX_GROUPS_UNDER_TEST: usize = 32;
static NEXT_TEST_IDENTIFICATION: AtomicU16 = AtomicU16::new(1);

fn next_identification() -> u16 {
    NEXT_TEST_IDENTIFICATION.fetch_add(1, Ordering::Relaxed)
}

fn reset_reassembly_table() {
    let mut table = REASSEMBLY_TABLE.lock();
    table.reset();
}

fn bytes_are(slice: &[u8], expected: u8) -> bool {
    slice.iter().all(|b| *b == expected)
}

pub fn test_reassembly_two_fragments() -> TestResult {
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let src = Ipv4Addr([10, 0, 0, 1]);
    let dst = Ipv4Addr([10, 0, 0, 2]);
    let identification = next_identification();
    let protocol = 17;
    let frag1 = [0x11; 100];
    let frag2 = [0x22; 50];

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 0, true, &frag1)
            .is_none(),
        "first fragment should not complete reassembly"
    );

    let packet = table.insert(src, dst, identification, protocol, 100, false, &frag2);
    assert_test!(
        packet.is_some(),
        "second fragment should complete reassembly"
    );
    let packet = packet.unwrap();

    assert_eq_test!(packet.len, 150, "reassembled length should be 150 bytes");
    assert_test!(
        bytes_are(&packet.data[..100], 0x11),
        "first 100 bytes should match fragment 1"
    );
    assert_test!(
        bytes_are(&packet.data[100..150], 0x22),
        "last 50 bytes should match fragment 2"
    );

    pass!()
}

pub fn test_reassembly_timeout_drops_incomplete() -> TestResult {
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let src = Ipv4Addr([10, 0, 1, 1]);
    let dst = Ipv4Addr([10, 0, 1, 2]);
    let identification = next_identification();
    let protocol = 6;
    let frag1 = [0x33; 100];
    let frag2 = [0x44; 50];

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 0, true, &frag1)
            .is_none(),
        "single first fragment should remain incomplete"
    );

    table.reset();

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 100, false, &frag2)
            .is_none(),
        "timed-out group should not keep stale first fragment"
    );

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 0, true, &frag1)
            .is_some(),
        "reinserting both fragments should complete the group"
    );

    pass!()
}

pub fn test_reassembly_out_of_order() -> TestResult {
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let src = Ipv4Addr([10, 0, 2, 1]);
    let dst = Ipv4Addr([10, 0, 2, 2]);
    let identification = next_identification();
    let protocol = 17;
    let frag1 = [0x55; 100];
    let frag2 = [0x66; 50];

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 100, false, &frag2)
            .is_none(),
        "last fragment first should not complete"
    );

    let packet = table.insert(src, dst, identification, protocol, 0, true, &frag1);
    assert_test!(
        packet.is_some(),
        "in-order completion should happen on second insert"
    );
    let packet = packet.unwrap();

    assert_eq_test!(packet.len, 150, "reassembled length should be 150 bytes");
    assert_test!(
        bytes_are(&packet.data[..100], 0x55),
        "first 100 bytes should come from fragment offset 0"
    );
    assert_test!(
        bytes_are(&packet.data[100..150], 0x66),
        "last 50 bytes should come from fragment offset 100"
    );

    pass!()
}

pub fn test_reassembly_duplicate_fragment() -> TestResult {
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let src = Ipv4Addr([10, 0, 3, 1]);
    let dst = Ipv4Addr([10, 0, 3, 2]);
    let identification = next_identification();
    let protocol = 17;
    let frag1 = [0x77; 100];
    let frag2 = [0x88; 50];

    assert_test!(
        table
            .insert(src, dst, identification, protocol, 0, true, &frag1)
            .is_none(),
        "first fragment insert should be incomplete"
    );
    assert_test!(
        table
            .insert(src, dst, identification, protocol, 0, true, &frag1)
            .is_none(),
        "duplicate fragment should overwrite slot without completing"
    );

    let packet = table.insert(src, dst, identification, protocol, 100, false, &frag2);
    assert_test!(
        packet.is_some(),
        "reassembly should complete with duplicate fragment present"
    );
    let packet = packet.unwrap();

    assert_eq_test!(packet.len, 150, "reassembled length should be 150 bytes");
    assert_test!(
        bytes_are(&packet.data[..100], 0x77),
        "first 100 bytes should remain uncorrupted"
    );
    assert_test!(
        bytes_are(&packet.data[100..150], 0x88),
        "last 50 bytes should remain uncorrupted"
    );

    pass!()
}

pub fn test_reassembly_max_groups_eviction() -> TestResult {
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let dst = Ipv4Addr([10, 0, 4, 250]);
    let protocol = 17;
    let oldest_identification = next_identification();

    assert_test!(
        table
            .insert(
                Ipv4Addr([10, 0, 4, 1]),
                dst,
                oldest_identification,
                protocol,
                0,
                true,
                &[0x91; 100],
            )
            .is_none(),
        "oldest incomplete group should be created"
    );

    for i in 1..MAX_GROUPS_UNDER_TEST {
        let identification = next_identification();
        let src = Ipv4Addr([10, 0, 4, (i + 1) as u8]);
        let payload = [i as u8; 100];
        assert_test!(
            table
                .insert(src, dst, identification, protocol, 0, true, &payload)
                .is_none(),
            "incomplete group should be inserted while filling table"
        );
    }

    let overflow_src = Ipv4Addr([10, 0, 4, 240]);
    let overflow_identification = next_identification();
    assert_test!(
        table
            .insert(
                overflow_src,
                dst,
                overflow_identification,
                protocol,
                0,
                true,
                &[0xAA; 100],
            )
            .is_none(),
        "overflow group first fragment should be accepted as incomplete"
    );

    assert_test!(
        table
            .insert(
                Ipv4Addr([10, 0, 4, 1]),
                dst,
                oldest_identification,
                protocol,
                100,
                false,
                &[0xBB; 50],
            )
            .is_none(),
        "oldest group should be evicted and not complete"
    );

    assert_test!(
        table
            .insert(
                overflow_src,
                dst,
                overflow_identification,
                protocol,
                100,
                false,
                &[0xCC; 50],
            )
            .is_some(),
        "overflow group should complete, confirming allocation after eviction"
    );

    pass!()
}

pub fn test_non_fragmented_bypasses_reassembly() -> TestResult {
    reset_reassembly_table();

    let pending_before = NET_TIMER_WHEEL.pending_count();

    let _src = Ipv4Addr([10, 0, 5, 1]);
    let _dst = Ipv4Addr([10, 0, 5, 2]);
    let _non_fragmented_payload = [0x5A; 64];

    let pending_after = NET_TIMER_WHEEL.pending_count();
    assert_eq_test!(
        pending_after,
        pending_before,
        "non-fragmented path should not touch reassembly table/timers"
    );

    pass!()
}

slopos_testing::stest!(name = test_reassembly_two_fragments, suite = reassembly);
slopos_testing::stest!(
    name = test_reassembly_timeout_drops_incomplete,
    suite = reassembly
);
slopos_testing::stest!(name = test_reassembly_out_of_order, suite = reassembly);
slopos_testing::stest!(
    name = test_reassembly_duplicate_fragment,
    suite = reassembly
);
slopos_testing::stest!(
    name = test_reassembly_max_groups_eviction,
    suite = reassembly
);
slopos_testing::stest!(
    name = test_non_fragmented_bypasses_reassembly,
    suite = reassembly
);
