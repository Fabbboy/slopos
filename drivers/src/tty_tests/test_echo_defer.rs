//! The output boundary: echo the line discipline produces under a slot guard
//! reaches its driver only after that guard drops.
//!
//! `TTY_WRITE_LOCKS[i]` is outside every `TTY_SLOTS[j]`, because a driver
//! write for a PTY end delivers into the peer's slot. A driver write taken
//! with a slot guard live is therefore an inversion, and these tests drive the
//! two paths that can produce one.

use super::fixtures::*;
use crate::tty::driver::inject_vconsole_input;
use crate::tty::table::TTY_SLOTS_CLASS;
use crate::tty::vconsole;

/// The virtual console's polled drain echoes what it read.
///
/// This is the reported failure, wire-clean: the drain stages echo while
/// `TTY_SLOTS[1]` is held, and the emission happens after the guard drops.
/// Emitting inline instead takes `TTY_WRITE_LOCKS[1]` under the slot lock,
/// which the declared order rejects on first execution.
///
/// The serial mirror is off for the duration, so the echo lands on the
/// framebuffer only and cannot interleave with the harness's own output.
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
/// slot.
///
/// IXOFF is enabled *after* the queue is already over the mark, so the
/// crossing is not consumed by the push path that filled it: `set_termios`
/// neither clears `xoff_sent` nor re-checks the water mark, leaving the drain
/// as the first place the condition is seen. Emitting inline there would take
/// `TTY_SLOTS[master]` while `TTY_SLOTS[slave]` and `TTY_WRITE_LOCKS[slave]`
/// are both held.
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

    // Fill past IXOFF_HIGH_WATER (9830 of EDIT_BUF_SIZE + COOKED_BUF_SIZE)
    // with IXOFF off, so `ixoff_check_xoff` short-circuits on the flag and
    // leaves `xoff_sent` clear. Newline-terminated lines land in `cooked`;
    // the unterminated tail stays in the edit buffer, which is what carries
    // the sum past the mark without tripping the 6144-byte throttle first.
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

    // Arm IXOFF now that the queue is already over the mark.
    t.c_iflag = InputFlags::IXOFF;
    if tty::set_termios(pair.slave, &t).is_err() {
        let _ = tty::set_termios(pair.slave, &saved);
        return TestResult::Fail;
    }

    // Reaches `drain_hw_input_locked` under `TTY_SLOTS[slave]`.
    let _ = tty::bytes_available(pair.slave);

    // Reading the byte back off the master is what proves the path ran: a
    // test that only asserts "no panic" passes whether or not it reached it.
    let mut back = [0u8; 8];
    let got = tty::read(pair.master, &mut back, true);
    let _ = tty::set_termios(pair.slave, &saved);

    match got {
        Ok(n) if n >= 1 && back[..n].contains(&0x13) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected XOFF (0x13) on the master, got {:?} {:?}",
                other,
                back
            );
            TestResult::Fail
        }
    }
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

    // A short write puts its tail back at the front, ahead of the rest.
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

    // Past capacity the newest bytes are refused and counted.
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
/// first, and something has actually taken it in the declared direction.
///
/// A declaration nothing exercises describes code that no longer runs, so the
/// observed count is as load-bearing as the declaration itself.
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
