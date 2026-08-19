//! SlopRing kernel-side tests: the shared-region volatile accessors, the ring
//! object's CQE post / overflow / reserve logic, the in-flight table, and
//! `OP_CANCEL` semantics. End-to-end opcode *parity* needs a mapped ring and a
//! process fd table, so it lives in `userland/`.

use slopos_abi::Errno;
use slopos_abi::ring::{
    Cqe, IouringBuf, OP_ACCEPT, OP_CANCEL, OP_CONNECT, OP_NOP, OP_POLL_ADD, OP_READ, RingLayout,
    SLOPRING_ASYNC_CANCEL_ALL, SLOPRING_CQ_OVERFLOW, SLOPRING_CQE_BUFFER_MASK,
    SLOPRING_CQE_BUFFER_SHIFT, SLOPRING_CQE_F_BUFFER, SLOPRING_CQE_F_MORE, SLOPRING_CQE_F_NOTIF,
    SLOPRING_SQE_BUFFER_SELECT, SLOPRING_SQE_FIXED_BUFFER, SLOPRING_SQE_MULTISHOT, Sqe,
};
use slopos_fs::fileio::FdTable;
use slopos_mm::pinned_user_buffer::PinnedUserBuffer;
use slopos_ostd::KVec;
use slopos_ostd::{TxReclaimToken, ZcNotifToken};
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
        owner: FdTable::Kernel,
        allowed_ops: crate::opcode::OpcodeSet::all(),
        cq_overflow: 0,
        buffers: slopos_ostd::KBox::try_new(crate::buffers::BufferRegistry::new())
            .expect("ring test: buffer registry alloc"),
        pending_reap: KVec::with_capacity(layout.cq_entries as usize)
            .expect("ring test: reap alloc"),
    }
}

fn inflight(user_data: u64, opcode: u8) -> InFlight {
    InFlight {
        user_data,
        opcode,
        file: None,
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
    // 4096-32 puts a 64-byte copy across two frames. The ABI layout never
    // places one there; the guard is structural.
    let mut out = [0u8; 64];
    if r.copy_out(4096 - 32, &mut out).is_ok() {
        return slopos_testing::fail!("straddling copy_out should fail");
    }
    TestResult::Pass
}
stest!(name = region_rejects_straddle, suite = slopring);

fn cqe_post_advances_tail() -> TestResult {
    let mut ring = make_ring(4);
    let posted = ring.post_cqe(0x1234, 42, 0).unwrap_or(false);
    if !posted {
        return slopos_testing::fail!("post_cqe returned full on empty CQ");
    }
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("cq_tail not advanced");
    }
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
    // entries=1 → cq_entries=2.
    let mut ring = make_ring(1);
    assert_post(&mut ring, 1, true);
    assert_post(&mut ring, 2, true);
    let flags_before = ring
        .region
        .load_u32_acquire(ring.layout.cq_off_flags as usize)
        .unwrap_or(SLOPRING_CQ_OVERFLOW);
    if (flags_before & SLOPRING_CQ_OVERFLOW) != 0 {
        return slopos_testing::fail!("CQ-overflow flag set before any drop");
    }
    let posted = ring.post_cqe(3, 0, 0).unwrap_or(true);
    if posted {
        return slopos_testing::fail!("expected CQ-full drop");
    }
    if ring.cq_overflow != 1 {
        return slopos_testing::fail!("cq_overflow not incremented");
    }
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
    let mut ring2 = make_ring(1);
    ring2.cq_tail = 2;
    if !ring2.cq_full(0) {
        return slopos_testing::fail!("full CQ not reported full");
    }
    TestResult::Pass
}
stest!(name = cq_full_predicate, suite = slopring);

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
    if t.find_user_data(10).is_none() || t.find_user_data(30).is_none() {
        return slopos_testing::fail!("swap-remove lost a surviving row");
    }
    TestResult::Pass
}
stest!(name = inflight_find_and_remove, suite = slopring);

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
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &cancel);
    if ring.inflight.find_user_data(0xaaaa).is_some() {
        return slopos_testing::fail!("cancelled row not removed");
    }
    // Two CQEs: the cancelled op (-ECANCELED) and the cancel result (0 found).
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
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &cancel);
    // One CQE: the cancel result (-ENOENT).
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("expected 1 CQE, cq_tail={}", ring.cq_tail);
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    // `Errno::raw()` is already negative; the CQE carries it verbatim.
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
    // Three rows share a user_data: cancel_all targets user_data.
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
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &cancel);
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
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &nop);
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

