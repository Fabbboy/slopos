use core::sync::atomic::{AtomicU16, Ordering};

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::net_scope::NetTestScope;
use crate::packetbuf::PacketBuf;
use crate::pool::PACKET_POOL;
use crate::reassembly::REASSEMBLY_TABLE;
use crate::tests::tcp_common::{LOCAL_IP, REMOTE_IP};
use crate::types::{DevIndex, Ipv4Addr};

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

/// Must run before the scope's `Drop`: a surviving group holds a timeout token
/// minted in the test wheel.
struct ResetOnExit;

impl Drop for ResetOnExit {
    fn drop(&mut self) {
        reset_reassembly_table();
    }
}

pub fn test_reassembly_two_fragments() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;
    reset_reassembly_table();

    let mut table = REASSEMBLY_TABLE.lock();
    let src = Ipv4Addr(LOCAL_IP);
    let dst = Ipv4Addr(REMOTE_IP);
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;
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
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;
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

const BYPASS_DST: Ipv4Addr = Ipv4Addr([10, 0, 5, 250]);
const BYPASS_PROBE_SRC: Ipv4Addr = Ipv4Addr([10, 0, 5, 200]);
/// Unassigned (RFC 3692), so `dispatch_l4` has no handler for it.
const BYPASS_PROTOCOL: u8 = 253;
const BYPASS_FRAGMENT_LEN: usize = 100;

#[derive(Clone, Copy)]
struct GroupKey {
    src: Ipv4Addr,
    identification: u16,
}

fn ipv4_packet(key: GroupKey, more_fragments: bool) -> Option<PacketBuf> {
    const HEADER_LEN: usize = 20;
    const TOTAL_LEN: usize = HEADER_LEN + BYPASS_FRAGMENT_LEN;
    let mut frame = [0u8; TOTAL_LEN];
    frame[0] = 0x45;
    frame[2..4].copy_from_slice(&(TOTAL_LEN as u16).to_be_bytes());
    frame[4..6].copy_from_slice(&key.identification.to_be_bytes());
    if more_fragments {
        frame[6] = 0x20;
    }
    frame[8] = 64;
    frame[9] = BYPASS_PROTOCOL;
    frame[12..16].copy_from_slice(&key.src.0);
    frame[16..20].copy_from_slice(&BYPASS_DST.0);
    let csum = crate::checksum::internet_checksum(&frame[..HEADER_LEN]);
    frame[10..12].copy_from_slice(&csum.to_be_bytes());
    frame[HEADER_LEN..].fill(0x5A);
    PacketBuf::from_raw_copy(&frame)
}

fn drive_ingress(dev: DevIndex, key: GroupKey, more_fragments: bool) -> bool {
    match ipv4_packet(key, more_fragments) {
        Some(pkt) => {
            crate::ipv4::handle_rx(dev, pkt, false);
            true
        }
        None => false,
    }
}

/// Returns the key eviction reaches first: group ids are a monotonic counter.
fn fill_groups() -> Option<GroupKey> {
    reset_reassembly_table();
    let mut table = REASSEMBLY_TABLE.lock();
    let mut oldest = None;
    for i in 0..MAX_GROUPS_UNDER_TEST {
        let key = GroupKey {
            src: Ipv4Addr([10, 0, 5, (i + 1) as u8]),
            identification: next_identification(),
        };
        if table
            .insert(
                key.src,
                BYPASS_DST,
                key.identification,
                BYPASS_PROTOCOL,
                0,
                true,
                &[0x11; BYPASS_FRAGMENT_LEN],
            )
            .is_some()
        {
            return None;
        }
        if oldest.is_none() {
            oldest = Some(key);
        }
    }
    oldest
}

fn complete_group(key: GroupKey) -> bool {
    let mut table = REASSEMBLY_TABLE.lock();
    table
        .insert(
            key.src,
            BYPASS_DST,
            key.identification,
            BYPASS_PROTOCOL,
            BYPASS_FRAGMENT_LEN as u16,
            false,
            &[0x22; 50],
        )
        .is_some()
}

pub fn test_non_fragmented_bypasses_reassembly() -> TestResult {
    PACKET_POOL.init();
    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    let _reset = ResetOnExit;

    let Some(oldest) = fill_groups() else {
        return fail!("could not fill the reassembly table");
    };
    let probe = GroupKey {
        src: BYPASS_PROBE_SRC,
        identification: next_identification(),
    };
    if !drive_ingress(scope.dev(), probe, false) {
        return fail!("could not build the probe packet");
    }
    let bypassed = complete_group(oldest);

    // With More Fragments the same packet does take a slot: the control.
    let Some(oldest) = fill_groups() else {
        return fail!("could not refill the reassembly table");
    };
    let probe = GroupKey {
        src: BYPASS_PROBE_SRC,
        identification: next_identification(),
    };
    if !drive_ingress(scope.dev(), probe, true) {
        return fail!("could not build the fragment packet");
    }
    let fragment_took_a_group = !complete_group(oldest);

    drop(_reset);
    drop(scope);

    assert_test!(
        bypassed,
        "a non-fragmented packet claims no reassembly group"
    );
    assert_test!(
        fragment_took_a_group,
        "a fragment claims one, so the check above is not vacuous"
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
