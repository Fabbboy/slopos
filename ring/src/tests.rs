//! SlopRing kernel-side tests.
//!
//! These exercise the pieces that do not require a live userspace
//! process: the shared-region volatile accessor round-trips, the ring
//! object's CQE post / overflow / reserve logic, the in-flight table,
//! and `OP_CANCEL` semantics. End-to-end opcode *parity* (driving the
//! same input through an opcode and its sync syscall and diffing the
//! result) is a userland test because it needs a mapped ring and a
//! process fd table — see `userland/` for those.

use slopos_abi::Errno;
use slopos_abi::ring::{
    Cqe, IouringBuf, OP_ACCEPT, OP_CANCEL, OP_NOP, OP_POLL_ADD, OP_READ, RingLayout,
    SLOPRING_ASYNC_CANCEL_ALL, SLOPRING_CQ_OVERFLOW, SLOPRING_CQE_BUFFER_MASK,
    SLOPRING_CQE_BUFFER_SHIFT, SLOPRING_CQE_F_BUFFER, SLOPRING_CQE_F_MORE, SLOPRING_CQE_F_NOTIF,
    SLOPRING_SQE_MULTISHOT, Sqe,
};
use slopos_mm::pinned_user_buffer::PinnedUserBuffer;
use slopos_ostd::KVec;
use slopos_testing::{TestResult, stest};

use crate::buffers::BufferRegistry;
use crate::region::RingRegion;
use crate::ring_obj::{InFlight, Ring, heapless_vec::InFlightVec};

fn make_ring(entries: u32) -> Ring {
    let layout = RingLayout::new(entries);
    let n_pages = (layout.region_bytes as usize).div_ceil(4096);
    let region = RingRegion::alloc(n_pages).expect("ring test: region alloc");
    // Initialise control words the post/submit paths read.
    region
        .store_u32_release(layout.sq_off_mask as usize, layout.sq_entries - 1)
        .unwrap();
    region
        .store_u32_release(layout.cq_off_mask as usize, layout.cq_entries - 1)
        .unwrap();
    region
        .store_u32_release(layout.cq_off_head as usize, 0)
        .unwrap();
    region
        .store_u32_release(layout.cq_off_tail as usize, 0)
        .unwrap();
    region
        .store_u32_release(layout.sq_off_head as usize, 0)
        .unwrap();
    region
        .store_u32_release(layout.sq_off_tail as usize, 0)
        .unwrap();
    region
        .store_u32_release(layout.cq_off_flags as usize, 0)
        .unwrap();
    Ring {
        region,
        layout,
        sq_head: 0,
        cq_tail: 0,
        inflight: InFlightVec::with_capacity(layout.cq_entries as usize),
        user_addr: 0,
        owner_pid: 0,
        cq_overflow: 0,
        buffers: slopos_ostd::KBox::try_new(crate::buffers::BufferRegistry::new())
            .expect("ring test: buffer registry alloc"),
    }
}

fn inflight(user_data: u64, opcode: u8) -> InFlight {
    InFlight {
        user_data,
        opcode,
        fd: 3,
        addr: 0,
        addr2: 0,
        len: 0,
        op_flags: 0,
        off: 0,
        deadline_ms: 0,
        is_multishot: false,
        last_revents: 0,
        buf_group: 0,
        buf_index: 0,
        buf_flags: 0,
    }
}

// ---------------------------------------------------------------------------
// Region volatile accessor round-trips.
// ---------------------------------------------------------------------------

fn region_u32_round_trip() -> TestResult {
    let r = match RingRegion::alloc(1) {
        Ok(r) => r,
        Err(_) => return slopos_testing::fail!("region alloc failed"),
    };
    if r.store_u32_release(0, 0xdead_beef).is_err() {
        return slopos_testing::fail!("store failed");
    }
    match r.load_u32_acquire(0) {
        Ok(0xdead_beef) => TestResult::Pass,
        other => slopos_testing::fail!("u32 round-trip mismatch: {:?}", other),
    }
}
stest!(name = region_u32_round_trip, suite = slopring);