/// `SLOPRING_CQE_F_MORE` reaches the shared CQ verbatim — the
/// interim-completion marker that keeps an armed multishot slot alive.
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

/// `set_last_revents` mutates the *live* in-flight row, so the harvest's
/// snapshot-walk decision is reflected back onto the OP_POLL_ADD edge cache.
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
        Some(r) => slopos_testing::fail!(
            "last_revents not updated on live row: revents={:#x} multishot={}",
            r.last_revents,
            r.is_multishot
        ),
        None => slopos_testing::fail!("row missing from snapshot"),
    }
}
stest!(name = set_last_revents_updates_live_row, suite = slopring);

/// Cancelling an armed multishot row removes it and posts exactly one terminal
/// CQE with `-ECANCELED` and **F_MORE clear** (SLOPRING §1.3 trigger 4).
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
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &cancel);
    if ring.inflight.find_user_data(0xabcd).is_some() {
        return slopos_testing::fail!("cancelled multishot row not removed");
    }
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
    let row = crate::opcode::inflight_from(&round, 0, None);
    if row.is_multishot && row.last_revents == 0 {
        TestResult::Pass
    } else {
        slopos_testing::fail!("inflight_from did not arm multishot")
    }
}
stest!(name = sqe_multishot_flag_round_trips, suite = slopring);

/// CQE provided-buffer bits pack a buffer id into the high 16 bits alongside
/// `F_BUFFER`; the shift/mask recover it.
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
/// through the wire `Cqe` encoding — the terminal CQE of an `OP_SEND_ZC`
/// two-CQE completion.
fn cqe_notif_bit_pack() -> TestResult {
    if SLOPRING_CQE_F_NOTIF == SLOPRING_CQE_F_MORE
        || SLOPRING_CQE_F_NOTIF == SLOPRING_CQE_F_BUFFER
        || SLOPRING_CQE_F_NOTIF != (1 << 3)
    {
        return slopos_testing::fail!("F_NOTIF must be a distinct bit 3");
    }
    let result = Cqe {
        user_data: 0xC0DE,
        res: 42,
        flags: SLOPRING_CQE_F_MORE,
    };
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

/// Fixed-buffer reservation lifecycle over fabricated pins: checkout,
/// double-checkout, out-of-range, unregister-while-held, and reuse after
/// check-in.
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

/// Zero-copy deferred-notification lifecycle (`OP_SEND_ZC`): a deferred row
/// keeps the fixed buffer checked out until the driver reclaims the NIC TX
/// descriptor; only then does the harvest post the terminal `F_NOTIF` and check
/// the buffer back in. Drives the reclaim token directly, so no NIC is needed.
fn send_zc_deferred_notif_lifecycle() -> TestResult {
    let mut ring = make_ring(8);
    let mut pins: KVec<PinnedUserBuffer> = KVec::new();
    let Some(p) = PinnedUserBuffer::alloc_for_test(256) else {
        return slopos_testing::fail!("pin alloc failed");
    };
    if pins.push(p).is_err() {
        return slopos_testing::fail!("pin push failed");
    }
    ring.buffers.register_fixed_for_test(pins);

    // Mirrors `send_zc_fixed`: reserve, then record the deferred row.
    if ring.buffers.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("checkout failed");
    }
    let Some(token) = TxReclaimToken::new() else {
        return slopos_testing::fail!("token alloc failed");
    };
    let snap = token.snapshot();
    ring.buffers.push_deferred(0xDEF, token.clone(), snap, 0);

    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 0 {
        return slopos_testing::fail!(
            "no CQE should post before reclaim (cq_tail={})",
            ring.cq_tail
        );
    }
    if ring.buffers.check_out_fixed(0) != Err(Errno::EBUSY) {
        return slopos_testing::fail!("buffer must stay checked out pre-reclaim");
    }

    token.signal_reclaimed();
    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("exactly one F_NOTIF expected (cq_tail={})", ring.cq_tail);
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    if cqe.user_data != 0xDEF
        || (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0
        || (cqe.flags & SLOPRING_CQE_F_MORE) != 0
    {
        return slopos_testing::fail!("terminal CQE must be F_NOTIF only: {:?}", cqe);
    }
    if ring.buffers.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("buffer must be checked in after F_NOTIF");
    }
    TestResult::Pass
}
stest!(name = send_zc_deferred_notif_lifecycle, suite = slopring);

