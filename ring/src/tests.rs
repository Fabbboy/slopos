//! SlopRing kernel-side tests.
//!
//! These exercise the pieces that do not require a live userspace
//! process: the shared-region volatile accessor round-trips, the ring
//! object's CQE post / overflow / reserve logic, the in-flight table,
//! and `OP_CANCEL` semantics. End-to-end opcode *parity* (driving the
//! same input through an opcode and its sync syscall and diffing the
//! result) is a userland test because it needs a mapped ring and a
//! process fd table — see `userland/` for those.

use slopos_abi::ring::{
    Cqe, OP_CANCEL, OP_NOP, OP_READ, RingLayout, SLOPRING_ASYNC_CANCEL_ALL, Sqe,
};
use slopos_testing::{TestResult, stest};

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
    Ring {
        region,
        layout,
        sq_head: 0,
        cq_tail: 0,
        inflight: InFlightVec::with_capacity(layout.cq_entries as usize),
        user_addr: 0,
        owner_pid: 0,
        cq_overflow: 0,
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
        _resv: [0, 0],
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
    let posted = ring.post_cqe(0x1234, 42).unwrap_or(false);
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
    // CQ is now full (2/2); next post overflows.
    let posted = ring.post_cqe(3, 0).unwrap_or(true);
    if posted {
        return slopos_testing::fail!("expected CQ-full drop");
    }
    if ring.cq_overflow != 1 {
        return slopos_testing::fail!("cq_overflow not incremented");
    }
    TestResult::Pass
}
stest!(name = cqe_overflow_counts_when_full, suite = slopring);

fn assert_post(ring: &mut Ring, ud: u64, want: bool) {
    let got = ring.post_cqe(ud, 0).unwrap_or(!want);
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
        _resv: [0, 0],
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
        _resv: [0, 0],
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
    let enoent = -(slopos_abi::Errno::ENOENT.raw() as i32);
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
        _resv: [0, 0],
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
        _resv: [0, 0],
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