fn region_sqe_byte_round_trip() -> TestResult {
    let r = match RingRegion::alloc(1) {
        Ok(r) => r,
        Err(_) => return slopos_testing::fail!("region alloc failed"),
    };
    let sqe = Sqe {
        opcode: OP_READ,
        flags: 0,
        _pad0: 0,
        fd: 9,
        off: 0x1122_3344,
        addr: 0xcafe,
        len: 4096,
        op_flags: 1,
        user_data: 0xfeed_face_dead_beef,
        addr2: 0x5000,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    let bytes = sqe.to_bytes();
    if r.copy_in(128, &bytes).is_err() {
        return slopos_testing::fail!("copy_in failed");
    }
    let mut out = [0u8; 64];
    if r.copy_out(128, &mut out).is_err() {
        return slopos_testing::fail!("copy_out failed");
    }
    if Sqe::from_bytes(&out) == sqe {
        TestResult::Pass
    } else {
        slopos_testing::fail!("SQE byte round-trip mismatch")
    }
}
stest!(name = region_sqe_byte_round_trip, suite = slopring);

fn region_rejects_straddle() -> TestResult {
    let r = match RingRegion::alloc(2) {
        Ok(r) => r,
        Err(_) => return slopos_testing::fail!("region alloc failed"),
    };
    // A 64-byte copy starting 32 bytes before a page boundary straddles
    // two frames — must be rejected (the ABI layout never places one
    // there, but the guard is structural).
    let mut out = [0u8; 64];
    if r.copy_out(4096 - 32, &mut out).is_ok() {
        return slopos_testing::fail!("straddling copy_out should fail");
    }
    TestResult::Pass
}
stest!(name = region_rejects_straddle, suite = slopring);

// ---------------------------------------------------------------------------
// CQE post / overflow / reserve.
// ---------------------------------------------------------------------------

fn cqe_post_advances_tail() -> TestResult {
    let mut ring = make_ring(4);
    let posted = ring.post_cqe(0x1234, 42, 0).unwrap_or(false);
    if !posted {
        return slopos_testing::fail!("post_cqe returned full on empty CQ");
    }
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("cq_tail not advanced");
    }
    // Read the CQE back out of the region.
    let off = ring.layout.cqe_off(0) as usize;
    let mut bytes = [0u8; 16];
    ring.region.copy_out(off, &mut bytes).unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    if cqe.user_data == 0x1234 && cqe.res == 42 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("CQE content mismatch: {:?}", cqe)
    }
}
stest!(name = cqe_post_advances_tail, suite = slopring);

fn cqe_overflow_counts_when_full() -> TestResult {
    // entries=1 → cq_entries=2. Fill it, then overflow.
    let mut ring = make_ring(1);
    assert_post(&mut ring, 1, true);
    assert_post(&mut ring, 2, true);
    // Flag is clear while the CQ is not yet overflowing.
    let flags_before = ring
        .region
        .load_u32_acquire(ring.layout.cq_off_flags as usize)
        .unwrap_or(SLOPRING_CQ_OVERFLOW);
    if (flags_before & SLOPRING_CQ_OVERFLOW) != 0 {
        return slopos_testing::fail!("CQ-overflow flag set before any drop");
    }
    // CQ is now full (2/2); next post overflows.
    let posted = ring.post_cqe(3, 0, 0).unwrap_or(true);
    if posted {
        return slopos_testing::fail!("expected CQ-full drop");
    }
    if ring.cq_overflow != 1 {
        return slopos_testing::fail!("cq_overflow not incremented");
    }
    // The drop must raise the shared CQ-overflow flag so userland sees it.
    let flags_after = ring
        .region
        .load_u32_acquire(ring.layout.cq_off_flags as usize)
        .unwrap_or(0);
    if (flags_after & SLOPRING_CQ_OVERFLOW) == 0 {
        return slopos_testing::fail!("CQ-overflow flag not set after drop");
    }
    TestResult::Pass
}
stest!(name = cqe_overflow_counts_when_full, suite = slopring);

fn assert_post(ring: &mut Ring, ud: u64, want: bool) {
    let got = ring.post_cqe(ud, 0, 0).unwrap_or(!want);
    assert_eq!(got, want, "post_cqe({ud}) expected {want}");
}