/// TCP `MSG_ZEROCOPY` deferred-notification lifecycle: a refcounted
/// [`ZcNotifToken`] keeps the fixed buffer checked out across **retransmits**
/// (each an extra in-flight DMA reference), reusable only once the bytes are
/// cumulatively ACKed **and** every DMA is reclaimed. Drives the token's
/// refcount directly, so no NIC is needed.
fn tcp_zc_pin_held_across_retransmit_and_ack() -> TestResult {
    let mut ring = make_ring(8);
    let mut pins: KVec<PinnedUserBuffer> = KVec::new();
    let Some(p) = PinnedUserBuffer::alloc_for_test(256) else {
        return slopos_testing::fail!("pin alloc failed");
    };
    if pins.push(p).is_err() {
        return slopos_testing::fail!("pin push failed");
    }
    ring.buffers.register_fixed_for_test(pins);

    // The send-queue chunk owns one token reference; the first transmit adds an
    // in-flight DMA reference.
    if ring.buffers.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("checkout failed");
    }
    let Some(token) = ZcNotifToken::new() else {
        return slopos_testing::fail!("token alloc failed");
    };
    token.acquire(); // first transmit DMA in flight (refs = chunk + 1)
    ring.buffers.push_deferred_notif(0x7C90, token.clone(), 0);

    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 0 {
        return slopos_testing::fail!("no CQE before reclaim (cq_tail={})", ring.cq_tail);
    }
    if ring.buffers.check_out_fixed(0) != Err(Errno::EBUSY) {
        return slopos_testing::fail!("buffer must stay checked out pre-reclaim");
    }

    token.acquire(); // retransmit DMA (refs = chunk + 2)
    token.release(); // first DMA reclaimed (refs = chunk + 1)
    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 0 {
        return slopos_testing::fail!("no CQE while a DMA is still in flight");
    }
    if ring.buffers.check_out_fixed(0) != Err(Errno::EBUSY) {
        return slopos_testing::fail!("buffer must stay held across the retransmit");
    }

    // Not yet ACKed: the chunk reference still holds the buffer.
    token.release(); // refs = chunk (1)
    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 0 {
        return slopos_testing::fail!("chunk reference must hold until cumulative ACK");
    }
    if ring.buffers.check_out_fixed(0) != Err(Errno::EBUSY) {
        return slopos_testing::fail!("buffer must stay held until ACK");
    }

    // Cumulative ACK drops the chunk reference → count reaches zero → reusable.
    token.mark_acked_and_release();
    let _ = crate::enter::harvest_step_for_test(FdTable::Kernel, &mut ring, 1);
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("exactly one F_NOTIF expected (cq_tail={})", ring.cq_tail);
    }
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(0) as usize, &mut bytes)
        .unwrap();
    let cqe = Cqe::from_bytes(&bytes);
    if (cqe.flags & SLOPRING_CQE_F_NOTIF) == 0 || (cqe.flags & SLOPRING_CQE_F_MORE) != 0 {
        return slopos_testing::fail!("terminal CQE must be F_NOTIF only: {:?}", cqe);
    }
    if ring.buffers.check_out_fixed(0).is_err() {
        return slopos_testing::fail!("buffer must be checked in after F_NOTIF");
    }
    TestResult::Pass
}
stest!(
    name = tcp_zc_pin_held_across_retransmit_and_ack,
    suite = slopring
);

