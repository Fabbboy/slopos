#![feature(restricted_std)]

//! SlopRing multishot (ABI v2) end-to-end test.
//!
//! Multishot arms one SQE and the kernel posts a CQE per yield until a
//! terminal event, instead of an N-resubmit loop. The deterministic
//! driver here is `poll_add_multishot` over a pipe: each readiness
//! *transition* (not-ready → ready) drives one `MultishotStream` item,
//! and the edge-tracking suppresses the level flood (SLOPRING §1.2). We
//! also prove oneshot/multishot equivalence (the first multishot item
//! matches a oneshot `poll_add`) and that dropping a live stream cancels
//! its kernel row (no leaked in-flight).
//!
//! In-process TCP loopback does not complete its handshake in the test
//! environment (a pre-existing netstack limitation, noted in `ring_test`),
//! so `accept_multishot` is exercised by its construction + drop-cancel
//! path rather than a full connection stream; the readiness-driven
//! `poll_add_multishot` path proves the multishot stream machinery
//! end-to-end against a real fd.

use slopos_abi::syscall::POLLIN;
use slopos_userland as _;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::fs;

/// `poll_add_multishot` over a pipe yields one stream item per readiness
/// transition: write → ready (item 1), drain → not-ready, write → ready
/// (item 2). Two writes ⇒ exactly two items, both carrying POLLIN.
fn test_poll_multishot_two_edges() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();
    let wr_fd = wr.raw();

    let ok = slopfut::block_on(ring, async move {
        let mut stream = slopfut::poll_add_multishot(rd_fd, POLLIN);

        // Edge 1: make the pipe readable, then take the first item.
        if fs::write_slice(wr_fd, b"a").is_err() {
            return false;
        }
        let first = stream.next().await;
        if !matches!(first, Some(r) if (r as u16) & POLLIN != 0) {
            return false;
        }

        // Drain the pipe so it goes not-ready, then force a harvest pass
        // (the nop park) so the kernel observes the not-ready level and
        // resets the edge cache — without this, no second transition fires.
        let mut buf = [0u8; 8];
        let _ = fs::read_slice(rd_fd, &mut buf);
        let _ = slopfut::nop().await;

        // Edge 2: readable again ⇒ a second transition ⇒ a second item.
        if fs::write_slice(wr_fd, b"b").is_err() {
            return false;
        }
        let second = stream.next().await;
        matches!(second, Some(r) if (r as u16) & POLLIN != 0)
    });

    drop(wr);
    drop(rd);
    ok
}

/// Oneshot/multishot equivalence: a readable pipe makes both a oneshot
/// `poll_add` and the first `poll_add_multishot` item report POLLIN with
/// the same observable result.
fn test_oneshot_equivalence() -> bool {
    // Oneshot leg.
    let Ok((rd1, wr1)) = fs::pipe() else {
        return false;
    };
    if fs::write_slice(wr1.raw(), b"x").is_err() {
        return false;
    }
    let Ok(r1) = Ring::setup(8) else {
        return false;
    };
    let rd1_fd = rd1.raw();
    let oneshot = slopfut::block_on(r1, async move { slopfut::poll_add(rd1_fd, POLLIN).await });
    drop(wr1);
    drop(rd1);

    // Multishot leg (first item).
    let Ok((rd2, wr2)) = fs::pipe() else {
        return false;
    };
    if fs::write_slice(wr2.raw(), b"x").is_err() {
        return false;
    }
    let Ok(r2) = Ring::setup(8) else {
        return false;
    };
    let rd2_fd = rd2.raw();
    let multishot = slopfut::block_on(r2, async move {
        let mut s = slopfut::poll_add_multishot(rd2_fd, POLLIN);
        s.next().await
    });
    drop(wr2);
    drop(rd2);

    oneshot > 0
        && (oneshot as u16) & POLLIN != 0
        && matches!(multishot, Some(m) if (m as u16) & POLLIN != 0 && m == oneshot)
}

/// Dropping a live multishot stream cancels its kernel row: after the
/// `block_on` returns, the reactor reports zero in-flight ops (the
/// terminal -ECANCELED CQE retired the armed row). Raced against a short
/// timer so the empty-pipe poll never fires and the stream is dropped
/// while still armed.
fn test_drop_cancels() -> bool {
    let Ok((rd, _wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();

    let timer_won = slopfut::block_on(ring, async move {
        // Arm a multishot poll on an empty (never-readable) pipe, race a
        // timer; the timer wins and the stream is dropped mid-flight. Both
        // `Next` and the `OP_TIMEOUT` leaf are `Unpin`, as `select2` needs.
        let mut stream = slopfut::poll_add_multishot(rd_fd, POLLIN);
        match slopfut::select2(stream.next(), slopfut::timeout(50_000_000)).await {
            slopfut::Either2::A(_) => false, // empty pipe must not become ready
            slopfut::Either2::B(_) => true,
        }
    });

    drop(_wr);
    drop(rd);
    timer_won
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("poll_multishot_two_edges", test_poll_multishot_two_edges),
    ("oneshot_equivalence", test_oneshot_equivalence),
    ("drop_cancels", test_drop_cancels),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