fn cq_full_predicate() -> TestResult {
    let ring = make_ring(1); // cq_entries = 2
    if ring.cq_full(0) {
        return slopos_testing::fail!("empty CQ reported full");
    }
    // Simulate tail at capacity vs head 0.
    let mut ring2 = make_ring(1);
    ring2.cq_tail = 2;
    if !ring2.cq_full(0) {
        return slopos_testing::fail!("full CQ not reported full");
    }
    TestResult::Pass
}
stest!(name = cq_full_predicate, suite = slopring);

// ---------------------------------------------------------------------------
// In-flight table.
// ---------------------------------------------------------------------------

fn inflight_push_until_full() -> TestResult {
    let mut t = InFlightVec::with_capacity(2);
    if !t.push(inflight(1, OP_READ)) {
        return slopos_testing::fail!("first push failed");
    }
    if !t.push(inflight(2, OP_READ)) {
        return slopos_testing::fail!("second push failed");
    }
    if t.push(inflight(3, OP_READ)) {
        return slopos_testing::fail!("push past capacity should fail");
    }
    if t.len() != 2 {
        return slopos_testing::fail!("len mismatch");
    }
    TestResult::Pass
}
stest!(name = inflight_push_until_full, suite = slopring);

fn inflight_find_and_remove() -> TestResult {
    let mut t = InFlightVec::with_capacity(4);
    t.push(inflight(10, OP_READ));
    t.push(inflight(20, OP_READ));
    t.push(inflight(30, OP_READ));
    let Some(i) = t.find_user_data(20) else {
        return slopos_testing::fail!("find_user_data(20) missed");
    };
    let removed = t.remove_at(i).map(|r| r.user_data);
    if removed != Some(20) {
        return slopos_testing::fail!("remove returned wrong row: {:?}", removed);
    }
    if t.find_user_data(20).is_some() {
        return slopos_testing::fail!("removed row still found");
    }
    // The other rows survive.
    if t.find_user_data(10).is_none() || t.find_user_data(30).is_none() {
        return slopos_testing::fail!("swap-remove lost a surviving row");
    }
    TestResult::Pass
}
stest!(name = inflight_find_and_remove, suite = slopring);

// ---------------------------------------------------------------------------
// OP_CANCEL semantics (SLOPRING § 10).
// ---------------------------------------------------------------------------

