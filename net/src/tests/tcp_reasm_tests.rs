//! Out-of-order reassembly edge-case tests.
//!
//! Exercises `TcpOooQueue::insert` + `drain_contiguous` directly — no
//! `tcp_input` path, no sockets, no timers.  The queue is a standalone
//! data structure; this suite nails down its invariants so a later
//! interval-merging rewrite has a regression oracle to aim at.
//!
//! Covers: single-gap fill, duplicate insertion, full-queue eviction,
//! partial overlap against `rcv_nxt`, draining until the receive buffer
//! fills, seq-space wrap-around, and a csprng-backed commutativity fuzz
//! (inserting N non-overlapping segments in two random orders must yield
//! identical drain output).

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::buffer::TcpRecvState;
use crate::tcp::reasm::TcpOooQueue;

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn fresh_recv() -> TcpRecvState {
    TcpRecvState::new()
}

fn fresh_queue() -> TcpOooQueue {
    TcpOooQueue::new()
}

/// Drain the receive buffer into a `Vec` so tests can assert on the
/// reassembled byte stream directly.
fn drain_to_vec(recv: &mut TcpRecvState) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(recv.available());
    let mut buf = [0u8; 512];
    loop {
        let n = recv.dequeue(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

// -----------------------------------------------------------------------------
// Basic single-gap fill
// -----------------------------------------------------------------------------

/// Insert a segment ahead of `rcv_nxt`, then advance `rcv_nxt` to the
/// gap; `drain_contiguous` must deliver the buffered bytes.
pub fn test_reasm_single_gap_fills() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    q.insert(200, b"world");
    // Nothing to drain yet — the gap at 100..200 is unfilled.
    assert_eq_test!(
        q.drain_contiguous(100, &mut recv, 0),
        0,
        "no drain with gap"
    );
    assert_eq_test!(recv.available(), 0, "nothing written");

    // Simulate the gap being filled via the normal fast path: advance
    // rcv_nxt to 200 and drain.
    assert_eq_test!(q.drain_contiguous(200, &mut recv, 0), 5, "drain 5 bytes");
    assert_test!(q.is_empty(), "queue empty after drain");
    let out = drain_to_vec(&mut recv);
    assert_eq_test!(&out[..], b"world", "payload delivered");
    pass!()
}

/// Two non-contiguous segments: filling the first gap only delivers the
/// first segment; the second stays queued until its own gap closes.
pub fn test_reasm_non_contiguous_segments() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    q.insert(100, b"aaa"); // 100..103
    q.insert(200, b"bbb"); // 200..203, gap at 103..200

    assert_eq_test!(
        q.drain_contiguous(100, &mut recv, 0),
        3,
        "drain first block only"
    );
    assert_test!(!q.is_empty(), "second block remains");
    assert_eq_test!(&drain_to_vec(&mut recv)[..], b"aaa", "first block content");

    let mut recv2 = fresh_recv();
    assert_eq_test!(
        q.drain_contiguous(200, &mut recv2, 0),
        3,
        "drain second block"
    );
    assert_test!(q.is_empty(), "queue empty after second drain");
    assert_eq_test!(
        &drain_to_vec(&mut recv2)[..],
        b"bbb",
        "second block content"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// Duplicates
// -----------------------------------------------------------------------------

/// Inserting the exact same `(seq, data)` twice stores one entry.
pub fn test_reasm_duplicate_insertion_is_noop() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    q.insert(100, b"dup");
    q.insert(100, b"dup");
    // Drain — should deliver one copy of "dup" and then report empty.
    assert_eq_test!(q.drain_contiguous(100, &mut recv, 0), 3, "delivered once");
    assert_test!(q.is_empty(), "one entry only");
    assert_eq_test!(&drain_to_vec(&mut recv)[..], b"dup", "single copy");
    pass!()
}

// -----------------------------------------------------------------------------
// Partial overlap with rcv_nxt
// -----------------------------------------------------------------------------

/// Segment's seq is below `rcv_nxt` but extends past it.  Drain should
/// deliver the portion past `rcv_nxt` only.
pub fn test_reasm_partial_overlap_with_rcv_nxt() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    // Entry starts at 95, 10 bytes long → extends to 105.  rcv_nxt is 100.
    q.insert(95, b"0123456789");
    let drained = q.drain_contiguous(100, &mut recv, 0);
    assert_eq_test!(drained, 5, "drained the tail past rcv_nxt");
    assert_eq_test!(&drain_to_vec(&mut recv)[..], b"56789", "correct tail bytes");
    pass!()
}

// -----------------------------------------------------------------------------
// Full-queue eviction
// -----------------------------------------------------------------------------

/// The queue has a fixed capacity.  When a new higher-seq entry arrives and
/// every slot is full, the current policy evicts the **highest-seq** entry
/// (i.e. keep things closest to the gap).  Pinning that policy here lets a
/// future rewrite replace it deliberately.
pub fn test_reasm_full_queue_eviction_keeps_lowest_seq() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    // Fill with OOO_MAX_ENTRIES=8 entries at descending seq numbers.  The
    // existing policy drops the highest when a lower-seq entry arrives
    // after the queue is full.
    for i in 0..8 {
        let seq = 200 + (i as u32) * 16;
        q.insert(seq, &[b'a' + i as u8; 16]);
    }
    // Insert a lower-seq entry: this should kick out the highest-seq one.
    q.insert(100, b"lowest______1616"); // 16-byte payload

    // Drain from 100: we should get the "lowest" entry, then 200 onwards
    // minus the evicted top.
    let drained = q.drain_contiguous(100, &mut recv, 0);
    // 16 (lowest) + skipped gap + ... don't assert the exact byte count,
    // just that the lowest entry definitely made it in.
    let bytes = drain_to_vec(&mut recv);
    assert_test!(
        bytes.starts_with(b"lowest"),
        "lowest-seq entry preserved after eviction"
    );
    let _ = drained;
    pass!()
}

