use super::*;

// ===========================================================================
// TtySession tests
// ===========================================================================

/// New TtySession has zero values.
pub fn test_session_new_empty() -> TestResult {
    let s = TtySession::new();
    if s.session_leader_raw() != NO_SESSION
        || s.session_id_raw() != NO_SESSION
        || s.fg_pgrp_raw() != NO_FOREGROUND_PGRP
        || s.focused_task_id != 0
    {
        klog_info!("TTY_TEST: BUG - new TtySession has non-zero fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Attaching a session sets leader, session_id, and fg_pgrp.
pub fn test_session_attach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(100, 100);
    if s.session_leader_raw() != 100 || s.session_id_raw() != 100 || s.fg_pgrp_raw() != 100 {
        klog_info!("TTY_TEST: BUG - session attach did not set fields correctly");
        return TestResult::Fail;
    }
    if !s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() false after attach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Detaching a session resets leader, session_id, and fg_pgrp.
pub fn test_session_detach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(200, 200);
    s.detach();
    if s.session_leader_raw() != NO_SESSION
        || s.session_id_raw() != NO_SESSION
        || s.fg_pgrp_raw() != NO_FOREGROUND_PGRP
    {
        klog_info!("TTY_TEST: BUG - session detach did not reset fields");
        return TestResult::Fail;
    }
    if s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() true after detach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Foreground reader gets Allowed.
pub fn test_session_check_read_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(10, 10) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - foreground read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Background reader gets BackgroundRead.
pub fn test_session_check_read_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - background read expected BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// No session attached — check_read returns BootstrapAllowed (permissive).
pub fn test_session_check_read_no_session() -> TestResult {
    let s = TtySession::new();
    match s.check_read(42, 42) {
        ForegroundCheck::BootstrapAllowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no-session read expected BootstrapAllowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Kernel task (pgid=0) gets Allowed even if not in foreground group.
pub fn test_session_check_read_kernel_task() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
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

/// check_write without TOSTOP always returns Allowed.
pub fn test_session_check_write_no_tostop() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Background process, but TOSTOP is false.
    match s.check_write(99, 10, false) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - write without TOSTOP expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write with TOSTOP and background caller returns BackgroundWrite.
pub fn test_session_check_write_tostop_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_write(99, 10, true) {
        ForegroundCheck::BackgroundWrite => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TOSTOP background write expected BackgroundWrite, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read replaces task_has_access — foreground task allowed.
pub fn test_session_check_read_replaces_task_has_access_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(10, 10) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fg pgrp member should be Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read replaces task_has_access — background task gets BackgroundRead.
pub fn test_session_check_read_replaces_task_has_access_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    s.focused_task_id = 0; // No compositor focus.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - background task should be BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_read replaces task_has_access — permissive when no session.
pub fn test_session_check_read_replaces_task_has_access_permissive() -> TestResult {
    let s = TtySession::new();
    match s.check_read(999, 0) {
        ForegroundCheck::BootstrapAllowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no session should be BootstrapAllowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// set_fg_pgrp_checked: allowed when caller is in the same session.
pub fn test_session_set_fg_pgrp_checked_allowed() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    if !s.set_fg_pgrp_checked(20, 10) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow same-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp_raw() != 20 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 20");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_fg_pgrp_checked: denied when caller is in a different session.
pub fn test_session_set_fg_pgrp_checked_denied() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    if s.set_fg_pgrp_checked(20, 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should deny different-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp_raw() != 10 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should remain 10 after denied set");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_fg_pgrp_checked: allowed when no session is attached (permissive).
pub fn test_session_set_fg_pgrp_checked_no_session() -> TestResult {
    let mut s = TtySession::new();
    if !s.set_fg_pgrp_checked(50, 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow when no session");
        return TestResult::Fail;
    }
    if s.fg_pgrp_raw() != 50 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 50");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: get_session_id returns 0 when no session is attached.
pub fn test_tty_get_session_id_default() -> TestResult {
    tty::table::tty_table_init();
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - default session_id should be 0, got {}",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: attach_session + get_session_id round-trip.
pub fn test_tty_attach_session() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 300, 300);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    // Clean up.
    tty::detach_session(TtyIndex(0));
    if sid != 300 {
        klog_info!(
            "TTY_TEST: BUG - attach_session/get_session_id round-trip failed (got {})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: detach_session resets session_id to 0.
pub fn test_tty_detach_session() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 400, 400);
    tty::detach_session(TtyIndex(0));
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - detach_session did not reset session_id (got {})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: detach_session_by_id only detaches matching session.
pub fn test_tty_detach_session_by_id() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 500, 500);
    // Detach with wrong ID — should be a no-op.
    tty::detach_session_by_id(999);
    let sid_after_wrong = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    // Detach with correct ID.
    tty::detach_session_by_id(500);
    let sid_after_correct = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    if sid_after_wrong != 500 {
        klog_info!(
            "TTY_TEST: BUG - detach_session_by_id with wrong ID should be no-op (got {})",
            sid_after_wrong
        );
        return TestResult::Fail;
    }
    if sid_after_correct != 0 {
        klog_info!(
            "TTY_TEST: BUG - detach_session_by_id with correct ID should reset (got {})",
            sid_after_correct
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: set_foreground_pgrp_checked with session validation.
///
/// The outer API now validates that the target pgrp has
/// living members in the session.  For the "same-session allows" case we
/// use pgid=0 (clear foreground group) which bypasses pgrp existence.
/// The cross-session case uses a synthetic pgid that doesn't exist, which
/// is also correctly denied (now for pgrp-existence rather than session).
pub fn test_tty_set_fg_pgrp_checked() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);

    // Same session, pgid=0 (clear) — should succeed.
    let ok = tty::set_foreground_pgrp_checked(TtyIndex(0), 0, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

    // Different session — should fail (pgrp 800 doesn't exist in session 600).
    let denied = tty::set_foreground_pgrp_checked(TtyIndex(0), 800, 999);
    let pgid_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

    // Clean up.
    tty::detach_session(TtyIndex(0));
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0);

    if ok.is_err() {
        klog_info!(
            "TTY_TEST: BUG - set_fg_pgrp_checked same-session clear returned {:?}",
            ok
        );
        return TestResult::Fail;
    }
    if pgid != 0 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should be 0 after checked clear (got {})",
            pgid
        );
        return TestResult::Fail;
    }
    if denied.is_ok() {
        klog_info!(
            "TTY_TEST: BUG - set_fg_pgrp_checked different-session should fail (got {:?})",
            denied
        );
        return TestResult::Fail;
    }
    if pgid_after != 0 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should remain 0 after denied set (got {})",
            pgid_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
