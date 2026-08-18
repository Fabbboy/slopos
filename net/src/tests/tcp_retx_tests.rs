//! Send-map unit tests.
//!
//! Exercises the SACK-aware `SendMap` in isolation — no TCP state machine, no
//! buffers, no timers.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::retx::{SENDMAP_CAPACITY, SendMap};
use crate::tcp::seq::SeqNum;

pub fn test_sendmap_empty_is_empty() -> TestResult {
    let q = SendMap::new();
    assert_test!(q.is_empty(), "fresh map is empty");
    assert_eq_test!(q.len(), 0, "len 0");
    assert_eq_test!(q.total_bytes(), 0, "no bytes tracked");
    assert_eq_test!(q.pipe(), 0, "no pipe");
    assert_test!(!q.has_lost(), "no lost entries");
    assert_test!(q.next_lost().is_none(), "no lost head");
    pass!()
}

pub fn test_sendmap_push_single_segment() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 10, 50).expect("room");
    assert_eq_test!(q.len(), 1, "one entry");
    assert_eq_test!(q.total_bytes(), 10, "10 bytes tracked");
    assert_eq_test!(q.pipe(), 10, "10 bytes in pipe");
    assert_test!(!q.has_lost(), "nothing lost");
    pass!()
}

pub fn test_sendmap_full_ack_frees_entry_and_reports_rtt_origin() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 10, 50).unwrap();
    let out = q.on_cumulative_ack(SeqNum::new(110));
    assert_eq_test!(out.bytes_freed, 10, "freed 10 bytes");
    assert_eq_test!(out.entries_removed, 1, "removed 1 entry");
    assert_eq_test!(out.rtt_sample_origin_ms, Some(50), "RTT origin");
    assert_test!(q.is_empty(), "map drained");
    assert_eq_test!(q.total_bytes(), 0, "no bytes tracked");
    pass!()
}

pub fn test_sendmap_partial_ack_shrinks_head() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 20, 50).unwrap();
    let out = q.on_cumulative_ack(SeqNum::new(105));
    assert_eq_test!(out.bytes_freed, 5, "freed 5 bytes");
    assert_eq_test!(out.entries_removed, 0, "no entries removed");
    assert_eq_test!(out.rtt_sample_origin_ms, None, "partial ack: no rtt sample");
    assert_eq_test!(q.len(), 1, "entry still present");
    assert_eq_test!(q.total_bytes(), 15, "total_bytes shrunk");
    assert_eq_test!(q.pipe(), 15, "pipe shrunk");
    pass!()
}

pub fn test_sendmap_cumulative_ack_across_entries() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 10, 10).unwrap();
    q.push_sent(SeqNum::new(110), 10, 20).unwrap();
    q.push_sent(SeqNum::new(120), 10, 30).unwrap();
    let out = q.on_cumulative_ack(SeqNum::new(130));
    assert_eq_test!(out.bytes_freed, 30, "freed 30 bytes");
    assert_eq_test!(out.entries_removed, 3, "removed 3 entries");
    assert_eq_test!(
        out.rtt_sample_origin_ms,
        Some(10),
        "RTT origin is oldest first_send_ms"
    );
    assert_test!(q.is_empty(), "map drained");
    pass!()
}

pub fn test_sendmap_karn_suppresses_rtt_on_retransmit() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 10, 50).unwrap();
    q.mark_all_lost();
    assert_test!(q.has_lost(), "entry is Lost after mark_all_lost");
    q.mark_retransmitted(SeqNum::new(100));
    let out = q.on_cumulative_ack(SeqNum::new(110));
    assert_eq_test!(out.bytes_freed, 10, "freed");
    assert_eq_test!(out.entries_removed, 1, "removed");
    assert_eq_test!(out.rtt_sample_origin_ms, None, "Karn: no sample on retx");
    pass!()
}

pub fn test_sendmap_push_fails_when_full() -> TestResult {
    let mut q = SendMap::new();
    for i in 0..SENDMAP_CAPACITY {
        let seq = SeqNum::new(100 + (i as u32) * 10);
        q.push_sent(seq, 10, 0).unwrap();
    }
    assert_eq_test!(q.len(), SENDMAP_CAPACITY, "map full");
    assert_eq_test!(q.capacity_remaining(), 0, "nothing left");
    let over = q.push_sent(SeqNum::new(9_999), 10, 0);
    assert_test!(over.is_err(), "push over capacity errors");
    assert_eq_test!(q.len(), SENDMAP_CAPACITY, "len unchanged");
    assert_eq_test!(
        q.total_bytes(),
        (SENDMAP_CAPACITY as u32) * 10,
        "total_bytes unchanged"
    );
    pass!()
}

/// Only the head entry (`drop_count == 0`) is eligible for an RTT sample, so
/// SACK-confirming it suppresses the sample even though the second entry was
/// InFlight.
pub fn test_sendmap_sack_confirmed_no_rtt_sample() -> TestResult {
    let mut q = SendMap::new();
    // [100..110) ts=10, [110..120) ts=20
    q.push_sent(SeqNum::new(100), 10, 10).unwrap();
    q.push_sent(SeqNum::new(110), 10, 20).unwrap();

    let blocks = [(100u32, 110u32)];
    q.apply_sack_blocks(&blocks, 1);

    let out = q.on_cumulative_ack(SeqNum::new(120));
    assert_eq_test!(out.bytes_freed, 20, "freed all bytes");
    assert_eq_test!(out.entries_removed, 2, "removed 2 entries");
    assert_eq_test!(
        out.rtt_sample_origin_ms,
        None,
        "no RTT sample when head is SackConfirmed"
    );
    pass!()
}