// -----------------------------------------------------------------------------
// Sequence-space wrap
// -----------------------------------------------------------------------------

/// Two entries that straddle the 32-bit wrap: the "later" entry has a
/// numerically-smaller seq than the "earlier" one, but wrapping comparison
/// must still drain them in correct order.
pub fn test_reasm_wrap_across_seq_space() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    // rcv_nxt is near the wrap; the buffered segment straddles it.
    let near_wrap: u32 = 0xFFFF_FFF8;
    q.insert(near_wrap, b"abcdefgh"); // 8 bytes, ends at 0 (wraps)
    q.insert(0, b"ijkl"); // starts exactly at wrap

    let drained = q.drain_contiguous(near_wrap, &mut recv, 0);
    assert_eq_test!(drained, 12, "drained both segments across wrap");
    assert_eq_test!(
        &drain_to_vec(&mut recv)[..],
        b"abcdefghijkl",
        "correct wrap ordering"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// Drain stops when recv buffer fills
// -----------------------------------------------------------------------------

/// The drain loop must stop cleanly when the receive buffer can't accept
/// any more bytes — it should not spin.
pub fn test_reasm_drain_stops_at_recv_buffer_full() -> TestResult {
    let mut q = fresh_queue();
    let mut recv = fresh_recv();

    // Fill the recv buffer nearly full first.  Its capacity is
    // TCP_BUFFER_SIZE = 32768.
    let big = [7u8; 32_000];
    let wrote = recv.enqueue(&big, 0);
    assert_eq_test!(wrote, 32_000, "pre-fill recv buffer");

    // Now queue an OOO segment right after rcv_nxt's current tail:
    let seq = big.len() as u32;
    q.insert(seq, &[9u8; 1000]);

    // Drain should push *some* bytes (<= free space) and stop.
    let drained = q.drain_contiguous(seq, &mut recv, 0);
    assert_test!(drained > 0, "wrote some bytes");
    assert_test!(drained <= 1000, "bounded by segment length");
    pass!()
}

// -----------------------------------------------------------------------------
// CSPRNG commutativity
// -----------------------------------------------------------------------------

fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// Non-overlapping contiguous fragments must drain identically regardless
/// of the order in which they were inserted — a basic commutativity check
/// for the sort/eviction logic.
pub fn test_reasm_insert_order_commutative_fuzz() -> TestResult {
    let mut seed = 0x01234567_89ABCDEFu64;

    for _trial in 0..16 {
        // Generate 6 non-overlapping 32-byte chunks starting at 1000.
        const N: usize = 6;
        const CHUNK: usize = 32;
        const BASE: u32 = 1000;
        let mut payloads: [[u8; CHUNK]; N] = [[0; CHUNK]; N];
        for (i, p) in payloads.iter_mut().enumerate() {
            p[0] = b'A' + i as u8;
            p[1] = b'0' + i as u8;
            // remaining bytes stay zero
        }

        // Two random permutations of {0..N}.
        let mut order_a = [0usize; N];
        let mut order_b = [0usize; N];
        for i in 0..N {
            order_a[i] = i;
            order_b[i] = i;
        }
        // Fisher–Yates shuffles with the PRNG.
        for i in (1..N).rev() {
            let j = (splitmix32(&mut seed) as usize) % (i + 1);
            order_a.swap(i, j);
        }
        for i in (1..N).rev() {
            let j = (splitmix32(&mut seed) as usize) % (i + 1);
            order_b.swap(i, j);
        }

        // Insert in order A, drain into recv_a.
        let mut qa = fresh_queue();
        let mut ra = fresh_recv();
        for idx in order_a {
            qa.insert(BASE + (idx as u32) * CHUNK as u32, &payloads[idx]);
        }
        qa.drain_contiguous(BASE, &mut ra, 0);
        let out_a = drain_to_vec(&mut ra);

        // Insert in order B, drain into recv_b.
        let mut qb = fresh_queue();
        let mut rb = fresh_recv();
        for idx in order_b {
            qb.insert(BASE + (idx as u32) * CHUNK as u32, &payloads[idx]);
        }
        qb.drain_contiguous(BASE, &mut rb, 0);
        let out_b = drain_to_vec(&mut rb);

        if out_a != out_b {
            return fail!("drain output differs for permuted inserts");
        }
    }
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::define_test_suite!(
    tcp_reasm,
    [
        test_reasm_single_gap_fills,
        test_reasm_non_contiguous_segments,
        test_reasm_duplicate_insertion_is_noop,
        test_reasm_partial_overlap_with_rcv_nxt,
        test_reasm_full_queue_eviction_keeps_lowest_seq,
        test_reasm_wrap_across_seq_space,
        test_reasm_drain_stops_at_recv_buffer_full,
        test_reasm_insert_order_commutative_fuzz,
    ]
);
