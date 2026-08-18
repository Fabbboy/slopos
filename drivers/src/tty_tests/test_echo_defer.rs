//! The output boundary: echo the line discipline produces under a slot guard
//! reaches its driver only after that guard drops.
//!
//! `TTY_WRITE_LOCKS[i]` is outside every `TTY_SLOTS[j]`, because a driver write
//! for a PTY end delivers into the peer's slot; taking one with a slot guard
//! live is an inversion.

use super::fixtures::*;
use crate::tty::driver::inject_vconsole_input;
use crate::tty::table::TTY_SLOTS_CLASS;
use crate::tty::vconsole;

/// The virtual console's polled drain stages echo while `TTY_SLOTS[1]` is held
/// and emits only after the guard drops; emitting inline instead takes
/// `TTY_WRITE_LOCKS[1]` under the slot lock, which the declared order rejects.
///
/// The serial mirror is off for the duration, so the echo cannot interleave
/// with the harness's own output.
pub fn test_drain_echo_defers_write_lock() -> TestResult {
    let vt = TtyIndex(1);
    let mirror = vconsole::serial_mirror_enabled();
    vconsole::set_serial_mirror(false);

    drain_tty_nonblock(vt);
    inject_vconsole_input(b"hi\n");

    // Reaches `drain_hw_input_locked` under `TTY_SLOTS[1]`.
    let ready = tty::has_data(vt);

    let mut back = [0u8; 8];
    let read = tty::read(vt, &mut back, true);

    vconsole::set_serial_mirror(mirror);

    if !ready {
        klog_info!("TTY_TEST: BUG - injected vconsole input did not reach the ldisc");
        return TestResult::Fail;
    }
    match read {
        Ok(3) if &back[..3] == b"hi\n" => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected to read back \"hi\\n\", got {:?} {:?}",
                other,
                &back[..3]
            );
            TestResult::Fail
        }
    }
}

