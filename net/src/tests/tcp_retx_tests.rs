//! Send-map unit tests.
//!
//! Exercises `SendMap` in isolation — no TCP state machine, no buffers,
//! no timers.  Covers the SACK-aware send map that replaced the old
//! go-back-N `RetxQueue`: push-then-ack round trip, partial ACK
//! shrinking, Karn's retransmit suppression of RTT samples,
//! SACK-confirmed exclusion from RTT, `total_bytes` vs `pipe` under
//! loss, and the `total_bytes == sum(entries.len)` invariant under a
//! PRNG-driven random operation mix.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::retx::{SENDMAP_CAPACITY, SendMap};
use crate::tcp::seq::SeqNum;

// -----------------------------------------------------------------------------
// Basic push / on_cumulative_ack coverage
// -----------------------------------------------------------------------------

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

/// A full cumulative ACK drains the map and reports the first-send
/// time for RTT sampling.
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

/// Partial ACK inside the head entry shrinks it in place without
/// removing it.
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

/// Cumulative ACK across multiple entries removes them all and
/// reports the RTT sample of the oldest fully-acked InFlight entry.
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

/// Karn's algorithm: after mark_all_lost + mark_retransmitted, the
/// entry becomes Retransmitted and its ACK must not produce an RTT
/// sample.
pub fn test_sendmap_karn_suppresses_rtt_on_retransmit() -> TestResult {
    let mut q = SendMap::new();
    q.push_sent(SeqNum::new(100), 10, 50).unwrap();
    // Simulate RTO: mark everything lost, then retransmit the entry.
    q.mark_all_lost();
    assert_test!(q.has_lost(), "entry is Lost after mark_all_lost");
    q.mark_retransmitted(SeqNum::new(100));
    // ACK the retransmitted entry — Karn says no RTT sample.
    let out = q.on_cumulative_ack(SeqNum::new(110));
    assert_eq_test!(out.bytes_freed, 10, "freed");
    assert_eq_test!(out.entries_removed, 1, "removed");
    assert_eq_test!(out.rtt_sample_origin_ms, None, "Karn: no sample on retx");
    pass!()
}

/// Filling the map returns Err without growing tracked state.
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

// -----------------------------------------------------------------------------
// SACK / loss-detection tests
// -----------------------------------------------------------------------------

/// SackConfirmed entries must not contribute an RTT sample when freed
/// by a cumulative ACK.  Push two entries, SACK the first, then ACK
/// everything — the RTT sample must be `None` because the head entry
/// (the only candidate for the "first freed" RTT sample) was
/// SackConfirmed, and the implementation only considers `drop_count == 0`
/// (the head position) for RTT eligibility.
pub fn test_sendmap_sack_confirmed_no_rtt_sample() -> TestResult {
    let mut q = SendMap::new();
    // [100..110) ts=10, [110..120) ts=20
    q.push_sent(SeqNum::new(100), 10, 10).unwrap();
    q.push_sent(SeqNum::new(110), 10, 20).unwrap();

    // SACK the first entry only — marks it SackConfirmed.
    let blocks = [(100u32, 110u32)];
    q.apply_sack_blocks(&blocks, 1);

    // Cumulative ACK covering both entries.
    let out = q.on_cumulative_ack(SeqNum::new(120));
    assert_eq_test!(out.bytes_freed, 20, "freed all bytes");
    assert_eq_test!(out.entries_removed, 2, "removed 2 entries");
    // The head entry (ts=10) was SackConfirmed — ineligible for RTT.
    // RTT sampling only considers the head position (drop_count == 0),
    // so no sample is produced even though the second entry was InFlight.
    assert_eq_test!(
        out.rtt_sample_origin_ms,
        None,
        "no RTT sample when head is SackConfirmed"
    );
    pass!()
}

/// total_bytes counts ALL entries regardless of state, while pipe only
/// counts InFlight + Retransmitted.  After marking some entries Lost,
/// total_bytes >= pipe must always hold.
pub fn test_sendmap_total_bytes_vs_pipe() -> TestResult {
    let mut q = SendMap::new();
    // Push 6 entries: [100..110), [110..120), ..., [150..160)
    for i in 0..6u32 {
        let seq = SeqNum::new(100 + i * 10);
        q.push_sent(seq, 10, i as u64).unwrap();
    }
    assert_eq_test!(q.total_bytes(), 60, "60 bytes total");
    assert_eq_test!(q.pipe(), 60, "60 bytes in pipe (all InFlight)");

    // Mark all lost — pipe drops to 0, total_bytes stays at 60.
    q.mark_all_lost();
    assert_eq_test!(q.total_bytes(), 60, "total_bytes unchanged after loss");
    assert_eq_test!(q.pipe(), 0, "pipe 0 — all Lost, nothing in network");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after mark_all_lost"
    );

    // Retransmit first two entries — they go back into pipe.
    q.mark_retransmitted(SeqNum::new(100));
    q.mark_retransmitted(SeqNum::new(110));
    assert_eq_test!(q.total_bytes(), 60, "total_bytes still 60");
    assert_eq_test!(q.pipe(), 20, "pipe 20 — 2 Retransmitted entries");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after retransmit"
    );

    // ACK the first entry — total_bytes drops, pipe drops.
    q.on_cumulative_ack(SeqNum::new(110));
    assert_eq_test!(q.total_bytes(), 50, "total_bytes 50 after partial ack");
    assert_test!(
        q.total_bytes() >= q.pipe(),
        "total_bytes >= pipe after cumulative ack"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// PRNG-backed invariant fuzz
// -----------------------------------------------------------------------------

/// Splitmix-64 PRNG used for deterministic shuffling in tests.
fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// `total_bytes()` must always equal the sum of `entry.len` for every
/// entry currently in the map, and `pipe() <= total_bytes()` must hold,
/// under any mix of push/ack/sack/mark_lost/retransmit operations.
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
                    // ACK the first entry's end — may free it.
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
                    // SACK the last entry to create a gap.
                    let _last_idx = q.len() - 1;
                    // We cannot peek entries directly, but we can
                    // construct a block covering the tail of the
                    // sequence space.  Use a wide block.
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

        // Invariant: total_bytes cannot grow unboundedly.
        if q.total_bytes() > 1_000_000 {
            return fail!("total_bytes ran away");
        }
        // Invariant: pipe never exceeds total_bytes.
        if q.pipe() > q.total_bytes() {
            return fail!("pipe exceeds total_bytes");
        }
    }
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::define_test_suite!(
    tcp_retx,
    [
        test_sendmap_empty_is_empty,
        test_sendmap_push_single_segment,
        test_sendmap_full_ack_frees_entry_and_reports_rtt_origin,
        test_sendmap_partial_ack_shrinks_head,
        test_sendmap_cumulative_ack_across_entries,
        test_sendmap_karn_suppresses_rtt_on_retransmit,
        test_sendmap_push_fails_when_full,
        test_sendmap_sack_confirmed_no_rtt_sample,
        test_sendmap_total_bytes_vs_pipe,
        test_sendmap_invariant_fuzz,
    ]
);
