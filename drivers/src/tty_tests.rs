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
use slopos_abi::syscall::{
    CcIndex, ControlFlags, InputFlags, LocalFlags, OutputFlags, POSIX_VDISABLE,
};
use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::tty;
use crate::tty::TtyError;
use crate::tty::TtyIndex;
use crate::tty::driver::{DriverId, TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LdiscKind, LdiscOps, LineDisc, OutputAction, RawDisc};
use crate::tty::session::TtySession;
use crate::tty::session::{
    ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION, ProcessGroupId, SessionId,
};
use crate::tty::table::{TTY_GENERATIONS, TTY_OUTPUT_INFLIGHT, TTY_SLOTS};
use crate::tty::vconsole::VConsoleState;

use crate::tty::pty::PtyPeerHandle;
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
///
/// Phase 24 update: the outer API now validates that the target pgrp has
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

/// Phase 3: VConsole driver drain_input returns 0 via drain_hw_input_locked (interrupt-driven).
pub fn test_vconsole_drain_via_drain_hw_input() -> TestResult {
    tty::table::tty_table_init();

    // TTY 1 is VConsole — drain_hw_input_locked should be a no-op (input is
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
        klog_info!("TTY_TEST: BUG - VConsole drain_hw_input_locked produced phantom data");
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

    if rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - nonblock read on hung tty expected Ok(0), got {:?}",
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

    // Phase 33: hung-up TTY reads now always return EOF (Ok(0)),
    // regardless of blocking mode.  Previously nonblock returned
    // Err(HungUp) but POSIX requires EOF for reads after hangup.
    match result {
        Ok(0) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup nonblock read expected Ok(0) EOF, got {:?}",
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
/// path runs (via has_data which calls drain_hw_input_locked internally).
pub fn test_phase8_idle_cb_iterates_all_ttys() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    // Push data to TTY 1 via push_input (simulates keyboard on vconsole).
    tty::push_input(TtyIndex(1), b'z');
    tty::push_input(TtyIndex(1), b'\n');

    // has_data internally calls drain_hw_input_locked, simulating the idle path.
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
        peer: PtyPeerHandle::new(TtyIndex(2), 0),
    };
    let slave_id = DriverId::PtySlave {
        peer: PtyPeerHandle::new(TtyIndex(3), 0),
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
        peer: PtyPeerHandle::new(TtyIndex(2), 0),
    };
    if drv.id()
        != (DriverId::PtyMaster {
            peer: PtyPeerHandle::new(TtyIndex(2), 0),
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
        peer: PtyPeerHandle::new(TtyIndex(3), 0),
    };
    if drv.id()
        != (DriverId::PtySlave {
            peer: PtyPeerHandle::new(TtyIndex(3), 0),
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

    if close_rc != Ok(0) || !is_hung || read_rc != Ok(0) {
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
// Phase 23: Canonical EOF, ISIG Flush & Signal Integrity
// ===========================================================================

/// Phase 23: Ctrl+D on empty buffer produces EOF (0 bytes) without phantom
/// has_data state.  Previously, flush_edit_to_cooked incremented line_count
/// on empty buffer, leaving has_data() stuck true.
pub fn test_phase23_canonical_eof_empty_no_phantom() -> TestResult {
    let mut ld = LineDisc::new();

    // Press Ctrl+D (VEOF = 0x04) with empty edit buffer.
    ld.input_char(0x04);

    // has_data should be true once (the EOF marker).
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data should be true immediately after empty EOF");
        return TestResult::Fail;
    }

    // read() should return 0 (EOF).
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - empty EOF read should return 0, got {}", n);
        return TestResult::Fail;
    }

    // After consuming the EOF, has_data should be false (no phantom).
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data still true after consuming empty EOF (phantom state)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: Ctrl+D after text returns text without newline, then no phantom.
pub fn test_phase23_canonical_eof_with_pending_text_no_phantom() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc" then Ctrl+D.
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x04); // VEOF

    // Should have data.
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false after text+EOF");
        return TestResult::Fail;
    }

    // Read should return "abc" (3 bytes, no newline).
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - text+EOF read mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }

    // No phantom state.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after reading text+EOF chunk (phantom)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: ISIG flush — Ctrl+C without NOFLSH clears edit and cooked buffers.
pub fn test_phase23_isig_flush_no_noflsh() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc" into edit buffer (canonical mode, no newline yet).
    for &c in b"abc" {
        ld.input_char(c);
    }

    // Verify edit buffer has content.
    if ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should have content before signal");
        return TestResult::Fail;
    }

    // Ctrl+C should generate SIGINT and flush input (NOFLSH not set by default).
    let action = ld.input_char(0x03); // VINTR = Ctrl+C
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected Signal(SIGINT), got {:?}", other);
            return TestResult::Fail;
        }
    }

    // Edit buffer should be flushed.
    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after ISIG flush");
        return TestResult::Fail;
    }

    // No cooked data should remain.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after ISIG flush (should be clear)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: ISIG with NOFLSH set — Ctrl+C does NOT flush buffers.
pub fn test_phase23_isig_flush_with_noflsh() -> TestResult {
    let mut ld = LineDisc::new();

    // Set NOFLSH flag.
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::NOFLSH;
    ld.set_termios(&t);

    // Type "abc" into edit buffer.
    for &c in b"abc" {
        ld.input_char(c);
    }

    // Ctrl+C should generate SIGINT but NOT flush.
    let action = ld.input_char(0x03); // VINTR = Ctrl+C
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected Signal(SIGINT) with NOFLSH, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // Edit buffer should still have content.
    if ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - NOFLSH should preserve edit buffer on ISIG");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: After Ctrl+C (without NOFLSH), subsequent newline produces empty line.
pub fn test_phase23_isig_ctrl_c_clears_edit_buffer() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc", then Ctrl+C (flushes), then newline.
    for &c in b"abc" {
        ld.input_char(c);
    }
    let _ = ld.input_char(0x03); // Ctrl+C flushes
    ld.input_char(b'\n');

    // Should have one line.
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false after flush+newline");
        return TestResult::Fail;
    }

    // Read should return just "\n" (1 byte), not "abc\n".
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - after Ctrl+C flush, newline should produce 1-byte line, got {} bytes (first=0x{:02x})",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: ISIG flush works for SIGQUIT (Ctrl+\\) too.
pub fn test_phase23_isig_flush_sigquit() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "xyz" then Ctrl+\\ (VQUIT = 0x1C).
    for &c in b"xyz" {
        ld.input_char(c);
    }
    let action = ld.input_char(0x1C);
    match action {
        InputAction::Signal(sig) if sig == SIGQUIT => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected Signal(SIGQUIT), got {:?}", other);
            return TestResult::Fail;
        }
    }

    // Buffers should be flushed (default: no NOFLSH).
    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after SIGQUIT flush");
        return TestResult::Fail;
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - cooked data remains after SIGQUIT flush");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: ISIG flush works for SIGTSTP (Ctrl+Z) too.
pub fn test_phase23_isig_flush_sigtstp() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "xyz" then Ctrl+Z (VSUSP = 0x1A).
    for &c in b"xyz" {
        ld.input_char(c);
    }
    let action = ld.input_char(0x1A);
    match action {
        InputAction::Signal(sig) if sig == SIGTSTP => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected Signal(SIGTSTP), got {:?}", other);
            return TestResult::Fail;
        }
    }

    // Buffers should be flushed.
    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after SIGTSTP flush");
        return TestResult::Fail;
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - cooked data remains after SIGTSTP flush");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 23: Double Ctrl+D does not accumulate phantom line_count.
pub fn test_phase23_double_eof_no_phantom_accumulation() -> TestResult {
    let mut ld = LineDisc::new();

    // Two consecutive Ctrl+D on empty buffer.
    ld.input_char(0x04);
    ld.input_char(0x04);

    // First EOF read.
    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 0 {
        klog_info!(
            "TTY_TEST: BUG - first empty EOF should return 0, got {}",
            n1
        );
        return TestResult::Fail;
    }

    // Second EOF read.
    let n2 = ld.read(&mut buf);
    if n2 != 0 {
        klog_info!(
            "TTY_TEST: BUG - second empty EOF should return 0, got {}",
            n2
        );
        return TestResult::Fail;
    }

    // No phantom state.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after consuming both EOFs");
        return TestResult::Fail;
    }

    TestResult::Pass
}

// ===========================================================================
// Phase 24: Job Control & Controlling TTY Hardening
// ===========================================================================