fn cancel_pending_posts_ecanceled() -> TestResult {
    let mut ring = make_ring(4);
    ring.inflight.push(inflight(0xaaaa, OP_READ));
    ring.inflight.push(inflight(0xbbbb, OP_READ));
    let cancel = Sqe {
        opcode: OP_CANCEL,
        flags: 0,
        _pad0: 0,
        fd: -1,
        off: 0,
        addr: 0xaaaa, // target user_data
        len: 0,
        op_flags: 0,
        user_data: 0xc0,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    crate::enter::process_sqe_for_test(0, &mut ring, &cancel);
    // The cancelled op's row is gone.
    if ring.inflight.find_user_data(0xaaaa).is_some() {
        return slopos_testing::fail!("cancelled row not removed");
    }
    // Two CQEs posted: the cancelled op (-ECANCELED) and the cancel
    // result (0 found). cq_tail advanced by 2.
    if ring.cq_tail != 2 {
        return slopos_testing::fail!("expected 2 CQEs, cq_tail={}", ring.cq_tail);
    }
    TestResult::Pass
}
stest!(name = cancel_pending_posts_ecanceled, suite = slopring);

fn cancel_missing_returns_enoent() -> TestResult {
    let mut ring = make_ring(4);
    let cancel = Sqe {
        opcode: OP_CANCEL,
        flags: 0,
        _pad0: 0,
        fd: -1,
        off: 0,
        addr: 0xdead, // no such in-flight op
        len: 0,
        op_flags: 0,
        user_data: 0xc1,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    crate::enter::process_sqe_for_test(0, &mut ring, &cancel);
    // One CQE posted: the cancel result (-ENOENT).
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("expected 1 CQE, cq_tail={}", ring.cq_tail);
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    // `Errno::raw()` is already negative; the CQE carries the negated
    // errno (`-ENOENT`) verbatim, matching the userland `res < 0` check.
    let enoent = slopos_abi::Errno::ENOENT.raw();
    if cqe.user_data == 0xc1 && cqe.res == enoent {
        TestResult::Pass
    } else {
        slopos_testing::fail!("cancel-miss CQE mismatch: {:?}", cqe)
    }
}
stest!(name = cancel_missing_returns_enoent, suite = slopring);

fn cancel_all_removes_every_match() -> TestResult {
    let mut ring = make_ring(8);
    // Three rows with the same user_data (cancel_all targets user_data).
    ring.inflight.push(inflight(0x77, OP_READ));
    ring.inflight.push(inflight(0x77, OP_READ));
    ring.inflight.push(inflight(0x77, OP_READ));
    let cancel = Sqe {
        opcode: OP_CANCEL,
        flags: 0,
        _pad0: 0,
        fd: -1,
        off: 0,
        addr: 0x77,
        len: 0,
        op_flags: SLOPRING_ASYNC_CANCEL_ALL,
        user_data: 0xc2,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    crate::enter::process_sqe_for_test(0, &mut ring, &cancel);
    if ring.inflight.find_user_data(0x77).is_some() {
        return slopos_testing::fail!("cancel_all left a row");
    }
    // 3 cancelled + 1 cancel-result = 4 CQEs.
    if ring.cq_tail != 4 {
        return slopos_testing::fail!("expected 4 CQEs, got {}", ring.cq_tail);
    }
    TestResult::Pass
}
stest!(name = cancel_all_removes_every_match, suite = slopring);

// ---------------------------------------------------------------------------
// NOP inline completion.
// ---------------------------------------------------------------------------

fn nop_completes_inline() -> TestResult {
    let mut ring = make_ring(4);
    let nop = Sqe {
        opcode: OP_NOP,
        flags: 0,
        _pad0: 0,
        fd: -1,
        off: 0,
        addr: 0,
        len: 0,
        op_flags: 0,
        user_data: 0x9999,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    crate::enter::process_sqe_for_test(0, &mut ring, &nop);
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("NOP did not post a CQE");
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    if cqe.user_data == 0x9999 && cqe.res == 0 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("NOP CQE mismatch: {:?}", cqe)
    }
}
stest!(name = nop_completes_inline, suite = slopring);

// ---------------------------------------------------------------------------
// Layout invariants (mirror the abi-side host tests, run in-kernel).
// ---------------------------------------------------------------------------

fn layout_arrays_page_aligned() -> TestResult {
    for &e in &[1u32, 2, 16, 256, 4096] {
        let l = RingLayout::new(e);
        if l.sqe_array_off % 4096 != 0 || l.cqe_array_off % 4096 != 0 {
            return slopos_testing::fail!("arrays not page-aligned for entries={}", e);
        }
        if l.cqe_array_off + l.cq_entries * 16 > l.region_bytes {
            return slopos_testing::fail!("CQE array overruns region for entries={}", e);
        }
    }
    TestResult::Pass
}
stest!(name = layout_arrays_page_aligned, suite = slopring);

// ---------------------------------------------------------------------------
// Multishot (ABI v2) — F_MORE / edge-cache / cancel / buffer bits.
// ---------------------------------------------------------------------------

/// A CQE posted with `SLOPRING_CQE_F_MORE` carries that bit verbatim into
/// the shared CQ — the interim-completion marker the userland reactor uses
/// to keep an armed multishot slot alive.
fn post_cqe_carries_f_more() -> TestResult {
    let mut ring = make_ring(4);
    if !ring.post_cqe(0x55, 7, SLOPRING_CQE_F_MORE).unwrap_or(false) {
        return slopos_testing::fail!("post_cqe(F_MORE) reported full on empty CQ");
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    if cqe.user_data == 0x55 && cqe.res == 7 && (cqe.flags & SLOPRING_CQE_F_MORE) != 0 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("interim CQE missing F_MORE: {:?}", cqe)
    }
}
stest!(name = post_cqe_carries_f_more, suite = slopring);

/// `set_last_revents` mutates the *live* in-flight row (the OP_POLL_ADD
/// edge cache), so the harvest's snapshot-walk decision is reflected back
/// onto the persistent row.
fn set_last_revents_updates_live_row() -> TestResult {
    let mut t = InFlightVec::with_capacity(4);
    let mut row = inflight(0x1234, OP_POLL_ADD);
    row.is_multishot = true;
    t.push(row);
    t.set_last_revents(0x1234, 0x0001);
    // Re-fetch via snapshot (the harvest's read path).
    let snap = t.snapshot();
    let found = snap.iter().find(|r| r.user_data == 0x1234);
    match found {
        Some(r) if r.last_revents == 0x0001 && r.is_multishot => TestResult::Pass,
        other => slopos_testing::fail!("last_revents not updated on live row: {:?}", other),
    }
}
stest!(name = set_last_revents_updates_live_row, suite = slopring);

/// Cancelling an armed multishot row posts exactly one terminal CQE with
/// `-ECANCELED` and **F_MORE clear** (SLOPRING §1.3 trigger 4), and
/// removes the row.
fn multishot_cancel_clears_more() -> TestResult {
    let mut ring = make_ring(8);
    let mut row = inflight(0xabcd, OP_ACCEPT);
    row.is_multishot = true;
    ring.inflight.push(row);
    let cancel = Sqe {
        opcode: OP_CANCEL,
        flags: 0,
        _pad0: 0,
        fd: -1,
        off: 0,
        addr: 0xabcd,
        len: 0,
        op_flags: 0,
        user_data: 0xc9,
        addr2: 0,
        sqe_flags2: 0,
        buf_group: 0,
        buf_index: 0,
        _resv0: 0,
        _resv1: 0,
    };
    crate::enter::process_sqe_for_test(0, &mut ring, &cancel);
    if ring.inflight.find_user_data(0xabcd).is_some() {
        return slopos_testing::fail!("cancelled multishot row not removed");
    }
    // CQE 0 = the cancelled op (-ECANCELED, F_MORE clear).
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    let ecanceled = slopos_abi::Errno::ECANCELED.raw();
    if cqe.user_data == 0xabcd && cqe.res == ecanceled && (cqe.flags & SLOPRING_CQE_F_MORE) == 0 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("multishot cancel CQE mismatch: {:?}", cqe)
    }
}
stest!(name = multishot_cancel_clears_more, suite = slopring);

/// ABI v2: `SLOPRING_SQE_MULTISHOT` survives the SQE byte round-trip in
/// `sqe_flags2`, and `inflight_from` reads it back as `is_multishot`.
fn sqe_multishot_flag_round_trips() -> TestResult {
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_ACCEPT;
    sqe.fd = 4;
    sqe.sqe_flags2 = SLOPRING_SQE_MULTISHOT;
    sqe.user_data = 0xfeed;
    let round = Sqe::from_bytes(&sqe.to_bytes());
    if round != sqe {
        return slopos_testing::fail!("SQE v2 round-trip mismatch");
    }
    let row = crate::opcode::inflight_from(&round, 0);
    if row.is_multishot && row.last_revents == 0 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("inflight_from did not arm multishot")
    }
}
stest!(name = sqe_multishot_flag_round_trips, suite = slopring);

/// CQE provided-buffer bits pack a buffer id into the high 16 bits
/// alongside `F_BUFFER`; the shift/mask recover it. Freezes the Phase-4
/// ABI in-kernel.
fn cqe_buffer_bits_pack() -> TestResult {
    let bid: u32 = 0x1357;
    let flags = SLOPRING_CQE_F_BUFFER | (bid << SLOPRING_CQE_BUFFER_SHIFT);
    let c = Cqe {
        user_data: 0xaa,
        res: 3,
        flags,
    };
    let round = Cqe::from_bytes(&c.to_bytes());
    let got_bid = (round.flags & SLOPRING_CQE_BUFFER_MASK) >> SLOPRING_CQE_BUFFER_SHIFT;
    if round == c && (round.flags & SLOPRING_CQE_F_BUFFER) != 0 && got_bid == bid {
        TestResult::Pass
    } else {
        slopos_testing::fail!("CQE buffer-bit pack/unpack mismatch: {:?}", round)
    }
}
stest!(name = cqe_buffer_bits_pack, suite = slopring);

/// The zero-copy-send notification flag (`SLOPRING_CQE_F_NOTIF`) is bit 3,
/// distinct from `F_MORE` (bit 0) and `F_BUFFER` (bit 1), and round-trips
/// through the wire `Cqe` encoding — the terminal CQE of an `OP_SEND_ZC` two-CQE
/// completion (first carries `F_MORE`, then this carries `F_NOTIF`).
fn cqe_notif_bit_pack() -> TestResult {
    // Distinct, non-overlapping flag bits.
    if SLOPRING_CQE_F_NOTIF == SLOPRING_CQE_F_MORE
        || SLOPRING_CQE_F_NOTIF == SLOPRING_CQE_F_BUFFER
        || SLOPRING_CQE_F_NOTIF != (1 << 3)
    {
        return slopos_testing::fail!("F_NOTIF must be a distinct bit 3");
    }
    // Result CQE: F_MORE set, F_NOTIF clear.
    let result = Cqe {
        user_data: 0xC0DE,
        res: 42,
        flags: SLOPRING_CQE_F_MORE,
    };
    // Terminal notification CQE: F_NOTIF set, F_MORE clear.
    let notif = Cqe {
        user_data: 0xC0DE,
        res: 0,
        flags: SLOPRING_CQE_F_NOTIF,
    };
    let rr = Cqe::from_bytes(&result.to_bytes());
    let rn = Cqe::from_bytes(&notif.to_bytes());
    if rr == result
        && (rr.flags & SLOPRING_CQE_F_MORE) != 0
        && (rr.flags & SLOPRING_CQE_F_NOTIF) == 0
        && rn == notif
        && (rn.flags & SLOPRING_CQE_F_NOTIF) != 0
        && (rn.flags & SLOPRING_CQE_F_MORE) == 0
    {
        TestResult::Pass
    } else {
        slopos_testing::fail!("F_NOTIF pack/unpack mismatch: {:?} / {:?}", rr, rn)
    }
}
stest!(name = cqe_notif_bit_pack, suite = slopring);

// ---------------------------------------------------------------------------
// Registered / provided buffer registry (ABI v2). These exercise the
// buffer-selection bookkeeping headless — fabricated pins over kernel frames,
// no live process VM — covering the check-out reservation lifecycle, the
// volatile stage/publish round-trip, and the provided-ring peek/commit cursor.
// ---------------------------------------------------------------------------

/// Fixed-buffer reservation lifecycle: in-range checkout succeeds, a second
/// checkout of the same index is `-EBUSY`, an out-of-range index is `-EINVAL`,
/// unregister while a buffer is held is `-EBUSY`, and after check-in the index
/// is reusable and the set unregisters.
fn bufpool_fixed_checkout_lifecycle() -> TestResult {
    let mut reg = BufferRegistry::new();
    let mut pins: KVec<PinnedUserBuffer> = KVec::new();
    for _ in 0..4 {
        let Some(p) = PinnedUserBuffer::alloc_for_test(256) else {
            return slopos_testing::fail!("pin alloc failed");
        };
        if pins.push(p).is_err() {
            return slopos_testing::fail!("pin push failed");
        }
    }
    reg.register_fixed_for_test(pins);

    if reg.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("checkout 0 should succeed");
    }
    if reg.check_out_fixed(0) != Err(Errno::EBUSY) {
        return slopos_testing::fail!("double checkout should be EBUSY");
    }
    if reg.check_out_fixed(4) != Err(Errno::EINVAL) {
        return slopos_testing::fail!("out-of-range index should be EINVAL");
    }
    if reg.unregister_fixed() != Err(Errno::EBUSY) {
        return slopos_testing::fail!("unregister while busy should be EBUSY");
    }
    reg.check_in_fixed(0);
    if reg.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("recheckout after check-in should succeed");
    }
    reg.check_in_fixed(0);
    if reg.unregister_fixed().is_err() {
        return slopos_testing::fail!("idle unregister should succeed");
    }
    if reg.unregister_fixed() != Err(Errno::EINVAL) {
        return slopos_testing::fail!("unregister with no set should be EINVAL");
    }
    TestResult::Pass
}
stest!(name = bufpool_fixed_checkout_lifecycle, suite = slopring);

