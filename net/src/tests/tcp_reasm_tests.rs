//! Out-of-order reassembly tests for the interval-merging [`Assembler`],
//! driving the OOO receive path without going through `tcp_input`.

use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::buffer::{TCP_BUFFER_SIZE, TcpRecvState};
use crate::tcp::reasm::Assembler;

fn fresh_recv() -> TcpRecvState {
    TcpRecvState::new(TCP_BUFFER_SIZE).expect("alloc")
}

fn fresh_asm() -> Assembler {
    Assembler::new()
}

fn drain_to_vec(recv: &mut TcpRecvState) -> slopos_ostd::KVec<u8> {
    let mut out = slopos_ostd::KVec::<u8>::with_capacity(recv.available()).expect("test alloc");
    let mut buf = [0u8; 512];
    loop {
        let n = recv.dequeue(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]).expect("test alloc");
    }
    out
}

pub fn test_reasm_single_ooo_segment() -> TestResult {
    let mut asm = fresh_asm();
    let mut recv = fresh_recv();

    let rcv_nxt: u32 = 100;
    let seg_seq: u32 = 200;
    let payload = b"hello";
    let offset = seg_seq.wrapping_sub(rcv_nxt) as usize;

    let wrote = recv.buf.write_at_offset(offset, payload);
    assert_eq_test!(wrote, 5, "wrote 5 bytes at offset");
    asm.insert(seg_seq, wrote);

    assert_eq_test!(asm.drain_contiguous(rcv_nxt), 0, "gap still open");

    let gap_data = [0xAAu8; 100];
    let gap_wrote = recv.enqueue(&gap_data, 0);
    assert_eq_test!(gap_wrote, 100, "gap fill wrote");
    let new_rcv_nxt = rcv_nxt.wrapping_add(gap_wrote as u32);

    let drained = asm.drain_contiguous(new_rcv_nxt);
    assert_eq_test!(drained, 5, "drained OOO segment");
    recv.buf.advance_head(drained);
    assert_test!(asm.is_empty(), "assembler empty after drain");

    let out = drain_to_vec(&mut recv);
    assert_eq_test!(out.len(), 105, "total bytes");
    assert_eq_test!(&out[100..], b"hello", "OOO payload correct");
    pass!()
}

pub fn test_reasm_non_contiguous_ranges() -> TestResult {
    let mut asm = fresh_asm();
    let mut recv = fresh_recv();

    recv.buf.write_at_offset(100, b"aaa");
    asm.insert(100, 3);
    recv.buf.write_at_offset(200, b"bbb");
    asm.insert(200, 3);

    assert_eq_test!(asm.range_count(), 2, "two disjoint ranges");

    recv.enqueue(&[0u8; 100], 0);
    let drained = asm.drain_contiguous(100);
    assert_eq_test!(drained, 3, "drained first range");
    recv.buf.advance_head(drained);

    assert_eq_test!(asm.range_count(), 1, "one range remains");
    assert_test!(!asm.is_empty(), "not empty");

    let out = drain_to_vec(&mut recv);
    assert_eq_test!(out.len(), 103, "103 bytes total");
    assert_eq_test!(&out[100..], b"aaa", "first range payload");
    pass!()
}

// -----------------------------------------------------------------------------
// Overlapping inserts
// -----------------------------------------------------------------------------

/// Insert [100,110) then [105,115) — should merge to [100,115).
pub fn test_reasm_overlapping_merge() -> TestResult {
    let mut asm = fresh_asm();

    asm.insert(100, 10); // [100, 110)
    asm.insert(105, 10); // [105, 115) — overlaps
    assert_eq_test!(asm.range_count(), 1, "merged to one range");

    let (blocks, count) = asm.sack_blocks();
    assert_eq_test!(count, 1, "one SACK block");
    assert_eq_test!(blocks[0], (100, 115), "merged range [100,115)");
    pass!()
}

// -----------------------------------------------------------------------------
// Adjacent merge
// -----------------------------------------------------------------------------