/// A PTY slave that crosses its IXOFF high-water mark while the drain is
/// running emits XOFF through its own driver, which routes into the master's
/// slot; emitting inline there would take `TTY_SLOTS[master]` while
/// `TTY_SLOTS[slave]` and `TTY_WRITE_LOCKS[slave]` are both held.
///
/// IXOFF is enabled *after* the queue is already over the mark: `set_termios`
/// neither clears `xoff_sent` nor re-checks the water mark, leaving the drain
/// as the first place the condition is seen.
///
/// Whichever CPU's `input_available_cb` reaches the drain first observes the
/// crossing; `tcsbrk` still makes the read deterministic, because the latch is
/// set in the same critical section that stages the byte.
pub fn test_drain_ixoff_defers_peer_write() -> TestResult {
    let pair = open_pty_pair();

    let Ok(saved) = tty::get_termios(pair.slave) else {
        return TestResult::Fail;
    };
    let mut t = saved;
    t.c_lflag = LocalFlags::ICANON;
    t.c_iflag = InputFlags::empty();
    t.c_cc[CcIndex::Vstop.as_usize()] = 0x13;
    if tty::set_termios(pair.slave, &t).is_err() {
        return TestResult::Fail;
    }

    // Fill past IXOFF_HIGH_WATER (9830 of EDIT_BUF_SIZE + COOKED_BUF_SIZE) with
    // IXOFF off, so `ixoff_check_xoff` short-circuits on the flag and leaves
    // `xoff_sent` clear. See `fill_past_ixoff_mark` for the buffer arithmetic.
    let mut line = [b'x'; 256];
    line[255] = b'\n';
    for _ in 0..23 {
        if tty::write(pair.master, &line, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }
    let unterminated = [b'y'; 256];
    for _ in 0..16 {
        if tty::write(pair.master, &unterminated, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }

    t.c_iflag = InputFlags::IXOFF;
    if tty::set_termios(pair.slave, &t).is_err() {
        let _ = tty::set_termios(pair.slave, &saved);
        return TestResult::Fail;
    }

    // Exactly one byte: the crossing is a one-shot latch and nothing else in
    // this test writes toward the master.
    let arrived = drain_then_read_byte(pair.slave, pair.master, 0x13);
    let _ = tty::set_termios(pair.slave, &saved);

    if !arrived {
        klog_info!("TTY_TEST: BUG - XOFF never reached the master");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Fill `slave`'s input past `IXOFF_HIGH_WATER` with IXOFF armed, so the write
/// that crosses the mark is the one that emits the stop.
///
/// Newline-terminated lines land in `cooked` and the unterminated tail stays in
/// the edit buffer, which is what carries the sum past the mark without
/// tripping the narrower throttle on `cooked` alone. 256-byte chunks because a
/// buffer big enough to cross the mark in one call would not fit a kernel stack
/// frame.
fn fill_past_ixoff_mark(master: TtyIndex) -> bool {
    let mut line = [b'x'; 256];
    line[255] = b'\n';
    for _ in 0..23 {
        if tty::write(master, &line, false) != Ok(256) {
            return false;
        }
    }
    let unterminated = [b'y'; 256];
    for _ in 0..16 {
        if tty::write(master, &unterminated, false) != Ok(256) {
            return false;
        }
    }
    true
}

/// Discarding a slave's pending output re-arms its IXOFF stop: the stop latches
/// when it is generated, not when it lands, so a `TCOFLUSH` that threw away a
/// staged one would leave the peer never told to stop and — the latch still set
/// — never told to resume either.
pub fn test_tcoflush_rearms_ixoff() -> TestResult {
    let pair = open_pty_pair();

    let Ok(saved) = tty::get_termios(pair.slave) else {
        return TestResult::Fail;
    };
    let mut t = saved;
    t.c_lflag = LocalFlags::ICANON;
    t.c_iflag = InputFlags::IXOFF;
    t.c_cc[CcIndex::Vstop.as_usize()] = 0x13;
    if tty::set_termios(pair.slave, &t).is_err() {
        return TestResult::Fail;
    }

    let filled = fill_past_ixoff_mark(pair.master);
    let restore = |r: TestResult| {
        let _ = tty::set_termios(pair.slave, &saved);
        r
    };
    if !filled {
        return restore(TestResult::Fail);
    }

    if !drain_then_read_byte(pair.slave, pair.master, 0x13) {
        klog_info!("TTY_TEST: BUG - no first XOFF to re-arm from");
        return restore(TestResult::Fail);
    }

    if tty::tcflush(pair.slave, slopos_abi::syscall::TCOFLUSH).is_err() {
        klog_info!("TTY_TEST: BUG - TCOFLUSH on the slave failed");
        return restore(TestResult::Fail);
    }
    if !drain_then_read_byte(pair.slave, pair.master, 0x13) {
        klog_info!("TTY_TEST: BUG - TCOFLUSH left the IXOFF stop latched");
        return restore(TestResult::Fail);
    }
    restore(TestResult::Pass)
}

/// A TTY never reports its output idle while it still owes a driver bytes: echo
/// staged in the discipline has not reached a driver. The master's buffer is
/// filled first so the slave's echo has nowhere to go and stays staged.
///
/// `output_queued_bytes` is sampled on both sides of the idle check because any
/// CPU may drain the slave's queue in between, and a queue that emptied is a
/// legitimate reason to report idle.
pub fn test_staged_echo_counts_as_pending_output() -> TestResult {
    let pair = open_pty_pair();

    let Ok(saved) = tty::get_termios(pair.slave) else {
        return TestResult::Fail;
    };
    let restore = |r: TestResult| {
        let _ = tty::set_termios(pair.slave, &saved);
        r
    };
    let mut t = saved;
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.c_iflag = InputFlags::empty();
    if tty::set_termios(pair.slave, &t).is_err() {
        return TestResult::Fail;
    }

    // Fills the master's 4096-byte buffer.
    let block = [b'.'; 256];
    for _ in 0..20 {
        if tty::write(pair.slave, &block, false).is_err() {
            return restore(TestResult::Fail);
        }
    }
    if !matches!(tty::is_output_idle(pair.slave), Ok(true)) {
        klog_info!("TTY_TEST: BUG - slave should be idle before the echo is staged");
        return restore(TestResult::Fail);
    }

    if tty::write(pair.master, b"abc", false).is_err() {
        return restore(TestResult::Fail);
    }

    let before = tty::output_queued_bytes(pair.slave);
    let idle = tty::is_output_idle(pair.slave);
    let after = tty::output_queued_bytes(pair.slave);

    let queued_throughout = matches!(before, Ok(n) if n > 0) && matches!(after, Ok(n) if n > 0);
    if !queued_throughout {
        klog_info!(
            "TTY_TEST: BUG - staged echo never showed up in TIOCOUTQ, before={:?} after={:?}",
            before,
            after
        );
        return restore(TestResult::Fail);
    }
    if !matches!(idle, Ok(false)) {
        klog_info!(
            "TTY_TEST: BUG - output reported idle with {:?} bytes still queued",
            after
        );
        return restore(TestResult::Fail);
    }

    // Draining the master gives the echo somewhere to land, so the slave's
    // tcdrain settles instead of waiting on a queue nobody will empty.
    drain_tty_nonblock(pair.master);
    if tty::tcsbrk(pair.slave, 1).is_err() {
        klog_info!("TTY_TEST: BUG - tcdrain on the slave never settled");
        return restore(TestResult::Fail);
    }
    restore(TestResult::Pass)
}

/// The echo queue is a ring: it hands bytes back oldest-first, an unread
/// re-offers a short write's tail ahead of everything staged after it, and an
/// overflow refuses the newest rather than corrupting what is already queued.
pub fn test_echo_queue_ring_semantics() -> TestResult {
    let mut ld = LineDisc::new();

    ld.echo_stage(b"abcdef");
    let mut chunk = [0u8; 3];
    if ld.echo_take(&mut chunk) != 3 || &chunk != b"abc" {
        klog_info!("TTY_TEST: BUG - echo_take should return the oldest bytes");
        return TestResult::Fail;
    }

    ld.echo_unread(b"bc");
    let echo = EchoScratch::drain(&mut ld);
    if echo.as_slice() != b"bcdef" {
        klog_info!(
            "TTY_TEST: BUG - echo_unread should re-offer in order, got {:?}",
            echo.as_slice()
        );
        return TestResult::Fail;
    }
    if !ld.echo_is_empty() {
        klog_info!("TTY_TEST: BUG - queue should be empty after a full drain");
        return TestResult::Fail;
    }

    // A full VREPRINT redisplay of a full edit line fits: 1 + EDIT_BUF_SIZE.
    let dropped_before = ld.echo_dropped();
    let big = KBox::<[u8; 4097]>::zeroed().expect("test alloc");
    ld.echo_stage(&big[..]);
    if ld.echo_dropped() != dropped_before {
        klog_info!("TTY_TEST: BUG - a full redisplay should not be clipped");
        return TestResult::Fail;
    }

    ld.echo_stage(&big[..]);
    if ld.echo_dropped() == dropped_before {
        klog_info!("TTY_TEST: BUG - overflow should be counted as dropped");
        return TestResult::Fail;
    }

    ld.echo_discard();
    if !ld.echo_is_empty() {
        klog_info!("TTY_TEST: BUG - echo_discard should empty the queue");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The TTY lock order is declared, not inferred from whichever direction ran
/// first, and something has actually taken it in the declared direction — a
/// declaration nothing exercises describes code that no longer runs.
pub fn test_tty_lock_order_is_declared() -> TestResult {
    use slopos_ostd::sync::{self, LockdepMode};

    if !sync::tracking_enabled()
        || sync::graph_overflowed()
        || sync::lockdep_mode() == LockdepMode::Off
    {
        return TestResult::Pass;
    }

    if sync::declared_count() == 0 {
        klog_info!("TTY_TEST: BUG - no lock order was declared");
        return TestResult::Fail;
    }
    if sync::declared_observed() != sync::declared_count() {
        klog_info!(
            "TTY_TEST: BUG - {} of {} declared orders never observed",
            sync::declared_count() - sync::declared_observed(),
            sync::declared_count()
        );
        return TestResult::Fail;
    }

    // Re-declaring the reverse must be rejected: if it were accepted, the
    // graph would already contain both directions and report nothing.
    match sync::declare_order(TTY_SLOTS_CLASS, crate::tty::output::TTY_WRITE_CLASS) {
        Err(sync::DeclareOrderError::Contradicted) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - the reverse TTY order should be contradicted, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}