/// Single-direct-copy cursor round-trip: a pattern seeded into a fixed pin
/// reads back through a `VmReader` (`fixed_reader`, the send source), and a
/// fresh pattern written through a `VmWriter` (`fixed_writer`, the recv sink)
/// lands in the pin and reads back. Exercises the scratch-free path the net
/// leaves use, without a live socket.
fn bufpool_fixed_cursor_roundtrip() -> TestResult {
    let mut reg = BufferRegistry::new();
    let mut pins: KVec<PinnedUserBuffer> = KVec::new();
    let Some(p) = PinnedUserBuffer::alloc_for_test(64) else {
        return slopos_testing::fail!("pin alloc failed");
    };
    let pattern_a = [0xABu8; 16];
    if p.copy_in(0, &pattern_a).is_err() {
        return slopos_testing::fail!("seed copy_in failed");
    }
    if pins.push(p).is_err() {
        return slopos_testing::fail!("pin push failed");
    }
    reg.register_fixed_for_test(pins);

    // Read the seeded pattern out through a VmReader (the send source).
    {
        let mut reader = match reg.fixed_reader(0, 16) {
            Ok(r) => r,
            Err(_) => return slopos_testing::fail!("fixed_reader err"),
        };
        if reader.remain() != 16 {
            return slopos_testing::fail!("fixed_reader remain != 16");
        }
        let mut out = [0u8; 16];
        if reader.read(&mut out) != 16 {
            return slopos_testing::fail!("fixed_reader short read");
        }
        if out != pattern_a {
            return slopos_testing::fail!("fixed_reader content mismatch");
        }
    }

    // Write a fresh pattern into the pin through a VmWriter (the recv sink).
    {
        let mut writer = match reg.fixed_writer(0) {
            Ok(w) => w,
            Err(_) => return slopos_testing::fail!("fixed_writer err"),
        };
        let src = [0xCDu8; 16];
        if writer.write(&src) != 16 {
            return slopos_testing::fail!("fixed_writer short write");
        }
    }

    // Read it back through a fresh reader.
    {
        let mut reader = match reg.fixed_reader(0, 16) {
            Ok(r) => r,
            Err(_) => return slopos_testing::fail!("readback fixed_reader err"),
        };
        let mut out = [0u8; 16];
        reader.read(&mut out);
        if out.iter().any(|&b| b != 0xCD) {
            return slopos_testing::fail!("writer readback mismatch");
        }
    }

    // Out-of-range / no-set error paths.
    if reg.fixed_reader(7, 16).is_ok() {
        return slopos_testing::fail!("fixed_reader OOB should err");
    }
    TestResult::Pass
}
stest!(name = bufpool_fixed_cursor_roundtrip, suite = slopring);