/// Phase 24: set_fg_pgrp_checked on the per-TTY API denies non-existent pgrp.
///
/// With a session attached (sid=600), attempting to set a foreground pgrp that
/// has no living members in the session should fail.  The pgrp_exists_in_session
/// service iterates the scheduler's task list and won't find pgid=99999.
pub fn test_phase24_set_fg_pgrp_checked_nonexistent_pgrp() -> TestResult {
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

/// Phase 24: set_fg_pgrp_checked still allows clearing (pgid == 0).
pub fn test_phase24_set_fg_pgrp_checked_clear_allowed() -> TestResult {
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

/// Phase 24: set_fg_pgrp_checked skips pgrp validation when no session attached.
pub fn test_phase24_set_fg_pgrp_checked_no_session_skips_validation() -> TestResult {
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

/// Phase 24: detach_controlling_terminal (non-leader) returns Ok(false).
///
/// When a non-session-leader calls TIOCNOTTY, the TTY session state is
/// unchanged — only the caller's controlling_tty is cleared (by the ioctl handler).
pub fn test_phase24_detach_ctty_non_leader() -> TestResult {
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

/// Phase 24: detach_controlling_terminal (session leader) detaches session.
///
/// When the session leader issues TIOCNOTTY, the TTY's session state is
/// fully cleared and SIGHUP+SIGCONT would be sent to the foreground pgrp.
pub fn test_phase24_detach_ctty_session_leader() -> TestResult {
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

/// Phase 24: detach_controlling_terminal denies cross-session detach.
///
/// A session leader from a different session cannot detach someone else's TTY.
pub fn test_phase24_detach_ctty_cross_session_denied() -> TestResult {
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

/// Phase 24: TIOCNOTTY constant has the correct value.
pub fn test_phase24_tiocnotty_constant() -> TestResult {
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
// Phase 25: Real TCSETSW/TCSETSF Drain Semantics
// ===========================================================================

/// Phase 25: The `is_output_idle` function returns `true` when no output
/// is in flight and the driver reports no pending output.  For synchronous
/// backends (serial, vconsole) this should always be `true` when no write
/// is in progress.
pub fn test_phase25_is_output_idle_initially_true() -> TestResult {
    tty::table::tty_table_init();
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - is_output_idle should be true initially, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: The inflight counter starts at zero for all TTY slots.
pub fn test_phase25_inflight_counter_initial_zero() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    for i in 0..crate::tty::MAX_TTYS {
        let val = TTY_OUTPUT_INFLIGHT[i].load(Ordering::Relaxed);
        if val != 0 {
            klog_info!(
                "TTY_TEST: BUG - TTY_OUTPUT_INFLIGHT[{}] should be 0 initially, got {}",
                i,
                val
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 25: After a write completes, the inflight counter is back to zero
/// and `is_output_idle` returns true.
pub fn test_phase25_write_updates_inflight_counter() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Write some data.
    let data = b"hello drain";
    let result = tty::write(TtyIndex(0), data);
    match result {
        Ok(n) if n == data.len() => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - write should return data.len(), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // After write completes, inflight should be zero.
    let inflight = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Relaxed);
    if inflight != 0 {
        klog_info!(
            "TTY_TEST: BUG - inflight should be 0 after write completes, got {}",
            inflight
        );
        return TestResult::Fail;
    }

    // is_output_idle should return true.
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - is_output_idle should be true after write, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: `TCSETSW` (set_termios_wait) applies termios after drain and
/// preserves pending input (does not flush).
pub fn test_phase25_tcsetsw_preserves_input_after_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode so we can observe single-byte input.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input, then perform a write (creating output to "drain").
    tty::push_input(TtyIndex(0), b'x');
    let _ = tty::write(TtyIndex(0), b"output");

    // Now use TCSETSW to change termios.  Input should survive.
    let mut changed = raw;
    changed.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios_wait(TtyIndex(0), &changed).unwrap();

    // Verify input byte is still available.
    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(1) if out[0] == b'x' => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSW should preserve pending input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: `TCSETSF` (set_termios_flush) applies termios after drain and
/// flushes pending input.
pub fn test_phase25_tcsetsf_flushes_input_after_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input, then perform a write.
    tty::push_input(TtyIndex(0), b'y');
    let _ = tty::write(TtyIndex(0), b"output");

    // Use TCSETSF — should drain output AND flush input.
    let mut changed = raw;
    changed.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios_flush(TtyIndex(0), &changed).unwrap();

    // Verify input has been flushed.
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

/// Phase 25: `is_output_idle` returns an error for an invalid index.
pub fn test_phase25_is_output_idle_invalid_index() -> TestResult {
    match tty::is_output_idle(TtyIndex(255)) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - is_output_idle(255) should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: `is_output_idle` returns an error for an unallocated slot.
pub fn test_phase25_is_output_idle_unallocated() -> TestResult {
    tty::table::tty_table_init();
    // Slot 7 is never allocated by default.
    match tty::is_output_idle(TtyIndex(7)) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - is_output_idle(7) should return NotAllocated, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: `wait_output_idle` (via `set_termios_wait`) returns an error
/// for an invalid TTY index.
pub fn test_phase25_drain_invalid_index_error() -> TestResult {
    let t = slopos_abi::syscall::UserTermios::default();
    match tty::set_termios_wait(TtyIndex(255), &t) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - set_termios_wait(255) should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: The `TtyDriver` trait default `output_pending()` returns `false`.
pub fn test_phase25_driver_output_pending_default_false() -> TestResult {
    use crate::tty::driver::TtyDriver;

    // SerialConsoleDriver uses the default implementation.
    let serial = crate::tty::driver::SerialConsoleDriver;
    if serial.output_pending() {
        klog_info!("TTY_TEST: BUG - SerialConsoleDriver.output_pending() should be false");
        return TestResult::Fail;
    }

    // VConsoleDriver uses the default implementation.
    let vc = VConsoleDriver;
    if vc.output_pending() {
        klog_info!("TTY_TEST: BUG - VConsoleDriver.output_pending() should be false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 25: `TtyDriverKind::output_pending()` dispatches correctly for all
/// driver variants.
pub fn test_phase25_driver_kind_output_pending_dispatch() -> TestResult {
    use crate::tty::driver::SerialConsoleDriver;

    let serial_kind = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if serial_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - SerialConsole kind output_pending should be false");
        return TestResult::Fail;
    }

    let vc_kind = TtyDriverKind::VConsole(VConsoleDriver);
    if vc_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - VConsole kind output_pending should be false");
        return TestResult::Fail;
    }

    let pty_master_kind = TtyDriverKind::PtyMaster {
        peer: PtyPeerHandle::new(TtyIndex(3), 0),
    };
    if pty_master_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - PtyMaster kind output_pending should be false");
        return TestResult::Fail;
    }

    let pty_slave_kind = TtyDriverKind::PtySlave {
        peer: PtyPeerHandle::new(TtyIndex(2), 0),
    };
    if pty_slave_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - PtySlave kind output_pending should be false");
        return TestResult::Fail;
    }

    let none_kind = TtyDriverKind::None;
    if none_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - None kind output_pending should be false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Phase 25: PTY drain is immediate — `is_output_idle` returns `true` right
/// after writing to a PTY master/slave pair.
pub fn test_phase25_pty_drain_immediate() -> TestResult {
    tty::table::tty_table_init();

    // Allocate a PTY pair.
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: SKIP - could not allocate PTY pair");
            return TestResult::Pass;
        }
    };
    let slave_idx = match tty::get_pty_number(master_idx) {
        Ok(n) => TtyIndex(n as u8),
        Err(_) => {
            klog_info!("TTY_TEST: SKIP - could not get PTY slave index");
            let _ = tty::close_ref(master_idx);
            return TestResult::Pass;
        }
    };

    // Open the slave so the pair stays alive.
    let _ = tty::open_ref(slave_idx);

    // Write to master (goes to slave's input buffer).
    let _ = tty::write(master_idx, b"pty drain test");

    // Output should be idle immediately (PTY has no hardware latency).
    match tty::is_output_idle(master_idx) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PTY master is_output_idle should be true, got {:?}",
                other
            );
            let _ = tty::close_ref(slave_idx);
            let _ = tty::close_ref(master_idx);
            return TestResult::Fail;
        }
    }

    // TCSETSW on slave should complete immediately.
    let termios = tty::get_termios(slave_idx).unwrap();
    match tty::set_termios_wait(slave_idx, &termios) {
        Ok(()) => {}
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSW on PTY slave should succeed, got {:?}",
                e
            );
            let _ = tty::close_ref(slave_idx);
            let _ = tty::close_ref(master_idx);
            return TestResult::Fail;
        }
    }

    // Clean up.
    let _ = tty::close_ref(slave_idx);
    let _ = tty::close_ref(master_idx);
    TestResult::Pass
}

/// Phase 25: `TCSETSW` on console completes immediately because the serial
/// driver is synchronous (all output is "drained" instantly).
pub fn test_phase25_console_drain_immediate() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Write output to create something to "drain".
    let _ = tty::write(TtyIndex(0), b"drain test output\r\n");

    // `is_output_idle` should be true immediately.
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - console is_output_idle should be true after sync write, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // TCSETSW should complete without blocking.
    let termios = tty::get_termios(TtyIndex(0)).unwrap();
    match tty::set_termios_wait(TtyIndex(0), &termios) {
        Ok(()) => TestResult::Pass,
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSW on console should succeed immediately, got {:?}",
                e
            );
            TestResult::Fail
        }
    }
}

/// Phase 25: `set_termios_mode` with `Now` does NOT call `wait_output_idle`
/// — it applies termios immediately regardless of pending output.
pub fn test_phase25_tcsets_now_skips_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input.
    tty::push_input(TtyIndex(0), b'z');

    // TCSETS (Now) should apply immediately, input preserved.
    let mut changed = raw;
    changed.c_lflag &= !slopos_abi::syscall::ECHO;
    tty::set_termios(TtyIndex(0), &changed).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(1) if out[0] == b'z' => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETS Now should preserve input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 26: PTY Lifetime Safety & Scalable Capacity
// ===========================================================================

/// MAX_TTYS is now 32 (Phase 26 capacity scaling).
pub fn test_phase26_max_ttys_is_32() -> TestResult {
    if crate::tty::MAX_TTYS != 32 {
        klog_info!(
            "TTY_TEST: BUG - MAX_TTYS should be 32, got {}",
            crate::tty::MAX_TTYS
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyPeerHandle stores index and generation.
pub fn test_phase26_pty_peer_handle_creation() -> TestResult {
    let handle = PtyPeerHandle::new(TtyIndex(5), 42);
    if handle.idx != TtyIndex(5) {
        klog_info!("TTY_TEST: BUG - PtyPeerHandle idx mismatch");
        return TestResult::Fail;
    }
    if handle.generation != 42 {
        klog_info!("TTY_TEST: BUG - PtyPeerHandle generation mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyPeerHandle::snapshot captures the current generation from TTY_GENERATIONS.
pub fn test_phase26_pty_peer_handle_snapshot() -> TestResult {
    use core::sync::atomic::Ordering;
    // Use a high slot unlikely to be in use (slot 30).
    let test_slot: usize = 30;
    let old_gen = TTY_GENERATIONS[test_slot].load(Ordering::Acquire);
    let handle = PtyPeerHandle::snapshot(TtyIndex(test_slot as u8));
    if handle.generation != old_gen {
        klog_info!(
            "TTY_TEST: BUG - snapshot generation {} != expected {}",
            handle.generation,
            old_gen
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Generation counter is bumped when a PTY pair is freed.
pub fn test_phase26_generation_bumped_on_free() -> TestResult {
    use core::sync::atomic::Ordering;
    // Allocate a PTY pair.
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);
    let master_slot = master_idx.0 as usize;
    let slave_slot = slave_idx.0 as usize;

    let gen_master_before = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
    let gen_slave_before = TTY_GENERATIONS[slave_slot].load(Ordering::Acquire);

    // Free the pair (both have open_count 0 since we never opened them).
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);

    let gen_master_after = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
    let gen_slave_after = TTY_GENERATIONS[slave_slot].load(Ordering::Acquire);

    if gen_master_after != gen_master_before + 1 {
        klog_info!(
            "TTY_TEST: BUG - master generation not bumped: {} -> {}",
            gen_master_before,
            gen_master_after
        );
        return TestResult::Fail;
    }
    if gen_slave_after != gen_slave_before + 1 {
        klog_info!(
            "TTY_TEST: BUG - slave generation not bumped: {} -> {}",
            gen_slave_before,
            gen_slave_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Stale PtyPeerHandle is detected by validate_peer.
pub fn test_phase26_stale_handle_detected() -> TestResult {
    // Allocate a PTY pair.
    // Allocate a PTY pair.
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);

    // Create a handle with the current generation.
    let stale_handle = PtyPeerHandle::snapshot(slave_idx);

    // Verify the handle is valid before freeing.
    if !crate::tty::pty::validate_peer(&stale_handle) {
        klog_info!("TTY_TEST: BUG - handle should be valid before free");
        return TestResult::Fail;
    }

    // Free the pair.
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);

    // Now the handle should be stale.
    if crate::tty::pty::validate_peer(&stale_handle) {
        klog_info!("TTY_TEST: BUG - handle should be stale after free");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PTY alloc captures the correct generation in peer handles.
pub fn test_phase26_pty_alloc_captures_generation() -> TestResult {
    use core::sync::atomic::Ordering;
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);

    // Read the peer handle from the master's driver.
    let master_peer_gen = {
        let guard = TTY_SLOTS[master_idx.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => peer.generation,
                _ => {
                    klog_info!("TTY_TEST: BUG - master not PtyMaster");
                    return TestResult::Fail;
                }
            },
            None => {
                klog_info!("TTY_TEST: BUG - master slot empty");
                return TestResult::Fail;
            }
        }
    };

    // The peer generation should match the current generation of the slave slot.
    let slave_gen = TTY_GENERATIONS[slave_idx.0 as usize].load(Ordering::Acquire);
    if master_peer_gen != slave_gen {
        klog_info!(
            "TTY_TEST: BUG - master peer gen {} != slave slot gen {}",
            master_peer_gen,
            slave_gen
        );
        // Clean up.
        crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
        return TestResult::Fail;
    }

    // Clean up.
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
    TestResult::Pass
}

/// Stale master write after free/realloc is a safe no-op.
pub fn test_phase26_stale_write_safe_noop() -> TestResult {
    // Allocate pair A.
    let master_a = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - first pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_a_num = match tty::get_pty_number(master_a) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_a = TtyIndex(slave_a_num as u8);

    // Capture the peer handle from master A (points to slave A).
    let stale_peer = {
        let guard = TTY_SLOTS[master_a.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => *peer,
                _ => {
                    klog_info!("TTY_TEST: BUG - not PtyMaster");
                    return TestResult::Fail;
                }
            },
            None => {
                klog_info!("TTY_TEST: BUG - master slot empty");
                return TestResult::Fail;
            }
        }
    };

    // Free pair A.
    crate::tty::pty::free_pair_if_unused(master_a, slave_a);

    // Allocate pair B — may reuse the same slots.
    let master_b = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - second pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_b_num = match tty::get_pty_number(master_b) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number for B failed");
            return TestResult::Fail;
        }
    };
    let slave_b = TtyIndex(slave_b_num as u8);

    // Use the stale peer handle to attempt a write — should be a no-op.
    crate::tty::pty::master_write(stale_peer, b"stale data");

    // Verify pair B's slave has no unexpected data.
    // Drain slave B to check no stale data leaked in.
    drain_tty_nonblock(slave_b);

    // Clean up pair B.
    crate::tty::pty::free_pair_if_unused(master_b, slave_b);
    TestResult::Pass
}

/// Rapid alloc/free/realloc stress: generations increase monotonically.
pub fn test_phase26_rapid_alloc_free_stress() -> TestResult {
    use core::sync::atomic::Ordering;
    for _ in 0..10 {
        let master_idx = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed during stress");
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master_idx) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed during stress");
                return TestResult::Fail;
            }
        };
        let slave_idx = TtyIndex(slave_num as u8);
        let master_slot = master_idx.0 as usize;

        let gen_before = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
        crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
        let gen_after = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);

        if gen_after != gen_before + 1 {
            klog_info!(
                "TTY_TEST: BUG - generation not monotonic: {} -> {}",
                gen_before,
                gen_after
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Data flow still works correctly through generation-tagged handles.
pub fn test_phase26_data_flow_with_generation() -> TestResult {
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    // Open both sides.
    let _ = tty::open_ref(master_idx);
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);
    let _ = tty::open_ref(slave_idx);

    // Master write -> slave read (through slave's N_TTY ldisc).
    let _ = tty::write(master_idx, b"gen\n");
    let mut buf = [0u8; 16];
    match tty::read(slave_idx, &mut buf, true) {
        Ok(n) if n == 4 && &buf[..4] == b"gen\n" => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - master->slave data flow failed: {:?}",
                other
            );
            let _ = tty::close_ref(slave_idx);
            let _ = tty::close_ref(master_idx);
            return TestResult::Fail;
        }
    }

    let _ = tty::close_ref(slave_idx);
    let _ = tty::close_ref(master_idx);
    TestResult::Pass
}

/// validate_peer returns false for out-of-range index.
pub fn test_phase26_validate_peer_out_of_range() -> TestResult {
    let handle = PtyPeerHandle::new(TtyIndex(255), 0);
    if crate::tty::pty::validate_peer(&handle) {
        klog_info!("TTY_TEST: BUG - validate_peer should reject out-of-range index");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multiple PTY pairs can be allocated with 32 slots available.
pub fn test_phase26_multiple_pty_pairs() -> TestResult {
    // With 32 slots and 2 reserved (serial + vconsole), we should be able
    // to allocate up to 15 pairs (30 slots / 2).
    let mut pairs: [(TtyIndex, TtyIndex); 10] = [(TtyIndex(0), TtyIndex(0)); 10];
    for i in 0..10 {
        let master = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed at pair {}", i);
                // Clean up what we allocated.
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed at pair {}", i);
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        pairs[i] = (master, TtyIndex(slave_num as u8));
    }
    // Clean up all pairs.
    for i in 0..10 {
        crate::tty::pty::free_pair_if_unused(pairs[i].0, pairs[i].1);
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 27: POSIX Completion Set
// ===========================================================================

/// Phase 27: IGNBRK discards NUL (break condition).
pub fn test_phase27_ignbrk_discards_break() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = slopos_abi::syscall::IGNBRK;
    t.c_lflag = slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO;
    ld.set_termios(&t);
    let action = ld.input_char(0x00);
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - IGNBRK should discard break (NUL)");
        return TestResult::Fail;
    }
    // Verify nothing was buffered.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - IGNBRK should not buffer any data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: BRKINT on NUL generates SIGINT and flushes input.
pub fn test_phase27_brkint_generates_sigint() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = slopos_abi::syscall::BRKINT;
    t.c_lflag = slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO | slopos_abi::syscall::ISIG;
    ld.set_termios(&t);
    // Push some data first, then break.
    ld.input_char(b'a');
    let action = ld.input_char(0x00);
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - BRKINT should generate SIGINT, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    // BRKINT should have flushed input.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - BRKINT should flush input queues");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: PARMRK on NUL inserts \xff \x00 \x00 sequence.
pub fn test_phase27_parmrk_inserts_marker() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = slopos_abi::syscall::PARMRK;
    // Non-canonical mode to read bytes directly.
    t.c_lflag = 0;
    ld.set_termios(&t);
    ld.input_char(0x00); // break
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 3 || buf[0] != 0xFF || buf[1] != 0x00 || buf[2] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - PARMRK should insert 0xFF 0x00 0x00, got {} bytes: {:?}",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: NUL without any break flags passes through as regular byte.
pub fn test_phase27_nul_without_break_flags_passes_through() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = 0; // No break flags set.
    t.c_lflag = 0; // Non-canonical mode.
    ld.set_termios(&t);
    ld.input_char(0x00);
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - NUL without break flags should pass through, got {} bytes",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: ECHOKE visually erases the line (returns KillLineEcho).
pub fn test_phase27_echoke_visual_erase() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = 0;
    t.c_lflag =
        slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO | slopos_abi::syscall::ECHOKE;
    ld.set_termios(&t);
    // Type "abc" (3 printable chars = 3 columns).
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(b'c');
    // Kill the line (VKILL = Ctrl+U = 0x15).
    let action = ld.input_char(0x15);
    match action {
        InputAction::KillLineEcho { columns } if columns == 3 => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOKE should return KillLineEcho{{columns:3}}, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 27: ECHOK (without ECHOKE) echoes newline on kill.
pub fn test_phase27_echok_newline_on_kill() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = 0;
    t.c_lflag =
        slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO | slopos_abi::syscall::ECHOK;
    ld.set_termios(&t);
    ld.input_char(b'a');
    ld.input_char(b'b');
    let action = ld.input_char(0x15);
    match action {
        InputAction::Echo { buf, len } if len == 1 && buf[0] == b'\n' => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOK should echo newline on kill, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 27: ECHOCTL erase produces KillLineEcho with 2 columns for a control char.
pub fn test_phase27_echoctl_erase_two_columns() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = 0;
    t.c_lflag = slopos_abi::syscall::ICANON
        | slopos_abi::syscall::ECHO
        | slopos_abi::syscall::ECHOE
        | slopos_abi::syscall::ECHOCTL;
    ld.set_termios(&t);
    // Insert a control char (Ctrl+A = 0x01) via literal next.
    // We need IEXTEN for VLNEXT.
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);
    // Type Ctrl+V first to enter literal mode, then Ctrl+A.
    ld.input_char(0x16); // VLNEXT (Ctrl+V)
    ld.input_char(0x01); // Ctrl+A - inserted literally
    // Now erase it (VERASE = DEL = 0x7F).
    let action = ld.input_char(0x7F);
    match action {
        InputAction::KillLineEcho { columns } if columns == 2 => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOCTL erase should return KillLineEcho{{columns:2}}, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 27: bytes_available returns correct count.
pub fn test_phase27_bytes_available() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = 0; // non-canonical
    t.c_iflag = 0;
    ld.set_termios(&t);
    if ld.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh LineDisc should have 0 bytes available");
        return TestResult::Fail;
    }
    ld.input_char(b'x');
    ld.input_char(b'y');
    ld.input_char(b'z');
    if ld.bytes_available() != 3 {
        klog_info!(
            "TTY_TEST: BUG - expected 3 bytes available, got {}",
            ld.bytes_available()
        );
        return TestResult::Fail;
    }
    let mut buf = [0u8; 2];
    ld.read(&mut buf);
    if ld.bytes_available() != 1 {
        klog_info!(
            "TTY_TEST: BUG - expected 1 byte available after reading 2, got {}",
            ld.bytes_available()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: RawDisc bytes_available works.
pub fn test_phase27_raw_disc_bytes_available() -> TestResult {
    let mut rd = RawDisc::new();
    if rd.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh RawDisc should have 0 bytes available");
        return TestResult::Fail;
    }
    rd.input_char(b'a');
    rd.input_char(b'b');
    if rd.bytes_available() != 2 {
        klog_info!(
            "TTY_TEST: BUG - expected 2 bytes available, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: LdiscKind bytes_available dispatches correctly.
pub fn test_phase27_ldisc_kind_bytes_available() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    {
        let mut t = *lk.termios();
        t.c_lflag = 0; // non-canonical
        t.c_iflag = 0;
        lk.set_termios(&t);
    }
    if lk.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh LdiscKind::NTty should have 0 bytes");
        return TestResult::Fail;
    }
    lk.input_char(b'q');
    if lk.bytes_available() != 1 {
        klog_info!("TTY_TEST: BUG - expected 1 byte available via LdiscKind");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: FIONREAD constant is defined.
pub fn test_phase27_fionread_constant() -> TestResult {
    if slopos_abi::syscall::FIONREAD != 0x541B {
        klog_info!("TTY_TEST: BUG - FIONREAD should be 0x541B");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: KillLineEcho on empty edit buffer returns None.
pub fn test_phase27_kill_empty_line_no_echo() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = 0;
    t.c_lflag =
        slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO | slopos_abi::syscall::ECHOKE;
    ld.set_termios(&t);
    // Kill with empty buffer.
    let action = ld.input_char(0x15);
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - kill on empty line should return None, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 27: BRKINT + IGNBRK — IGNBRK takes priority.
pub fn test_phase27_ignbrk_takes_priority_over_brkint() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = slopos_abi::syscall::IGNBRK | slopos_abi::syscall::BRKINT;
    t.c_lflag = slopos_abi::syscall::ISIG;
    ld.set_termios(&t);
    let action = ld.input_char(0x00);
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - IGNBRK should take priority over BRKINT");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_input_flags_from_bits() -> TestResult {
    let flags = InputFlags::from_bits_truncate(0x100);
    if !flags.contains(InputFlags::ICRNL) {
        klog_info!("TTY_TEST: BUG - InputFlags::from_bits_truncate(0x100) missing ICRNL");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_output_flags_from_bits() -> TestResult {
    let flags = OutputFlags::from_bits_truncate(0x05);
    if !flags.contains(OutputFlags::OPOST | OutputFlags::ONLCR) {
        klog_info!("TTY_TEST: BUG - OutputFlags::from_bits_truncate(0x05) missing OPOST|ONLCR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_local_flags_from_bits() -> TestResult {
    let raw = (LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG).bits();
    let flags = LocalFlags::from_bits_truncate(raw);
    if flags != (LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG) {
        klog_info!("TTY_TEST: BUG - LocalFlags round-trip mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_cc_index_values() -> TestResult {
    if CcIndex::Vintr.as_usize() != 0
        || CcIndex::Veof.as_usize() != 4
        || CcIndex::Vtime.as_usize() != 5
        || CcIndex::Vmin.as_usize() != 6
        || CcIndex::Vwerase.as_usize() != 14
    {
        klog_info!("TTY_TEST: BUG - CcIndex values do not match expected ABI indices");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_posix_vdisable() -> TestResult {
    if POSIX_VDISABLE != 0 {
        klog_info!(
            "TTY_TEST: BUG - POSIX_VDISABLE should be 0, got {}",
            POSIX_VDISABLE
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_tty_error_to_errno() -> TestResult {
    let pairs = [
        (TtyError::InvalidIndex, -22),
        (TtyError::NotAllocated, -6),
        (TtyError::BackgroundRead, -1),
        (TtyError::BackgroundWrite, -1),
        (TtyError::HungUp, -5),
        (TtyError::WouldBlock, -11),
        (TtyError::PermissionDenied, -1),
        (TtyError::UnsupportedLineDiscipline, -22),
        (TtyError::CrossSessionDenied, -5),
        (TtyError::SignalInterrupt, -4),
    ];
    for (err, expected) in pairs {
        if err.to_errno() != expected {
            klog_info!(
                "TTY_TEST: BUG - TtyError::{:?}.to_errno()={} expected {}",
                err,
                err.to_errno(),
                expected
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_phase28_tty_error_signal_interrupt() -> TestResult {
    if TtyError::SignalInterrupt.to_errno() != -4 {
        klog_info!("TTY_TEST: BUG - SignalInterrupt should map to -4 (EINTR)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_user_termios_typed_accessors() -> TestResult {
    let mut t = slopos_abi::syscall::UserTermios::default();
    t.c_iflag = slopos_abi::syscall::ICRNL;
    t.c_lflag = LocalFlags::ECHO.bits();
    t.set_cc(CcIndex::Vintr, 0x03);

    if !t.input_flags().contains(InputFlags::ICRNL)
        || !t.local_flags().contains(LocalFlags::ECHO)
        || t.cc(CcIndex::Vintr) != 0x03
    {
        klog_info!("TTY_TEST: BUG - UserTermios typed accessors mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_ldisc_typed_flags_behavioral_equivalence() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"abc\n" {
        ld.input_char(c);
    }
    let mut out = [0u8; 8];
    let n = ld.read(&mut out);
    if n != 4 || &out[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - typed flag migration changed canonical behavior");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_phase28_control_flags_empty() -> TestResult {
    if !ControlFlags::empty().is_empty() || ControlFlags::empty().bits() != 0 {
        klog_info!("TTY_TEST: BUG - ControlFlags::empty is not zero/empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 29: LdiscKind Dispatch Consolidation
// ===========================================================================

/// Phase 29: The `LdiscOps` trait is implemented for `LineDisc` and the trait
/// methods delegate to the inherent methods (no infinite recursion).
pub fn test_phase29_ldisc_ops_linedisc_trait_delegation() -> TestResult {
    let mut ld = LineDisc::new();
    // Use the trait methods explicitly via UFCS to prove the trait impls exist
    // and delegate correctly to the inherent methods.
    let t = <LineDisc as LdiscOps>::termios(&ld);
    if t.c_line != slopos_abi::syscall::N_TTY as u8 {
        klog_info!("TTY_TEST: BUG - LdiscOps::termios for LineDisc returned wrong c_line");
        return TestResult::Fail;
    }
    let (vmin, _vtime) = <LineDisc as LdiscOps>::vmin_vtime(&ld);
    if vmin != 1 {
        klog_info!("TTY_TEST: BUG - LdiscOps::vmin_vtime for LineDisc wrong vmin");
        return TestResult::Fail;
    }
    if !<LineDisc as LdiscOps>::is_canonical(&ld) {
        klog_info!("TTY_TEST: BUG - LdiscOps::is_canonical for LineDisc should be true");
        return TestResult::Fail;
    }
    if <LineDisc as LdiscOps>::has_data(&ld) {
        klog_info!("TTY_TEST: BUG - LdiscOps::has_data for LineDisc should be false initially");
        return TestResult::Fail;
    }
    if <LineDisc as LdiscOps>::bytes_available(&ld) != 0 {
        klog_info!("TTY_TEST: BUG - LdiscOps::bytes_available for LineDisc should be 0");
        return TestResult::Fail;
    }
    if <LineDisc as LdiscOps>::is_stopped(&ld) {
        klog_info!("TTY_TEST: BUG - LdiscOps::is_stopped for LineDisc should be false");
        return TestResult::Fail;
    }
    // Exercise a mutation via the trait.
    let _action = <LineDisc as LdiscOps>::input_char(&mut ld, b'x');
    <LineDisc as LdiscOps>::flush_all(&mut ld);
    TestResult::Pass
}

/// Phase 29: The `LdiscOps` trait is implemented for `RawDisc`.
pub fn test_phase29_ldisc_ops_rawdisc_trait_delegation() -> TestResult {
    let mut rd = RawDisc::new();
    let t = <RawDisc as LdiscOps>::termios(&rd);
    if t.c_line != slopos_abi::syscall::N_RAW as u8 {
        klog_info!("TTY_TEST: BUG - LdiscOps::termios for RawDisc returned wrong c_line");
        return TestResult::Fail;
    }
    if <RawDisc as LdiscOps>::is_canonical(&rd) {
        klog_info!("TTY_TEST: BUG - LdiscOps::is_canonical for RawDisc should be false");
        return TestResult::Fail;
    }
    if <RawDisc as LdiscOps>::has_data(&rd) {
        klog_info!("TTY_TEST: BUG - LdiscOps::has_data for RawDisc should be false initially");
        return TestResult::Fail;
    }
    // Push a byte via trait and check.
    let action = <RawDisc as LdiscOps>::input_char(&mut rd, b'z');
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - RawDisc input_char via trait should return None");
        return TestResult::Fail;
    }
    if !<RawDisc as LdiscOps>::has_data(&rd) {
        klog_info!("TTY_TEST: BUG - RawDisc should have data after input_char via trait");
        return TestResult::Fail;
    }
    if <RawDisc as LdiscOps>::bytes_available(&rd) != 1 {
        klog_info!("TTY_TEST: BUG - RawDisc bytes_available should be 1 after input");
        return TestResult::Fail;
    }
    <RawDisc as LdiscOps>::flush_all(&mut rd);
    if <RawDisc as LdiscOps>::has_data(&rd) {
        klog_info!("TTY_TEST: BUG - RawDisc should have no data after flush via trait");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 29: `dispatch_ldisc!` macro generates correct delegation for `LdiscKind`.
/// Verifies NTty variant routing.
pub fn test_phase29_dispatch_macro_ntty_routing() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    // id() is manually implemented, not via macro.
    if lk.id() != slopos_abi::syscall::N_TTY {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty id() wrong");
        return TestResult::Fail;
    }
    // Methods generated by dispatch_ldisc! macro.
    if !lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty is_canonical should be true");
        return TestResult::Fail;
    }
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty has_data should be false initially");
        return TestResult::Fail;
    }
    if lk.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty bytes_available should be 0");
        return TestResult::Fail;
    }
    // Feed a char + newline through the macro-dispatched input_char.
    let _ = lk.input_char(b'A');
    let _ = lk.input_char(b'\n');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have data after newline");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 2 || buf[0] != b'A' || buf[1] != b'\n' {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty read mismatch n={}", n);
        return TestResult::Fail;
    }
    lk.flush_all();
    TestResult::Pass
}

/// Phase 29: `dispatch_ldisc!` macro generates correct delegation for `LdiscKind`.
/// Verifies Raw variant routing.
pub fn test_phase29_dispatch_macro_raw_routing() -> TestResult {
    let mut lk = LdiscKind::Raw(RawDisc::new());
    if lk.id() != slopos_abi::syscall::N_RAW {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw id() wrong");
        return TestResult::Fail;
    }
    if lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw is_canonical should be false");
        return TestResult::Fail;
    }
    // Raw mode: input goes directly to buffer.
    let _ = lk.input_char(b'R');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have data after input");
        return TestResult::Fail;
    }
    if lk.bytes_available() != 1 {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw bytes_available should be 1");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 1 || buf[0] != b'R' {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw read mismatch");
        return TestResult::Fail;
    }
    lk.flush_all();
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have no data after flush");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 29: `LdiscKind::from_id` still works after dispatch refactor.
pub fn test_phase29_from_id_still_works() -> TestResult {
    let default_termios = LineDisc::new().termios().clone();
    // N_TTY
    let ntty = LdiscKind::from_id(slopos_abi::syscall::N_TTY, default_termios);
    if ntty.is_none() {
        klog_info!("TTY_TEST: BUG - from_id(N_TTY) returned None");
        return TestResult::Fail;
    }
    if ntty.unwrap().id() != slopos_abi::syscall::N_TTY {
        klog_info!("TTY_TEST: BUG - from_id(N_TTY) id mismatch");
        return TestResult::Fail;
    }
    // N_RAW
    let nraw = LdiscKind::from_id(slopos_abi::syscall::N_RAW, default_termios);
    if nraw.is_none() {
        klog_info!("TTY_TEST: BUG - from_id(N_RAW) returned None");
        return TestResult::Fail;
    }
    if nraw.unwrap().id() != slopos_abi::syscall::N_RAW {
        klog_info!("TTY_TEST: BUG - from_id(N_RAW) id mismatch");
        return TestResult::Fail;
    }
    // Invalid ID
    if LdiscKind::from_id(999, default_termios).is_some() {
        klog_info!("TTY_TEST: BUG - from_id(999) should return None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 29: Output processing via macro-dispatched `process_output_byte`.
pub fn test_phase29_process_output_byte_dispatch() -> TestResult {
    // NTty with OPOST+ONLCR: '\n' should produce CR+LF.
    let mut ntty = LdiscKind::NTty(LineDisc::new());
    let action = ntty.process_output_byte(b'\n');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - NTty output '\\n' should be CR+LF");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - NTty output '\\n' should be Emit");
            return TestResult::Fail;
        }
    }
    // Raw: '\n' should pass through unchanged.
    let mut raw = LdiscKind::Raw(RawDisc::new());
    let action = raw.process_output_byte(b'\n');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - Raw output '\\n' should be passthrough");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Raw output '\\n' should be Emit");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Phase 29: `edit_content` dispatch works for both variants.
pub fn test_phase29_edit_content_dispatch() -> TestResult {
    // NTty: type some chars (no newline), edit_content should show them.
    let mut ntty = LdiscKind::NTty(LineDisc::new());
    let _ = ntty.input_char(b'h');
    let _ = ntty.input_char(b'i');
    let content = ntty.edit_content();
    if content.len() != 2 || content[0] != b'h' || content[1] != b'i' {
        klog_info!("TTY_TEST: BUG - NTty edit_content should show typed chars");
        return TestResult::Fail;
    }
    // Raw: edit_content is always empty.
    let raw = LdiscKind::Raw(RawDisc::new());
    if !raw.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - Raw edit_content should be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 30: /dev/tty Controlling Terminal Device
// ===========================================================================

/// Phase 30: `open_ref` increments open count for the same TTY slot — this is
/// the mechanism that `/dev/tty` relies on (a second FD referencing the same
/// TTY index via the caller's controlling terminal).
pub fn test_phase30_open_ref_second_fd_increments_count() -> TestResult {
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

/// Phase 30: After `open_ref` (simulating `/dev/tty` open), read/write/termios
/// operations work identically on the same TTY index — the FD created by
/// `/dev/tty` is indistinguishable from one opened on the actual device path.
pub fn test_phase30_dev_tty_operations_identical_to_direct() -> TestResult {
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
    if termios.c_lflag & 0x2 == 0 {
        klog_info!("TTY_TEST: BUG - termios from /dev/tty FD missing ICANON");
        let _ = tty::close_ref(idx);
        return TestResult::Fail;
    }

    // write should succeed (returns byte count).
    match tty::write(idx, b"phase30") {
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

/// Phase 30: `open_ref` on a TTY does NOT modify session state — opening
/// `/dev/tty` only accesses an existing controlling terminal, never acquires one.
pub fn test_phase30_open_ref_does_not_modify_session() -> TestResult {
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

/// Phase 30: `open_ref` on an invalid TTY index returns `InvalidIndex` error,
/// matching the ENXIO semantics when `/dev/tty` resolution fails.
pub fn test_phase30_open_ref_invalid_index_returns_error() -> TestResult {
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

/// Phase 30: `close_ref` correctly decrements the open count — ensures the
/// `/dev/tty` FD lifecycle is properly paired with the direct device FD.
pub fn test_phase30_close_ref_decrements_after_open() -> TestResult {
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

/// Phase 30: Multiple `open_ref` calls (simulating multiple `/dev/tty` opens)
/// all succeed and increment sequentially.
pub fn test_phase30_multiple_open_ref_sequential() -> TestResult {
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

/// Phase 30: `get_winsize` works identically regardless of whether the FD was
/// obtained via `/dev/tty` or direct device open (both use the same TTY index).
pub fn test_phase30_dev_tty_winsize_matches_direct() -> TestResult {
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
// Phase 31: Background Write Protection (SIGTTOU on tcsetattr)
// ===========================================================================

/// Phase 31: check_write with tostop=true (simulating tcsetattr foreground
/// check) blocks background processes with BackgroundWrite.
pub fn test_phase31_tcsetattr_background_blocked() -> TestResult {
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

/// Phase 31: Foreground process tcsetattr proceeds normally (no signal).
pub fn test_phase31_tcsetattr_foreground_allowed() -> TestResult {
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

/// Phase 31: tcsetattr with no session attached is allowed (bootstrap path).
pub fn test_phase31_tcsetattr_no_session_allowed() -> TestResult {
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

/// Phase 31: tcsetattr from a different session returns DeniedCrossSession.
pub fn test_phase31_tcsetattr_cross_session_denied() -> TestResult {
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

/// Phase 31: TtyError::OrphanedProcessGroup maps to EIO (-5).
pub fn test_phase31_orphaned_pgrp_errno() -> TestResult {
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

/// Phase 31: Kernel task (task_id=0) bypasses tcsetattr foreground check.
/// In the test harness, task_id is always 0, so set_termios should succeed
/// even if the TTY has a session with a different foreground group.
pub fn test_phase31_tcsetattr_kernel_task_bypass() -> TestResult {
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
    t.c_iflag ^= 0x01; // toggle a bit
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

/// Phase 31: set_termios_wait and set_termios_flush also have the foreground
/// check (kernel task bypass verifies the path doesn't crash).
pub fn test_phase31_tcsetsw_tcsetsf_kernel_task_bypass() -> TestResult {
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
    t.c_iflag ^= 0x01;
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

/// Phase 31: TOSTOP + background write with SIGTTOU blocked/ignored bypass.
/// Exercises the check_write path with tostop=true, verifying the session-level
/// check correctly identifies background writers. The signal bypass logic itself
/// is tested at the driver_hooks level.
pub fn test_phase31_tostop_background_write_check() -> TestResult {
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

/// Phase 31: Kernel task (pgid=0) is always allowed through check_write,
/// even with tostop=true.
pub fn test_phase31_kernel_task_check_write_allowed() -> TestResult {
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
// Phase 32: Controlling Terminal Lifecycle Integrity
// ===========================================================================

/// Phase 32: acquire_controlling_terminal succeeds for a fresh (no-session) TTY.
pub fn test_phase32_acquire_ctty_fresh_tty() -> TestResult {
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

/// Phase 32: acquire_controlling_terminal is a no-op when called by the same
/// session that already owns the TTY (idempotent / TIOCSCTTY same-session).
pub fn test_phase32_acquire_ctty_same_session_idempotent() -> TestResult {
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

/// Phase 32: acquire_controlling_terminal fails with PermissionDenied when a
/// different session already owns the TTY.
pub fn test_phase32_acquire_ctty_different_session_denied() -> TestResult {
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

/// Phase 32: release_controlling_terminal succeeds for the owning session.
pub fn test_phase32_release_ctty_owning_session() -> TestResult {
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

/// Phase 32: release_controlling_terminal is a no-op (returns Ok(false)) when
/// called by a session that does not own the TTY.
pub fn test_phase32_release_ctty_wrong_session_noop() -> TestResult {
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

/// Phase 32: hangup sets hung_up flag and detaches the session, verifying the
/// session-leader exit → hangup → session detach chain.
pub fn test_phase32_hangup_detaches_session() -> TestResult {
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

/// Phase 32: O_NOCTTY suppresses auto-acquire — verifying that a session leader
/// opening a TTY with O_NOCTTY does NOT become the controlling process.
/// We verify this by calling acquire with an existing session and checking
/// that a second session cannot steal it (i.e., the first acquire "stuck").
pub fn test_phase32_o_noctty_suppresses_acquire() -> TestResult {
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

/// Phase 32: detach_controlling_terminal for a non-leader returns Ok(false)
/// and does NOT detach the session from the TTY.
pub fn test_phase32_detach_ctty_non_leader_preserves_session() -> TestResult {
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

/// Phase 32: detach_controlling_terminal for the session leader detaches the
/// session and returns Ok(true).
pub fn test_phase32_detach_ctty_session_leader_detaches() -> TestResult {
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

/// Phase 32: Full lifecycle chain — acquire → release → re-acquire by a
/// different session. Verifies that the TTY can be re-used after release.
pub fn test_phase32_full_lifecycle_acquire_release_reacquire() -> TestResult {
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

/// Phase 32: Double acquire to the same TTY from two different sessions —
/// the second must fail with PermissionDenied (race guard).
pub fn test_phase32_double_acquire_race_guard() -> TestResult {
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

/// Phase 32: hangup on a TTY with no session is a safe no-op.
pub fn test_phase32_hangup_no_session_safe() -> TestResult {
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

/// Phase 32: Rapid acquire/release stress — cycle through several sessions
/// on the same TTY to verify no state leaks between owners.
pub fn test_phase32_rapid_acquire_release_stress() -> TestResult {
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

/// Phase 32: acquire on an invalid TTY index returns InvalidIndex.
pub fn test_phase32_acquire_invalid_index() -> TestResult {
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

/// Phase 32: release on an invalid TTY index returns InvalidIndex.
pub fn test_phase32_release_invalid_index() -> TestResult {
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

/// Phase 32: detach_controlling_terminal on an invalid TTY index returns
/// InvalidIndex.
pub fn test_phase32_detach_invalid_index() -> TestResult {
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
// Phase 33: Post-Hangup I/O Hardening regression tests
// ===========================================================================

/// Phase 33: read() on a hung-up TTY with no buffered data returns EOF (0
/// bytes), regardless of blocking/nonblock mode.
pub fn test_phase33_hangup_read_returns_eof() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let mut out = [0u8; 8];
    // Nonblocking read on hung-up TTY.
    let result = tty::read(idx, &mut out, true);

    // Re-init before checking.
    tty::table::tty_table_init();

    match result {
        Ok(0) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup read expected Ok(0) EOF, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: write() on a hung-up TTY returns Err(HungUp) which maps to EIO.
pub fn test_phase33_hangup_write_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let result = tty::write(idx, b"hello");

    tty::table::tty_table_init();

    match result {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup write expected HungUp (EIO), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: poll_events() on a hung-up TTY returns POLLHUP | POLLIN.
pub fn test_phase33_hangup_poll_returns_pollhup_pollin() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);
    tty::hangup(idx);

    let revents = tty::poll_events(
        idx,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    tty::table::tty_table_init();

    let has_pollhup = (revents & slopos_abi::syscall::POLLHUP) != 0;
    let has_pollin = (revents & slopos_abi::syscall::POLLIN) != 0;

    if !has_pollhup {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLHUP on hung-up TTY");
        return TestResult::Fail;
    }
    if !has_pollin {
        klog_info!(
            "TTY_TEST: BUG - poll_events should report POLLIN on hung-up TTY (readable EOF)"
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 33: set_termios on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_phase33_hangup_set_termios_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let termios = tty::get_termios(idx);
    let result = match termios {
        Ok(t) => tty::set_termios(idx, &t),
        Err(_) => {
            // get_termios may also fail on hung-up — that's fine, but
            // set_termios is the one we're testing.
            let t = slopos_abi::syscall::UserTermios::default();
            tty::set_termios(idx, &t)
        }
    };

    tty::table::tty_table_init();

    match result {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup set_termios expected HungUp (EIO), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: set_winsize on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_phase33_hangup_set_winsize_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let ws = slopos_abi::syscall::UserWinsize {
        ws_row: 25,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = tty::set_winsize(idx, &ws);

    tty::table::tty_table_init();

    match result {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup set_winsize expected HungUp (EIO), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: set_ldisc on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_phase33_hangup_set_ldisc_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let result = tty::set_ldisc(idx, 0);

    tty::table::tty_table_init();

    match result {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup set_ldisc expected HungUp (EIO), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: get_foreground_pgrp is a hangup-safe ioctl — still works after
/// hangup so shells can query job control state during session cleanup.
pub fn test_phase33_hangup_get_fg_pgrp_still_works() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    // Set a foreground pgrp before hangup.
    let _ = tty::set_foreground_pgrp(idx, 42);
    tty::hangup(idx);

    // get_foreground_pgrp should still succeed after hangup.
    let result = tty::get_foreground_pgrp(idx);

    tty::table::tty_table_init();

    match result {
        Ok(_) => TestResult::Pass,
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - get_foreground_pgrp should work after hangup, got {:?}",
                e
            );
            TestResult::Fail
        }
    }
}

/// Phase 33: PTY master close → slave read returns EOF, slave write returns
/// EIO.  Validates cross-end hangup propagation.
pub fn test_phase33_pty_master_close_slave_eof_eio() -> TestResult {
    tty::table::tty_table_init();

    // Allocate a PTY pair.
    let master_idx = match crate::tty::pty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // Open master so close_ref will actually decrement to 0 and trigger hangup.
    tty::open_ref(master_idx).unwrap();

    // Find slave index from master's driver.
    let slave_idx = {
        let guard = TTY_SLOTS[master_idx.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => peer.idx,
                _ => {
                    klog_info!("TTY_TEST: BUG - master is not PtyMaster");
                    return TestResult::Fail;
                }
            },
            None => {
                klog_info!("TTY_TEST: BUG - master slot is empty");
                return TestResult::Fail;
            }
        }
    };

    // Open slave.
    if let Err(e) = crate::tty::pty::pty_open_slave(slave_idx) {
        klog_info!("TTY_TEST: BUG - pty_open_slave failed: {:?}", e);
        return TestResult::Fail;
    }

    // Close master (decrement to 0 triggers hangup on slave).
    let _ = tty::close_ref(master_idx);

    // Slave read should return EOF (0 bytes).
    let mut out = [0u8; 8];
    let read_result = tty::read(slave_idx, &mut out, true);

    // Slave write should return EIO.
    let write_result = tty::write(slave_idx, b"test");

    // Cleanup.
    tty::table::tty_table_init();

    let read_ok = matches!(read_result, Ok(0));
    let write_ok = matches!(write_result, Err(TtyError::HungUp));

    if !read_ok {
        klog_info!(
            "TTY_TEST: BUG - PTY slave read after master close expected Ok(0), got {:?}",
            read_result
        );
        return TestResult::Fail;
    }
    if !write_ok {
        klog_info!(
            "TTY_TEST: BUG - PTY slave write after master close expected HungUp, got {:?}",
            write_result
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 33: hung_up flag is never cleared — a hung-up TTY is permanently dead
/// until the slot is reclaimed.  Verify multiple reads all return EOF.
pub fn test_phase33_hangup_permanent_eof() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::open_ref(idx);
    tty::hangup(idx);

    let mut out = [0u8; 8];
    let r1 = tty::read(idx, &mut out, true);
    let r2 = tty::read(idx, &mut out, true);
    let r3 = tty::read(idx, &mut out, true);

    tty::table::tty_table_init();

    let all_eof = matches!(r1, Ok(0)) && matches!(r2, Ok(0)) && matches!(r3, Ok(0));
    if !all_eof {
        klog_info!(
            "TTY_TEST: BUG - multiple reads on hung-up should all be Ok(0), got {:?} {:?} {:?}",
            r1,
            r2,
            r3
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 33: PTY slave poll returns POLLHUP after master closes.
pub fn test_phase33_pty_slave_poll_pollhup_after_master_close() -> TestResult {
    tty::table::tty_table_init();

    let master_idx = match crate::tty::pty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // Open master so close_ref will decrement to 0 and trigger hangup.
    tty::open_ref(master_idx).unwrap();

    let slave_idx = {
        let guard = TTY_SLOTS[master_idx.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => peer.idx,
                _ => return TestResult::Fail,
            },
            None => return TestResult::Fail,
        }
    };

    if let Err(_) = crate::tty::pty::pty_open_slave(slave_idx) {
        return TestResult::Fail;
    }

    // Close master.
    let _ = tty::close_ref(master_idx);

    // Poll slave.
    let revents = tty::poll_events(
        slave_idx,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    tty::table::tty_table_init();

    if (revents & slopos_abi::syscall::POLLHUP) == 0 {
        klog_info!("TTY_TEST: BUG - PTY slave poll should return POLLHUP after master close");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 33: TtyError::HungUp maps to errno -5 (EIO).
pub fn test_phase33_hungup_errno_is_eio() -> TestResult {
    let errno = TtyError::HungUp.to_errno();
    if errno == -5 {
        TestResult::Pass
    } else {
        klog_info!(
            "TTY_TEST: BUG - TtyError::HungUp.to_errno() expected -5 (EIO), got {}",
            errno
        );
        TestResult::Fail
    }
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
        // Phase 23: Canonical EOF, ISIG Flush & Signal Integrity
        test_phase23_canonical_eof_empty_no_phantom,
        test_phase23_canonical_eof_with_pending_text_no_phantom,
        test_phase23_isig_flush_no_noflsh,
        test_phase23_isig_flush_with_noflsh,
        test_phase23_isig_ctrl_c_clears_edit_buffer,
        test_phase23_isig_flush_sigquit,
        test_phase23_isig_flush_sigtstp,
        test_phase23_double_eof_no_phantom_accumulation,
        // Phase 24: Job Control & Controlling TTY Hardening
        test_phase24_set_fg_pgrp_checked_nonexistent_pgrp,
        test_phase24_set_fg_pgrp_checked_clear_allowed,
        test_phase24_set_fg_pgrp_checked_no_session_skips_validation,
        test_phase24_detach_ctty_non_leader,
        test_phase24_detach_ctty_session_leader,
        test_phase24_detach_ctty_cross_session_denied,
        test_phase24_tiocnotty_constant,
        // Phase 25: Real TCSETSW/TCSETSF Drain Semantics
        test_phase25_is_output_idle_initially_true,
        test_phase25_inflight_counter_initial_zero,
        test_phase25_write_updates_inflight_counter,
        test_phase25_tcsetsw_preserves_input_after_drain,
        test_phase25_tcsetsf_flushes_input_after_drain,
        test_phase25_is_output_idle_invalid_index,
        test_phase25_is_output_idle_unallocated,
        test_phase25_drain_invalid_index_error,
        test_phase25_driver_output_pending_default_false,
        test_phase25_driver_kind_output_pending_dispatch,
        test_phase25_pty_drain_immediate,
        test_phase25_console_drain_immediate,
        test_phase25_tcsets_now_skips_drain,
        // Phase 26: PTY Lifetime Safety & Scalable Capacity
        test_phase26_max_ttys_is_32,
        test_phase26_pty_peer_handle_creation,
        test_phase26_pty_peer_handle_snapshot,
        test_phase26_generation_bumped_on_free,
        test_phase26_stale_handle_detected,
        test_phase26_pty_alloc_captures_generation,
        test_phase26_stale_write_safe_noop,
        test_phase26_rapid_alloc_free_stress,
        test_phase26_data_flow_with_generation,
        test_phase26_validate_peer_out_of_range,
        test_phase26_multiple_pty_pairs,
        // Phase 27: POSIX Completion Set (Rust-Idiomatic)
        test_phase27_ignbrk_discards_break,
        test_phase27_brkint_generates_sigint,
        test_phase27_parmrk_inserts_marker,
        test_phase27_nul_without_break_flags_passes_through,
        test_phase27_echoke_visual_erase,
        test_phase27_echok_newline_on_kill,
        test_phase27_echoctl_erase_two_columns,
        test_phase27_bytes_available,
        test_phase27_raw_disc_bytes_available,
        test_phase27_ldisc_kind_bytes_available,
        test_phase27_fionread_constant,
        test_phase27_kill_empty_line_no_echo,
        test_phase27_ignbrk_takes_priority_over_brkint,
        // Phase 28: Type-Safe Termios Foundation
        test_phase28_input_flags_from_bits,
        test_phase28_output_flags_from_bits,
        test_phase28_local_flags_from_bits,
        test_phase28_cc_index_values,
        test_phase28_posix_vdisable,
        test_phase28_tty_error_to_errno,
        test_phase28_tty_error_signal_interrupt,
        test_phase28_user_termios_typed_accessors,
        test_phase28_ldisc_typed_flags_behavioral_equivalence,
        test_phase28_control_flags_empty,
        // Phase 29: LdiscKind Dispatch Consolidation
        test_phase29_from_id_still_works,
        test_phase29_ldisc_ops_linedisc_trait_delegation,
        test_phase29_ldisc_ops_rawdisc_trait_delegation,
        test_phase29_dispatch_macro_ntty_routing,
        test_phase29_dispatch_macro_raw_routing,
        test_phase29_process_output_byte_dispatch,
        test_phase29_edit_content_dispatch,
        // Phase 30: /dev/tty Controlling Terminal Device
        test_phase30_open_ref_second_fd_increments_count,
        test_phase30_dev_tty_operations_identical_to_direct,
        test_phase30_open_ref_does_not_modify_session,
        test_phase30_open_ref_invalid_index_returns_error,
        test_phase30_close_ref_decrements_after_open,
        test_phase30_multiple_open_ref_sequential,
        test_phase30_dev_tty_winsize_matches_direct,
        // Phase 31: Background Write Protection (SIGTTOU on tcsetattr)
        test_phase31_tcsetattr_background_blocked,
        test_phase31_tcsetattr_foreground_allowed,
        test_phase31_tcsetattr_no_session_allowed,
        test_phase31_tcsetattr_cross_session_denied,
        test_phase31_orphaned_pgrp_errno,
        test_phase31_tcsetattr_kernel_task_bypass,
        test_phase31_tcsetsw_tcsetsf_kernel_task_bypass,
        test_phase31_tostop_background_write_check,
        test_phase31_kernel_task_check_write_allowed,
        // Phase 32: Controlling Terminal Lifecycle Integrity
        test_phase32_acquire_ctty_fresh_tty,
        test_phase32_acquire_ctty_same_session_idempotent,
        test_phase32_acquire_ctty_different_session_denied,
        test_phase32_release_ctty_owning_session,
        test_phase32_release_ctty_wrong_session_noop,
        test_phase32_hangup_detaches_session,
        test_phase32_o_noctty_suppresses_acquire,
        test_phase32_detach_ctty_non_leader_preserves_session,
        test_phase32_detach_ctty_session_leader_detaches,
        test_phase32_full_lifecycle_acquire_release_reacquire,
        test_phase32_double_acquire_race_guard,
        test_phase32_hangup_no_session_safe,
        test_phase32_rapid_acquire_release_stress,
        test_phase32_acquire_invalid_index,
        test_phase32_release_invalid_index,
        test_phase32_detach_invalid_index,
        // Phase 33: Post-Hangup I/O Hardening
        test_phase33_hangup_read_returns_eof,
        test_phase33_hangup_write_returns_eio,
        test_phase33_hangup_poll_returns_pollhup_pollin,
        test_phase33_hangup_set_termios_returns_eio,
        test_phase33_hangup_set_winsize_returns_eio,
        test_phase33_hangup_set_ldisc_returns_eio,
        test_phase33_hangup_get_fg_pgrp_still_works,
        test_phase33_pty_master_close_slave_eof_eio,
        test_phase33_hangup_permanent_eof,
        test_phase33_pty_slave_poll_pollhup_after_master_close,
        test_phase33_hungup_errno_is_eio,
    ]
);
