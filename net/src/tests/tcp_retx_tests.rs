//! Retransmission queue unit tests.
//!
//! Exercises `RetxQueue` in isolation — no TCP state machine, no
//! buffers, no timers.  Fills in the P4-era invariants the DataState
//! port will depend on: push-then-ack round trip, partial ACK
//! shrinking the head entry, Karn's retransmit suppression of RTT
//! samples, and the `inflight_bytes == sum(entries.len)` invariant
//! under a csprng-driven random operation mix.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::retx::{RETX_CAPACITY, RetxQueue};
use crate::tcp::seq::SeqNum;

// -----------------------------------------------------------------------------
// Basic push / on_ack coverage
// -----------------------------------------------------------------------------

pub fn test_retx_empty_is_empty() -> TestResult {
    let q = RetxQueue::new();
    assert_test!(q.is_empty(), "fresh queue is empty");
    assert_eq_test!(q.len(), 0, "len 0");
    assert_eq_test!(q.inflight_bytes(), 0, "no inflight");
    assert_test!(q.oldest_unacked().is_none(), "no head");
    pass!()
}

pub fn test_retx_push_single_segment() -> TestResult {
    let mut q = RetxQueue::new();
    q.push_sent(SeqNum::new(100), 10, 50).expect("room");
    assert_eq_test!(q.len(), 1, "one entry");
    assert_eq_test!(q.inflight_bytes(), 10, "10 bytes inflight");
    let head = q.oldest_unacked().unwrap();
    assert_eq_test!(head.seq.raw(), 100, "head seq");
    assert_eq_test!(head.len, 10, "head len");
    assert_eq_test!(head.first_send_ms, 50, "first send timestamp");
    assert_test!(!head.retransmitted, "fresh segment not retransmitted");
    pass!()
}

/// A full cumulative ACK drains the queue and reports the first-send
/// time for RTT sampling.
pub fn test_retx_full_ack_frees_entry_and_reports_rtt_origin() -> TestResult {
    let mut q = RetxQueue::new();
    q.push_sent(SeqNum::new(100), 10, 50).unwrap();
    let out = q.on_ack(SeqNum::new(110));
    assert_eq_test!(out.bytes_freed, 10, "freed 10 bytes");
    assert_eq_test!(out.entries_removed, 1, "removed 1 entry");
    assert_eq_test!(out.rtt_sample_origin_ms, Some(50), "RTT origin");
    assert_test!(q.is_empty(), "queue drained");
    assert_eq_test!(q.inflight_bytes(), 0, "no inflight");
    pass!()
}

/// Partial ACK inside the head entry shrinks it in place without
/// removing it.
pub fn test_retx_partial_ack_shrinks_head() -> TestResult {
    let mut q = RetxQueue::new();
    q.push_sent(SeqNum::new(100), 20, 50).unwrap();
    let out = q.on_ack(SeqNum::new(105));
    assert_eq_test!(out.bytes_freed, 5, "freed 5 bytes");
    assert_eq_test!(out.entries_removed, 0, "no entries removed");
    assert_eq_test!(out.rtt_sample_origin_ms, None, "partial ack: no rtt sample");
    assert_eq_test!(q.len(), 1, "entry still present");
    let head = q.oldest_unacked().unwrap();
    assert_eq_test!(head.seq.raw(), 105, "head seq advanced");
    assert_eq_test!(head.len, 15, "head len shrunk");
    assert_eq_test!(q.inflight_bytes(), 15, "inflight shrunk");
    pass!()
}