/// Insert [100,110) then [110,120) — adjacent intervals merge.
pub fn test_reasm_adjacent_merge() -> TestResult {
    let mut asm = fresh_asm();

    asm.insert(100, 10);
    asm.insert(110, 10);
    assert_eq_test!(asm.range_count(), 1, "adjacent merged");

    let (blocks, _) = asm.sack_blocks();
    assert_eq_test!(blocks[0], (100, 120), "merged [100,120)");
    pass!()
}

// -----------------------------------------------------------------------------
// Drain contiguous integration
// -----------------------------------------------------------------------------

/// Full integration: OOO data written into ring buffer, gap filled, drain
/// advances head, user reads correct byte stream.
pub fn test_reasm_drain_integration() -> TestResult {
    let mut asm = fresh_asm();
    let mut recv = fresh_recv();
    let rcv_nxt: u32 = 1000;

    // OOO segment at 1050, 30 bytes.
    let ooo_data = [0xBBu8; 30];
    let offset = 1050u32.wrapping_sub(rcv_nxt) as usize;
    recv.buf.write_at_offset(offset, &ooo_data);
    asm.insert(1050, 30);

    // In-order: fill the 50-byte gap.
    let gap = [0xAAu8; 50];
    recv.enqueue(&gap, 0);
    let new_rcv_nxt = rcv_nxt + 50; // 1050

    let drained = asm.drain_contiguous(new_rcv_nxt);
    assert_eq_test!(drained, 30, "drained 30 OOO bytes");
    recv.buf.advance_head(drained);

    let out = drain_to_vec(&mut recv);
    assert_eq_test!(out.len(), 80, "50 gap + 30 OOO");
    assert_test!(out[..50].iter().all(|&b| b == 0xAA), "gap bytes correct");
    assert_test!(out[50..].iter().all(|&b| b == 0xBB), "OOO bytes correct");
    pass!()
}

// -----------------------------------------------------------------------------
// SACK blocks
// -----------------------------------------------------------------------------

/// Up to 4 SACK blocks, sorted by left edge.
pub fn test_reasm_sack_blocks() -> TestResult {
    let mut asm = fresh_asm();

    asm.insert(500, 10);
    asm.insert(300, 10);
    asm.insert(100, 10);
    asm.insert(700, 10);
    asm.insert(900, 10); // 5th range — only 4 returned as SACK blocks

    let (blocks, count) = asm.sack_blocks();
    assert_eq_test!(count, 4, "capped at 4 SACK blocks");
    // Sorted by left edge.
    assert_eq_test!(blocks[0].0, 100, "first block");
    assert_eq_test!(blocks[1].0, 300, "second block");
    assert_eq_test!(blocks[2].0, 500, "third block");
    assert_eq_test!(blocks[3].0, 700, "fourth block");
    pass!()
}

// -----------------------------------------------------------------------------
// Duplicate insertion
// -----------------------------------------------------------------------------

/// Inserting the same range twice is a no-op (merges with itself).
pub fn test_reasm_duplicate_is_noop() -> TestResult {
    let mut asm = fresh_asm();

    asm.insert(100, 10);
    asm.insert(100, 10);
    assert_eq_test!(asm.range_count(), 1, "single range after duplicate");

    let (blocks, _) = asm.sack_blocks();
    assert_eq_test!(blocks[0], (100, 110), "unchanged");
    pass!()
}

// -----------------------------------------------------------------------------
// write_at_offset wrap-around
// -----------------------------------------------------------------------------