/// total_bytes counts every entry; pipe counts only InFlight + Retransmitted.
pub fn test_sendmap_total_bytes_vs_pipe() -> TestResult {
    let mut q = SendMap::new();
    for i in 0..6u32 {
        let seq = SeqNum::new(100 + i * 10);
        q.push_sent(seq, 10, i as u64).unwrap();
    }
    assert_eq_test!(q.total_bytes(), 60, "60 bytes total");
    assert_eq_test!(q.pipe(), 60, "60 bytes in pipe (all InFlight)");

    q.mark_all_lost();
    assert_eq_test!(q.total_bytes(), 60, "total_bytes unchanged after loss");
    assert_eq_test!(q.pipe(), 0, "pipe 0 — all Lost, nothing in network");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after mark_all_lost"
    );

    q.mark_retransmitted(SeqNum::new(100));
    q.mark_retransmitted(SeqNum::new(110));
    assert_eq_test!(q.total_bytes(), 60, "total_bytes still 60");
    assert_eq_test!(q.pipe(), 20, "pipe 20 — 2 Retransmitted entries");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after retransmit"
    );

    q.on_cumulative_ack(SeqNum::new(110));
    assert_eq_test!(q.total_bytes(), 50, "total_bytes 50 after partial ack");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after cumulative ack"
    );
    pass!()
}

/// Splitmix-64 PRNG, for a deterministic operation mix.
fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// `total_bytes()` stays bounded and `pipe() <= total_bytes()` holds under any
/// mix of push/ack/sack/mark_lost/retransmit operations.
pub fn test_sendmap_invariant_fuzz() -> TestResult {
    let mut rng = 0xDEAD_BEEF_CAFE_BABE_u64;
    let mut q = SendMap::new();
    let mut next_seq: u32 = 1000;

    for _ in 0..2_048 {
        let op = splitmix32(&mut rng) % 6;
        match op {
            0 => {
                // push if room
                if q.capacity_remaining() > 0 {
                    let len = 1 + (splitmix32(&mut rng) % 1460);
                    q.push_sent(SeqNum::new(next_seq), len, 0).unwrap();
                    next_seq = next_seq.wrapping_add(len);
                }
            }
            1 => {
                // full cumulative ack of head entry
                if !q.is_empty() {
                    let out = q.on_cumulative_ack(SeqNum::new(
                        next_seq.wrapping_sub(
                            q.total_bytes()
                                .saturating_sub(splitmix32(&mut rng) % q.total_bytes().max(1)),
                        ),
                    ));
                    let _ = out;
                }
            }
            2 => {
                // mark all lost (RTO simulation)
                q.mark_all_lost();
            }
            3 => {
                // retransmit first Lost entry
                if let Some(lost) = q.next_lost() {
                    let seq = lost.seq;
                    q.mark_retransmitted(seq);
                }
            }
            4 => {
                // apply a SACK block covering latest entry
                if q.len() >= 2 {
                    let _last_idx = q.len() - 1;
                    // Entries cannot be peeked, so cover the tail of the
                    // sequence space with a wide block instead.
                    let tail_start = next_seq.wrapping_sub(1460);
                    let blocks = [(tail_start, next_seq)];
                    q.apply_sack_blocks(&blocks, 1);
                }
            }
            _ => {
                // cumulative ack with a small advance
                if !q.is_empty() {
                    let advance = 1 + (splitmix32(&mut rng) % 200);
                    let base = next_seq.wrapping_sub(q.total_bytes());
                    q.on_cumulative_ack(SeqNum::new(base.wrapping_add(advance)));
                }
            }
        }

        if q.total_bytes() > 1_000_000 {
            return fail!("total_bytes ran away");
        }
        if q.pipe() > q.total_bytes() {
            return fail!("pipe exceeds total_bytes");
        }
    }
    pass!()
}

slopos_testing::stest!(name = test_sendmap_empty_is_empty, suite = tcp_retx);
slopos_testing::stest!(name = test_sendmap_push_single_segment, suite = tcp_retx);
slopos_testing::stest!(
    name = test_sendmap_full_ack_frees_entry_and_reports_rtt_origin,
    suite = tcp_retx
);
slopos_testing::stest!(
    name = test_sendmap_partial_ack_shrinks_head,
    suite = tcp_retx
);
slopos_testing::stest!(
    name = test_sendmap_cumulative_ack_across_entries,
    suite = tcp_retx
);
slopos_testing::stest!(
    name = test_sendmap_karn_suppresses_rtt_on_retransmit,
    suite = tcp_retx
);
slopos_testing::stest!(name = test_sendmap_push_fails_when_full, suite = tcp_retx);
slopos_testing::stest!(
    name = test_sendmap_sack_confirmed_no_rtt_sample,
    suite = tcp_retx
);
slopos_testing::stest!(name = test_sendmap_total_bytes_vs_pipe, suite = tcp_retx);
slopos_testing::stest!(name = test_sendmap_invariant_fuzz, suite = tcp_retx);