/// `OP_CONNECT` dispatch + re-probe idempotency, driven through the submit path
/// without a process fd table: a negative fd posts inline `-EBADF`, a fixed or
/// provided buffer selection is rejected `-EINVAL`, and re-running the same SQE
/// yields a byte-stable result. The live handshake is a userland test — it needs
/// a mapped ring and a real socket.
fn connect_probe_dispatch_idempotent() -> TestResult {
    let mut ring = make_ring(8);

    let read_cqe = |ring: &Ring, idx: u32| -> Cqe {
        let mut bytes = [0u8; 16];
        ring.region
            .copy_out(ring.layout.cqe_off(idx) as usize, &mut bytes)
            .unwrap();
        Cqe::from_bytes(&bytes)
    };

    let mut bad_fd = Sqe::ZERO;
    bad_fd.opcode = OP_CONNECT;
    bad_fd.fd = -1;
    bad_fd.user_data = 0xC0;
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &bad_fd);
    if ring.cq_tail != 1 {
        return slopos_testing::fail!("expected one CQE for fd<0 (cq_tail={})", ring.cq_tail);
    }
    let c0 = read_cqe(&ring, 0);
    if c0.user_data != 0xC0 || c0.res != Errno::EBADF.raw() || c0.flags != 0 {
        return slopos_testing::fail!("fd<0 must post inline -EBADF, no flags: {:?}", c0);
    }

    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &bad_fd);
    if ring.cq_tail != 2 {
        return slopos_testing::fail!("re-run must post a second CQE (cq_tail={})", ring.cq_tail);
    }
    let c1 = read_cqe(&ring, 1);
    if c1.res != c0.res || c1.user_data != c0.user_data || c1.flags != c0.flags {
        return slopos_testing::fail!("re-probe result drifted: {:?} vs {:?}", c1, c0);
    }

    // `addr` is connect's immutable SockAddrIn input, not a bulk buffer.
    let mut prov = Sqe::ZERO;
    prov.opcode = OP_CONNECT;
    prov.fd = 3;
    prov.flags = SLOPRING_SQE_BUFFER_SELECT;
    prov.user_data = 0xC1;
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &prov);
    if read_cqe(&ring, 2).res != Errno::EINVAL.raw() {
        return slopos_testing::fail!("provided-buffer OP_CONNECT must be -EINVAL");
    }

    let mut fixed = Sqe::ZERO;
    fixed.opcode = OP_CONNECT;
    fixed.fd = 3;
    fixed.flags = SLOPRING_SQE_FIXED_BUFFER;
    fixed.user_data = 0xC2;
    crate::enter::process_sqe_for_test(FdTable::Kernel, &mut ring, &fixed);
    if read_cqe(&ring, 3).res != Errno::EINVAL.raw() {
        return slopos_testing::fail!("fixed-buffer OP_CONNECT must be -EINVAL");
    }

    TestResult::Pass
}
stest!(name = connect_probe_dispatch_idempotent, suite = slopring);

/// Single-direct-copy cursor round-trip: a pattern seeded into a fixed pin reads
/// back through `fixed_reader` (the send source), and one written through
/// `fixed_writer` (the recv sink) lands in the pin — the scratch-free path the
/// net leaves use, without a live socket.
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

fn read_cqe_at(ring: &Ring, idx: u32) -> Cqe {
    let mut bytes = [0u8; 16];
    ring.region
        .copy_out(ring.layout.cqe_off(idx) as usize, &mut bytes)
        .unwrap();
    Cqe::from_bytes(&bytes)
}

/// Run `body` against a private descriptor table, torn down afterwards however
/// `body` exits. A pid with no table of its own is refused a descriptor, so a
/// test that opens files has to mint one the way process creation does.
fn with_fd_table(body: fn(FdTable) -> TestResult) -> TestResult {
    use slopos_fs::fileio::{
        fileio_create_empty_table_for_process, fileio_destroy_table_for_process,
    };

    let Ok(process) = slopos_ostd::process::process_spawn_root() else {
        return slopos_testing::fail!("could not register a scratch process");
    };
    let Some(handle) = process.handle() else {
        return slopos_testing::fail!("a registered process carries no handle");
    };
    if fileio_create_empty_table_for_process(handle) != 0 {
        slopos_ostd::process::process_retire(handle);
        return slopos_testing::fail!("fd table create failed for pid {}", process.id());
    }
    let result = body(FdTable::Process(
        slopos_ostd::process::ProcessId::of(&process).expect("registered"),
    ));
    fileio_destroy_table_for_process(handle);
    slopos_ostd::process::process_retire(handle);
    result
}

/// An in-flight op keeps operating against its held backing after userland
/// closes the fd, and releases that reference on completion (no leak).
fn op_survives_fd_close() -> TestResult {
    with_fd_table(op_survives_fd_close_body)
}

