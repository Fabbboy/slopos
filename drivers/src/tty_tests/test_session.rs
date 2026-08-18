use super::fixtures::*;

pub fn test_session_new_empty() -> TestResult {
    let s = TtySession::new();
    if s.session_id() != 0 || s.fg_pgrp_id() != 0 || s.focused_task_id != 0 {
        klog_info!("TTY_TEST: BUG - new TtySession has non-zero fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_attach() -> TestResult {
    let scope = SessionScope::new(100, 100);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    if s.session_id() != 100 || s.fg_pgrp_id() != 100 {
        klog_info!("TTY_TEST: BUG - session attach did not set fields correctly");
        return TestResult::Fail;
    }
    if !s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() false after attach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_detach() -> TestResult {
    let scope = SessionScope::new(200, 200);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    s.detach();
    if s.session_id() != 0 || s.fg_pgrp_id() != 0 {
        klog_info!("TTY_TEST: BUG - session detach did not reset fields");
        return TestResult::Fail;
    }
    if s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() true after detach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_check_read_foreground() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_session_check_read_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_session_check_read_kernel_task() -> TestResult {
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

pub fn test_session_check_write_no_tostop() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_session_check_write_tostop_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_session_check_read_replaces_task_has_access_foreground() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
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

pub fn test_session_check_read_replaces_task_has_access_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    s.focused_task_id = 0;
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

pub fn test_session_set_fg_pgrp_checked_allowed() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    let g20 = scope.extra_group(20);
    if !s.set_fg_pgrp_checked(KArc::downgrade(&g20), 10) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow same-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp_id() != 20 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 20");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_set_fg_pgrp_checked_denied() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    let g20 = scope.extra_group(20);
    if s.set_fg_pgrp_checked(KArc::downgrade(&g20), 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should deny different-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp_id() != 10 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should remain 10 after denied set");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_set_fg_pgrp_checked_no_session() -> TestResult {
    let scope = SessionScope::new(50, 50);
    let mut s = TtySession::new();
    if !s.set_fg_pgrp_checked(scope.pgrp_weak(), 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow when no session");
        return TestResult::Fail;
    }
    if s.fg_pgrp_id() != 50 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 50");
        return TestResult::Fail;
    }
    TestResult::Pass
}

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

pub fn test_tty_attach_session() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(300, 300);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::detach_session(TtyIndex(0));
    if sid != 300 {
        klog_info!(
            "TTY_TEST: BUG - install/get_session_id round-trip failed (got {})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_detach_session() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(400, 400);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
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

pub fn test_tty_detach_session_by_id() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(500, 500);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    tty::detach_session_by_id(999);
    let sid_after_wrong = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
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

/// The outer API also requires the target pgrp to have living members, so the
/// same-session case passes pgid 0 (clear) to bypass that existence check.
pub fn test_tty_set_fg_pgrp_checked() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());

    let ok = tty::set_foreground_pgrp_checked(TtyIndex(0), 0, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

    let denied = tty::set_foreground_pgrp_checked(TtyIndex(0), 800, 999);
    let pgid_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(u32::MAX);

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