/// Ring buffer near-full with head near the end of the backing array;
/// write_at_offset must wrap around correctly.
pub fn test_reasm_write_at_offset_wrap() -> TestResult {
    let mut recv = fresh_recv();

    // Fill the buffer almost completely, then consume to move tail forward.
    // This positions head near the end of the backing array.
    let fill: KBox<[u8; 32000]> = KBox::zeroed().expect("alloc");
    recv.enqueue(&*fill, 0);
    let mut discard: KBox<[u8; 32000]> = KBox::zeroed().expect("alloc");
    recv.dequeue(&mut *discard);
    // Now: tail≈32000, head≈32000, count=0, free=32768.

    // Write at offset 700 (wraps past end of backing array).
    let payload = [0xCCu8; 100];
    let wrote = recv.buf.write_at_offset(700, &payload);
    assert_eq_test!(wrote, 100, "wrote 100 bytes wrapping around");

    // Fill the gap so we can read the OOO data.
    let mut gap: KBox<[u8; 700]> = KBox::zeroed().expect("alloc");
    gap.iter_mut().for_each(|b| *b = 0xAA);
    recv.enqueue(&*gap, 0);
    recv.buf.advance_head(100);

    let out = drain_to_vec(&mut recv);
    assert_eq_test!(out.len(), 800, "800 total bytes");
    assert_test!(
        out[700..].iter().all(|&b| b == 0xCC),
        "OOO payload correct after wrap"
    );
    pass!()
}

// -----------------------------------------------------------------------------
// Capacity limit
// -----------------------------------------------------------------------------

/// write_at_offset returns 0 when offset is beyond free space.
pub fn test_reasm_write_at_offset_capacity() -> TestResult {
    let mut recv = fresh_recv();

    // Fill buffer to leave only 100 bytes free.
    let fill: KBox<[u8; 32668]> = KBox::zeroed().expect("alloc"); // 32768 - 100
    recv.enqueue(&*fill, 0);

    // Offset 100 → exactly at the boundary, no room for data.
    let wrote = recv.buf.write_at_offset(100, b"x");
    assert_eq_test!(wrote, 0, "no room at offset=free_space");

    // Offset 50 → 50 bytes available.
    let wrote = recv.buf.write_at_offset(50, &[0xFFu8; 100]);
    assert_eq_test!(wrote, 50, "capped at available space");
    pass!()
}

// -----------------------------------------------------------------------------
// Sequence-space wrap
// -----------------------------------------------------------------------------

/// Assembler intervals near u32::MAX — wrapping-aware comparison must
/// keep them sorted and merge correctly.
pub fn test_reasm_seq_wrap() -> TestResult {
    let mut asm = fresh_asm();

    let near_wrap: u32 = 0xFFFF_FFF0;
    asm.insert(near_wrap, 16); // [FFF0, 0000) — wraps
    asm.insert(0, 16); // [0000, 0010) — adjacent across wrap

    assert_eq_test!(asm.range_count(), 1, "merged across wrap");

    let (blocks, count) = asm.sack_blocks();
    assert_eq_test!(count, 1, "one block");
    assert_eq_test!(blocks[0].0, near_wrap, "start at near_wrap");
    assert_eq_test!(blocks[0].1, 16, "end wraps to 16");
    pass!()
}

// -----------------------------------------------------------------------------
// Drain stops at recv buffer capacity
// -----------------------------------------------------------------------------

/// When the recv buffer is nearly full, drain cannot advance head past
/// the buffer's capacity.
pub fn test_reasm_drain_respects_capacity() -> TestResult {
    let mut asm = fresh_asm();
    let mut recv = fresh_recv();

    // Fill recv buffer to leave only 50 bytes free.
    let fill: KBox<[u8; 32718]> = KBox::zeroed().expect("alloc"); // 32768 - 50
    recv.enqueue(&*fill, 0);

    let rcv_nxt: u32 = fill.len() as u32;

    // Write a 100-byte OOO segment — only 50 bytes fit.
    let ooo = [0xDDu8; 100];
    let wrote = recv.buf.write_at_offset(0, &ooo);
    assert_eq_test!(wrote, 50, "capped at free space");
    asm.insert(rcv_nxt, wrote);

    // Drain: the assembler tracked 50 bytes.
    let drained = asm.drain_contiguous(rcv_nxt);
    assert_eq_test!(drained, 50, "drained what fit");
    recv.buf.advance_head(drained);

    assert_eq_test!(recv.buf.len(), 32768, "buffer now full");
    pass!()
}

// -----------------------------------------------------------------------------
// Eviction policy
// -----------------------------------------------------------------------------