fn op_survives_fd_close_body(table: FdTable) -> TestResult {
    use core::ffi::c_int;
    use slopos_abi::syscall::POLLIN;
    use slopos_fs::fileio::{file_close_fd, file_pipe_create, fileio_clone_file_ref};

    let mut ring = make_ring(8);
    ring.owner = table;

    let mut rfd: c_int = -1;
    let mut wfd: c_int = -1;
    if file_pipe_create(table, 0, &mut rfd, &mut wfd) != 0 {
        return slopos_testing::fail!("pipe create failed");
    }

    // An alias of our own, to observe the ring's reference count.
    let probe_ref = match fileio_clone_file_ref(table, rfd) {
        Some(f) => f,
        None => return slopos_testing::fail!("clone read-end ref failed"),
    };
    // fd-table entry + probe_ref.
    let baseline = probe_ref.description_strong_count();

    // POLL_ADD on an empty pipe defers, holding a strong reference to the read
    // end.
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_POLL_ADD;
    sqe.fd = rfd;
    sqe.op_flags = POLLIN as u32;
    sqe.user_data = 0xA1;
    crate::enter::process_sqe_for_test(table, &mut ring, &sqe);
    if ring.inflight.len() != 1 {
        return slopos_testing::fail!("poll should defer, inflight={}", ring.inflight.len());
    }
    if probe_ref.description_strong_count() != baseline + 1 {
        return slopos_testing::fail!(
            "ring should hold one extra reference: {}",
            probe_ref.description_strong_count()
        );
    }

    // The held reference keeps the read end alive past the close.
    let _ = file_close_fd(table, rfd);

    // Peer write end still open → no readiness; the op stays in flight against
    // the held backing rather than resolving to a closed-fd error.
    let done = crate::enter::harvest_step_for_test(table, &mut ring, 1);
    drop(core::mem::take(&mut ring.pending_reap));
    if done || ring.inflight.len() != 1 {
        return slopos_testing::fail!(
            "op must survive the fd close, inflight={}",
            ring.inflight.len()
        );
    }

    // Close the write end → the read end reports POLLHUP → the op completes.
    let _ = file_close_fd(table, wfd);
    let _ = crate::enter::harvest_step_for_test(table, &mut ring, 1);
    // Drops the retired row's reference off-lock, as `ring_enter` would.
    drop(core::mem::take(&mut ring.pending_reap));

    if ring.inflight.len() != 0 {
        return slopos_testing::fail!("op must complete after peer close");
    }
    let cqe = read_cqe_at(&ring, 0);
    if cqe.user_data != 0xA1 {
        return slopos_testing::fail!("wrong completion: {:?}", cqe);
    }
    // Read fd closed + ring reference released → only our probe_ref remains.
    if probe_ref.description_strong_count() != 1 {
        return slopos_testing::fail!(
            "ring leaked its reference: strong_count={}",
            probe_ref.description_strong_count()
        );
    }
    TestResult::Pass
}
stest!(name = op_survives_fd_close, suite = slopring);

/// Closing an in-flight op's fd and reusing its number for a different file
/// does not retarget the op: it stays bound to the original open file.
fn no_reuse_aliasing() -> TestResult {
    with_fd_table(no_reuse_aliasing_body)
}

fn no_reuse_aliasing_body(table: FdTable) -> TestResult {
    use slopos_abi::syscall::POLLIN;
    use slopos_fs::fileio::{file_close_fd, file_pipe_create};

    let mut ring = make_ring(8);
    ring.owner = table;

    let (mut rfd_a, mut wfd_a) = (-1i32, -1i32);
    if file_pipe_create(table, 0, &mut rfd_a, &mut wfd_a) != 0 {
        return slopos_testing::fail!("pipe A create failed");
    }

    // POLL_ADD on A's read end defers, holding a reference to A.
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_POLL_ADD;
    sqe.fd = rfd_a;
    sqe.op_flags = POLLIN as u32;
    sqe.user_data = 0xB1;
    crate::enter::process_sqe_for_test(table, &mut ring, &sqe);
    if ring.inflight.len() != 1 {
        return slopos_testing::fail!("poll A should defer");
    }

    // B's read end reuses A's fd number.
    let _ = file_close_fd(table, rfd_a);
    let (mut rfd_b, mut wfd_b) = (-1i32, -1i32);
    if file_pipe_create(table, 0, &mut rfd_b, &mut wfd_b) != 0 {
        return slopos_testing::fail!("pipe B create failed");
    }
    if rfd_b != rfd_a {
        return slopos_testing::fail!(
            "test needs fd-number reuse: rfd_a={} rfd_b={}",
            rfd_a,
            rfd_b
        );
    }

    // Make the held file A ready via POLLHUP while the reused number B stays
    // not-ready (B's write end open, buffer empty).
    let _ = file_close_fd(table, wfd_a);

    let _ = crate::enter::harvest_step_for_test(table, &mut ring, 1);
    drop(core::mem::take(&mut ring.pending_reap));
    if ring.inflight.len() != 0 {
        return slopos_testing::fail!("op must target held file A, not reused B");
    }
    let cqe = read_cqe_at(&ring, 0);
    if cqe.user_data != 0xB1 {
        return slopos_testing::fail!("wrong completion: {:?}", cqe);
    }

    let _ = file_close_fd(table, rfd_b);
    let _ = file_close_fd(table, wfd_b);
    TestResult::Pass
}
stest!(name = no_reuse_aliasing, suite = slopring);
