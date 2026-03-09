//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (compositor focus, foreground pgrp, active TTY).
//!
//! Phase 2 additions: input flag processing, output processing, signal
//! generation, flow control, VLNEXT, VWERASE, ECHOCTL.
//!
//! Phase 6 additions: compositor focus / fg_pgrp split, check_read() as sole
//! read gate, TtyIndex type safety, signal constant verification.

use slopos_abi::signal::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU, SIGWINCH};
use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::tty;
use crate::tty::TtyError;
use crate::tty::TtyIndex;
use crate::tty::driver::{DriverId, TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LdiscKind, LineDisc, OutputAction, RawDisc};
use crate::tty::session::TtySession;
use crate::tty::session::{
    ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION, ProcessGroupId, SessionId,
};
use crate::tty::table::TTY_SLOTS;
use crate::tty::vconsole::VConsoleState;

fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        match tty::read(idx, &mut scratch, true) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

// ===========================================================================
// LineDisc tests
// ===========================================================================

/// A fresh LineDisc has no data.
pub fn test_ldisc_new_has_no_data() -> TestResult {
    let ld = LineDisc::new();
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - new LineDisc reports has_data()=true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Reading from an empty LineDisc returns 0 bytes.
pub fn test_ldisc_read_empty() -> TestResult {
    let mut ld = LineDisc::new();
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - read from empty LineDisc returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: characters accumulate in edit buffer, flush on newline.
pub fn test_ldisc_canonical_newline() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc" — should not produce cooked data yet.
    for &c in b"abc" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has data before newline");
        return TestResult::Fail;
    }

    // Press Enter — should flush "abc\n" to cooked.
    ld.input_char(b'\n');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has no data after newline");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - cooked data mismatch");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Canonical mode: VERASE (backspace) removes the last character.
pub fn test_ldisc_canonical_backspace() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abcd", then backspace, then newline.
    for &c in b"abcd" {
        ld.input_char(c);
    }
    ld.input_char(0x08); // BS
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes (abc\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - expected \"abc\\n\", got {:?}", &buf[..n]);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: VKILL clears the entire edit buffer.
pub fn test_ldisc_canonical_kill() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"hello" {
        ld.input_char(c);
    }
    // Kill line (default VKILL = 0x15 = Ctrl+U).
    ld.input_char(0x15);
    // Type "world" and newline.
    for &c in b"world" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 6 {
        klog_info!("TTY_TEST: BUG - expected 6 bytes (world\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..6] != b"world\n" {
        klog_info!("TTY_TEST: BUG - data mismatch after kill");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: VEOF (Ctrl+D) flushes without adding a newline.
pub fn test_ldisc_canonical_eof() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    // EOF = 0x04
    ld.input_char(0x04);

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 {
        klog_info!("TTY_TEST: BUG - expected 3 bytes after EOF, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - data mismatch after EOF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ISIG: Ctrl+C (VINTR) generates a signal action.
pub fn test_ldisc_signal_ctrl_c() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x03); // Ctrl+C
    match action {
        InputAction::Signal(SIGINT) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!("TTY_TEST: BUG - expected SIGINT({}), got {}", SIGINT, s);
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+C did not produce Signal action");
            TestResult::Fail
        }
    }
}

/// Non-canonical mode: characters go directly to cooked buffer.
pub fn test_ldisc_raw_mode() -> TestResult {
    let mut ld = LineDisc::new();

    // Switch to raw mode.
    let mut termios = *ld.termios();
    termios.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&termios);

    // Each character should be immediately available.
    ld.input_char(b'a');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - raw mode char not immediately available");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 1];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'a' {
        klog_info!("TTY_TEST: BUG - raw mode read mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_termios: switching from canonical to raw flushes the edit buffer.
pub fn test_ldisc_set_termios_flush() -> TestResult {
    let mut ld = LineDisc::new();

    // Type some chars in canonical mode (not yet flushed).
    for &c in b"partial" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical should not have data before newline");
        return TestResult::Fail;
    }

    // Switch to raw mode — edit buffer should flush.
    let mut termios = *ld.termios();
    termios.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&termios);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - set_termios to raw did not flush edit buffer");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 7 || &buf[..7] != b"partial" {
        klog_info!("TTY_TEST: BUG - flushed data mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_flush_all() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"abc\n" {
        ld.input_char(c);
    }
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - expected data before flush_all");
        return TestResult::Fail;
    }
    ld.flush_all();
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - flush_all left cooked data");
        return TestResult::Fail;
    }
    let mut out = [0u8; 8];
    if ld.read(&mut out) != 0 {
        klog_info!("TTY_TEST: BUG - flush_all should empty read path");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ECHO mode: printable characters return Echo action.
pub fn test_ldisc_echo_printable() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'x');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'x' {
                klog_info!("TTY_TEST: BUG - echo mismatch for 'x'");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for printable char");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// ECHO mode: newline returns Echo action with '\n'.
pub fn test_ldisc_echo_newline() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'\n');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - echo mismatch for newline");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for newline");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

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

/// Phase 6: check_read replaces task_has_access — foreground task allowed.
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

/// Phase 6: check_read replaces task_has_access — background task gets BackgroundRead.
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

/// Phase 6: check_read replaces task_has_access — permissive when no session.
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
pub fn test_tty_set_fg_pgrp_checked() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);
    // Same session — should succeed.
    let ok = tty::set_foreground_pgrp_checked(TtyIndex(0), 700, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    // Different session — should fail.
    let denied = tty::set_foreground_pgrp_checked(TtyIndex(0), 800, 999);
    let pgid_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    // Clean up.
    tty::detach_session(TtyIndex(0));
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0);
    if ok.is_err() {
        klog_info!(
            "TTY_TEST: BUG - set_fg_pgrp_checked same-session returned {:?}",
            ok
        );
        return TestResult::Fail;
    }
    if pgid != 700 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should be 700 after checked set (got {})",
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
    if pgid_after != 700 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should remain 700 after denied set (got {})",
            pgid_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TtyIndex tests
// ===========================================================================

/// TtyIndex equality.
pub fn test_tty_index_eq() -> TestResult {
    let a = TtyIndex(0);
    let b = TtyIndex(0);
    let c = TtyIndex(1);
    if a != b {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) != TtyIndex(0)");
        return TestResult::Fail;
    }
    if a == c {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) == TtyIndex(1)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TtyDriverKind tests
// ===========================================================================

