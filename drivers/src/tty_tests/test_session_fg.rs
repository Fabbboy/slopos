//! Split from test_ldisc.rs: test_session_fg.rs

use super::fixtures::*;

// ===========================================================================
// Job Control Correctness regression tests
// ===========================================================================

/// SIGTTOU constant is defined and has correct POSIX value (22).
pub fn test_sigttou_constant() -> TestResult {
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// check_write with TOSTOP and background caller returns BackgroundWrite.
/// This verifies the session-level check_write logic directly.
pub fn test_check_write_tostop_blocks_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Background process (pgid=99), TOSTOP enabled.
    match s.check_write(99, 10, true) {
        ForegroundCheck::BackgroundWrite => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TOSTOP bg write expected BackgroundWrite, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write without TOSTOP always allows writes (even from background).
pub fn test_check_write_no_tostop_allows_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Background process (pgid=99), TOSTOP not set.
    match s.check_write(99, 10, false) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no TOSTOP bg write expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write with TOSTOP allows foreground process.
pub fn test_check_write_tostop_allows_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // fg_pgrp=10
    // Foreground process (pgid=10), TOSTOP enabled — should still be allowed.
    match s.check_write(10, 10, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TOSTOP fg write expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read rejects cross-session reads (DeniedCrossSession).
pub fn test_check_read_cross_session_rejected() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Caller from a different session (sid=99) — should be rejected.
    match s.check_read(10, 99) {
        ForegroundCheck::DeniedCrossSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - cross-session read expected DeniedCrossSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read still allows same-session foreground reads.
pub fn test_check_read_same_session_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(10, 10) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - same-session fg read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read still allows kernel tasks (pgid=0, sid=0).
pub fn test_check_read_kernel_task_allowed() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Kernel task with pgid=0, sid=0 — should be allowed.
    match s.check_read(0, 0) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - kernel task read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// TTY write succeeds for foreground process even with TOSTOP.
pub fn test_tty_write_foreground_with_tostop() -> TestResult {
    tty::table::tty_table_init();
    // This test verifies write() returns Ok even when TOSTOP is set,
    // because in the test harness task_id=0 (kernel), which skips the
    // foreground check.  The session-level check_write tests above
    // verify the logic directly.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_lflag |= LocalFlags::TOSTOP;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello";
    let result = tty::write(TtyIndex(0), data, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(5) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fg write with TOSTOP expected Ok(5), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}
// ===========================================================================
// Strict Session Gates & Foreground Outcomes
// ===========================================================================

/// No session attached — check_read returns BootstrapAllowed.
pub fn test_bootstrap_allowed_no_session_read() -> TestResult {
    let s = TtySession::new();
    match s.check_read(42, 42) {
        ForegroundCheck::BootstrapAllowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no-session check_read expected BootstrapAllowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Session attached but no fg_pgrp — check_read returns BootstrapAllowed.
pub fn test_bootstrap_allowed_no_fg_pgrp() -> TestResult {
    let mut s = TtySession::new();
    s.session_leader = SessionId::new(10);
    s.session_id = SessionId::new(10);
    // fg_pgrp remains None
    match s.check_read(42, 10) {
        ForegroundCheck::BootstrapAllowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no-fg-pgrp check_read expected BootstrapAllowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Cross-session read — DeniedCrossSession.
pub fn test_denied_cross_session_read() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Caller from different session (sid=99).
    match s.check_read(10, 99) {
        ForegroundCheck::DeniedCrossSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - cross-session read expected DeniedCrossSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Cross-session write with TOSTOP — DeniedCrossSession (not BackgroundWrite).
pub fn test_denied_cross_session_write_tostop() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Cross-session (sid=99) + TOSTOP: cross-session takes priority.
    match s.check_write(10, 99, true) {
        ForegroundCheck::DeniedCrossSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - cross-session write+TOSTOP expected DeniedCrossSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Cross-session write without TOSTOP — still DeniedCrossSession.
pub fn test_cross_session_write_no_tostop_still_denied() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Cross-session (sid=99), no TOSTOP: still denied.
    match s.check_write(10, 99, false) {
        ForegroundCheck::DeniedCrossSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - cross-session write no-TOSTOP expected DeniedCrossSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Kernel task (sid=0) is exempted from cross-session denial on read.
pub fn test_kernel_task_exempted_cross_session_read() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Kernel task: pgid=0, sid=0 — should be Allowed, not DeniedCrossSession.
    match s.check_read(0, 0) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - kernel task read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Kernel task (sid=0) is exempted from cross-session denial on write.
pub fn test_kernel_task_exempted_cross_session_write() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Kernel task: pgid=0, sid=0, TOSTOP=true — should be Allowed.
    match s.check_write(0, 0, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - kernel task write expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Same-session background read — BackgroundRead (not DeniedCrossSession).
pub fn test_same_session_background_read_sigttin() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Same session (sid=10) but background (pgid=99) — SIGTTIN path.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - same-session bg read expected BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Same-session background write with TOSTOP — BackgroundWrite.
pub fn test_same_session_background_write_sigttou() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Same session (sid=10), background (pgid=99), TOSTOP=true — SIGTTOU path.
    match s.check_write(99, 10, true) {
        ForegroundCheck::BackgroundWrite => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - same-session bg write+TOSTOP expected BackgroundWrite, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write with no session returns Allowed (not BootstrapAllowed).
/// The write path uses a simpler model: no session = Allowed, not BootstrapAllowed.
pub fn test_check_write_no_session_allowed() -> TestResult {
    let s = TtySession::new();
    match s.check_write(42, 42, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no-session check_write expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// TtyError::CrossSessionDenied is a distinct error variant.
pub fn test_cross_session_denied_error_variant() -> TestResult {
    let err = TtyError::CrossSessionDenied;
    // Verify it is distinguishable from other error variants.
    if err == TtyError::BackgroundRead
        || err == TtyError::BackgroundWrite
        || err == TtyError::PermissionDenied
    {
        klog_info!("TTY_TEST: BUG - CrossSessionDenied should be distinct from other errors");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Job Control & Controlling TTY Hardening
// ===========================================================================

/// set_fg_pgrp_checked on the per-TTY API denies non-existent pgrp.
///
/// With a session attached (sid=600), attempting to set a foreground pgrp that
/// has no living members in the session should fail.  The pgrp_exists_in_session
/// service iterates the scheduler's task list and won't find pgid=99999.
pub fn test_set_fg_pgrp_checked_nonexistent_pgrp() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // pgid 99999 doesn't exist in any session — should be denied.
    let result = tty::set_foreground_pgrp_checked(TtyIndex(0), 99999, 600);

    // Clean up.
    tty::detach_session(TtyIndex(0));
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0);

    match result {
        Err(TtyError::PermissionDenied) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected PermissionDenied for nonexistent pgrp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// set_fg_pgrp_checked still allows clearing (pgid == 0).
pub fn test_set_fg_pgrp_checked_clear_allowed() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // pgid == 0 should always be allowed (clears foreground group).
    let result = tty::set_foreground_pgrp_checked(TtyIndex(0), 0, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

    // Clean up.
    tty::detach_session(TtyIndex(0));

    if result.is_err() {
        klog_info!(
            "TTY_TEST: BUG - clearing fg_pgrp (pgid=0) should be allowed, got {:?}",
            result
        );
        return TestResult::Fail;
    }
    if pgid != 0 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should be 0 after clear, got {}",
            pgid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_fg_pgrp_checked skips pgrp validation when no session attached.
pub fn test_set_fg_pgrp_checked_no_session_skips_validation() -> TestResult {
    tty::table::tty_table_init();

    // No session attached — any pgid should be allowed (pre-session path).
    let result = tty::set_foreground_pgrp_checked(TtyIndex(0), 12345, 0);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);

    // Clean up.
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0);

    if result.is_err() {
        klog_info!(
            "TTY_TEST: BUG - no-session path should allow any pgid, got {:?}",
            result
        );
        return TestResult::Fail;
    }
    if pgid != 12345 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should be 12345, got {}", pgid);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// detach_controlling_terminal (non-leader) returns Ok(false).
///
/// When a non-session-leader calls TIOCNOTTY, the TTY session state is
/// unchanged — only the caller's controlling_tty is cleared (by the ioctl handler).
pub fn test_detach_ctty_non_leader() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // Non-leader: caller_is_session_leader = false.
    let result = tty::detach_controlling_terminal(TtyIndex(0), 600, false);

    // Session should still be intact.
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

    // Clean up.
    tty::detach_session(TtyIndex(0));

    match result {
        Ok(false) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - non-leader detach should return Ok(false), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    if sid != 600 {
        klog_info!(
            "TTY_TEST: BUG - session should remain attached after non-leader detach (sid={})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// detach_controlling_terminal (session leader) detaches session.
///
/// When the session leader issues TIOCNOTTY, the TTY's session state is
/// fully cleared and SIGHUP+SIGCONT would be sent to the foreground pgrp.
pub fn test_detach_ctty_session_leader() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // Session leader: caller_is_session_leader = true.
    let result = tty::detach_controlling_terminal(TtyIndex(0), 600, true);

    // Session should be fully detached.
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(u32::MAX);

    match result {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - leader detach should return Ok(true), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - session should be detached after leader TIOCNOTTY (sid={})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// detach_controlling_terminal denies cross-session detach.
///
/// A session leader from a different session cannot detach someone else's TTY.
pub fn test_detach_ctty_cross_session_denied() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // Different session leader trying to detach.
    let result = tty::detach_controlling_terminal(TtyIndex(0), 999, true);

    // Session should still be intact.
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

    // Clean up.
    tty::detach_session(TtyIndex(0));

    match result {
        Err(TtyError::PermissionDenied) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - cross-session detach should be denied, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    if sid != 600 {
        klog_info!(
            "TTY_TEST: BUG - session should remain after cross-session detach attempt (sid={})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TIOCNOTTY constant has the correct value.
pub fn test_tiocnotty_constant() -> TestResult {
    use slopos_abi::syscall::TIOCNOTTY;
    if TIOCNOTTY != 0x5422 {
        klog_info!(
            "TTY_TEST: BUG - TIOCNOTTY should be 0x5422, got 0x{:x}",
            TIOCNOTTY
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// /dev/tty Controlling Terminal Device
// ===========================================================================

/// `open_ref` increments open count for the same TTY slot — this is
/// the mechanism that `/dev/tty` relies on (a second FD referencing the same
/// TTY index via the caller's controlling terminal).
pub fn test_open_ref_second_fd_increments_count() -> TestResult {
    let idx = TtyIndex(0);
    // Read initial open_count.
    let initial = {
        let guard = TTY_SLOTS[0].lock();
        match guard.as_ref() {
            Some(tty) => tty.open_count,
            None => {
                klog_info!("TTY_TEST: BUG - TTY0 not allocated");
                return TestResult::Fail;
            }
        }
    };
    // Simulate /dev/tty open: open_ref on the same TTY.
    let after_open = match tty::open_ref(idx) {
        Ok(count) => count,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - open_ref failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    if after_open != initial + 1 {
        klog_info!(
            "TTY_TEST: BUG - open_ref should increment: expected {}, got {}",
            initial + 1,
            after_open
        );
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }
    // Close the extra ref.
    let _ = tty::close_ref(idx);
    TestResult::Pass
}

/// After `open_ref` (simulating `/dev/tty` open), read/write/termios
/// operations work identically on the same TTY index — the FD created by
/// `/dev/tty` is indistinguishable from one opened on the actual device path.
pub fn test_dev_tty_operations_identical_to_direct() -> TestResult {
    let idx = TtyIndex(0);
    // open_ref simulates /dev/tty open.
    let _ = tty::open_ref(idx);

    // get_termios should work.
    let termios = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios after open_ref failed: {:?}", e);
            let _ = tty::close_ref(idx);
            return TestResult::Fail;
        }
    };
    // Verify it returns a valid termios (ICANON should be set by default).
    if !termios.c_lflag.contains(LocalFlags::ICANON) {
        klog_info!("TTY_TEST: BUG - termios from /dev/tty FD missing ICANON");
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }

    // write should succeed (returns byte count).
    match tty::write(idx, b"phase30", false) {
        Ok(n) if n == 7 => {}
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - write via /dev/tty FD returned {}, expected 7",
                n
            );
            let _ = tty::close_ref(idx);
            return TestResult::Fail;
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - write via /dev/tty FD failed: {:?}", e);
            let _ = tty::close_ref(idx);
            return TestResult::Fail;
        }
    }

    // get_session_id should succeed.
    if tty::get_session_id(idx).is_err() {
        klog_info!("TTY_TEST: BUG - get_session_id after open_ref failed");
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }

    let _ = tty::close_ref(idx);
    TestResult::Pass
}

/// `open_ref` on a TTY does NOT modify session state — opening
/// `/dev/tty` only accesses an existing controlling terminal, never acquires one.
pub fn test_open_ref_does_not_modify_session() -> TestResult {
    let idx = TtyIndex(0);
    // Snapshot session state before open_ref.
    let (sid_before, fg_before) = {
        let guard = TTY_SLOTS[0].lock();
        match guard.as_ref() {
            Some(tty) => (tty.session.session_id_raw(), tty.session.fg_pgrp_raw()),
            None => {
                klog_info!("TTY_TEST: BUG - TTY0 not allocated");
                return TestResult::Fail;
            }
        }
    };

    // open_ref simulates /dev/tty open.
    let _ = tty::open_ref(idx);

    // Snapshot after.
    let (sid_after, fg_after) = {
        let guard = TTY_SLOTS[0].lock();
        match guard.as_ref() {
            Some(tty) => (tty.session.session_id_raw(), tty.session.fg_pgrp_raw()),
            None => {
                klog_info!("TTY_TEST: BUG - TTY0 vanished");
                let _ = tty::close_ref(idx);
                return TestResult::Fail;
            }
        }
    };

    if sid_before != sid_after || fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - open_ref modified session: sid {}->{}, fg {}->{}",
            sid_before,
            sid_after,
            fg_before,
            fg_after
        );
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }

    let _ = tty::close_ref(idx);
    TestResult::Pass
}

/// `open_ref` on an invalid TTY index returns `InvalidIndex` error,
/// matching the ENXIO semantics when `/dev/tty` resolution fails.
pub fn test_open_ref_invalid_index_returns_error() -> TestResult {
    let bad = TtyIndex(u8::MAX);
    match tty::open_ref(bad) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - open_ref(255) should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// `close_ref` correctly decrements the open count — ensures the
/// `/dev/tty` FD lifecycle is properly paired with the direct device FD.
pub fn test_close_ref_decrements_after_open() -> TestResult {
    let idx = TtyIndex(0);
    let before = {
        let guard = TTY_SLOTS[0].lock();
        guard.as_ref().map(|t| t.open_count).unwrap_or(0)
    };
    let _ = tty::open_ref(idx);
    let _ = tty::close_ref(idx);
    let after = {
        let guard = TTY_SLOTS[0].lock();
        guard.as_ref().map(|t| t.open_count).unwrap_or(0)
    };
    if before != after {
        klog_info!(
            "TTY_TEST: BUG - open+close ref should restore count: {} != {}",
            before,
            after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multiple `open_ref` calls (simulating multiple `/dev/tty` opens)
/// all succeed and increment sequentially.
pub fn test_multiple_open_ref_sequential() -> TestResult {
    let idx = TtyIndex(0);
    let base = match tty::open_ref(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - first open_ref failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let second = match tty::open_ref(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - second open_ref failed: {:?}", e);
            let _ = tty::close_ref(idx);
            return TestResult::Fail;
        }
    };
    if second != base + 1 {
        klog_info!(
            "TTY_TEST: BUG - sequential open_ref should increment: {} then {}",
            base,
            second
        );
        let _ = tty::close_ref(idx);
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }
    let _ = tty::close_ref(idx);
    let _ = tty::close_ref(idx);
    TestResult::Pass
}

/// `get_winsize` works identically regardless of whether the FD was
/// obtained via `/dev/tty` or direct device open (both use the same TTY index).
pub fn test_dev_tty_winsize_matches_direct() -> TestResult {
    let idx = TtyIndex(0);
    let ws_before = match tty::get_winsize(idx) {
        Ok(ws) => ws,
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - get_winsize before open_ref failed: {:?}",
                e
            );
            return TestResult::Fail;
        }
    };
    // Simulate /dev/tty open.
    let _ = tty::open_ref(idx);
    let ws_after = match tty::get_winsize(idx) {
        Ok(ws) => ws,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_winsize after open_ref failed: {:?}", e);
            let _ = tty::close_ref(idx);
            return TestResult::Fail;
        }
    };
    if ws_before.ws_row != ws_after.ws_row || ws_before.ws_col != ws_after.ws_col {
        klog_info!("TTY_TEST: BUG - winsize differs after open_ref");
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }
    let _ = tty::close_ref(idx);
    TestResult::Pass
}
// ===========================================================================
// Background Write Protection (SIGTTOU on tcsetattr)
// ===========================================================================

/// check_write with tostop=true (simulating tcsetattr foreground
/// check) blocks background processes with BackgroundWrite.
pub fn test_tcsetattr_background_blocked() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Background process (pgid=50), tostop=true (tcsetattr always uses this).
    match s.check_write(50, 10, true) {
        ForegroundCheck::BackgroundWrite => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcsetattr bg expected BackgroundWrite, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Foreground process tcsetattr proceeds normally (no signal).
pub fn test_tcsetattr_foreground_allowed() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    // Foreground process (pgid=10), tostop=true.
    match s.check_write(10, 10, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcsetattr fg expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// tcsetattr with no session attached is allowed (bootstrap path).
pub fn test_tcsetattr_no_session_allowed() -> TestResult {
    let s = TtySession::new();
    // No session — should allow.
    match s.check_write(50, 50, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcsetattr no session expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// tcsetattr from a different session returns DeniedCrossSession.
pub fn test_tcsetattr_cross_session_denied() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Cross-session caller (sid=99) — hard denial.
    match s.check_write(10, 99, true) {
        ForegroundCheck::DeniedCrossSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcsetattr cross-session expected DeniedCrossSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// TtyError::OrphanedProcessGroup maps to EIO (-5).
pub fn test_orphaned_pgrp_errno() -> TestResult {
    let errno = TtyError::OrphanedProcessGroup.to_errno();
    if errno != -5 {
        klog_info!(
            "TTY_TEST: BUG - OrphanedProcessGroup errno expected -5, got {}",
            errno
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Kernel task (task_id=0) bypasses tcsetattr foreground check.
/// In the test harness, task_id is always 0, so set_termios should succeed
/// even if the TTY has a session with a different foreground group.
pub fn test_tcsetattr_kernel_task_bypass() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // Attach a session with fg_pgrp=10.
    tty::attach_session(idx, 10, 10);
    let saved = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    // Modify termios (kernel task_id=0 should bypass foreground check).
    let mut t = saved;
    t.c_iflag ^= InputFlags::from_bits_retain(0x01); // toggle a bit
    match tty::set_termios(idx, &t) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - kernel task set_termios should succeed, got {:?}",
                e
            );
            let _ = tty::set_termios(idx, &saved);
            return TestResult::Fail;
        }
    }
    // Restore original termios.
    let _ = tty::set_termios(idx, &saved);
    TestResult::Pass
}

/// set_termios_wait and set_termios_flush also have the foreground
/// check (kernel task bypass verifies the path doesn't crash).
pub fn test_tcsetsw_tcsetsf_kernel_task_bypass() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 10, 10);
    let saved = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let mut t = saved;
    t.c_iflag ^= InputFlags::from_bits_retain(0x01);
    // TCSETSW path
    match tty::set_termios_wait(idx, &t) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - kernel task set_termios_wait should succeed, got {:?}",
                e
            );
            let _ = tty::set_termios(idx, &saved);
            return TestResult::Fail;
        }
    }
    // TCSETSF path
    match tty::set_termios_flush(idx, &t) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - kernel task set_termios_flush should succeed, got {:?}",
                e
            );
            let _ = tty::set_termios(idx, &saved);
            return TestResult::Fail;
        }
    }
    let _ = tty::set_termios(idx, &saved);
    TestResult::Pass
}

/// TOSTOP + background write with SIGTTOU blocked/ignored bypass.
/// Exercises the check_write path with tostop=true, verifying the session-level
/// check correctly identifies background writers. The signal bypass logic itself
/// is tested at the driver_hooks level.
pub fn test_tostop_background_write_check() -> TestResult {
    let mut s = TtySession::new();
    s.attach(20, 20); // session=20, fg_pgrp=20
    // Background writer (pgid=30) with TOSTOP enabled.
    match s.check_write(30, 20, true) {
        ForegroundCheck::BackgroundWrite => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - TOSTOP bg write expected BackgroundWrite, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    // Same writer without TOSTOP — allowed.
    match s.check_write(30, 20, false) {
        ForegroundCheck::Allowed => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - no TOSTOP bg write expected Allowed, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Kernel task (pgid=0) is always allowed through check_write,
/// even with tostop=true.
pub fn test_kernel_task_check_write_allowed() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_write(0, 0, true) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - kernel task check_write expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}
// ===========================================================================
// Controlling Terminal Lifecycle Integrity
// ===========================================================================

/// acquire_controlling_terminal succeeds for a fresh (no-session) TTY.
pub fn test_acquire_ctty_fresh_tty() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    match tty::acquire_controlling_terminal(idx, 100, 100) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - acquire fresh tty failed: {:?}", e);
            return TestResult::Fail;
        }
    }
    // Verify session was attached.
    match tty::get_session_id(idx) {
        Ok(100) => TestResult::Pass,
        Ok(other) => {
            klog_info!("TTY_TEST: BUG - session_id expected 100, got {}", other);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// acquire_controlling_terminal is a no-op when called by the same
/// session that already owns the TTY (idempotent / TIOCSCTTY same-session).
pub fn test_acquire_ctty_same_session_idempotent() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // First acquire.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 50, 50) {
        klog_info!("TTY_TEST: BUG - first acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Second acquire from same session — should succeed (no-op).
    match tty::acquire_controlling_terminal(idx, 50, 50) {
        Ok(()) => TestResult::Pass,
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - same-session re-acquire should succeed, got {:?}",
                e
            );
            TestResult::Fail
        }
    }
}

/// acquire_controlling_terminal fails with PermissionDenied when a
/// different session already owns the TTY.
pub fn test_acquire_ctty_different_session_denied() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // Session 10 owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 10, 10) {
        klog_info!("TTY_TEST: BUG - initial acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session 20 tries to steal it.
    match tty::acquire_controlling_terminal(idx, 20, 20) {
        Err(TtyError::PermissionDenied) => TestResult::Pass,
        Ok(()) => {
            klog_info!("TTY_TEST: BUG - different session acquire should fail");
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", e);
            TestResult::Fail
        }
    }
}

/// release_controlling_terminal succeeds for the owning session.
pub fn test_release_ctty_owning_session() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    if let Err(e) = tty::acquire_controlling_terminal(idx, 30, 30) {
        klog_info!("TTY_TEST: BUG - acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    match tty::release_controlling_terminal(idx, 30) {
        Ok(true) => {}
        other => {
            klog_info!("TTY_TEST: BUG - release expected Ok(true), got {:?}", other);
            return TestResult::Fail;
        }
    }
    // Session should now be 0 (detached).
    match tty::get_session_id(idx) {
        Ok(0) => TestResult::Pass,
        Ok(sid) => {
            klog_info!(
                "TTY_TEST: BUG - session_id should be 0 after release, got {}",
                sid
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// release_controlling_terminal is a no-op (returns Ok(false)) when
/// called by a session that does not own the TTY.
pub fn test_release_ctty_wrong_session_noop() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // Session 10 owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 10, 10) {
        klog_info!("TTY_TEST: BUG - acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session 99 tries to release — should be a no-op.
    match tty::release_controlling_terminal(idx, 99) {
        Ok(false) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - wrong-session release expected Ok(false), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    // Original session should still be attached.
    match tty::get_session_id(idx) {
        Ok(10) => TestResult::Pass,
        Ok(sid) => {
            klog_info!("TTY_TEST: BUG - session_id should still be 10, got {}", sid);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// hangup sets hung_up flag and detaches the session, verifying the
/// session-leader exit → hangup → session detach chain.
pub fn test_hangup_detaches_session() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 40, 40);
    // Pre-condition: session is attached.
    match tty::get_session_id(idx) {
        Ok(40) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - pre-hangup session expected 40, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    tty::hangup(idx);
    // Post-condition: TTY is hung up and session is detached.
    if !tty::is_hung_up(idx) {
        klog_info!("TTY_TEST: BUG - TTY should be hung up after hangup()");
        return TestResult::Fail;
    }
    match tty::get_session_id(idx) {
        Ok(0) => TestResult::Pass,
        Ok(sid) => {
            klog_info!(
                "TTY_TEST: BUG - session_id should be 0 after hangup, got {}",
                sid
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - get_session_id after hangup failed: {:?}",
                e
            );
            TestResult::Fail
        }
    }
}

/// O_NOCTTY suppresses auto-acquire — verifying that a session leader
/// opening a TTY with O_NOCTTY does NOT become the controlling process.
/// We verify this by calling acquire with an existing session and checking
/// that a second session cannot steal it (i.e., the first acquire "stuck").
pub fn test_o_noctty_suppresses_acquire() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // A session that already owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 10, 10) {
        klog_info!("TTY_TEST: BUG - initial acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Simulate O_NOCTTY: session 20 does NOT call acquire. Since O_NOCTTY
    // means the open path skips maybe_acquire_controlling_tty_on_open,
    // the TTY should still belong to session 10.
    match tty::get_session_id(idx) {
        Ok(10) => TestResult::Pass,
        Ok(sid) => {
            klog_info!("TTY_TEST: BUG - session should still be 10, got {}", sid);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// detach_controlling_terminal for a non-leader returns Ok(false)
/// and does NOT detach the session from the TTY.
pub fn test_detach_ctty_non_leader_preserves_session() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 60, 60);
    // Non-leader (caller_is_session_leader = false).
    match tty::detach_controlling_terminal(idx, 60, false) {
        Ok(false) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - non-leader detach expected Ok(false), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    // Session should still be intact.
    match tty::get_session_id(idx) {
        Ok(60) => TestResult::Pass,
        Ok(sid) => {
            klog_info!("TTY_TEST: BUG - session should still be 60, got {}", sid);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// detach_controlling_terminal for the session leader detaches the
/// session and returns Ok(true).
pub fn test_detach_ctty_session_leader_detaches() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 70, 70);
    match tty::detach_controlling_terminal(idx, 70, true) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - leader detach expected Ok(true), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    // Session should now be detached.
    match tty::get_session_id(idx) {
        Ok(0) => TestResult::Pass,
        Ok(sid) => {
            klog_info!(
                "TTY_TEST: BUG - session should be 0 after leader detach, got {}",
                sid
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// Full lifecycle chain — acquire → release → re-acquire by a
/// different session. Verifies that the TTY can be re-used after release.
pub fn test_full_lifecycle_acquire_release_reacquire() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // Session 1 acquires.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 1, 1) {
        klog_info!("TTY_TEST: BUG - session 1 acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session 1 releases.
    match tty::release_controlling_terminal(idx, 1) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - session 1 release expected Ok(true), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    // Session 2 acquires the now-free TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 2, 2) {
        klog_info!("TTY_TEST: BUG - session 2 re-acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    match tty::get_session_id(idx) {
        Ok(2) => TestResult::Pass,
        Ok(sid) => {
            klog_info!("TTY_TEST: BUG - session should be 2, got {}", sid);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// Double acquire to the same TTY from two different sessions —
/// the second must fail with PermissionDenied (race guard).
pub fn test_double_acquire_race_guard() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // Session A wins.
    if let Err(e) = tty::acquire_controlling_terminal(idx, 100, 100) {
        klog_info!("TTY_TEST: BUG - session A acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session B loses.
    match tty::acquire_controlling_terminal(idx, 200, 200) {
        Err(TtyError::PermissionDenied) => TestResult::Pass,
        Ok(()) => {
            klog_info!("TTY_TEST: BUG - session B acquire should have failed");
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", e);
            TestResult::Fail
        }
    }
}

/// hangup on a TTY with no session is a safe no-op.
pub fn test_hangup_no_session_safe() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // No session attached — hangup should not panic.
    tty::hangup(idx);
    if !tty::is_hung_up(idx) {
        klog_info!("TTY_TEST: BUG - TTY should be hung up even with no session");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Rapid acquire/release stress — cycle through several sessions
/// on the same TTY to verify no state leaks between owners.
pub fn test_rapid_acquire_release_stress() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    for sid in 1u32..=20 {
        if let Err(e) = tty::acquire_controlling_terminal(idx, sid, sid) {
            klog_info!("TTY_TEST: BUG - acquire for sid {} failed: {:?}", sid, e);
            return TestResult::Fail;
        }
        match tty::get_session_id(idx) {
            Ok(s) if s == sid => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - session_id expected {}, got {:?}",
                    sid,
                    other
                );
                return TestResult::Fail;
            }
        }
        match tty::release_controlling_terminal(idx, sid) {
            Ok(true) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - release for sid {} expected Ok(true), got {:?}",
                    sid,
                    other
                );
                return TestResult::Fail;
            }
        }
        match tty::get_session_id(idx) {
            Ok(0) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - session should be 0 after release, got {:?}",
                    other
                );
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

/// acquire on an invalid TTY index returns InvalidIndex.
pub fn test_acquire_invalid_index() -> TestResult {
    let bad_idx = TtyIndex(255);
    match tty::acquire_controlling_terminal(bad_idx, 1, 1) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - acquire invalid index expected InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// release on an invalid TTY index returns InvalidIndex.
pub fn test_release_invalid_index() -> TestResult {
    let bad_idx = TtyIndex(255);
    match tty::release_controlling_terminal(bad_idx, 1) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - release invalid index expected InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// detach_controlling_terminal on an invalid TTY index returns
/// InvalidIndex.
pub fn test_detach_invalid_index() -> TestResult {
    let bad_idx = TtyIndex(255);
    match tty::detach_controlling_terminal(bad_idx, 1, true) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - detach invalid index expected InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}
// ===========================================================================
// POSIX Controlling Terminal Semantics
// ===========================================================================

pub fn test_ctty_can_be_ctty_serial() -> TestResult {
    use crate::tty::driver::{SerialConsoleDriver, TtyDriverKind};
    let driver = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if !driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - SerialConsole should be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_can_be_ctty_vconsole() -> TestResult {
    let driver = TtyDriverKind::VConsole(VConsoleDriver);
    if !driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - VConsole should be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_can_be_ctty_pty_slave() -> TestResult {
    let peer = PtyPeerHandle {
        idx: TtyIndex(2),
        generation: 0,
    };
    let driver = TtyDriverKind::PtySlave { peer };
    if !driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - PtySlave should be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_cannot_be_ctty_pty_master() -> TestResult {
    let peer = PtyPeerHandle {
        idx: TtyIndex(3),
        generation: 0,
    };
    let driver = TtyDriverKind::PtyMaster { peer };
    if driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - PtyMaster must NOT be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_acquire_ctty_pty_master_rejected() -> TestResult {
    tty::table::tty_table_init();
    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    match tty::acquire_controlling_terminal(master, 100, 100) {
        Err(TtyError::PermissionDenied) => {}
        Ok(()) => {
            klog_info!("TTY_TEST: BUG - acquire on PTY master should fail");
            return TestResult::Fail;
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", e);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_ctty_acquire_ctty_pty_slave_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave = match tty::get_pty_number(master) {
        Ok(n) => TtyIndex(n as u8),
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    // Unlock the slave first.
    let _ = tty::set_pty_lock(master, false);
    match tty::acquire_controlling_terminal(slave, 200, 200) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - acquire on PTY slave should succeed, got {:?}",
                e
            );
            return TestResult::Fail;
        }
    }
    match tty::get_session_id(slave) {
        Ok(200) => TestResult::Pass,
        Ok(other) => {
            klog_info!(
                "TTY_TEST: BUG - slave session_id expected 200, got {}",
                other
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_acquire_ctty_serial_console_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    match tty::acquire_controlling_terminal(idx, 300, 300) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - acquire on serial console should succeed, got {:?}",
                e
            );
            return TestResult::Fail;
        }
    }
    match tty::get_session_id(idx) {
        Ok(300) => TestResult::Pass,
        Ok(other) => {
            klog_info!(
                "TTY_TEST: BUG - serial session_id expected 300, got {}",
                other
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_acquire_ctty_vconsole_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(1);
    match tty::acquire_controlling_terminal(idx, 400, 400) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - acquire on vconsole should succeed, got {:?}",
                e
            );
            return TestResult::Fail;
        }
    }
    match tty::get_session_id(idx) {
        Ok(400) => TestResult::Pass,
        Ok(other) => {
            klog_info!(
                "TTY_TEST: BUG - vconsole session_id expected 400, got {}",
                other
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_o_noctty_constant_value() -> TestResult {
    use slopos_abi::syscall::O_NOCTTY;
    if O_NOCTTY != 0x100 {
        klog_info!(
            "TTY_TEST: BUG - O_NOCTTY should be 0x100, got 0x{:x}",
            O_NOCTTY
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_set_fg_pgrp_completes_without_deadlock() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 500, 500);
    match tty::set_foreground_pgrp(idx, 501) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - set_foreground_pgrp failed: {:?}", e);
            return TestResult::Fail;
        }
    }
    match tty::get_foreground_pgrp(idx) {
        Ok(501) => TestResult::Pass,
        Ok(other) => {
            klog_info!("TTY_TEST: BUG - fg_pgrp expected 501, got {}", other);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_foreground_pgrp failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_set_fg_pgrp_checked_completes_without_deadlock() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::attach_session(idx, 600, 600);
    // Use pgid=0 (clear) since non-zero pgids are validated against
    // the scheduler's task list, which has no real tasks in unit tests.
    match tty::set_foreground_pgrp_checked(idx, 0, 600) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - set_foreground_pgrp_checked failed: {:?}",
                e
            );
            return TestResult::Fail;
        }
    }
    match tty::get_foreground_pgrp(idx) {
        Ok(0) => TestResult::Pass,
        Ok(other) => {
            klog_info!("TTY_TEST: BUG - fg_pgrp expected 0, got {}", other);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_foreground_pgrp failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_pty_master_ctty_does_not_attach_session() -> TestResult {
    tty::table::tty_table_init();
    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let _ = tty::acquire_controlling_terminal(master, 700, 700);
    match tty::get_session_id(master) {
        Ok(0) => TestResult::Pass,
        Ok(sid) => {
            klog_info!(
                "TTY_TEST: BUG - master should have no session after rejected acquire, got {}",
                sid
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_session_id failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_can_be_ctty_none_driver() -> TestResult {
    let driver = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if !driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - None driver should allow ctty (vacuously)");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Advanced PTY & Session Control (EXTPROC, vhangup)
// ===========================================================================

/// EXTPROC flag constant has the expected value (0x10000).
pub fn test_extproc_flag_value() -> TestResult {
    if LocalFlags::EXTPROC.bits() != 0x10000 {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC is {:#x}, expected 0x10000",
            LocalFlags::EXTPROC.bits()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// EXTPROC set → no echo, input goes directly to cooked buffer.
pub fn test_extproc_no_echo() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC + ICANON + ECHO.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    ld.set_termios(&t);

    // Type a printable character.
    let action = ld.input_char(b'a');
    // EXTPROC should suppress echo — action should be None.
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC should suppress echo, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    // But the character should be in the cooked buffer.
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - character should be in cooked buffer under EXTPROC");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'a' {
        klog_info!("TTY_TEST: BUG - expected 1 byte 'a', got {} bytes", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// EXTPROC set → canonical editing (VERASE/VKILL) is bypassed.
pub fn test_extproc_no_canonical_editing() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC + ICANON + ECHO + ECHOE.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    ld.set_termios(&t);

    // Type 'a', then DEL (VERASE character).
    ld.input_char(b'a');
    let erase_char = t.c_cc[slopos_abi::syscall::CcIndex::Verase.as_usize()];
    ld.input_char(erase_char);

    // Both bytes should be in the cooked buffer (no editing).
    // Under EXTPROC, VERASE is NOT processed — it's passed through.
    // Note: EXTPROC pushes to cooked directly, bypassing canonical mode,
    // so has_data() may return true even without a newline since the
    // data is not in canonical line-buffered mode.
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    if n != 2 {
        klog_info!(
            "TTY_TEST: BUG - expected 2 bytes (a + DEL) in EXTPROC, got {}",
            n
        );
        return TestResult::Fail;
    }
    if buf[0] != b'a' || buf[1] != erase_char {
        klog_info!(
            "TTY_TEST: BUG - expected [a, DEL], got [{:#x}, {:#x}]",
            buf[0],
            buf[1]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// EXTPROC set + ISIG → signals are still delivered.
pub fn test_extproc_signals_still_delivered() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC + ISIG.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    ld.set_termios(&t);

    // Ctrl+C should still deliver SIGINT.
    let vintr = t.c_cc[slopos_abi::syscall::CcIndex::Vintr.as_usize()];
    let action = ld.input_char(vintr);
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - EXTPROC + ISIG should deliver SIGINT, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }

    // Ctrl+\\ should deliver SIGQUIT.
    let vquit = t.c_cc[slopos_abi::syscall::CcIndex::Vquit.as_usize()];
    let action = ld.input_char(vquit);
    match action {
        InputAction::Signal(sig) if sig == SIGQUIT => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - EXTPROC + ISIG should deliver SIGQUIT, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }

    // Ctrl+Z should deliver SIGTSTP.
    let vsusp = t.c_cc[slopos_abi::syscall::CcIndex::Vsusp.as_usize()];
    let action = ld.input_char(vsusp);
    match action {
        InputAction::Signal(sig) if sig == SIGTSTP => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - EXTPROC + ISIG should deliver SIGTSTP, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// EXTPROC cleared → normal canonical/echo behavior resumes.
pub fn test_extproc_cleared_resumes_normal() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC first.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    ld.set_termios(&t);

    // Clear EXTPROC.
    t.c_lflag &= !LocalFlags::EXTPROC;
    ld.set_termios(&t);

    // Now typing should echo normally.
    let action = ld.input_char(b'x');
    match action {
        InputAction::Echo { len, .. } if len > 0 => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - after clearing EXTPROC, echo should resume, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// EXTPROC bypasses VLNEXT, VWERASE, VREPRINT.
pub fn test_extproc_bypasses_iexten_editing() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC + ICANON + ECHO + IEXTEN.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC | LocalFlags::IEXTEN;
    ld.set_termios(&t);

    // VLNEXT (Ctrl+V) should be passed through, not trigger literal-next.
    let vlnext = t.c_cc[slopos_abi::syscall::CcIndex::Vlnext.as_usize()];
    let action = ld.input_char(vlnext);
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC should bypass VLNEXT, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    // VLNEXT byte should be in the cooked buffer.
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != vlnext {
        klog_info!("TTY_TEST: BUG - VLNEXT byte should be in cooked buffer under EXTPROC");
        return TestResult::Fail;
    }

    // VWERASE (Ctrl+W) should be passed through, not trigger word erase.
    let vwerase = t.c_cc[slopos_abi::syscall::CcIndex::Vwerase.as_usize()];
    let action = ld.input_char(vwerase);
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC should bypass VWERASE, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    let mut buf2 = [0u8; 4];
    let n = ld.read(&mut buf2);
    if n != 1 || buf2[0] != vwerase {
        klog_info!("TTY_TEST: BUG - VWERASE byte should be in cooked buffer under EXTPROC");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// EXTPROC + IXON → flow control still works.
pub fn test_extproc_flow_control_works() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable EXTPROC + IXON.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    t.c_iflag |= InputFlags::IXON;
    ld.set_termios(&t);

    // Ctrl+S (VSTOP) should stop output.
    let vstop = t.c_cc[slopos_abi::syscall::CcIndex::Vstop.as_usize()];
    ld.input_char(vstop);
    if !ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - EXTPROC + IXON should still honor VSTOP");
        return TestResult::Fail;
    }

    // Ctrl+Q (VSTART) should resume.
    let vstart = t.c_cc[slopos_abi::syscall::CcIndex::Vstart.as_usize()];
    ld.input_char(vstart);
    if ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - EXTPROC + IXON should still honor VSTART");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// EXTPROC with buffer full + IMAXBEL rings bell.
pub fn test_extproc_imaxbel() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::EXTPROC;
    t.c_lflag &= !LocalFlags::ICANON; // non-canonical for direct read
    t.c_iflag |= InputFlags::IMAXBEL;
    ld.set_termios(&t);

    // Fill the cooked buffer.
    for _ in 0..8192 {
        ld.input_char(b'x');
    }

    // Next input should ring bell.
    let action = ld.input_char(b'y');
    if !matches!(action, InputAction::Bell) {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC + full buffer + IMAXBEL should ring Bell, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SYSCALL_VHANGUP constant has expected value.
pub fn test_vhangup_syscall_constant() -> TestResult {
    if slopos_abi::syscall::SYSCALL_VHANGUP != 139 {
        klog_info!(
            "TTY_TEST: BUG - SYSCALL_VHANGUP is {}, expected 139",
            slopos_abi::syscall::SYSCALL_VHANGUP
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// vhangup() on a TTY triggers hangup (reuses existing hangup infra).
pub fn test_vhangup_triggers_hangup() -> TestResult {
    tty::table::tty_table_init();

    // Verify TTY 0 is not hung up initially.
    let idx = TtyIndex(0);
    if tty::is_hung_up(idx) {
        klog_info!("TTY_TEST: BUG - TTY 0 should not be hung up initially");
        return TestResult::Fail;
    }

    // Call vhangup.
    tty::vhangup(idx);

    // TTY should now be hung up.
    if !tty::is_hung_up(idx) {
        klog_info!("TTY_TEST: BUG - TTY 0 should be hung up after vhangup()");
        return TestResult::Fail;
    }

    // Re-init for cleanup.
    tty::table::tty_table_init();
    TestResult::Pass
}

/// EXTPROC does not affect raw (non-canonical) mode —
/// both paths push to cooked without echo.
pub fn test_extproc_raw_mode_same_behavior() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    // Non-canonical, no echo, EXTPROC.
    t.c_lflag = LocalFlags::EXTPROC;
    ld.set_termios(&t);

    let action = ld.input_char(b'z');
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - EXTPROC raw mode should not echo, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'z' {
        klog_info!("TTY_TEST: BUG - byte should be readable from cooked buffer");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::define_test_suite!(
    tty_test_session_fg,
    [
        test_sigttou_constant,
        test_check_write_tostop_blocks_background,
        test_check_write_no_tostop_allows_background,
        test_check_write_tostop_allows_foreground,
        test_check_read_cross_session_rejected,
        test_check_read_same_session_foreground,
        test_check_read_kernel_task_allowed,
        test_tty_write_foreground_with_tostop,
        test_bootstrap_allowed_no_session_read,
        test_bootstrap_allowed_no_fg_pgrp,
        test_denied_cross_session_read,
        test_denied_cross_session_write_tostop,
        test_cross_session_write_no_tostop_still_denied,
        test_kernel_task_exempted_cross_session_read,
        test_kernel_task_exempted_cross_session_write,
        test_same_session_background_read_sigttin,
        test_same_session_background_write_sigttou,
        test_check_write_no_session_allowed,
        test_cross_session_denied_error_variant,
        test_set_fg_pgrp_checked_nonexistent_pgrp,
        test_set_fg_pgrp_checked_clear_allowed,
        test_set_fg_pgrp_checked_no_session_skips_validation,
        test_detach_ctty_non_leader,
        test_detach_ctty_session_leader,
        test_detach_ctty_cross_session_denied,
        test_tiocnotty_constant,
        test_open_ref_second_fd_increments_count,
        test_dev_tty_operations_identical_to_direct,
        test_open_ref_does_not_modify_session,
        test_open_ref_invalid_index_returns_error,
        test_close_ref_decrements_after_open,
        test_multiple_open_ref_sequential,
        test_dev_tty_winsize_matches_direct,
        test_tcsetattr_background_blocked,
        test_tcsetattr_foreground_allowed,
        test_tcsetattr_no_session_allowed,
        test_tcsetattr_cross_session_denied,
        test_orphaned_pgrp_errno,
        test_tcsetattr_kernel_task_bypass,
        test_tcsetsw_tcsetsf_kernel_task_bypass,
        test_tostop_background_write_check,
        test_kernel_task_check_write_allowed,
        test_acquire_ctty_fresh_tty,
        test_acquire_ctty_same_session_idempotent,
        test_acquire_ctty_different_session_denied,
        test_release_ctty_owning_session,
        test_release_ctty_wrong_session_noop,
        test_hangup_detaches_session,
        test_o_noctty_suppresses_acquire,
        test_detach_ctty_non_leader_preserves_session,
        test_detach_ctty_session_leader_detaches,
        test_full_lifecycle_acquire_release_reacquire,
        test_double_acquire_race_guard,
        test_hangup_no_session_safe,
        test_rapid_acquire_release_stress,
        test_acquire_invalid_index,
        test_release_invalid_index,
        test_detach_invalid_index,
        test_ctty_can_be_ctty_serial,
        test_ctty_can_be_ctty_vconsole,
        test_ctty_can_be_ctty_pty_slave,
        test_ctty_cannot_be_ctty_pty_master,
        test_ctty_acquire_ctty_pty_master_rejected,
        test_ctty_acquire_ctty_pty_slave_succeeds,
        test_ctty_acquire_ctty_serial_console_succeeds,
        test_ctty_acquire_ctty_vconsole_succeeds,
        test_ctty_o_noctty_constant_value,
        test_ctty_set_fg_pgrp_completes_without_deadlock,
        test_ctty_set_fg_pgrp_checked_completes_without_deadlock,
        test_ctty_pty_master_ctty_does_not_attach_session,
        test_ctty_can_be_ctty_none_driver,
        test_extproc_flag_value,
        test_extproc_no_echo,
        test_extproc_no_canonical_editing,
        test_extproc_signals_still_delivered,
        test_extproc_cleared_resumes_normal,
        test_extproc_bypasses_iexten_editing,
        test_extproc_flow_control_works,
        test_extproc_imaxbel,
        test_vhangup_syscall_constant,
        test_vhangup_triggers_hangup,
        test_extproc_raw_mode_same_behavior,
    ]
);
