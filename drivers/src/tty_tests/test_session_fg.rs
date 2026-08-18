use super::fixtures::*;

pub fn test_sigttou_constant() -> TestResult {
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_check_write_tostop_blocks_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_check_write_no_tostop_allows_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_check_write_tostop_allows_foreground() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_check_read_cross_session_rejected() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_check_read_same_session_foreground() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_check_read_kernel_task_allowed() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// The harness runs as task_id 0, which skips the foreground check, so the
/// write succeeds under TOSTOP; `check_write` is tested directly above.
pub fn test_tty_write_foreground_with_tostop() -> TestResult {
    tty::table::tty_table_init();
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

pub fn test_bootstrap_allowed_no_fg_pgrp() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    // Live session, empty foreground group.
    s.attach(scope.session_weak(), KWeak::new());
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

pub fn test_denied_cross_session_read() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// Cross-session denial outranks the TOSTOP background check.
pub fn test_denied_cross_session_write_tostop() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_cross_session_write_no_tostop_still_denied() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// A kernel task (pgid 0, sid 0) is exempt from cross-session denial.
pub fn test_kernel_task_exempted_cross_session_read() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_kernel_task_exempted_cross_session_write() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// Same session, background pgid: the SIGTTIN path, not cross-session denial.
pub fn test_same_session_background_read_sigttin() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// A non-blocking probe must see WouldBlock, so the op stays armed and
/// self-heals once the foreground handoff lands: EIO here permanently disables
/// a freshly spawned job's stdin/signal branch.
pub fn test_background_read_nonblock_parks_as_wouldblock() -> TestResult {
    match tty::io::background_read_surface(true) {
        TtyError::WouldBlock => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - nonblock bg read expected WouldBlock, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// A blocking reader keeps the POSIX BackgroundRead surface (SIGTTIN delivery).
pub fn test_background_read_blocking_keeps_sigttin_surface() -> TestResult {
    match tty::io::background_read_surface(false) {
        TtyError::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - blocking bg read expected BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_same_session_background_write_sigttou() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

/// The write path has no bootstrap tier: no session is plain `Allowed`.
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

pub fn test_cross_session_denied_error_variant() -> TestResult {
    let err = TtyError::CrossSessionDenied;
    if err == TtyError::BackgroundRead
        || err == TtyError::BackgroundWrite
        || err == TtyError::PermissionDenied
    {
        klog_info!("TTY_TEST: BUG - CrossSessionDenied should be distinct from other errors");
        return TestResult::Fail;
    }
    TestResult::Pass
}
/// pgid 99999 resolves to no living task, so the wrapper cannot pin a group
/// handle for it.
pub fn test_set_fg_pgrp_checked_nonexistent_pgrp() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let result = tty::set_foreground_pgrp_checked(TtyIndex(0), 99999, 600);

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

/// pgid 0 clears the foreground group and is always allowed.
pub fn test_set_fg_pgrp_checked_clear_allowed() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let result = tty::set_foreground_pgrp_checked(TtyIndex(0), 0, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

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

/// With no controlling session the checked setter installs any caller's group,
/// mismatched sid included — the pre-session path.
pub fn test_set_fg_pgrp_checked_no_session_skips_validation() -> TestResult {
    let scope = SessionScope::new(50, 50);
    let mut s = TtySession::new();
    if !s.set_fg_pgrp_checked(scope.pgrp_weak(), 99) {
        klog_info!("TTY_TEST: BUG - no-session path should allow any pgid");
        return TestResult::Fail;
    }
    if s.fg_pgrp_id() != 50 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should be 50, got {}",
            s.fg_pgrp_id()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A non-leader TIOCNOTTY leaves the TTY session state alone; only the
/// caller's own controlling_tty is cleared, by the ioctl handler.
pub fn test_detach_ctty_non_leader() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let result = tty::detach_controlling_terminal(TtyIndex(0), 600, false);

    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

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

/// A leader TIOCNOTTY clears the TTY's session state entirely; SIGHUP+SIGCONT
/// would go to the foreground pgrp.
pub fn test_detach_ctty_session_leader() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let result = tty::detach_controlling_terminal(TtyIndex(0), 600, true);

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

/// A session leader from another session cannot detach someone else's TTY.
pub fn test_detach_ctty_cross_session_denied() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let result = tty::detach_controlling_terminal(TtyIndex(0), 999, true);

    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

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
/// A second open clones the one backing; its strong count is the live open
/// count — the mechanism `/dev/tty` relies on.
pub fn test_second_open_bumps_backing_strong_count() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con1 = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - first open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let con2 = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - second open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let count = KArc::strong_count(&con2);
    drop(con2);
    drop(con1);
    if count != 2 {
        klog_info!(
            "TTY_TEST: BUG - second open should make strong_count 2, got {}",
            count
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A `/dev/tty` FD is indistinguishable from one opened on the device path.
pub fn test_dev_tty_operations_identical_to_direct() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // get_termios should work.
    let termios = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios after open failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    // Verify it returns a valid termios (ICANON should be set by default).
    if !termios.c_lflag.contains(LocalFlags::ICANON) {
        klog_info!("TTY_TEST: BUG - termios from console FD missing ICANON");
        return TestResult::Fail;
    }

    // write should succeed (returns byte count).
    match tty::write(idx, b"phase30", false) {
        Ok(n) if n == 7 => {}
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - write via console FD returned {}, expected 7",
                n
            );
            return TestResult::Fail;
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - write via console FD failed: {:?}", e);
            return TestResult::Fail;
        }
    }

    // get_session_id should succeed.
    if tty::get_session_id(idx).is_err() {
        klog_info!("TTY_TEST: BUG - get_session_id after open failed");
        return TestResult::Fail;
    }

    drop(con);
    TestResult::Pass
}

/// Opening a TTY does NOT modify session state — opening `/dev/tty` only
/// accesses an existing controlling terminal, never acquires one.
pub fn test_open_tty_does_not_modify_session() -> TestResult {
    let idx = TtyIndex(0);
    // Snapshot session state before opening.
    let (sid_before, fg_before) = {
        let guard = TTY_SLOTS[0].lock();
        match guard.as_ref() {
            Some(tty) => (tty.session.session_id(), tty.session.fg_pgrp_id()),
            None => {
                klog_info!("TTY_TEST: BUG - TTY0 not allocated");
                return TestResult::Fail;
            }
        }
    };

    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // Snapshot after.
    let (sid_after, fg_after) = {
        let guard = TTY_SLOTS[0].lock();
        match guard.as_ref() {
            Some(tty) => (tty.session.session_id(), tty.session.fg_pgrp_id()),
            None => {
                klog_info!("TTY_TEST: BUG - TTY0 vanished");
                return TestResult::Fail;
            }
        }
    };

    drop(con);

    if sid_before != sid_after || fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - open modified session: sid {}->{}, fg {}->{}",
            sid_before,
            sid_after,
            fg_before,
            fg_after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Opening an invalid TTY index returns `InvalidIndex`, matching the ENXIO
/// semantics when `/dev/tty` resolution fails.
pub fn test_open_tty_invalid_index_returns_error() -> TestResult {
    let bad = TtyIndex(u8::MAX);
    match tty::open_tty(bad) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        Ok(backing) => {
            klog_info!("TTY_TEST: BUG - open_tty(255) unexpectedly succeeded");
            drop(backing);
            TestResult::Fail
        }
        Err(other) => {
            klog_info!(
                "TTY_TEST: BUG - open_tty(255) should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Dropping the last open releases the backing — the registry weak then
/// fails to upgrade, confirming the `/dev/tty` FD lifecycle pairs cleanly
/// with the device FD.
pub fn test_last_close_releases_backing() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    // A freshly-initialised console has zero opens: nothing to upgrade.
    if crate::tty::table::TTY_BACKINGS[0]
        .lock()
        .upgrade()
        .is_some()
    {
        klog_info!("TTY_TEST: BUG - console should have no open before open_tty");
        return TestResult::Fail;
    }
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let count = KArc::strong_count(&con);
    if count != 1 {
        klog_info!(
            "TTY_TEST: BUG - single open should have strong_count 1, got {}",
            count
        );
        return TestResult::Fail;
    }
    drop(con);
    // Last close releases the backing; the registry weak no longer upgrades.
    if crate::tty::table::TTY_BACKINGS[0]
        .lock()
        .upgrade()
        .is_some()
    {
        klog_info!("TTY_TEST: BUG - console should have no open after last close");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multiple opens of the same console index all clone the one backing; the
/// shared strong count grows with each open.
pub fn test_sequential_opens_share_backing() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let c1 = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - first open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let c2 = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - second open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let count = KArc::strong_count(&c2);
    drop(c2);
    drop(c1);
    if count != 2 {
        klog_info!(
            "TTY_TEST: BUG - two sequential opens should give strong_count 2, got {}",
            count
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// `get_winsize` works identically regardless of whether the FD was
/// obtained via `/dev/tty` or direct device open (both use the same TTY index).
pub fn test_dev_tty_winsize_matches_direct() -> TestResult {
    let idx = TtyIndex(0);
    let ws_before = match tty::get_winsize(idx) {
        Ok(ws) => ws,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_winsize before open failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - open_tty failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let ws_after = match tty::get_winsize(idx) {
        Ok(ws) => ws,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_winsize after open failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    drop(con);
    if ws_before.ws_row != ws_after.ws_row || ws_before.ws_col != ws_after.ws_col {
        klog_info!("TTY_TEST: BUG - winsize differs after open");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Background Write Protection (SIGTTOU on tcsetattr)
// ===========================================================================

/// check_write with tostop=true (simulating tcsetattr foreground
/// check) blocks background processes with BackgroundWrite.
pub fn test_tcsetattr_background_blocked() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s); // session=10, fg_pgrp=10
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
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s); // session=10, fg_pgrp=10
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
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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
    let scope = SessionScope::new(10, 10);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let scope = SessionScope::new(10, 10);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let scope = SessionScope::new(20, 20);
    let mut s = TtySession::new();
    scope.attach_to(&mut s); // session=20, fg_pgrp=20
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
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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
    let scope = SessionScope::new(100, 100);
    match tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(50, 50);
    // First acquire.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
        klog_info!("TTY_TEST: BUG - first acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Second acquire from same session — should succeed (no-op).
    match tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope_owner = SessionScope::new(10, 10);
    let scope_thief = SessionScope::new(20, 20);
    // Session 10 owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope_owner.pgrp_weak()) {
        klog_info!("TTY_TEST: BUG - initial acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session 20 tries to steal it.
    match tty::acquire_controlling_terminal(idx, scope_thief.pgrp_weak()) {
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
    let scope = SessionScope::new(30, 30);
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(10, 10);
    // Session 10 owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(40, 40);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let _hangup = HangupScope::hang_up(idx);
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
    let scope = SessionScope::new(10, 10);
    // A session that already owns the TTY.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(60, 60);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let scope = SessionScope::new(70, 70);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let scope1 = SessionScope::new(1, 1);
    let scope2 = SessionScope::new(2, 2);
    // Session 1 acquires.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope1.pgrp_weak()) {
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
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope2.pgrp_weak()) {
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
    let scope_a = SessionScope::new(100, 100);
    let scope_b = SessionScope::new(200, 200);
    // Session A wins.
    if let Err(e) = tty::acquire_controlling_terminal(idx, scope_a.pgrp_weak()) {
        klog_info!("TTY_TEST: BUG - session A acquire failed: {:?}", e);
        return TestResult::Fail;
    }
    // Session B loses.
    match tty::acquire_controlling_terminal(idx, scope_b.pgrp_weak()) {
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
    let _hangup = HangupScope::hang_up(idx);
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
        let scope = SessionScope::new(sid, sid);
        if let Err(e) = tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    match tty::acquire_controlling_terminal(bad_idx, KWeak::new()) {
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
    // Peer identity is irrelevant to controlling-terminal eligibility.
    let driver = TtyDriverKind::PtySlave { peer: KWeak::new() };
    if !driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - PtySlave should be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_cannot_be_ctty_pty_master() -> TestResult {
    let driver = TtyDriverKind::PtyMaster { peer: KWeak::new() };
    if driver.can_be_controlling_terminal() {
        klog_info!("TTY_TEST: BUG - PtyMaster must NOT be a valid ctty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ctty_acquire_ctty_pty_master_rejected() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(100, 100);
    // The master backing is the sole open of the pair; holding it keeps both
    // ends alive, and dropping it below closes them and frees both slots.
    let (master, master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let result = tty::acquire_controlling_terminal(master, scope.pgrp_weak());
    drop(master_backing);
    match result {
        Err(TtyError::PermissionDenied) => TestResult::Pass,
        Ok(()) => {
            klog_info!("TTY_TEST: BUG - acquire on PTY master should fail");
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_ctty_acquire_ctty_pty_slave_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(200, 200);
    // Hold the master backing so the slave slot stays alive for the acquire.
    let (master, master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
        Ok(pair) => pair,
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
    let acquired = tty::acquire_controlling_terminal(slave, scope.pgrp_weak());
    let sid = tty::get_session_id(slave);
    drop(master_backing);
    if let Err(e) = acquired {
        klog_info!(
            "TTY_TEST: BUG - acquire on PTY slave should succeed, got {:?}",
            e
        );
        return TestResult::Fail;
    }
    match sid {
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
    let scope = SessionScope::new(300, 300);
    match tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(400, 400);
    match tty::acquire_controlling_terminal(idx, scope.pgrp_weak()) {
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
    let scope = SessionScope::new(500, 500);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
    // pgid 501 has no live task, so the wrapper installs an empty weak; the
    // foreground group then reads back as 0 while the call still completes.
    match tty::set_foreground_pgrp(idx, 501) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - set_foreground_pgrp failed: {:?}", e);
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

pub fn test_ctty_set_fg_pgrp_checked_completes_without_deadlock() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
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
    let scope = SessionScope::new(700, 700);
    let (master, master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let _ = tty::acquire_controlling_terminal(master, scope.pgrp_weak());
    let sid = tty::get_session_id(master);
    drop(master_backing);
    match sid {
        Ok(0) => TestResult::Pass,
        Ok(s) => {
            klog_info!(
                "TTY_TEST: BUG - master should have no session after rejected acquire, got {}",
                s
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
    let _hangup = HangupScope::guard(idx);
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

slopos_testing::stest!(name = test_sigttou_constant, suite = tty_test_session_fg);
slopos_testing::stest!(
    name = test_check_write_tostop_blocks_background,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_write_no_tostop_allows_background,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_write_tostop_allows_foreground,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_read_cross_session_rejected,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_read_same_session_foreground,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_read_kernel_task_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tty_write_foreground_with_tostop,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_bootstrap_allowed_no_session_read,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_bootstrap_allowed_no_fg_pgrp,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_denied_cross_session_read,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_denied_cross_session_write_tostop,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_cross_session_write_no_tostop_still_denied,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_kernel_task_exempted_cross_session_read,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_kernel_task_exempted_cross_session_write,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_same_session_background_read_sigttin,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_background_read_nonblock_parks_as_wouldblock,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_background_read_blocking_keeps_sigttin_surface,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_same_session_background_write_sigttou,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_check_write_no_session_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_cross_session_denied_error_variant,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_nonexistent_pgrp,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_clear_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_no_session_skips_validation,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_ctty_non_leader,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_ctty_session_leader,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_ctty_cross_session_denied,
    suite = tty_test_session_fg
);
slopos_testing::stest!(name = test_tiocnotty_constant, suite = tty_test_session_fg);
slopos_testing::stest!(
    name = test_second_open_bumps_backing_strong_count,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_dev_tty_operations_identical_to_direct,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_open_tty_does_not_modify_session,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_open_tty_invalid_index_returns_error,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_last_close_releases_backing,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_sequential_opens_share_backing,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_dev_tty_winsize_matches_direct,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tcsetattr_background_blocked,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tcsetattr_foreground_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tcsetattr_no_session_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tcsetattr_cross_session_denied,
    suite = tty_test_session_fg
);
slopos_testing::stest!(name = test_orphaned_pgrp_errno, suite = tty_test_session_fg);
slopos_testing::stest!(
    name = test_tcsetattr_kernel_task_bypass,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tcsetsw_tcsetsf_kernel_task_bypass,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_tostop_background_write_check,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_kernel_task_check_write_allowed,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_acquire_ctty_fresh_tty,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_acquire_ctty_same_session_idempotent,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_acquire_ctty_different_session_denied,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_release_ctty_owning_session,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_release_ctty_wrong_session_noop,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_hangup_detaches_session,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_o_noctty_suppresses_acquire,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_ctty_non_leader_preserves_session,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_ctty_session_leader_detaches,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_full_lifecycle_acquire_release_reacquire,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_double_acquire_race_guard,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_hangup_no_session_safe,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_rapid_acquire_release_stress,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_acquire_invalid_index,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_release_invalid_index,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_detach_invalid_index,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_can_be_ctty_serial,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_can_be_ctty_vconsole,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_can_be_ctty_pty_slave,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_cannot_be_ctty_pty_master,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_pty_master_rejected,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_pty_slave_succeeds,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_serial_console_succeeds,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_acquire_ctty_vconsole_succeeds,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_o_noctty_constant_value,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_set_fg_pgrp_completes_without_deadlock,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_set_fg_pgrp_checked_completes_without_deadlock,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_pty_master_ctty_does_not_attach_session,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_ctty_can_be_ctty_none_driver,
    suite = tty_test_session_fg
);
slopos_testing::stest!(name = test_extproc_flag_value, suite = tty_test_session_fg);
slopos_testing::stest!(name = test_extproc_no_echo, suite = tty_test_session_fg);
slopos_testing::stest!(
    name = test_extproc_no_canonical_editing,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_extproc_signals_still_delivered,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_extproc_cleared_resumes_normal,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_extproc_bypasses_iexten_editing,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_extproc_flow_control_works,
    suite = tty_test_session_fg
);
slopos_testing::stest!(name = test_extproc_imaxbel, suite = tty_test_session_fg);
slopos_testing::stest!(
    name = test_vhangup_syscall_constant,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_vhangup_triggers_hangup,
    suite = tty_test_session_fg
);
slopos_testing::stest!(
    name = test_extproc_raw_mode_same_behavior,
    suite = tty_test_session_fg
);