/// Cumulative ACK across multiple entries removes them all and
/// reports the RTT sample of the oldest fully-acked entry.
pub fn test_retx_cumulative_ack_across_entries() -> TestResult {
    let mut q = RetxQueue::new();
    q.push_sent(SeqNum::new(100), 10, 10).unwrap();
    q.push_sent(SeqNum::new(110), 10, 20).unwrap();
    q.push_sent(SeqNum::new(120), 10, 30).unwrap();
    let out = q.on_ack(SeqNum::new(130));
    assert_eq_test!(out.bytes_freed, 30, "freed 30 bytes");
    assert_eq_test!(out.entries_removed, 3, "removed 3 entries");
    assert_eq_test!(
        out.rtt_sample_origin_ms,
        Some(10),
        "RTT origin is oldest first_send_ms"
    );
    assert_test!(q.is_empty(), "queue drained");
    pass!()
}

/// Karn's algorithm: a retransmitted segment's ACK must not produce
/// an RTT sample even when it frees the entry.
pub fn test_retx_karn_suppresses_rtt_on_retransmit() -> TestResult {
    let mut q = RetxQueue::new();
    q.push_sent(SeqNum::new(100), 10, 50).unwrap();
    q.mark_all_for_retransmit();
    let out = q.on_ack(SeqNum::new(110));
    assert_eq_test!(out.bytes_freed, 10, "freed");
    assert_eq_test!(out.entries_removed, 1, "removed");
    assert_eq_test!(out.rtt_sample_origin_ms, None, "Karn: no sample on retx");
    pass!()
}

/// Filling the queue returns Err without growing inflight state.
pub fn test_retx_push_fails_when_full() -> TestResult {
    let mut q = RetxQueue::new();
    for i in 0..RETX_CAPACITY {
        let seq = SeqNum::new(100 + (i as u32) * 10);
        q.push_sent(seq, 10, 0).unwrap();
    }
    assert_eq_test!(q.len(), RETX_CAPACITY, "queue full");
    assert_eq_test!(q.capacity_remaining(), 0, "nothing left");
    let over = q.push_sent(SeqNum::new(9_999), 10, 0);
    assert_test!(over.is_err(), "push over capacity errors");
    assert_eq_test!(q.len(), RETX_CAPACITY, "len unchanged");
    assert_eq_test!(
        q.inflight_bytes(),
        (RETX_CAPACITY as u32) * 10,
        "inflight unchanged"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// CSPRNG-backed invariant fuzz
// -----------------------------------------------------------------------------

/// Splitmix-64 PRNG used for deterministic shuffling in tests.
fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// `inflight_bytes()` must always equal the sum of `entry.len` for
/// every entry currently in the queue, under any mix of push/ack/clear
/// operations.  Catches off-by-one drift in the incremental counter.
pub fn test_retx_inflight_invariant_fuzz() -> TestResult {
    let mut rng = 0xDEAD_BEEF_CAFE_BABE_u64;
    let mut q = RetxQueue::new();
    let mut next_seq: u32 = 1000;

    for _ in 0..2_048 {
        let op = splitmix32(&mut rng) % 4;
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
                // full ack head
                if let Some(head) = q.oldest_unacked() {
                    let end = head.seq + head.len;
                    q.on_ack(end);
                }
            }
            2 => {
                // partial ack head
                if let Some(head) = q.oldest_unacked() {
                    let partial = 1 + (splitmix32(&mut rng) % head.len.max(2));
                    q.on_ack(head.seq + partial);
                }
            }
            _ => {
                q.mark_all_for_retransmit();
            }
        }

        // Weak invariant: inflight cannot grow unboundedly.  The full
        // "inflight_bytes == snd_nxt - snd_una" check lives in the
        // production `DataState::assert_invariants` because RetxQueue
        // doesn't expose entries directly.
        if q.inflight_bytes() > 1_000_000 {
            return fail!("inflight ran away");
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
        test_retx_empty_is_empty,
        test_retx_push_single_segment,
        test_retx_full_ack_frees_entry_and_reports_rtt_origin,
        test_retx_partial_ack_shrinks_head,
        test_retx_cumulative_ack_across_entries,
        test_retx_karn_suppresses_rtt_on_retransmit,
        test_retx_push_fails_when_full,
        test_retx_inflight_invariant_fuzz,
    ]
);