/// TtyDriverKind::None does not panic on write/drain.
pub fn test_driver_none_no_panic() -> TestResult {
    let driver = TtyDriverKind::None;
    driver.write_output(b"test");
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - None driver returned {} from drain", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VConsoleDriver drain_input returns 0 (input is interrupt-driven).
pub fn test_vconsole_drain_returns_zero() -> TestResult {
    let driver = TtyDriverKind::VConsole(VConsoleDriver);
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - VConsole drain returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TTY Table tests
// ===========================================================================

/// After tty_table_init, TTY 0 and TTY 1 are allocated.
pub fn test_table_init_allocates_tty0_and_tty1() -> TestResult {
    // Ensure init has been called (it's idempotent — re-calling overwrites).
    tty::table::tty_table_init();

    let slot0 = TTY_SLOTS[0].lock();
    if slot0.is_none() {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated after init");
        return TestResult::Fail;
    }
    drop(slot0);
    let slot1 = TTY_SLOTS[1].lock();
    if slot1.is_none() {
        klog_info!("TTY_TEST: BUG - TTY 1 not allocated after init");
        return TestResult::Fail;
    }
    drop(slot1);
    // Slots 2..MAX_TTYS should be None.
    for i in 2..tty::MAX_TTYS {
        let slot = TTY_SLOTS[i].lock();
        if slot.is_some() {
            klog_info!("TTY_TEST: BUG - TTY {} unexpectedly allocated", i);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// TTY 0 has the correct index.
pub fn test_table_tty0_has_index_zero() -> TestResult {
    tty::table::tty_table_init();

    let guard = TTY_SLOTS[0].lock();
    if let Some(tty) = guard.as_ref() {
        if tty.index != TtyIndex(0) {
            klog_info!("TTY_TEST: BUG - TTY 0 has wrong index {:?}", tty.index);
            return TestResult::Fail;
        }
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TTY 0 is active by default.
pub fn test_table_tty0_active() -> TestResult {
    tty::table::tty_table_init();

    let guard = TTY_SLOTS[0].lock();
    if let Some(tty) = guard.as_ref() {
        if !tty.active {
            klog_info!("TTY_TEST: BUG - TTY 0 is not active");
            return TestResult::Fail;
        }
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// with_tty helper works for existing TTY.
pub fn test_table_with_tty_exists() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(0), |tty| tty.index);
    match result {
        Some(idx) => {
            if idx != TtyIndex(0) {
                klog_info!("TTY_TEST: BUG - with_tty returned wrong index");
                return TestResult::Fail;
            }
        }
        None => {
            klog_info!("TTY_TEST: BUG - with_tty returned None for TTY 0");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// with_tty helper returns None for empty slot.
pub fn test_table_with_tty_empty() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(5), |_tty| ());
    if result.is_some() {
        klog_info!("TTY_TEST: BUG - with_tty returned Some for empty slot 5");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Per-TTY API tests (replaced compat shim tests)
// ===========================================================================
/// active_tty defaults to 0.
pub fn test_active_tty_default() -> TestResult {
    let idx = tty::active_tty();
    if idx != TtyIndex(0) {
        klog_info!(
            "TTY_TEST: BUG - active_tty default is {:?}, expected 0",
            idx
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_active_tty + active_tty round-trip.
pub fn test_set_active_tty() -> TestResult {
    tty::set_active_tty(TtyIndex(1));
    let idx = tty::active_tty();
    // Reset to default.
    tty::set_active_tty(TtyIndex(0));

    if idx != TtyIndex(1) {
        klog_info!("TTY_TEST: BUG - set_active_tty(1) did not stick");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_foreground_pgrp / get_foreground_pgrp round-trip via per-TTY API.
pub fn test_foreground_pgrp() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 42);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0); // Reset.

    if pgid != 42 {
        klog_info!(
            "TTY_TEST: BUG - foreground pgrp round-trip failed (got {})",
            pgid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: set_compositor_focus / get_compositor_focus round-trip.
///
/// Verifies that compositor focus only sets `focused_task_id`, NOT `fg_pgrp`.
pub fn test_compositor_focus() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::set_compositor_focus(99);
    let focus = tty::get_compositor_focus().unwrap_or(0);
    let _ = tty::set_compositor_focus(0); // Reset.

    if focus != 99 {
        klog_info!(
            "TTY_TEST: BUG - compositor focus round-trip failed (got {})",
            focus
        );
        return TestResult::Fail;
    }

    // Verify that fg_pgrp was NOT modified by set_compositor_focus.
    tty::table::tty_table_init();
    let fg_before = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let _ = tty::set_compositor_focus(42);
    let fg_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let _ = tty::set_compositor_focus(0); // Reset.

    if fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - set_compositor_focus changed fg_pgrp ({} -> {})",
            fg_before,
            fg_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_enter_scancode_reaches_active_tty() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    crate::ps2::keyboard::handle_scancode(0x1C);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    if n != Ok(1) || out[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - enter scancode did not reach active tty (n={:?}, b0={})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_keyboard_scancode_routes_to_active_tty_index() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(1));
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    crate::ps2::keyboard::handle_scancode(0x1C);

    let mut out0 = [0u8; 8];
    let n0 = tty::read(TtyIndex(0), &mut out0, true);
    let mut out1 = [0u8; 8];
    let n1 = tty::read(TtyIndex(1), &mut out1, true);

    tty::set_active_tty(TtyIndex(0));

    if n0 != Err(TtyError::WouldBlock) || n1 != Ok(1) || out1[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - active tty routing mismatch (n0={:?}, n1={:?}, b1={})",
            n0,
            n1,
            out1[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_keyboard_extended_up_arrow_reaches_tty() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    crate::ps2::keyboard::handle_scancode(0xE0);
    crate::ps2::keyboard::handle_scancode(0x48);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();
    if n != Ok(1) || out[0] != 0x82 {
        klog_info!(
            "TTY_TEST: BUG - extended up arrow not delivered (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// ===========================================================================
// Cooked ring buffer boundary tests
// ===========================================================================

/// Multiple reads drain the cooked buffer correctly.
pub fn test_ldisc_multiple_reads() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abcdef\n" — 7 bytes in cooked.
    for &c in b"abcdef" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    // Read 3 bytes.
    let mut buf1 = [0u8; 3];
    let n1 = ld.read(&mut buf1);
    if n1 != 3 || &buf1 != b"abc" {
        klog_info!("TTY_TEST: BUG - first read mismatch");
        return TestResult::Fail;
    }

    // Read remaining 4 bytes.
    let mut buf2 = [0u8; 10];
    let n2 = ld.read(&mut buf2);
    if n2 != 4 || &buf2[..4] != b"def\n" {
        klog_info!("TTY_TEST: BUG - second read mismatch (got {} bytes)", n2);
        return TestResult::Fail;
    }

    // Buffer should now be empty.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - buffer not empty after full drain");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Backspace on empty edit buffer is a no-op.
pub fn test_ldisc_backspace_empty() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x08); // BS on empty buffer.
    match action {
        InputAction::None => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - backspace on empty produced non-None action");
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 2: Input flag processing tests
// ===========================================================================

/// ICRNL: CR (0x0D) is mapped to NL (0x0A) when ICRNL is set.
pub fn test_ldisc_icrnl() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable ICRNL in c_iflag.
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::ICRNL;
    ld.set_termios(&t);

    // Feed CR — should be treated as NL and flush edit buffer.
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(0x0D); // CR

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - ICRNL did not flush on CR");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Should get "ab\n" (3 bytes) — CR was converted to NL.
    if n != 3 || buf[2] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - ICRNL mismatch (n={}, b2=0x{:02x})",
            n,
            buf[2]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// IGNCR: CR is discarded entirely when IGNCR is set.
pub fn test_ldisc_igncr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::IGNCR;
    ld.set_termios(&t);

    // Feed CR — should be silently discarded.
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x0D); // CR — should be ignored

    // No newline was delivered, so canonical mode should NOT have flushed.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - IGNCR did not discard CR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// INLCR: NL (0x0A) is mapped to CR (0x0D) when INLCR is set.
pub fn test_ldisc_inlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::INLCR;
    // Disable ICANON so we can inspect raw bytes.
    t.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&t);

    ld.input_char(b'\n'); // NL — should become CR
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'\r' {
        klog_info!(
            "TTY_TEST: BUG - INLCR did not map NL to CR (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ISTRIP: bit 7 is stripped from input bytes.
pub fn test_ldisc_istrip() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::ISTRIP;
    t.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&t);

    ld.input_char(0xC1); // 0xC1 with bit 7 set -> 0x41 = 'A'
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != 0x41 {
        klog_info!(
            "TTY_TEST: BUG - ISTRIP did not strip bit 7 (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Output processing tests
// ===========================================================================

/// OPOST+ONLCR: NL is converted to CR+NL on output.
pub fn test_ldisc_opost_onlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - ONLCR expected CR+NL, got len={}", len);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress | OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - ONLCR suppressed or tabbed NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// OPOST+OCRNL: CR is converted to NL on output.
pub fn test_ldisc_opost_ocrnl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::OCRNL;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\r') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - OCRNL expected NL, got 0x{:02x}", buf[0]);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress | OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - OCRNL suppressed or tabbed CR");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// No OPOST: bytes pass through unmodified.
pub fn test_ldisc_output_raw() -> TestResult {
    let mut ld = LineDisc::new();
    // Explicitly disable OPOST (default now has OPOST|ONLCR since Phase 12).
    let mut t = *ld.termios();
    t.c_oflag = 0;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - raw output modified NL");
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - raw output suppressed NL");
            return TestResult::Fail;
        }
        OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - raw output produced Tab for NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Signal generation tests
// ===========================================================================

/// SIGQUIT: Ctrl+\ generates SIGQUIT (signal 3).
pub fn test_ldisc_signal_ctrl_backslash() -> TestResult {
    let mut ld = LineDisc::new();
    let action = ld.input_char(0x1C); // Ctrl+\ = VQUIT default
    match action {
        InputAction::Signal(SIGQUIT) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGQUIT({}), got signal {}",
                SIGQUIT,
                s
            );
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+\\ did not produce Signal action");
            TestResult::Fail
        }
    }
}

/// SIGTSTP: Ctrl+Z generates SIGTSTP (signal 20).
pub fn test_ldisc_signal_ctrl_z() -> TestResult {
    let mut ld = LineDisc::new();
    // VSUSP default = 0x1A = Ctrl+Z.
    let action = ld.input_char(0x1A);
    match action {
        InputAction::Signal(SIGTSTP) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGTSTP({}), got signal {}",
                SIGTSTP,
                s
            );
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+Z did not produce Signal action");
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 2: Flow control tests
// ===========================================================================

/// IXON: Ctrl+S stops output, Ctrl+Q resumes.
pub fn test_ldisc_flow_control_ixon() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::IXON;
    ld.set_termios(&t);

    // Ctrl+S (VSTOP = 0x13) should stop output.
    ld.input_char(0x13);
    if !ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - IXON Ctrl+S did not stop output");
        return TestResult::Fail;
    }

    // Ctrl+Q (VSTART = 0x11) should resume.
    ld.input_char(0x11);
    if ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - IXON Ctrl+Q did not resume output");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: ECHOCTL tests
// ===========================================================================

/// ECHOCTL: control characters are echoed as ^X.
pub fn test_ldisc_echoctl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::ECHOCTL;
    // Disable ISIG so Ctrl+C is not caught as signal.
    t.c_lflag &= !slopos_abi::syscall::ISIG;
    ld.set_termios(&t);

    // Feed Ctrl+C (0x03) — should echo ^C (2 bytes).
    let action = ld.input_char(0x03);
    match action {
        InputAction::Echo { buf, len } => {
            if len != 2 || buf[0] != b'^' || buf[1] != b'C' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOCTL expected ^C, got [{}, {}] len={}",
                    buf[0] as char,
                    buf[1] as char,
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ECHOCTL did not produce Echo for Ctrl+C");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: VLNEXT (literal next) tests
// ===========================================================================

/// VLNEXT: Ctrl+V makes the next character literal.
pub fn test_ldisc_vlnext() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Press Ctrl+V (VLNEXT = 0x16).
    ld.input_char(0x16);

    // Now press Ctrl+C (0x03) — should be inserted literally, not generate signal.
    let action = ld.input_char(0x03);
    match action {
        InputAction::Signal(_) => {
            klog_info!("TTY_TEST: BUG - VLNEXT did not prevent signal");
            return TestResult::Fail;
        }
        _ => {} // Any non-signal action is correct.
    }

    // Flush and read — should contain 0x03 as a literal byte.
    ld.input_char(b'\n');
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Expect: 0x03 + '\n' = 2 bytes.
    if n < 2 || buf[0] != 0x03 {
        klog_info!(
            "TTY_TEST: BUG - VLNEXT literal byte missing (n={}, b0=0x{:02x})",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: VWERASE (word erase) tests
// ===========================================================================

/// VWERASE: Ctrl+W erases the previous word.
pub fn test_ldisc_vwerase() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Type "hello world".
    for &c in b"hello world" {
        ld.input_char(c);
    }

    // Ctrl+W (VWERASE = 0x17) should erase "world".
    ld.input_char(0x17);

    // Now press Enter — should get "hello \n" (the trailing space stays
    // because word erase only removes the word, not trailing spaces before it).
    ld.input_char(b'\n');
    let mut buf = [0u8; 32];
    let n = ld.read(&mut buf);
    // "hello " + NL = 7 bytes.
    if n != 7 || &buf[..6] != b"hello " {
        klog_info!(
            "TTY_TEST: BUG - VWERASE mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: edit_content() for ReprintLine
// ===========================================================================

/// edit_content returns current edit buffer contents.
pub fn test_ldisc_edit_content() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"hello" {
        ld.input_char(c);
    }
    let content = ld.edit_content();
    if content != b"hello" {
        klog_info!("TTY_TEST: BUG - edit_content mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Output processing via TTY write
// ===========================================================================

/// TTY write with OPOST+ONLCR: verify data.len() is returned (bytes consumed).
pub fn test_tty_write_returns_input_len() -> TestResult {
    tty::table::tty_table_init();
    // Enable OPOST+ONLCR on TTY 0.
    let mut t = tty::get_termios(TtyIndex(0)).unwrap();
    let saved = t;
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello\n";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved).unwrap();
    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 3: Input pipeline cleanup tests
// ===========================================================================

/// Phase 3: Keyboard events no longer routed to the input_event compositor queue.
/// After pressing a key, the compositor event queue should remain empty.
pub fn test_keyboard_no_input_event_delivery() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Set keyboard focus in the compositor to a dummy task.
    let dummy_task: u32 = 9999;
    crate::input_event::input_set_keyboard_focus(dummy_task);

    // Press 'a' (scancode 0x1E).
    crate::ps2::keyboard::handle_scancode(0x1E);

    // The compositor queue for the dummy task should be empty.
    let has_events = crate::input_event::input_has_events(dummy_task);

    // Clean up keyboard focus.
    crate::input_event::input_set_keyboard_focus(0);
    drain_tty_nonblock(TtyIndex(0));

    if has_events {
        klog_info!("TTY_TEST: BUG - keyboard event leaked into input_event queue");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Break codes (key release) do not produce TTY input.
pub fn test_keyboard_break_code_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode so any delivered byte is immediately readable.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Send break code for 'a' (0x1E | 0x80 = 0x9E).
    crate::ps2::keyboard::handle_scancode(0x9E);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if matches!(n, Ok(v) if v > 0) {
        klog_info!(
            "TTY_TEST: BUG - break code produced input (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Modifier key presses (shift, ctrl, alt, caps lock) do not produce
/// TTY input.
pub fn test_keyboard_modifier_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Press Left Shift (make code 0x2A), Left Ctrl (0x1D), Left Alt (0x38).
    crate::ps2::keyboard::handle_scancode(0x2A); // shift press
    crate::ps2::keyboard::handle_scancode(0x1D); // ctrl press
    crate::ps2::keyboard::handle_scancode(0x38); // alt press

    // Release them.
    crate::ps2::keyboard::handle_scancode(0xAA); // shift release
    crate::ps2::keyboard::handle_scancode(0x9D); // ctrl release
    crate::ps2::keyboard::handle_scancode(0xB8); // alt release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if matches!(n, Ok(v) if v > 0) {
        klog_info!(
            "TTY_TEST: BUG - modifier key produced input (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Press + release produces exactly one character (no duplication).
pub fn test_keyboard_press_release_single_char() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Press 'a' (0x1E) then release 'a' (0x9E).
    crate::ps2::keyboard::handle_scancode(0x1E); // press
    crate::ps2::keyboard::handle_scancode(0x9E); // release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(1) || out[0] != b'a' {
        klog_info!(
            "TTY_TEST: BUG - press+release should yield 1 char 'a' (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: VConsole driver drain_input returns 0 via drain_hw_input (interrupt-driven).
pub fn test_vconsole_drain_via_drain_hw_input() -> TestResult {
    tty::table::tty_table_init();

    // TTY 1 is VConsole — drain_hw_input should be a no-op (input is
    // interrupt-driven via push_input), so no data should appear.
    drain_tty_nonblock(TtyIndex(1));

    // Switch to raw mode on TTY 1.
    let saved = tty::get_termios(TtyIndex(1)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(1), &raw).unwrap();

    // has_data should be false — no hardware polling for VConsole.
    let has = tty::has_data(TtyIndex(1));
    tty::set_termios(TtyIndex(1), &saved).unwrap();

    if has {
        klog_info!("TTY_TEST: BUG - VConsole drain_hw_input produced phantom data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Multiple key presses produce correct sequence in active TTY.
pub fn test_keyboard_multi_key_sequence() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Press 'h' (0x23), 'i' (0x17).
    crate::ps2::keyboard::handle_scancode(0x23); // 'h'
    crate::ps2::keyboard::handle_scancode(0x17); // 'i'

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(2) || out[0] != b'h' || out[1] != b'i' {
        klog_info!(
            "TTY_TEST: BUG - multi-key sequence mismatch (n={:?}, b0=0x{:02x}, b1=0x{:02x})",
            n,
            out[0],
            out[1]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 5: FD integration tests
// ===========================================================================

/// Phase 5: tty::write routes bytes through output processing.
/// With OPOST+ONLCR enabled, writing "\n" should produce 2 bytes on the wire
/// (CR+LF), but write() must return the *input* byte count.
pub fn test_tty_write_output_processing() -> TestResult {
    tty::table::tty_table_init();
    // Save current termios.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    // Enable OPOST + ONLCR.
    let mut t = saved;
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello\nworld\n";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    // write() returns input length regardless of output expansion.
    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - write with OPOST+ONLCR returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::write with output processing disabled passes bytes through.
pub fn test_tty_write_raw_passthrough() -> TestResult {
    tty::table::tty_table_init();
    // Ensure c_oflag is 0 (no output processing — default).
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_oflag = 0;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"raw\ndata";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - raw write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::write to non-existent slot returns NotAllocated.
pub fn test_tty_write_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    let data = b"nothing";
    let n = tty::write(TtyIndex(7), data); // Slot 7 is not allocated.
    if n != Err(TtyError::NotAllocated) {
        klog_info!(
            "TTY_TEST: BUG - write to invalid TTY returned {:?} instead of NotAllocated",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY termios isolation — changing TTY 0's termios does not
/// affect TTY 1.
pub fn test_tty_per_tty_termios_isolation() -> TestResult {
    tty::table::tty_table_init();

    // Save TTY 0 and TTY 1 termios.
    let t0_saved = tty::get_termios(TtyIndex(0)).unwrap();
    let t1_saved = tty::get_termios(TtyIndex(1)).unwrap();

    // Set OPOST on TTY 0 only.
    let mut t0_new = t0_saved;
    t0_new.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t0_new).unwrap();

    // Read back TTY 1 — it should still have its original c_oflag.
    let t1_check = tty::get_termios(TtyIndex(1)).unwrap();

    // Restore TTY 0.
    tty::set_termios(TtyIndex(0), &t0_saved).unwrap();

    if t1_check.c_oflag != t1_saved.c_oflag {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 c_oflag changed when TTY 0 was modified ({} vs {})",
            t1_check.c_oflag,
            t1_saved.c_oflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY winsize isolation — setting winsize on TTY 0 does not
/// affect TTY 1.
pub fn test_tty_per_tty_winsize_isolation() -> TestResult {
    tty::table::tty_table_init();

    let ws0_saved = tty::get_winsize(TtyIndex(0)).unwrap();
    let ws1_saved = tty::get_winsize(TtyIndex(1)).unwrap();

    // Set a distinct winsize on TTY 0.
    let custom = slopos_abi::syscall::UserWinsize {
        ws_row: 42,
        ws_col: 120,
        ws_xpixel: 1920,
        ws_ypixel: 1080,
    };
    tty::set_winsize(TtyIndex(0), &custom).unwrap();

    // Read back TTY 1 — should be unchanged.
    let ws1_check = tty::get_winsize(TtyIndex(1)).unwrap();

    // Restore TTY 0.
    tty::set_winsize(TtyIndex(0), &ws0_saved).unwrap();

    if ws1_check.ws_row != ws1_saved.ws_row || ws1_check.ws_col != ws1_saved.ws_col {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 winsize changed when TTY 0 was modified ({}x{} vs {}x{})",
            ws1_check.ws_row,
            ws1_check.ws_col,
            ws1_saved.ws_row,
            ws1_saved.ws_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY foreground pgrp isolation.
pub fn test_tty_per_tty_fg_pgrp_isolation() -> TestResult {
    tty::table::tty_table_init();

    // Set different foreground pgrps on TTY 0 and TTY 1.
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 100);
    let _ = tty::set_foreground_pgrp(TtyIndex(1), 200);

    let pgid0 = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let pgid1 = tty::get_foreground_pgrp(TtyIndex(1)).unwrap_or(0);

    // Clean up.
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 0);
    let _ = tty::set_foreground_pgrp(TtyIndex(1), 0);

    if pgid0 != 100 {
        klog_info!("TTY_TEST: BUG - TTY 0 fg_pgrp should be 100, got {}", pgid0);
        return TestResult::Fail;
    }
    if pgid1 != 200 {
        klog_info!("TTY_TEST: BUG - TTY 1 fg_pgrp should be 200, got {}", pgid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY has_data isolation — data pushed to TTY 0 does not
/// appear on TTY 1.
pub fn test_tty_per_tty_has_data_isolation() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    // Push a character + newline to TTY 0 only.
    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'\n');

    let has0 = tty::has_data(TtyIndex(0));
    let has1 = tty::has_data(TtyIndex(1));

    // Clean up.
    drain_tty_nonblock(TtyIndex(0));

    if !has0 {
        klog_info!("TTY_TEST: BUG - TTY 0 should have data after push_input");
        return TestResult::Fail;
    }
    if has1 {
        klog_info!("TTY_TEST: BUG - TTY 1 should NOT have data (isolation failure)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY session isolation — attaching session to TTY 0 does not
/// affect TTY 1's session.
pub fn test_tty_per_tty_session_isolation() -> TestResult {
    tty::table::tty_table_init();

    tty::attach_session(TtyIndex(0), 500, 500);
    let sid0 = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let sid1 = tty::get_session_id(TtyIndex(1)).unwrap_or(0);

    // Clean up.
    tty::detach_session(TtyIndex(0));

    if sid0 != 500 {
        klog_info!(
            "TTY_TEST: BUG - TTY 0 session_id should be 500, got {}",
            sid0
        );
        return TestResult::Fail;
    }
    if sid1 != 0 {
        klog_info!("TTY_TEST: BUG - TTY 1 session_id should be 0, got {}", sid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::read on non-existent TTY returns -1.
pub fn test_tty_read_invalid_tty_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let mut buf = [0u8; 8];
    let n = tty::read(TtyIndex(7), &mut buf, true);
    if n != Err(TtyError::NotAllocated) {
        klog_info!(
            "TTY_TEST: BUG - read from invalid TTY returned {:?} instead of NotAllocated",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 6: Control-Plane Correctness regression tests
// ===========================================================================

/// Phase 6: TtyIndex from ABI crate is the same type used in drivers.
pub fn test_tty_index_abi_type() -> TestResult {
    let idx: slopos_abi::syscall::TtyIndex = slopos_abi::syscall::TtyIndex(3);
    let idx2: TtyIndex = TtyIndex(3);
    if idx != idx2 {
        klog_info!("TTY_TEST: BUG - ABI TtyIndex != drivers TtyIndex");
        return TestResult::Fail;
    }
    if idx.0 != 3 {
        klog_info!("TTY_TEST: BUG - TtyIndex inner value mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: Signal constants from ABI match expected POSIX values.
pub fn test_signal_constants() -> TestResult {
    if SIGINT != 2 {
        klog_info!("TTY_TEST: BUG - SIGINT should be 2, got {}", SIGINT);
        return TestResult::Fail;
    }
    if SIGQUIT != 3 {
        klog_info!("TTY_TEST: BUG - SIGQUIT should be 3, got {}", SIGQUIT);
        return TestResult::Fail;
    }
    if SIGTSTP != 20 {
        klog_info!("TTY_TEST: BUG - SIGTSTP should be 20, got {}", SIGTSTP);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: set_compositor_focus does NOT modify fg_pgrp.
pub fn test_set_compositor_focus_does_not_set_fg_pgrp() -> TestResult {
    tty::table::tty_table_init();
    // Set a known fg_pgrp first.
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 42);
    let fg_before = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);

    // Change compositor focus.
    let _ = tty::set_compositor_focus(99);
    let fg_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let _ = tty::set_compositor_focus(0); // Reset.

    if fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - set_compositor_focus changed fg_pgrp: {} -> {}",
            fg_before,
            fg_after
        );
        return TestResult::Fail;
    }
    if fg_before != 42 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should be 42, got {}", fg_before);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: check_read is the sole read gate — BackgroundRead for non-fg pgrp.
pub fn test_check_read_sole_gate_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    s.focused_task_id = 42; // compositor says task 42 is focused

    // Even though task 42 has compositor focus, if its pgid (99) is NOT
    // in the foreground pgrp (10), check_read must return BackgroundRead.
    // This is the key Phase 6 semantic: compositor focus != POSIX foreground.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - compositor-focused but bg pgrp should be BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_tty_open_count_lifecycle() -> TestResult {
    tty::table::tty_table_init();

    let open1 = tty::open_ref(TtyIndex(0));
    let open2 = tty::open_ref(TtyIndex(0));
    let close1 = tty::close_ref(TtyIndex(0));
    let close2 = tty::close_ref(TtyIndex(0));

    if open1 != Ok(1) || open2 != Ok(2) || close1 != Ok(1) || close2 != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - open/close ref counts mismatch: {:?} {:?} {:?} {:?}",
            open1,
            open2,
            close1,
            close2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_sets_flag_and_detaches_session() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 500, 500);
    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'\n');

    tty::hangup(TtyIndex(0));
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let hung = tty::is_hung_up(TtyIndex(0));
    let has_data = tty::has_data(TtyIndex(0));

    let _ = tty::open_ref(TtyIndex(0));
    let _ = tty::close_ref(TtyIndex(0));

    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - hangup should detach session, got sid={}",
            sid
        );
        return TestResult::Fail;
    }
    if !hung {
        klog_info!("TTY_TEST: BUG - hangup did not set hung_up flag");
        return TestResult::Fail;
    }
    if has_data {
        klog_info!("TTY_TEST: BUG - hangup should flush cooked data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_nonblock_read_eio() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::open_ref(TtyIndex(0));
    tty::hangup(TtyIndex(0));

    let mut out = [0u8; 8];
    let rc = tty::read(TtyIndex(0), &mut out, true);

    let _ = tty::open_ref(TtyIndex(0));
    let _ = tty::close_ref(TtyIndex(0));

    if rc != Err(TtyError::HungUp) {
        klog_info!(
            "TTY_TEST: BUG - nonblock read on hung tty expected HungUp, got {:?}",
            rc
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_blocking_read_eof() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::open_ref(TtyIndex(0));
    tty::hangup(TtyIndex(0));

    let mut out = [0u8; 8];
    let rc = tty::read(TtyIndex(0), &mut out, false);

    let _ = tty::open_ref(TtyIndex(0));
    let _ = tty::close_ref(TtyIndex(0));

    if rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - blocking read on hung tty expected EOF 0, got {:?}",
            rc
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase9_tty_error_variants() -> TestResult {
    let e1 = TtyError::InvalidIndex;
    let e2 = TtyError::NotAllocated;
    let e3 = TtyError::WouldBlock;
    let e4 = TtyError::HungUp;
    let e5 = TtyError::PermissionDenied;
    let e6 = TtyError::BackgroundRead;
    let e7 = TtyError::UnsupportedLineDiscipline;
    if e1 == e2 || e2 == e3 || e3 == e4 || e4 == e5 || e5 == e6 || e6 == e7 {
        klog_info!("TTY_TEST: BUG - TtyError variants not distinct");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase9_read_returns_result() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(0), &mut buf, true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - empty nonblock read expected WouldBlock, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_phase9_read_invalid_index_error() -> TestResult {
    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(99), &mut buf, true) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected InvalidIndex, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_phase9_read_not_allocated_error() -> TestResult {
    tty::table::tty_table_init();
    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(5), &mut buf, true) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected NotAllocated, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_phase9_write_returns_result() -> TestResult {
    tty::table::tty_table_init();
    match tty::write(TtyIndex(0), b"hello") {
        Ok(5) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - write expected Ok(5), got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_phase9_get_termios_returns_result() -> TestResult {
    tty::table::tty_table_init();
    match tty::get_termios(TtyIndex(0)) {
        Ok(t) => {
            if (t.c_lflag & slopos_abi::syscall::ICANON) == 0 {
                klog_info!("TTY_TEST: BUG - default termios should have ICANON");
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_phase9_vmin0_vtime0_immediate_return() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 0;
    raw.c_cc[5] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(0) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=0/VTIME=0 expected Ok(0), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase9_vmin_enforcement() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 3;
    raw.c_cc[5] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');
    tty::push_input(TtyIndex(0), b'b');
    tty::push_input(TtyIndex(0), b'c');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(n) if n >= 3 => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3 read expected Ok(>=3), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase9_set_fg_pgrp_checked_permission_denied() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 10, 10);
    match tty::set_foreground_pgrp_checked(TtyIndex(0), 20, 99) {
        Err(TtyError::PermissionDenied) => {
            tty::detach_session(TtyIndex(0));
            TestResult::Pass
        }
        other => {
            tty::detach_session(TtyIndex(0));
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_phase9_hangup_read_returns_hung_up() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::open_ref(TtyIndex(0));
    tty::hangup(TtyIndex(0));

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);

    let _ = tty::open_ref(TtyIndex(0));
    let _ = tty::close_ref(TtyIndex(0));

    match result {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup nonblock read expected HungUp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 8: Per-TTY Locking & Performance regression tests
// ===========================================================================

/// Phase 8: Per-TTY slots are independently lockable — locking slot 0 does
/// not prevent access to slot 1.
pub fn test_phase8_per_tty_lock_independence() -> TestResult {
    tty::table::tty_table_init();

    // Lock slot 0 and, while holding it, verify we can lock slot 1.
    let guard0 = TTY_SLOTS[0].lock();
    let guard1 = TTY_SLOTS[1].lock();

    let ok0 = guard0.is_some();
    let ok1 = guard1.is_some();
    drop(guard1);
    drop(guard0);

    if !ok0 || !ok1 {
        klog_info!("TTY_TEST: BUG - per-TTY slots not independently lockable");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: DriverId round-trip — TtyDriverKind::id() returns the matching
/// DriverId variant for each driver kind.
pub fn test_phase8_driver_id_round_trip() -> TestResult {
    let serial = TtyDriverKind::SerialConsole(crate::tty::driver::SerialConsoleDriver);
    let vconsole = TtyDriverKind::VConsole(VConsoleDriver);
    let none = TtyDriverKind::None;

    if serial.id() != DriverId::SerialConsole {
        klog_info!("TTY_TEST: BUG - SerialConsole id mismatch");
        return TestResult::Fail;
    }
    if vconsole.id() != DriverId::VConsole {
        klog_info!("TTY_TEST: BUG - VConsole id mismatch");
        return TestResult::Fail;
    }
    if none.id() != DriverId::None {
        klog_info!("TTY_TEST: BUG - None id mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: Split-write returns correct byte count (input length, not output
/// expansion) through the per-slot locking path.
pub fn test_phase8_split_write_returns_input_len() -> TestResult {
    tty::table::tty_table_init();

    // Enable OPOST+ONLCR on TTY 0 so NL expands to CR+NL.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"abc\ndef\n";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - split-write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: Idle callback iterates all active TTYs (not just TTY 0).
/// Push data to TTY 1 and verify has_data reports it after the idle-loop
/// path runs (via has_data which calls drain_hw_input internally).
pub fn test_phase8_idle_cb_iterates_all_ttys() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    // Push data to TTY 1 via push_input (simulates keyboard on vconsole).
    tty::push_input(TtyIndex(1), b'z');
    tty::push_input(TtyIndex(1), b'\n');

    // has_data internally calls drain_hw_input, simulating the idle path.
    let has1 = tty::has_data(TtyIndex(1));
    drain_tty_nonblock(TtyIndex(1));

    if !has1 {
        klog_info!("TTY_TEST: BUG - idle callback path did not find data on TTY 1");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: Merged drain+read in a single lock acquisition — verify that
/// read() returns data that was pushed to the serial TTY (TTY 0) without
/// requiring multiple separate lock acquisitions.
pub fn test_phase8_merged_drain_read() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Push "ok\n" into TTY 0.
    tty::push_input(TtyIndex(0), b'o');
    tty::push_input(TtyIndex(0), b'k');
    tty::push_input(TtyIndex(0), b'\n');

    let mut out = [0u8; 16];
    let n = tty::read(TtyIndex(0), &mut out, true);
    if n != Ok(3) || &out[..3] != b"ok\n" {
        klog_info!(
            "TTY_TEST: BUG - merged drain+read mismatch (n={:?}, data={:?})",
            n,
            &out[..3]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: TTY_SLOTS uses per-slot locking — with_tty operates on the
/// correct slot without holding a global lock.
pub fn test_phase8_with_tty_per_slot() -> TestResult {
    tty::table::tty_table_init();

    // Verify with_tty returns the correct index for each allocated slot.
    let idx0 = tty::table::with_tty(TtyIndex(0), |tty| tty.index);
    let idx1 = tty::table::with_tty(TtyIndex(1), |tty| tty.index);
    let idx_empty = tty::table::with_tty(TtyIndex(5), |tty| tty.index);

    if idx0 != Some(TtyIndex(0)) {
        klog_info!("TTY_TEST: BUG - with_tty slot 0 returned wrong index");
        return TestResult::Fail;
    }
    if idx1 != Some(TtyIndex(1)) {
        klog_info!("TTY_TEST: BUG - with_tty slot 1 returned wrong index");
        return TestResult::Fail;
    }
    if idx_empty.is_some() {
        klog_info!("TTY_TEST: BUG - with_tty empty slot 5 returned Some");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 8: DriverId is Copy + Clone + Eq — verify that the derive attributes
/// work correctly for the lock-free I/O dispatch identifier.
pub fn test_phase8_driver_id_traits() -> TestResult {
    let id = DriverId::SerialConsole;
    let id_copy = id; // Copy
    let id_clone = id.clone(); // Clone

    if id != id_copy || id != id_clone {
        klog_info!("TTY_TEST: BUG - DriverId Copy/Clone/Eq broken");
        return TestResult::Fail;
    }
    if id == DriverId::VConsole {
        klog_info!("TTY_TEST: BUG - DriverId Eq does not distinguish variants");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 10: Job Control Correctness regression tests
// ===========================================================================

/// Phase 10: SIGTTOU constant is defined and has correct POSIX value (22).
pub fn test_phase10_sigttou_constant() -> TestResult {
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 10: check_write with TOSTOP and background caller returns BackgroundWrite.
/// This verifies the session-level check_write logic directly.
pub fn test_phase10_check_write_tostop_blocks_background() -> TestResult {
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

/// Phase 10: check_write without TOSTOP always allows writes (even from background).
pub fn test_phase10_check_write_no_tostop_allows_background() -> TestResult {
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

/// Phase 10: check_write with TOSTOP allows foreground process.
pub fn test_phase10_check_write_tostop_allows_foreground() -> TestResult {
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

/// Phase 10: check_read rejects cross-session reads (DeniedCrossSession).
pub fn test_phase10_check_read_cross_session_rejected() -> TestResult {
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

/// Phase 10: check_read still allows same-session foreground reads.
pub fn test_phase10_check_read_same_session_foreground() -> TestResult {
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

/// Phase 10: check_read still allows kernel tasks (pgid=0, sid=0).
pub fn test_phase10_check_read_kernel_task_allowed() -> TestResult {
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

/// Phase 10: TTY write succeeds for foreground process even with TOSTOP.
pub fn test_phase10_tty_write_foreground_with_tostop() -> TestResult {
    tty::table::tty_table_init();
    // This test verifies write() returns Ok even when TOSTOP is set,
    // because in the test harness task_id=0 (kernel), which skips the
    // foreground check.  The session-level check_write tests above
    // verify the logic directly.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_lflag |= slopos_abi::syscall::TOSTOP;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello";
    let result = tty::write(TtyIndex(0), data);
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
// Phase 11: Non-Canonical Timing Fix regression tests
// ===========================================================================

/// Phase 11: VMIN>0/VTIME>0 — returns immediately when VMIN bytes are
/// already available (no timeout needed).
pub fn test_phase11_vmin_vtime_enough_data_returns_immediately() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push exactly VMIN bytes.
    tty::push_input(TtyIndex(0), b'a');
    tty::push_input(TtyIndex(0), b'b');
    tty::push_input(TtyIndex(0), b'c');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(n) if n >= 3 => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 with 3 bytes expected Ok(>=3), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 11: VMIN>0/VTIME>0 — with partial data available (less than VMIN),
/// a nonblocking read returns what is available (WouldBlock if nothing).
pub fn test_phase11_vmin_vtime_partial_nonblock() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 5; // VMIN = 5
    raw.c_cc[5] = 2; // VTIME = 2 (200ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push fewer than VMIN bytes.
    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'y');

    // Nonblocking read: should return the 2 bytes we have (not block).
    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(2) => {
            if buf[0] == b'x' && buf[1] == b'y' {
                TestResult::Pass
            } else {
                klog_info!(
                    "TTY_TEST: BUG - VMIN=5/VTIME=2 nonblock data mismatch ({}, {})",
                    buf[0],
                    buf[1]
                );
                TestResult::Fail
            }
        }
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=5/VTIME=2 nonblock with 2 bytes expected Ok(2), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 11: VMIN>0/VTIME>0 — with no data, nonblocking read returns
/// WouldBlock (timer does NOT start without first byte).
pub fn test_phase11_vmin_vtime_no_data_nonblock() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Err(TtyError::WouldBlock) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 no data nonblock expected WouldBlock, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 11: VMIN>0/VTIME>0 — inter-byte timeout returns partial data.
/// Push 1 byte (less than VMIN=3), then do a blocking read with a short
/// VTIME.  The read should return the 1 byte after the inter-byte timeout
/// expires (not block indefinitely waiting for VMIN).
pub fn test_phase11_vmin_vtime_interbyte_timeout_returns_partial() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms inter-byte timeout)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push 1 byte — less than VMIN but enough to start the inter-byte timer.
    tty::push_input(TtyIndex(0), b'z');

    // Blocking read: should wait for VMIN=3 bytes but the inter-byte timer
    // (VTIME=100ms) will expire after the first byte, returning what we have.
    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(n) if n >= 1 => {
            if buf[0] != b'z' {
                klog_info!(
                    "TTY_TEST: BUG - inter-byte timeout data mismatch (got 0x{:02x})",
                    buf[0]
                );
                TestResult::Fail
            } else {
                TestResult::Pass
            }
        }
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 with 1 byte expected Ok(>=1) after timeout, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 11: Verify that the ldisc vmin_vtime() helper returns correct values
/// after setting non-canonical parameters.
pub fn test_phase11_ldisc_vmin_vtime_helper() -> TestResult {
    let mut ld = LineDisc::new();
    // Default: VMIN=1, VTIME=0.
    let (vmin, vtime) = ld.vmin_vtime();
    if vmin != 1 || vtime != 0 {
        klog_info!(
            "TTY_TEST: BUG - default vmin_vtime expected (1,0), got ({},{})",
            vmin,
            vtime
        );
        return TestResult::Fail;
    }

    // Set custom values.
    let mut t = *ld.termios();
    t.c_cc[6] = 5; // VMIN
    t.c_cc[5] = 3; // VTIME
    ld.set_termios(&t);
    let (vmin2, vtime2) = ld.vmin_vtime();
    if vmin2 != 5 || vtime2 != 3 {
        klog_info!(
            "TTY_TEST: BUG - custom vmin_vtime expected (5,3), got ({},{})",
            vmin2,
            vtime2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 12: Sane Defaults & Output Column Tracking
// ===========================================================================

/// Phase 12: Verify default termios c_iflag contains ICRNL.
pub fn test_phase12_default_termios_has_icrnl() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    if (t.c_iflag & slopos_abi::syscall::ICRNL) == 0 {
        klog_info!(
            "TTY_TEST: BUG - default c_iflag missing ICRNL (got 0x{:x})",
            t.c_iflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 12: Verify default termios c_oflag contains OPOST | ONLCR.
pub fn test_phase12_default_termios_has_opost_onlcr() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    let expected = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    if (t.c_oflag & expected) != expected {
        klog_info!(
            "TTY_TEST: BUG - default c_oflag missing OPOST|ONLCR (got 0x{:x})",
            t.c_oflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 12: Verify default termios c_lflag contains ISIG|ICANON|ECHO|ECHOE|ECHOK|ECHOCTL|ECHOKE.
pub fn test_phase12_default_termios_has_full_lflag() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    let expected = slopos_abi::syscall::ISIG
        | slopos_abi::syscall::ICANON
        | slopos_abi::syscall::ECHO
        | slopos_abi::syscall::ECHOE
        | slopos_abi::syscall::ECHOK
        | slopos_abi::syscall::ECHOCTL
        | slopos_abi::syscall::ECHOKE;
    if (t.c_lflag & expected) != expected {
        klog_info!(
            "TTY_TEST: BUG - default c_lflag missing flags (got 0x{:x}, want 0x{:x})",
            t.c_lflag,
            expected
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 12: Output column advances by 1 for each printable ASCII character.
pub fn test_phase12_output_column_tracking_printable() -> TestResult {
    let mut ld = LineDisc::new();
    // Defaults have OPOST|ONLCR which is fine — printable chars just advance column.
    for ch in b"Hello" {
        ld.process_output_byte(*ch);
    }
    // After 5 printable chars, column should be 5.
    // Verify indirectly: a tab should expand to 8 - (5 % 8) = 3 spaces.
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 3 {
                klog_info!(
                    "TTY_TEST: BUG - after 5 chars expected tab=3 spaces, got {}",
                    n
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant for tab byte");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: Newline with ONLCR resets column to 0.
pub fn test_phase12_output_column_tracking_newline() -> TestResult {
    let mut ld = LineDisc::new();
    // Print 5 chars, then newline (ONLCR expands to CR+NL which resets column).
    for ch in b"Hello" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(b'\n');
    // Column should now be 0.  A tab at column 0 gives 8 spaces.
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - after NL expected tab=8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: CR resets column to 0.
pub fn test_phase12_output_column_tracking_cr() -> TestResult {
    let mut ld = LineDisc::new();
    // Disable ONLCR so CR is not suppressed/converted.
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST; // OPOST only, no ONLCR
    ld.set_termios(&t);

    for ch in b"ABCDE" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(b'\r');
    // Column should be 0 — tab at col 0 = 8 spaces.
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - after CR expected tab=8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: Tab expands to correct number of spaces (8-column tab stops).
pub fn test_phase12_output_column_tracking_tab() -> TestResult {
    let mut ld = LineDisc::new();
    // At column 0, tab should produce 8 spaces.
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - tab at col 0 expected 8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant at col 0");
            return TestResult::Fail;
        }
    }
    // Column is now 8.  Print 3 chars (column=11), then tab => 8 - (11 % 8) = 5.
    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 5 {
                klog_info!("TTY_TEST: BUG - tab at col 11 expected 5 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant at col 11");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: Backspace decrements column (but not below 0).
pub fn test_phase12_output_column_tracking_backspace() -> TestResult {
    let mut ld = LineDisc::new();
    for ch in b"AB" {
        ld.process_output_byte(*ch);
    }
    // Column=2.  Backspace -> column=1.
    ld.process_output_byte(0x08);
    // Tab at column 1 => 8 - (1 % 8) = 7.
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 7 {
                klog_info!("TTY_TEST: BUG - after BS expected tab=7 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    // Backspace at column 0 should not underflow.
    let mut ld2 = LineDisc::new();
    ld2.process_output_byte(0x08); // should stay at 0
    match ld2.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!(
                    "TTY_TEST: BUG - BS at col 0 should stay 0, tab gave {} spaces",
                    n
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: ONOCR suppresses CR when column is 0.
pub fn test_phase12_onocr_at_column_zero() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONOCR;
    ld.set_termios(&t);

    // At column 0, CR should be suppressed.
    match ld.process_output_byte(b'\r') {
        OutputAction::Suppress => {}
        _other => {
            klog_info!("TTY_TEST: BUG - ONOCR at col 0 should suppress CR");
            return TestResult::Fail;
        }
    }
    // Move to column 3, then CR should NOT be suppressed.
    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\r') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\r' {
                klog_info!("TTY_TEST: BUG - ONOCR at col 3 should emit CR");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ONOCR at col 3 should emit, not suppress");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 12: Default ONLCR correctly expands NL to CR+NL.
pub fn test_phase12_default_onlcr_newline_expands() -> TestResult {
    let mut ld = LineDisc::new();
    // With defaults (OPOST|ONLCR), NL should expand to CR+NL.
    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!(
                    "TTY_TEST: BUG - default ONLCR should produce CR+NL, got len={}",
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - default ONLCR should emit, not suppress/tab");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 13: ABI Signal Constant Unification
// ===========================================================================

/// Phase 13: All signal constants come from `abi/src/signal.rs` with correct
/// POSIX-compatible values.  This test verifies every signal used by the TTY
/// subsystem matches its expected numeric value.
pub fn test_phase13_signal_values_from_signal_module() -> TestResult {
    // These are now imported from slopos_abi::signal (the canonical source).
    if SIGINT != 2 {
        klog_info!("TTY_TEST: BUG - SIGINT should be 2, got {}", SIGINT);
        return TestResult::Fail;
    }
    if SIGQUIT != 3 {
        klog_info!("TTY_TEST: BUG - SIGQUIT should be 3, got {}", SIGQUIT);
        return TestResult::Fail;
    }
    if SIGTSTP != 20 {
        klog_info!("TTY_TEST: BUG - SIGTSTP should be 20, got {}", SIGTSTP);
        return TestResult::Fail;
    }
    if SIGHUP != 1 {
        klog_info!("TTY_TEST: BUG - SIGHUP should be 1, got {}", SIGHUP);
        return TestResult::Fail;
    }
    if SIGCONT != 18 {
        klog_info!("TTY_TEST: BUG - SIGCONT should be 18, got {}", SIGCONT);
        return TestResult::Fail;
    }
    if SIGTTIN != 21 {
        klog_info!("TTY_TEST: BUG - SIGTTIN should be 21, got {}", SIGTTIN);
        return TestResult::Fail;
    }
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 13: LineDisc signal generation uses constants from `signal.rs`.
/// Verifies that ISIG + Ctrl+C still produces the correct signal number after
/// the import migration.
pub fn test_phase13_ldisc_signal_uses_signal_module() -> TestResult {
    let mut ld = LineDisc::new();
    // Default termios has ISIG enabled.
    match ld.input_char(3) {
        // Ctrl+C = 0x03 → SIGINT
        InputAction::Signal(sig) if sig == SIGINT => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+C should produce Signal(SIGINT=2)");
            return TestResult::Fail;
        }
    }
    // Ctrl+\\ = 0x1C → SIGQUIT
    match ld.input_char(28) {
        InputAction::Signal(sig) if sig == SIGQUIT => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+\\ should produce Signal(SIGQUIT=3)");
            return TestResult::Fail;
        }
    }
    // Ctrl+Z = 0x1A → SIGTSTP
    match ld.input_char(26) {
        InputAction::Signal(sig) if sig == SIGTSTP => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+Z should produce Signal(SIGTSTP=20)");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 13: SIGHUP and SIGCONT are used by the hangup path.  Verify the
/// constants are accessible from the signal module and have correct values
/// (these were previously only imported in `mod.rs` from `signal` — now they
/// are the sole definition).
pub fn test_phase13_hangup_signals_from_signal_module() -> TestResult {
    // SIGHUP is sent to the foreground pgrp on TTY hangup.
    if SIGHUP != 1 {
        klog_info!("TTY_TEST: BUG - SIGHUP should be 1, got {}", SIGHUP);
        return TestResult::Fail;
    }
    // SIGCONT is sent after SIGHUP to wake stopped processes.
    if SIGCONT != 18 {
        klog_info!("TTY_TEST: BUG - SIGCONT should be 18, got {}", SIGCONT);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 13: Background-read and background-write signals (SIGTTIN, SIGTTOU)
/// are now sourced from `signal.rs` exclusively.  Verify values.
pub fn test_phase13_job_control_signals_from_signal_module() -> TestResult {
    if SIGTTIN != 21 {
        klog_info!("TTY_TEST: BUG - SIGTTIN should be 21, got {}", SIGTTIN);
        return TestResult::Fail;
    }
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 14: Responsibility Split — PTY Foundation
// ===========================================================================

// -- 18.4: SessionId / ProcessGroupId newtype tests --

/// SessionId::new(0) returns None (zero is the "no session" sentinel).
pub fn test_phase14_session_id_zero_is_none() -> TestResult {
    if SessionId::new(0).is_some() {
        klog_info!("TTY_TEST: BUG - SessionId::new(0) should be None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SessionId::new(non-zero) returns Some and round-trips through get().
pub fn test_phase14_session_id_round_trip() -> TestResult {
    match SessionId::new(42) {
        Some(sid) => {
            if sid.get() != 42 {
                klog_info!(
                    "TTY_TEST: BUG - SessionId(42).get() = {}, expected 42",
                    sid.get()
                );
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        None => {
            klog_info!("TTY_TEST: BUG - SessionId::new(42) returned None");
            TestResult::Fail
        }
    }
}

/// ProcessGroupId::new(0) returns None.
pub fn test_phase14_pgrp_id_zero_is_none() -> TestResult {
    if ProcessGroupId::new(0).is_some() {
        klog_info!("TTY_TEST: BUG - ProcessGroupId::new(0) should be None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ProcessGroupId::new(non-zero) round-trips through get().
pub fn test_phase14_pgrp_id_round_trip() -> TestResult {
    match ProcessGroupId::new(99) {
        Some(pgid) => {
            if pgid.get() != 99 {
                klog_info!(
                    "TTY_TEST: BUG - ProcessGroupId(99).get() = {}, expected 99",
                    pgid.get()
                );
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        None => {
            klog_info!("TTY_TEST: BUG - ProcessGroupId::new(99) returned None");
            TestResult::Fail
        }
    }
}

/// TtySession uses Option-based fields: new() has None for all IDs.
pub fn test_phase14_session_option_fields() -> TestResult {
    let s = TtySession::new();
    if s.session_leader.is_some() || s.session_id.is_some() || s.fg_pgrp.is_some() {
        klog_info!("TTY_TEST: BUG - new TtySession should have None for all Option fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// After attach(), Option fields are Some; after detach(), they are None.
pub fn test_phase14_session_option_attach_detach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 20);
    if s.session_leader.is_none() || s.session_id.is_none() || s.fg_pgrp.is_none() {
        klog_info!("TTY_TEST: BUG - Option fields should be Some after attach");
        return TestResult::Fail;
    }
    s.detach();
    if s.session_leader.is_some() || s.session_id.is_some() || s.fg_pgrp.is_some() {
        klog_info!("TTY_TEST: BUG - Option fields should be None after detach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// -- 18.2: RawDisc / LdiscKind tests --

/// RawDisc: new instance has no data.
pub fn test_phase14_raw_disc_new_empty() -> TestResult {
    let rd = RawDisc::new();
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - new RawDisc has data");
        return TestResult::Fail;
    }
    if rd.is_canonical() {
        klog_info!("TTY_TEST: BUG - RawDisc should not be canonical");
        return TestResult::Fail;
    }
    if rd.is_stopped() {
        klog_info!("TTY_TEST: BUG - RawDisc should not be stopped");
        return TestResult::Fail;
    }
    if !rd.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - RawDisc edit_content should be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc: input_char pushes byte, read retrieves it.
pub fn test_phase14_raw_disc_input_read() -> TestResult {
    let mut rd = RawDisc::new();
    let _ = rd.input_char(b'A');
    let _ = rd.input_char(b'B');
    if !rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should have data after input_char");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = rd.read(&mut buf);
    if n != 2 || buf[0] != b'A' || buf[1] != b'B' {
        klog_info!(
            "TTY_TEST: BUG - RawDisc read got {} bytes [{}, {}]",
            n,
            buf[0],
            buf[1]
        );
        return TestResult::Fail;
    }
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should be empty after reading all");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc: process_output_byte passes through unchanged.
pub fn test_phase14_raw_disc_output_passthrough() -> TestResult {
    let mut rd = RawDisc::new();
    match rd.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!(
                    "TTY_TEST: BUG - RawDisc output should passthrough, got len={} buf[0]={}",
                    len,
                    buf[0]
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - RawDisc output should emit, got other action");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// RawDisc: flush_all clears buffer.
pub fn test_phase14_raw_disc_flush() -> TestResult {
    let mut rd = RawDisc::new();
    let _ = rd.input_char(b'X');
    rd.flush_all();
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should be empty after flush_all");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// LdiscKind::NTty delegates to LineDisc correctly.
pub fn test_phase14_ldisc_kind_ntty_delegation() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    // NTty should be canonical by default.
    if !lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should be canonical by default");
        return TestResult::Fail;
    }
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have no data initially");
        return TestResult::Fail;
    }
    // Feed a character + newline to flush to cooked buffer.
    let _ = lk.input_char(b'A');
    let _ = lk.input_char(b'\n');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have data after newline");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 8];
    let n = lk.read(&mut buf);
    // Canonical: 'A' + '\n' = 2 bytes.
    if n != 2 || buf[0] != b'A' || buf[1] != b'\n' {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty read got {} bytes", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// LdiscKind::Raw delegates to RawDisc correctly.
pub fn test_phase14_ldisc_kind_raw_delegation() -> TestResult {
    let mut lk = LdiscKind::Raw(RawDisc::new());
    // Raw should NOT be canonical.
    if lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should not be canonical");
        return TestResult::Fail;
    }
    // Input bytes should go directly to buffer.
    let _ = lk.input_char(b'Z');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have data after input_char");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 1 || buf[0] != b'Z' {
        klog_info!(
            "TTY_TEST: BUG - LdiscKind::Raw read got {} bytes, buf[0]={}",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// -- 18.3: PTY driver stub tests --

/// PtyMaster and PtySlave DriverId variants exist and are distinct.
pub fn test_phase14_pty_driver_id_variants() -> TestResult {
    let master_id = DriverId::PtyMaster {
        slave_idx: TtyIndex(2),
    };
    let slave_id = DriverId::PtySlave {
        master_idx: TtyIndex(3),
    };
    if master_id == slave_id {
        klog_info!("TTY_TEST: BUG - PtyMaster and PtySlave DriverId should be distinct");
        return TestResult::Fail;
    }
    // Also verify they differ from existing IDs.
    if master_id == DriverId::SerialConsole
        || master_id == DriverId::VConsole
        || master_id == DriverId::None
    {
        klog_info!("TTY_TEST: BUG - PtyMaster should differ from SerialConsole/VConsole/None");
        return TestResult::Fail;
    }
    if slave_id == DriverId::SerialConsole
        || slave_id == DriverId::VConsole
        || slave_id == DriverId::None
    {
        klog_info!("TTY_TEST: BUG - PtySlave should differ from SerialConsole/VConsole/None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyMaster driver kind returns correct DriverId.
pub fn test_phase14_pty_master_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtyMaster {
        slave_idx: TtyIndex(2),
    };
    if drv.id()
        != (DriverId::PtyMaster {
            slave_idx: TtyIndex(2),
        })
    {
        klog_info!("TTY_TEST: BUG - PtyMaster TtyDriverKind should return DriverId::PtyMaster");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtySlave driver kind returns correct DriverId.
pub fn test_phase14_pty_slave_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtySlave {
        master_idx: TtyIndex(3),
    };
    if drv.id()
        != (DriverId::PtySlave {
            master_idx: TtyIndex(3),
        })
    {
        klog_info!("TTY_TEST: BUG - PtySlave TtyDriverKind should return DriverId::PtySlave");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 15: POSIX Quick Wins — Line Boundaries, SIGWINCH, Word Erase
// ===========================================================================

/// Phase 15: Canonical mode read returns at most one line per call.
pub fn test_phase15_canonical_one_line_per_read() -> TestResult {
    let mut ld = LineDisc::new();

    // Type two lines: "abc\n" and "def\n".
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');
    for &c in b"def" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    // First read should return only the first line.
    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 4 || &buf[..4] != b"abc\n" {
        klog_info!(
            "TTY_TEST: BUG - canonical read should return one line (got {} bytes)",
            n1
        );
        return TestResult::Fail;
    }

    // Second read should return the second line.
    let n2 = ld.read(&mut buf);
    if n2 != 4 || &buf[..4] != b"def\n" {
        klog_info!(
            "TTY_TEST: BUG - canonical second read mismatch (got {} bytes)",
            n2
        );
        return TestResult::Fail;
    }

    // No more data.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data should be false after reading both lines");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: has_data in canonical mode is gated by line_count.
pub fn test_phase15_canonical_has_data_line_count() -> TestResult {
    let mut ld = LineDisc::new();

    // Type characters without newline — has_data should be false.
    for &c in b"hello" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data true before newline");
        return TestResult::Fail;
    }

    // Press newline — has_data should become true.
    ld.input_char(b'\n');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data false after newline");
        return TestResult::Fail;
    }

    // Read the line — has_data should become false again.
    let mut buf = [0u8; 64];
    let _ = ld.read(&mut buf);
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data true after reading line");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: EOF flush (Ctrl+D) counts as a line boundary.
pub fn test_phase15_canonical_eof_line_boundary() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc" then EOF (Ctrl+D = 0x04).
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x04); // VEOF

    // has_data should be true (EOF-flushed line).
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data false after EOF flush");
        return TestResult::Fail;
    }

    // Read should return "abc" (3 bytes, no trailing newline).
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - EOF flush read mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }

    // has_data should be false after reading the EOF-flushed chunk.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after reading EOF-flushed chunk");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: SIGWINCH constant has the correct value.
pub fn test_phase15_sigwinch_constant() -> TestResult {
    if SIGWINCH != 28 {
        klog_info!("TTY_TEST: BUG - SIGWINCH should be 28, got {}", SIGWINCH);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: Word erase with path boundaries (slashes are non-word chars).
pub fn test_phase15_word_erase_path_boundary() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Type "/usr/local/bin".
    for &c in b"/usr/local/bin" {
        ld.input_char(c);
    }

    // Ctrl+W should erase "bin" (word chars), stopping at "/".
    ld.input_char(0x17);

    // Press Enter and read.
    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    // Expect "/usr/local/" + "\n" = 12 bytes.
    if n != 12 || &buf[..11] != b"/usr/local/" {
        klog_info!(
            "TTY_TEST: BUG - word erase path boundary mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: Word erase with mixed word/non-word boundaries.
pub fn test_phase15_word_erase_mixed_boundary() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Type "hello---world".
    for &c in b"hello---world" {
        ld.input_char(c);
    }

    // Ctrl+W should erase "world" (word chars), stopping at "-".
    ld.input_char(0x17);

    // Press Enter and read.
    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    // Expect "hello---" + "\n" = 9 bytes.
    if n != 9 || &buf[..8] != b"hello---" {
        klog_info!(
            "TTY_TEST: BUG - word erase mixed boundary mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: Word erase skips trailing non-word chars then deletes word.
pub fn test_phase15_word_erase_trailing_spaces() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Type "hello   " (hello + 3 spaces).
    for &c in b"hello   " {
        ld.input_char(c);
    }

    // Ctrl+W: Phase 1 skips 3 spaces (non-word), Phase 2 deletes "hello".
    ld.input_char(0x17);

    // Press Enter and read.
    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    // Expect "\n" = 1 byte (everything erased).
    if n != 1 || buf[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - word erase trailing spaces mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 15: Canonical mode small-buffer read does not lose data.
pub fn test_phase15_canonical_small_buffer_read() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abcdefgh\n" (9 bytes).
    for &c in b"abcdefgh" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    // Read with a 3-byte buffer.
    let mut buf = [0u8; 3];
    let n1 = ld.read(&mut buf);
    if n1 != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - small buffer first read mismatch");
        return TestResult::Fail;
    }

    // has_data should still be true (mid-line).
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false mid-line");
        return TestResult::Fail;
    }

    // Read remaining bytes — should stop at newline.
    let mut buf2 = [0u8; 64];
    let n2 = ld.read(&mut buf2);
    if n2 != 6 || &buf2[..6] != b"defgh\n" {
        klog_info!(
            "TTY_TEST: BUG - small buffer second read mismatch (got {} bytes)",
            n2
        );
        return TestResult::Fail;
    }

    // Now has_data should be false.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after full line consumed");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase16_tcsetsw_preserves_pending_input() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');

    let mut changed = raw;
    changed.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios_wait(TtyIndex(0), &changed).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(1) if out[0] == b'a' => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSW should preserve pending input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase16_tcsetsf_flushes_pending_input() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');

    let mut changed = raw;
    changed.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios_flush(TtyIndex(0), &changed).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Err(TtyError::WouldBlock) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSF should flush pending input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase16_read_with_attach_false_skips_auto_attach() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'z');

    let mut out = [0u8; 8];
    let result = tty::read_with_attach(TtyIndex(0), &mut out, true, false);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if result != Ok(1) || out[0] != b'z' || sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - read_with_attach(false) should not auto-attach (result={:?}, sid={}, b0=0x{:02x})",
            result,
            sid,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase18_read_with_attach_true_skips_durable_attach() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'y');

    let mut out = [0u8; 8];
    let result = tty::read_with_attach(TtyIndex(0), &mut out, true, true);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if result != Ok(1) || out[0] != b'y' || sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - read_with_attach(true) should no longer claim ownership (result={:?}, sid={}, b0=0x{:02x})",
            result,
            sid,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase18_acquire_and_release_controlling_terminal() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));

    let acquire = tty::acquire_controlling_terminal(TtyIndex(0), 42, 77);
    let sid_after_acquire = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let release = tty::release_controlling_terminal(TtyIndex(0), 42);
    let sid_after_release = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

    if acquire != Ok(()) || sid_after_acquire != 42 || release != Ok(true) || sid_after_release != 0
    {
        klog_info!(
            "TTY_TEST: BUG - explicit controlling-terminal transition mismatch (acquire={:?}, sid_acquire={}, release={:?}, sid_release={})",
            acquire,
            sid_after_acquire,
            release,
            sid_after_release
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase18_release_wrong_session_is_noop() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    tty::acquire_controlling_terminal(TtyIndex(0), 88, 88).unwrap();

    let release = tty::release_controlling_terminal(TtyIndex(0), 99);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::release_controlling_terminal(TtyIndex(0), 88).unwrap();

    if release != Ok(false) || sid != 88 {
        klog_info!(
            "TTY_TEST: BUG - wrong-session release should be ignored (release={:?}, sid={})",
            release,
            sid
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase16_get_ldisc_default_is_ntty() -> TestResult {
    tty::table::tty_table_init();

    match tty::get_ldisc(TtyIndex(0)) {
        Ok(slopos_abi::syscall::N_TTY) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - default line discipline should be N_TTY, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase16_set_ldisc_round_trip_preserves_termios() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut configured = saved;
    configured.c_lflag &= !slopos_abi::syscall::ICANON;
    configured.c_cc[slopos_abi::syscall::VMIN] = 7;
    configured.c_cc[slopos_abi::syscall::VTIME] = 3;
    tty::set_termios(TtyIndex(0), &configured).unwrap();

    tty::set_ldisc(TtyIndex(0), slopos_abi::syscall::N_RAW).unwrap();
    let raw_kind = tty::get_ldisc(TtyIndex(0));
    let raw_termios = tty::get_termios(TtyIndex(0)).unwrap();

    tty::push_input(TtyIndex(0), b'q');
    let mut out = [0u8; 8];
    let raw_read = tty::read(TtyIndex(0), &mut out, true);

    tty::set_ldisc(TtyIndex(0), slopos_abi::syscall::N_TTY).unwrap();
    let ntty_kind = tty::get_ldisc(TtyIndex(0));
    let ntty_termios = tty::get_termios(TtyIndex(0)).unwrap();
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if raw_kind != Ok(slopos_abi::syscall::N_RAW)
        || ntty_kind != Ok(slopos_abi::syscall::N_TTY)
        || raw_termios.c_cc[slopos_abi::syscall::VMIN] != 7
        || raw_termios.c_cc[slopos_abi::syscall::VTIME] != 3
        || ntty_termios.c_cc[slopos_abi::syscall::VMIN] != 7
        || ntty_termios.c_cc[slopos_abi::syscall::VTIME] != 3
        || raw_termios.c_line != slopos_abi::syscall::N_RAW as u8
        || ntty_termios.c_line != slopos_abi::syscall::N_TTY as u8
        || raw_read != Ok(1)
        || out[0] != b'q'
    {
        klog_info!(
            "TTY_TEST: BUG - ldisc round-trip mismatch (raw_kind={:?}, ntty_kind={:?}, raw_read={:?}, raw_line={}, ntty_line={})",
            raw_kind,
            ntty_kind,
            raw_read,
            raw_termios.c_line,
            ntty_termios.c_line
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase16_set_ldisc_invalid_id_rejected() -> TestResult {
    tty::table::tty_table_init();

    match tty::set_ldisc(TtyIndex(0), 99) {
        Err(TtyError::UnsupportedLineDiscipline) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - invalid ldisc id should be rejected, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase17_pty_alloc_returns_master_and_slave() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave = match tty::get_pty_number(master) {
        Ok(idx) => TtyIndex(idx as u8),
        Err(err) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", err);
            return TestResult::Fail;
        }
    };

    let master_ok = tty::open_ref(master).is_ok();
    let slave_ok = tty::open_ref(slave).is_ok();
    let slave_is_pty = tty::is_pty_slave(slave);
    let master_is_not_slave = !tty::is_pty_slave(master);

    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if !master_ok || !slave_ok || master == slave || !slave_is_pty || !master_is_not_slave {
        klog_info!(
            "TTY_TEST: BUG - PTY allocation mismatch (master_ok={}, slave_ok={}, master={}, slave={}, slave_is_pty={}, master_is_not_slave={})",
            master_ok,
            slave_ok,
            master.0,
            slave.0,
            slave_is_pty,
            master_is_not_slave
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase17_pty_master_to_slave_flow() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    let write_rc = tty::write(master, b"hello");
    let mut buf = [0u8; 16];
    let read_rc = tty::read(slave, &mut buf, true);

    tty::set_termios(slave, &saved).unwrap();
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if write_rc != Ok(5) || read_rc != Ok(5) || &buf[..5] != b"hello" {
        klog_info!(
            "TTY_TEST: BUG - PTY master->slave flow mismatch (write={:?}, read={:?}, data={:?})",
            write_rc,
            read_rc,
            &buf[..5]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase17_pty_slave_to_master_flow() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    let write_rc = tty::write(slave, b"world\n");
    let mut buf = [0u8; 16];
    let read_rc = tty::read(master, &mut buf, true);

    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if write_rc != Ok(6) || read_rc != Ok(7) || &buf[..7] != b"world\r\n" {
        klog_info!(
            "TTY_TEST: BUG - PTY slave->master flow mismatch (write={:?}, read={:?}, data={:?})",
            write_rc,
            read_rc,
            &buf[..7]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase17_master_close_hangs_up_slave() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    let close_rc = tty::close_ref(master);
    let is_hung = tty::is_hung_up(slave);
    let mut buf = [0u8; 8];
    let read_rc = tty::read(slave, &mut buf, true);

    let _ = tty::close_ref(slave);

    if close_rc != Ok(0) || !is_hung || read_rc != Err(TtyError::HungUp) {
        klog_info!(
            "TTY_TEST: BUG - master close should hang up slave (close={:?}, is_hung={}, read={:?})",
            close_rc,
            is_hung,
            read_rc
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase17_slave_close_returns_master_eof() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    let close_rc = tty::close_ref(slave);
    let mut buf = [0u8; 8];
    let read_rc = tty::read(master, &mut buf, true);

    let _ = tty::close_ref(master);

    if close_rc != Ok(0) || read_rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - slave close should give master EOF (close={:?}, read={:?})",
            close_rc,
            read_rc
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_phase17_pty_canonical_editing_on_slave() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    let saved = tty::get_termios(slave).unwrap();
    let mut no_echo = saved;
    no_echo.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios(slave, &no_echo).unwrap();

    let write_rc = tty::write(master, b"foo\nbar\n");
    let mut buf = [0u8; 16];
    let first_read = tty::read(slave, &mut buf, true);
    let second_read = tty::read(slave, &mut buf, true);

    tty::set_termios(slave, &saved).unwrap();
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if write_rc != Ok(8) || first_read != Ok(4) || second_read != Ok(4) {
        klog_info!(
            "TTY_TEST: BUG - PTY canonical reads mismatch (write={:?}, first={:?}, second={:?})",
            write_rc,
            first_read,
            second_read
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// ===========================================================================
// Phase 19: Strict Session Gates & Foreground Outcomes
// ===========================================================================

/// Phase 19: No session attached — check_read returns BootstrapAllowed.
pub fn test_phase19_bootstrap_allowed_no_session_read() -> TestResult {
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

/// Phase 19: Session attached but no fg_pgrp — check_read returns BootstrapAllowed.
pub fn test_phase19_bootstrap_allowed_no_fg_pgrp() -> TestResult {
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

/// Phase 19: Cross-session read — DeniedCrossSession.
pub fn test_phase19_denied_cross_session_read() -> TestResult {
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

/// Phase 19: Cross-session write with TOSTOP — DeniedCrossSession (not BackgroundWrite).
pub fn test_phase19_denied_cross_session_write_tostop() -> TestResult {
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

/// Phase 19: Cross-session write without TOSTOP — still DeniedCrossSession.
pub fn test_phase19_cross_session_write_no_tostop_still_denied() -> TestResult {
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

/// Phase 19: Kernel task (sid=0) is exempted from cross-session denial on read.
pub fn test_phase19_kernel_task_exempted_cross_session_read() -> TestResult {
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

/// Phase 19: Kernel task (sid=0) is exempted from cross-session denial on write.
pub fn test_phase19_kernel_task_exempted_cross_session_write() -> TestResult {
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

/// Phase 19: Same-session background read — BackgroundRead (not DeniedCrossSession).
pub fn test_phase19_same_session_background_read_sigttin() -> TestResult {
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

/// Phase 19: Same-session background write with TOSTOP — BackgroundWrite.
pub fn test_phase19_same_session_background_write_sigttou() -> TestResult {
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

/// Phase 19: check_write with no session returns Allowed (not BootstrapAllowed).
/// The write path uses a simpler model: no session = Allowed, not BootstrapAllowed.
pub fn test_phase19_check_write_no_session_allowed() -> TestResult {
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

/// Phase 19: TtyError::CrossSessionDenied is a distinct error variant.
pub fn test_phase19_cross_session_denied_error_variant() -> TestResult {
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
// Phase 20: PTY Pair Atomicity & Lifecycle Hardening
// ===========================================================================

/// Phase 20: pty_alloc initialises both master and slave slots atomically.
pub fn test_phase20_pty_alloc_pair_both_initialized() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master) {
        Ok(n) => n,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(slave_num as u8);

    // Both slots should be Some (initialised).
    let master_exists =
        tty::table::with_tty_ref(master, |tty| tty.index == master).unwrap_or(false);
    let slave_exists = tty::table::with_tty_ref(slave, |tty| tty.index == slave).unwrap_or(false);

    // Cleanup.
    tty::open_ref(master).ok();
    tty::open_ref(slave).ok();
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if !master_exists || !slave_exists {
        klog_info!(
            "TTY_TEST: BUG - pair not fully initialised (master={}, slave={})",
            master_exists,
            slave_exists
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: closing master then slave frees both slots.
pub fn test_phase20_pty_close_master_first_frees_pair() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Close master first (triggers hangup on slave), then slave.
    let _ = tty::close_ref(master);
    let _ = tty::close_ref(slave);

    // Both slots should now be None (freed).
    let master_freed = TTY_SLOTS[master.0 as usize].lock().is_none();
    let slave_freed = TTY_SLOTS[slave.0 as usize].lock().is_none();

    if !master_freed || !slave_freed {
        klog_info!(
            "TTY_TEST: BUG - pair not freed after master-first close (master_freed={}, slave_freed={})",
            master_freed,
            slave_freed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: closing slave then master frees both slots (order independence).
pub fn test_phase20_pty_close_slave_first_frees_pair() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Close slave first, then master.
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    let master_freed = TTY_SLOTS[master.0 as usize].lock().is_none();
    let slave_freed = TTY_SLOTS[slave.0 as usize].lock().is_none();

    if !master_freed || !slave_freed {
        klog_info!(
            "TTY_TEST: BUG - pair not freed after slave-first close (master_freed={}, slave_freed={})",
            master_freed,
            slave_freed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: freed pair can be reallocated with fresh state.
pub fn test_phase20_pty_reallocation_after_free() -> TestResult {
    tty::table::tty_table_init();

    // Allocate + open + close a pair to return slots to the free pool.
    let master1 = tty::pty_alloc().unwrap();
    let slave1 = TtyIndex(tty::get_pty_number(master1).unwrap() as u8);
    tty::open_ref(master1).unwrap();
    tty::open_ref(slave1).unwrap();
    let _ = tty::close_ref(slave1);
    let _ = tty::close_ref(master1);

    // Reallocate — should succeed and return valid indices.
    let master2 = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - reallocation failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave2 = TtyIndex(tty::get_pty_number(master2).unwrap() as u8);

    // Verify the reallocated pair is functional.
    tty::open_ref(master2).unwrap();
    tty::open_ref(slave2).unwrap();

    let slave_is_pty = tty::is_pty_slave(slave2);
    let master_is_not_slave = !tty::is_pty_slave(master2);

    let _ = tty::close_ref(slave2);
    let _ = tty::close_ref(master2);

    if !slave_is_pty || !master_is_not_slave {
        klog_info!(
            "TTY_TEST: BUG - reallocated pair has wrong types (slave_is_pty={}, master_is_not_slave={})",
            slave_is_pty,
            master_is_not_slave
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: pty_open_slave validates that the slot is actually a PTY slave.
pub fn test_phase20_pty_open_slave_validates_type() -> TestResult {
    tty::table::tty_table_init();

    // Try to open a serial console slot (index 0) as a PTY slave — should fail.
    let result = tty::pty_open_slave(TtyIndex(0));
    if result.is_ok() {
        klog_info!("TTY_TEST: BUG - pty_open_slave should reject non-slave index 0");
        // Undo the accidental open.
        let _ = tty::close_ref(TtyIndex(0));
        return TestResult::Fail;
    }

    // Try to open a non-existent slot — should fail.
    let result = tty::pty_open_slave(TtyIndex(5));
    if result.is_ok() {
        klog_info!("TTY_TEST: BUG - pty_open_slave should reject empty slot 5");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 20: pty_open_slave increments open_count, preventing pair free.
pub fn test_phase20_pty_open_slave_prevents_free() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();

    // Open slave via the validated path.
    let open_rc = tty::pty_open_slave(slave);
    if open_rc.is_err() {
        klog_info!("TTY_TEST: BUG - pty_open_slave failed on valid slave");
        let _ = tty::close_ref(master);
        return TestResult::Fail;
    }

    // Close master — slave still has open_count > 0, so pair should NOT be freed.
    let _ = tty::close_ref(master);

    let slave_still_exists = TTY_SLOTS[slave.0 as usize].lock().is_some();

    // Cleanup.
    let _ = tty::close_ref(slave);

    if !slave_still_exists {
        klog_info!("TTY_TEST: BUG - slave freed while open_count > 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: free_pair_if_unused does not free when one side has open_count > 0.
pub fn test_phase20_partial_open_no_free() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Open slave a second time to keep it alive.
    tty::open_ref(slave).unwrap();

    // Close master (open_count → 0, hangup slave).
    let _ = tty::close_ref(master);

    // Close slave once (open_count → 1, still alive).
    let _ = tty::close_ref(slave);

    let slave_alive = TTY_SLOTS[slave.0 as usize].lock().is_some();
    let master_alive = TTY_SLOTS[master.0 as usize].lock().is_some();

    // Final close of slave (open_count → 0).
    let _ = tty::close_ref(slave);

    if !slave_alive {
        klog_info!("TTY_TEST: BUG - slave freed with open_count > 0");
        return TestResult::Fail;
    }
    // Master should still be alive because pair-free only happens when BOTH are 0.
    if !master_alive {
        klog_info!("TTY_TEST: BUG - master freed while slave still has open_count > 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 20: rapid allocate/free/reallocate cycles produce valid pairs.
pub fn test_phase20_rapid_alloc_free_realloc() -> TestResult {
    tty::table::tty_table_init();

    for i in 0..3u8 {
        let master = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(err) => {
                klog_info!("TTY_TEST: BUG - rapid alloc cycle {} failed: {:?}", i, err);
                return TestResult::Fail;
            }
        };
        let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);

        tty::open_ref(master).unwrap();
        tty::open_ref(slave).unwrap();

        // Verify data flows correctly on this pair.
        let saved = tty::get_termios(slave).unwrap();
        let mut raw = saved;
        raw.c_lflag &= !(slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO);
        tty::set_termios(slave, &raw).unwrap();

        let write_ok = tty::write(master, b"x").is_ok();
        let mut buf = [0u8; 4];
        let read_ok = tty::read(slave, &mut buf, true) == Ok(1) && buf[0] == b'x';

        tty::set_termios(slave, &saved).unwrap();
        let _ = tty::close_ref(slave);
        let _ = tty::close_ref(master);

        if !write_ok || !read_ok {
            klog_info!(
                "TTY_TEST: BUG - rapid alloc cycle {} data flow broken (write={}, read={})",
                i,
                write_ok,
                read_ok
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 20: pty_open_slave on a freed slave returns NotAllocated.
pub fn test_phase20_pty_open_slave_after_free() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Free the pair.
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    // Attempting to open the freed slave should fail.
    let result = tty::pty_open_slave(slave);
    match result {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - pty_open_slave on freed slave expected NotAllocated, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 21: Event-Driven Readiness & IXON Completion
// ===========================================================================

/// Phase 21: poll_events returns POLLIN when cooked data is available.
pub fn test_phase21_poll_events_pollin_with_data() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Feed a complete canonical line ("a\n") so cooked data is available.
    tty::push_input(idx, b'a');
    tty::push_input(idx, b'\n');

    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLIN);
    drain_tty_nonblock(idx);

    if (revents & slopos_abi::syscall::POLLIN) == 0 {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLIN when cooked data exists");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events returns 0 for POLLIN when no cooked data.
pub fn test_phase21_poll_events_no_pollin_without_data() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLIN);

    if (revents & slopos_abi::syscall::POLLIN) != 0 {
        klog_info!("TTY_TEST: BUG - poll_events should NOT report POLLIN when no cooked data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events returns POLLOUT when output is not stopped.
pub fn test_phase21_poll_events_pollout_when_not_stopped() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLOUT);

    if (revents & slopos_abi::syscall::POLLOUT) == 0 {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLOUT when not stopped");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events returns 0 for POLLOUT when IXON-stopped.
pub fn test_phase21_poll_events_no_pollout_when_stopped() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Enable IXON and send Ctrl+S to stop output.
    {
        let mut guard = TTY_SLOTS[idx.0 as usize].lock();
        if let Some(tty) = guard.as_mut() {
            let mut t = *tty.ldisc.termios();
            t.c_iflag |= slopos_abi::syscall::IXON;
            tty.ldisc.set_termios(&t);
        }
    }
    tty::push_input(idx, 0x13); // Ctrl+S = VSTOP

    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLOUT);

    // Resume output for cleanup.
    tty::push_input(idx, 0x11); // Ctrl+Q = VSTART
    drain_tty_nonblock(idx);

    if (revents & slopos_abi::syscall::POLLOUT) != 0 {
        klog_info!("TTY_TEST: BUG - poll_events should NOT report POLLOUT when IXON-stopped");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events returns POLLHUP when TTY is hung up.
pub fn test_phase21_poll_events_pollhup_on_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Hang up TTY 0.
    tty::hangup(idx);

    let revents = tty::poll_events(
        idx,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    // Restore TTY 0 (re-init).
    tty::table::tty_table_init();

    if (revents & slopos_abi::syscall::POLLHUP) == 0 {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLHUP on hung-up TTY");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events returns 0 for invalid index.
pub fn test_phase21_poll_events_invalid_index_returns_zero() -> TestResult {
    let revents = tty::poll_events(
        TtyIndex(255),
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );
    if revents != 0 {
        klog_info!("TTY_TEST: BUG - poll_events on invalid index should return 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: IXON stopped state is tracked in ldisc via push_input.
pub fn test_phase21_ixon_stopped_state_via_push_input() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Enable IXON.
    {
        let mut guard = TTY_SLOTS[idx.0 as usize].lock();
        if let Some(tty) = guard.as_mut() {
            let mut t = *tty.ldisc.termios();
            t.c_iflag |= slopos_abi::syscall::IXON;
            tty.ldisc.set_termios(&t);
        }
    }

    // Ctrl+S stops output.
    tty::push_input(idx, 0x13);
    let stopped = {
        let guard = TTY_SLOTS[idx.0 as usize].lock();
        guard
            .as_ref()
            .map(|tty| tty.ldisc.is_stopped())
            .unwrap_or(false)
    };
    if !stopped {
        klog_info!("TTY_TEST: BUG - Ctrl+S via push_input should set stopped state");
        tty::push_input(idx, 0x11); // cleanup
        drain_tty_nonblock(idx);
        return TestResult::Fail;
    }

    // Ctrl+Q resumes output.
    tty::push_input(idx, 0x11);
    let stopped_after = {
        let guard = TTY_SLOTS[idx.0 as usize].lock();
        guard
            .as_ref()
            .map(|tty| tty.ldisc.is_stopped())
            .unwrap_or(true)
    };
    drain_tty_nonblock(idx);

    if stopped_after {
        klog_info!("TTY_TEST: BUG - Ctrl+Q via push_input should clear stopped state");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: IXON resume clears stopped and any character resumes.
pub fn test_phase21_ixon_any_char_resumes() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Enable IXON.
    {
        let mut guard = TTY_SLOTS[idx.0 as usize].lock();
        if let Some(tty) = guard.as_mut() {
            let mut t = *tty.ldisc.termios();
            t.c_iflag |= slopos_abi::syscall::IXON;
            tty.ldisc.set_termios(&t);
        }
    }

    // Ctrl+S stops, then any printable char resumes.
    tty::push_input(idx, 0x13);
    tty::push_input(idx, b'x'); // any char resumes when IXON

    let stopped = {
        let guard = TTY_SLOTS[idx.0 as usize].lock();
        guard
            .as_ref()
            .map(|tty| tty.ldisc.is_stopped())
            .unwrap_or(true)
    };
    drain_tty_nonblock(idx);

    if stopped {
        klog_info!("TTY_TEST: BUG - any char should resume output when IXON stopped");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: poll_events only returns events that are requested.
pub fn test_phase21_poll_events_respects_requested_mask() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // With data available, requesting only POLLOUT should not set POLLIN.
    tty::push_input(idx, b'a');
    tty::push_input(idx, b'\n');

    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLOUT);
    drain_tty_nonblock(idx);

    if (revents & slopos_abi::syscall::POLLIN) != 0 {
        klog_info!(
            "TTY_TEST: BUG - poll_events should not return POLLIN when only POLLOUT requested"
        );
        return TestResult::Fail;
    }
    if (revents & slopos_abi::syscall::POLLOUT) == 0 {
        klog_info!(
            "TTY_TEST: BUG - poll_events should return POLLOUT when requested and not stopped"
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: POLLHUP is always returned even if not requested (POSIX).
pub fn test_phase21_pollhup_always_reported() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    tty::hangup(idx);

    // Request only POLLIN -- POLLHUP should still appear.
    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLIN);
    tty::table::tty_table_init(); // restore

    if (revents & slopos_abi::syscall::POLLHUP) == 0 {
        klog_info!("TTY_TEST: BUG - POLLHUP should always be reported on hung-up TTY");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 21: PTY peer_closed sets POLLHUP when no data remains.
pub fn test_phase21_poll_events_peer_closed_pollhup() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).ok();
    tty::open_ref(slave).ok();

    // Close the slave to mark peer_closed on master.
    let _ = tty::close_ref(slave);

    let revents = tty::poll_events(master, slopos_abi::syscall::POLLIN);

    // Cleanup.
    let _ = tty::close_ref(master);

    if (revents & slopos_abi::syscall::POLLHUP) == 0 {
        klog_info!(
            "TTY_TEST: BUG - poll_events should report POLLHUP when peer_closed and no data"
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_default_console_tty_initial_value() -> TestResult {
    if tty::default_console_tty() != TtyIndex(0) {
        klog_info!(
            "TTY_TEST: BUG - default_console_tty should start at 0, got {:?}",
            tty::default_console_tty()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_set_default_console_tty() -> TestResult {
    tty::set_default_console_tty(TtyIndex(1));
    let updated = tty::default_console_tty();
    tty::set_default_console_tty(TtyIndex(0));

    if updated != TtyIndex(1) {
        klog_info!(
            "TTY_TEST: BUG - set_default_console_tty did not stick (got {:?})",
            updated
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_switch_active_tty_valid() -> TestResult {
    tty::table::tty_table_init();
    let original = tty::active_tty();
    let switched = tty::switch_active_tty(TtyIndex(1));
    let active = tty::active_tty();
    let _ = tty::switch_active_tty(original);

    if switched.is_err() || active != TtyIndex(1) {
        klog_info!("TTY_TEST: BUG - switch_active_tty valid switch failed");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_switch_active_tty_invalid_index() -> TestResult {
    match tty::switch_active_tty(TtyIndex(tty::MAX_TTYS as u8)) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - invalid active tty index should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase22_switch_active_tty_unallocated() -> TestResult {
    tty::table::tty_table_init();
    match tty::switch_active_tty(TtyIndex(5)) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - unallocated active tty should return NotAllocated, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_phase22_vconsole_state_initial() -> TestResult {
    let state = VConsoleState::new();
    if state.cursor_row != 0 || state.cursor_col != 0 || state.rows != 25 || state.cols != 80 {
        klog_info!(
            "TTY_TEST: BUG - vconsole initial state mismatch row={} col={} rows={} cols={}",
            state.cursor_row,
            state.cursor_col,
            state.rows,
            state.cols
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_write_byte_printable() -> TestResult {
    let mut state = VConsoleState::new();
    state.write_byte(b'A');
    if state.cells[0][0] != b'A' || state.cursor_row != 0 || state.cursor_col != 1 {
        klog_info!("TTY_TEST: BUG - printable write did not update vconsole state");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_write_byte_newline() -> TestResult {
    let mut state = VConsoleState::new();
    state.write_byte(b'\n');
    if state.cursor_row != 1 || state.cursor_col != 0 {
        klog_info!(
            "TTY_TEST: BUG - newline did not move cursor to next row (row={}, col={})",
            state.cursor_row,
            state.cursor_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_write_byte_cr() -> TestResult {
    let mut state = VConsoleState::new();
    state.write_byte(b'A');
    state.write_byte(b'B');
    state.write_byte(b'\r');
    if state.cursor_col != 0 {
        klog_info!("TTY_TEST: BUG - carriage return should reset column to 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_write_byte_backspace() -> TestResult {
    let mut state = VConsoleState::new();
    state.write_byte(b'A');
    state.write_byte(b'B');
    state.write_byte(0x08);
    if state.cursor_col != 1 || state.cells[0][1] != b' ' {
        klog_info!("TTY_TEST: BUG - backspace did not erase previous column");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_scroll_at_bottom() -> TestResult {
    let mut state = VConsoleState::new();
    state.rows = 2;
    state.cols = 4;
    state.cells[0][0] = b'A';
    state.cells[1][0] = b'B';
    state.cursor_row = 1;
    state.cursor_col = 0;

    state.write_byte(b'\n');

    if state.cells[0][0] != b'B' || state.cells[1][0] != b' ' || state.cursor_row != 1 {
        klog_info!("TTY_TEST: BUG - vconsole scroll did not shift/clear rows correctly");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_active_tty_independent_of_fg_pgrp() -> TestResult {
    tty::table::tty_table_init();
    let _ = tty::set_foreground_pgrp(TtyIndex(0), 100);
    let _ = tty::set_foreground_pgrp(TtyIndex(1), 200);

    let before0 = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let before1 = tty::get_foreground_pgrp(TtyIndex(1)).unwrap_or(0);

    let _ = tty::switch_active_tty(TtyIndex(1));

    let after0 = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let after1 = tty::get_foreground_pgrp(TtyIndex(1)).unwrap_or(0);
    let _ = tty::switch_active_tty(TtyIndex(0));

    if before0 != after0 || before1 != after1 {
        klog_info!("TTY_TEST: BUG - switch_active_tty modified fg_pgrp state");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase22_vconsole_has_framebuffer_default_false() -> TestResult {
    tty::vconsole::reset_for_tests();
    if tty::vconsole::has_framebuffer() {
        klog_info!("TTY_TEST: BUG - vconsole framebuffer should be absent by default");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Test suite registration
// ===========================================================================
slopos_lib::define_test_suite!(
    tty,
    [
        test_ldisc_new_has_no_data,
        test_ldisc_read_empty,
        test_ldisc_canonical_newline,
        test_ldisc_canonical_backspace,
        test_ldisc_canonical_kill,
        test_ldisc_canonical_eof,
        test_ldisc_signal_ctrl_c,
        test_ldisc_raw_mode,
        test_ldisc_set_termios_flush,
        test_ldisc_flush_all,
        test_ldisc_echo_printable,
        test_ldisc_echo_newline,
        test_ldisc_multiple_reads,
        test_ldisc_backspace_empty,
        test_session_new_empty,
        test_session_attach,
        test_session_detach,
        test_session_check_read_foreground,
        test_session_check_read_background,
        test_session_check_read_no_session,
        test_session_check_read_kernel_task,
        test_session_check_write_no_tostop,
        test_session_check_write_tostop_background,
        // Phase 6: check_read replaces task_has_access
        test_session_check_read_replaces_task_has_access_foreground,
        test_session_check_read_replaces_task_has_access_background,
        test_session_check_read_replaces_task_has_access_permissive,
        test_session_set_fg_pgrp_checked_allowed,
        test_session_set_fg_pgrp_checked_denied,
        test_session_set_fg_pgrp_checked_no_session,
        test_tty_get_session_id_default,
        test_tty_attach_session,
        test_tty_detach_session,
        test_tty_detach_session_by_id,
        test_tty_set_fg_pgrp_checked,
        test_tty_index_eq,
        test_driver_none_no_panic,
        test_vconsole_drain_returns_zero,
        test_table_init_allocates_tty0_and_tty1,
        test_table_tty0_has_index_zero,
        test_table_tty0_active,
        test_table_with_tty_exists,
        test_table_with_tty_empty,
        test_active_tty_default,
        test_set_active_tty,
        test_foreground_pgrp,
        test_compositor_focus,
        test_keyboard_enter_scancode_reaches_active_tty,
        test_keyboard_scancode_routes_to_active_tty_index,
        test_keyboard_extended_up_arrow_reaches_tty,
        // Phase 2: Input flag processing
        test_ldisc_icrnl,
        test_ldisc_igncr,
        test_ldisc_inlcr,
        test_ldisc_istrip,
        // Phase 2: Output processing
        test_ldisc_opost_onlcr,
        test_ldisc_opost_ocrnl,
        test_ldisc_output_raw,
        // Phase 2: Signal generation
        test_ldisc_signal_ctrl_backslash,
        test_ldisc_signal_ctrl_z,
        // Phase 2: Flow control
        test_ldisc_flow_control_ixon,
        // Phase 2: ECHOCTL
        test_ldisc_echoctl,
        // Phase 2: VLNEXT
        test_ldisc_vlnext,
        // Phase 2: VWERASE
        test_ldisc_vwerase,
        // Phase 2: edit_content / reprint
        test_ldisc_edit_content,
        // Phase 2: Output processing via TTY write
        test_tty_write_returns_input_len,
        // Phase 3: Input pipeline cleanup
        test_keyboard_no_input_event_delivery,
        test_keyboard_break_code_no_input,
        test_keyboard_modifier_no_input,
        test_keyboard_press_release_single_char,
        test_vconsole_drain_via_drain_hw_input,
        test_keyboard_multi_key_sequence,
        // Phase 5: FD integration
        test_tty_write_output_processing,
        test_tty_write_raw_passthrough,
        test_tty_write_invalid_index,
        test_tty_per_tty_termios_isolation,
        test_tty_per_tty_winsize_isolation,
        test_tty_per_tty_fg_pgrp_isolation,
        test_tty_per_tty_has_data_isolation,
        test_tty_per_tty_session_isolation,
        test_tty_read_invalid_tty_returns_error,
        // Phase 6: Control-Plane Correctness
        test_tty_index_abi_type,
        test_signal_constants,
        test_set_compositor_focus_does_not_set_fg_pgrp,
        test_check_read_sole_gate_background,
        test_tty_open_count_lifecycle,
        test_tty_hangup_sets_flag_and_detaches_session,
        test_tty_hangup_nonblock_read_eio,
        test_tty_hangup_blocking_read_eof,
        test_phase9_tty_error_variants,
        test_phase9_read_returns_result,
        test_phase9_read_invalid_index_error,
        test_phase9_read_not_allocated_error,
        test_phase9_write_returns_result,
        test_phase9_get_termios_returns_result,
        test_phase9_vmin0_vtime0_immediate_return,
        test_phase9_vmin_enforcement,
        test_phase9_set_fg_pgrp_checked_permission_denied,
        test_phase9_hangup_read_returns_hung_up,
        // Phase 8: Per-TTY Locking & Performance
        test_phase8_per_tty_lock_independence,
        test_phase8_driver_id_round_trip,
        test_phase8_split_write_returns_input_len,
        test_phase8_idle_cb_iterates_all_ttys,
        test_phase8_merged_drain_read,
        test_phase8_with_tty_per_slot,
        test_phase8_driver_id_traits,
        // Phase 10: Job Control Correctness
        test_phase10_sigttou_constant,
        test_phase10_check_write_tostop_blocks_background,
        test_phase10_check_write_no_tostop_allows_background,
        test_phase10_check_write_tostop_allows_foreground,
        test_phase10_check_read_cross_session_rejected,
        test_phase10_check_read_same_session_foreground,
        test_phase10_check_read_kernel_task_allowed,
        test_phase10_tty_write_foreground_with_tostop,
        // Phase 11: Non-Canonical Timing Fix
        test_phase11_vmin_vtime_enough_data_returns_immediately,
        test_phase11_vmin_vtime_partial_nonblock,
        test_phase11_vmin_vtime_no_data_nonblock,
        test_phase11_vmin_vtime_interbyte_timeout_returns_partial,
        test_phase11_ldisc_vmin_vtime_helper,
        // Phase 12: Sane Defaults & Output Column Tracking
        test_phase12_default_termios_has_icrnl,
        test_phase12_default_termios_has_opost_onlcr,
        test_phase12_default_termios_has_full_lflag,
        test_phase12_output_column_tracking_printable,
        test_phase12_output_column_tracking_newline,
        test_phase12_output_column_tracking_cr,
        test_phase12_output_column_tracking_tab,
        test_phase12_output_column_tracking_backspace,
        test_phase12_onocr_at_column_zero,
        test_phase12_default_onlcr_newline_expands,
        // Phase 13: ABI Signal Constant Unification
        test_phase13_signal_values_from_signal_module,
        test_phase13_ldisc_signal_uses_signal_module,
        test_phase13_hangup_signals_from_signal_module,
        test_phase13_job_control_signals_from_signal_module,
        // Phase 14: Responsibility Split — PTY Foundation
        test_phase14_session_id_zero_is_none,
        test_phase14_session_id_round_trip,
        test_phase14_pgrp_id_zero_is_none,
        test_phase14_pgrp_id_round_trip,
        test_phase14_session_option_fields,
        test_phase14_session_option_attach_detach,
        test_phase14_raw_disc_new_empty,
        test_phase14_raw_disc_input_read,
        test_phase14_raw_disc_output_passthrough,
        test_phase14_raw_disc_flush,
        test_phase14_ldisc_kind_ntty_delegation,
        test_phase14_ldisc_kind_raw_delegation,
        test_phase14_pty_driver_id_variants,
        test_phase14_pty_master_driver_kind,
        test_phase14_pty_slave_driver_kind,
        // Phase 15: POSIX Quick Wins
        test_phase15_canonical_one_line_per_read,
        test_phase15_canonical_has_data_line_count,
        test_phase15_canonical_eof_line_boundary,
        test_phase15_sigwinch_constant,
        test_phase15_word_erase_path_boundary,
        test_phase15_word_erase_mixed_boundary,
        test_phase15_word_erase_trailing_spaces,
        test_phase15_canonical_small_buffer_read,
        test_phase16_tcsetsw_preserves_pending_input,
        test_phase16_tcsetsf_flushes_pending_input,
        test_phase16_read_with_attach_false_skips_auto_attach,
        test_phase18_read_with_attach_true_skips_durable_attach,
        test_phase18_acquire_and_release_controlling_terminal,
        test_phase18_release_wrong_session_is_noop,
        test_phase16_get_ldisc_default_is_ntty,
        test_phase16_set_ldisc_round_trip_preserves_termios,
        test_phase16_set_ldisc_invalid_id_rejected,
        test_phase17_pty_alloc_returns_master_and_slave,
        test_phase17_pty_master_to_slave_flow,
        test_phase17_pty_slave_to_master_flow,
        test_phase17_master_close_hangs_up_slave,
        test_phase17_slave_close_returns_master_eof,
        test_phase17_pty_canonical_editing_on_slave,
        // Phase 19: Strict Session Gates & Foreground Outcomes
        test_phase19_bootstrap_allowed_no_session_read,
        test_phase19_bootstrap_allowed_no_fg_pgrp,
        test_phase19_denied_cross_session_read,
        test_phase19_denied_cross_session_write_tostop,
        test_phase19_cross_session_write_no_tostop_still_denied,
        test_phase19_kernel_task_exempted_cross_session_read,
        test_phase19_kernel_task_exempted_cross_session_write,
        test_phase19_same_session_background_read_sigttin,
        test_phase19_same_session_background_write_sigttou,
        test_phase19_check_write_no_session_allowed,
        test_phase19_cross_session_denied_error_variant,
        // Phase 20: PTY Pair Atomicity & Lifecycle Hardening
        test_phase20_pty_alloc_pair_both_initialized,
        test_phase20_pty_close_master_first_frees_pair,
        test_phase20_pty_close_slave_first_frees_pair,
        test_phase20_pty_reallocation_after_free,
        test_phase20_pty_open_slave_validates_type,
        test_phase20_pty_open_slave_prevents_free,
        test_phase20_partial_open_no_free,
        test_phase20_rapid_alloc_free_realloc,
        test_phase20_pty_open_slave_after_free,
        // Phase 21: Event-Driven Readiness & IXON Completion
        test_phase21_poll_events_pollin_with_data,
        test_phase21_poll_events_no_pollin_without_data,
        test_phase21_poll_events_pollout_when_not_stopped,
        test_phase21_poll_events_no_pollout_when_stopped,
        test_phase21_poll_events_pollhup_on_hangup,
        test_phase21_poll_events_invalid_index_returns_zero,
        test_phase21_ixon_stopped_state_via_push_input,
        test_phase21_ixon_any_char_resumes,
        test_phase21_poll_events_respects_requested_mask,
        test_phase21_pollhup_always_reported,
        test_phase21_poll_events_peer_closed_pollhup,
        test_phase22_default_console_tty_initial_value,
        test_phase22_set_default_console_tty,
        test_phase22_switch_active_tty_valid,
        test_phase22_switch_active_tty_invalid_index,
        test_phase22_switch_active_tty_unallocated,
        test_phase22_vconsole_state_initial,
        test_phase22_vconsole_write_byte_printable,
        test_phase22_vconsole_write_byte_newline,
        test_phase22_vconsole_write_byte_cr,
        test_phase22_vconsole_write_byte_backspace,
        test_phase22_vconsole_scroll_at_bottom,
        test_phase22_active_tty_independent_of_fg_pgrp,
        test_phase22_vconsole_has_framebuffer_default_false,
    ]
);