/// Provided-ring peek/commit cursor: two published buffers are peeked in order
/// (reporting their bids), the producer `tail` overlapping `bufs[0].resv` is
/// honoured, an exhausted ring peeks `None`, and an unknown group is `-EINVAL`.
fn bufpool_provided_peek_commit() -> TestResult {
    let mut reg = BufferRegistry::new();
    let Some(ring) = PinnedUserBuffer::alloc_for_test(4 * 16) else {
        return slopos_testing::fail!("ring alloc failed");
    };
    // Slot 0's `resv` doubles as the producer `tail` (== 2 published buffers).
    let e0 = IouringBuf {
        addr: 0x1000,
        len: 64,
        bid: 10,
        resv: 2,
    };
    let e1 = IouringBuf {
        addr: 0x2000,
        len: 64,
        bid: 11,
        resv: 0,
    };
    if ring.copy_in(0, &e0.to_bytes()).is_err() || ring.copy_in(16, &e1.to_bytes()).is_err() {
        return slopos_testing::fail!("ring seed failed");
    }
    reg.register_provided_for_test(1, ring, 4);

    match reg.peek_provided(1) {
        Ok(Some(b)) if b.bid == 10 && b.addr == 0x1000 => {}
        other => return slopos_testing::fail!("peek slot 0 wrong: {:?}", other),
    }
    reg.commit_provided(1);
    match reg.peek_provided(1) {
        Ok(Some(b)) if b.bid == 11 && b.addr == 0x2000 => {}
        other => return slopos_testing::fail!("peek slot 1 wrong: {:?}", other),
    }
    reg.commit_provided(1);
    match reg.peek_provided(1) {
        Ok(None) => {}
        other => return slopos_testing::fail!("exhausted ring should peek None: {:?}", other),
    }
    if !matches!(reg.peek_provided(2), Err(Errno::EINVAL)) {
        return slopos_testing::fail!("unknown group should be EINVAL");
    }
    TestResult::Pass
}
stest!(name = bufpool_provided_peek_commit, suite = slopring);