/// When the assembler is full (16 ranges) and a lower-seq range arrives,
/// the highest-seq range is evicted to keep segments near the gap.
pub fn test_reasm_eviction_keeps_lowest() -> TestResult {
    let mut asm = fresh_asm();

    // Fill all 16 slots with ranges at seq 200, 216, 232, ...
    for i in 0..16 {
        asm.insert(200 + (i as u32) * 16, 8);
    }
    assert_eq_test!(asm.range_count(), 16, "full");

    // Insert a lower-seq range — should evict the highest.
    asm.insert(100, 8);
    assert_eq_test!(asm.range_count(), 16, "still full after eviction");

    let (blocks, _) = asm.sack_blocks();
    assert_eq_test!(blocks[0].0, 100, "lowest range preserved");
    pass!()
}

// -----------------------------------------------------------------------------
// Commutativity fuzz
// -----------------------------------------------------------------------------

fn splitmix32(state: &mut u64) -> u32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) as u32
}

/// Insert the same set of non-overlapping ranges in different random orders.
/// The Assembler (and ring buffer contents) must produce identical results.
pub fn test_reasm_insert_order_commutative_fuzz() -> TestResult {
    let mut seed = 0x01234567_89ABCDEFu64;

    for _trial in 0..16 {
        const N: usize = 6;
        const CHUNK: usize = 32;
        const BASE: u32 = 1000;
        let mut payloads: [[u8; CHUNK]; N] = [[0; CHUNK]; N];
        for (i, p) in payloads.iter_mut().enumerate() {
            p[0] = b'A' + i as u8;
            p[1] = b'0' + i as u8;
        }

        let mut order_a = [0usize; N];
        let mut order_b = [0usize; N];
        for i in 0..N {
            order_a[i] = i;
            order_b[i] = i;
        }
        for i in (1..N).rev() {
            let j = (splitmix32(&mut seed) as usize) % (i + 1);
            order_a.swap(i, j);
        }
        for i in (1..N).rev() {
            let j = (splitmix32(&mut seed) as usize) % (i + 1);
            order_b.swap(i, j);
        }

        // Order A: write into ring buffer + assembler.
        let mut asm_a = fresh_asm();
        let mut recv_a = fresh_recv();
        for idx in order_a {
            let seq = BASE + (idx as u32) * CHUNK as u32;
            let offset = seq.wrapping_sub(BASE) as usize;
            recv_a.buf.write_at_offset(offset, &payloads[idx]);
            asm_a.insert(seq, CHUNK);
        }
        let drained_a = asm_a.drain_contiguous(BASE);
        recv_a.buf.advance_head(drained_a);
        let out_a = drain_to_vec(&mut recv_a);

        // Order B.
        let mut asm_b = fresh_asm();
        let mut recv_b = fresh_recv();
        for idx in order_b {
            let seq = BASE + (idx as u32) * CHUNK as u32;
            let offset = seq.wrapping_sub(BASE) as usize;
            recv_b.buf.write_at_offset(offset, &payloads[idx]);
            asm_b.insert(seq, CHUNK);
        }
        let drained_b = asm_b.drain_contiguous(BASE);
        recv_b.buf.advance_head(drained_b);
        let out_b = drain_to_vec(&mut recv_b);

        if out_a != out_b {
            return fail!("drain output differs for permuted inserts");
        }
    }
    pass!()
}

// =============================================================================
// Register the test suite
// =============================================================================

slopos_testing::stest!(name = test_reasm_single_ooo_segment, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_non_contiguous_ranges, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_overlapping_merge, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_adjacent_merge, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_drain_integration, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_sack_blocks, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_duplicate_is_noop, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_write_at_offset_wrap, suite = tcp_reasm);
slopos_testing::stest!(
    name = test_reasm_write_at_offset_capacity,
    suite = tcp_reasm
);
slopos_testing::stest!(name = test_reasm_seq_wrap, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_drain_respects_capacity, suite = tcp_reasm);
slopos_testing::stest!(name = test_reasm_eviction_keeps_lowest, suite = tcp_reasm);
slopos_testing::stest!(
    name = test_reasm_insert_order_commutative_fuzz,
    suite = tcp_reasm
);
