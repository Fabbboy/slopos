//! Split from test_ldisc.rs: test_ldisc_signals.rs

use super::fixtures::*;

// ===========================================================================
// Signal generation tests
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
// Canonical EOF, ISIG Flush & Signal Integrity
// ===========================================================================

/// Ctrl+D on empty buffer produces EOF (0 bytes) without phantom
/// has_data state.  Previously, flush_edit_to_cooked incremented line_count
/// on empty buffer, leaving has_data() stuck true.
pub fn test_canonical_eof_empty_no_phantom() -> TestResult {
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

/// Ctrl+D after text returns text without newline, then no phantom.
pub fn test_canonical_eof_with_pending_text_no_phantom() -> TestResult {
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

/// ISIG flush — Ctrl+C without NOFLSH clears edit and cooked buffers.
pub fn test_isig_flush_no_noflsh() -> TestResult {
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

/// ISIG with NOFLSH set — Ctrl+C does NOT flush buffers.
pub fn test_isig_flush_with_noflsh() -> TestResult {
    let mut ld = LineDisc::new();

    // Set NOFLSH flag.
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::NOFLSH;
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

/// After Ctrl+C (without NOFLSH), subsequent newline produces empty line.
pub fn test_isig_ctrl_c_clears_edit_buffer() -> TestResult {
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

/// ISIG flush works for SIGQUIT (Ctrl+\\) too.
pub fn test_isig_flush_sigquit() -> TestResult {
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

/// ISIG flush works for SIGTSTP (Ctrl+Z) too.
pub fn test_isig_flush_sigtstp() -> TestResult {
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

/// Double Ctrl+D does not accumulate phantom line_count.
pub fn test_double_eof_no_phantom_accumulation() -> TestResult {
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
// Signal Restart Infrastructure (ERESTARTSYS)
// ===========================================================================

/// TtyError::Restart maps to -512 (ERESTARTSYS).
pub fn test_restart_error_to_errno() -> TestResult {
    if TtyError::Restart.to_errno() != -512 {
        klog_info!(
            "TTY_TEST: BUG - TtyError::Restart.to_errno()={} expected -512",
            TtyError::Restart.to_errno()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TtyError::Restart is distinct from SignalInterrupt.
pub fn test_restart_distinct_from_signal_interrupt() -> TestResult {
    if TtyError::Restart.to_errno() == TtyError::SignalInterrupt.to_errno() {
        klog_info!("TTY_TEST: BUG - Restart and SignalInterrupt map to same errno");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ERESTARTSYS constant value matches Linux convention.
pub fn test_erestartsys_constant_value() -> TestResult {
    if slopos_abi::syscall::ERRNO_ERESTARTSYS != (-512i64) as u64 {
        klog_info!("TTY_TEST: BUG - ERRNO_ERESTARTSYS is not -512");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ERRNO_EINTR constant value matches Linux convention.
pub fn test_eintr_constant_value() -> TestResult {
    if slopos_abi::syscall::ERRNO_EINTR != (-4i64) as u64 {
        klog_info!("TTY_TEST: BUG - ERRNO_EINTR is not -4");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SA_RESTART flag has correct Linux-compatible value.
pub fn test_sa_restart_flag_value() -> TestResult {
    if slopos_abi::signal::SA_RESTART != 0x10000000 {
        klog_info!(
            "TTY_TEST: BUG - SA_RESTART=0x{:08X} expected 0x10000000",
            slopos_abi::signal::SA_RESTART
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SA_RESTART is distinct from other SA_ flags.
pub fn test_sa_restart_distinct() -> TestResult {
    use slopos_abi::signal::*;
    let sa_restart = SA_RESTART;
    if (sa_restart & SA_RESTORER) != 0
        || (sa_restart & SA_SIGINFO) != 0
        || (sa_restart & SA_NODEFER) != 0
        || (sa_restart & SA_RESETHAND) != 0
    {
        klog_info!("TTY_TEST: BUG - SA_RESTART overlaps with another SA_ flag");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SignalInterrupt still maps to EINTR (-4).
/// Regression: ensure existing behavior is preserved.
pub fn test_signal_interrupt_still_eintr() -> TestResult {
    if TtyError::SignalInterrupt.to_errno() != -4 {
        klog_info!(
            "TTY_TEST: BUG - SignalInterrupt.to_errno()={} expected -4",
            TtyError::SignalInterrupt.to_errno()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// All existing TtyError variants preserve their errno mappings.
/// Regression test: ensure existing error codes are preserved.
pub fn test_all_error_variants_preserved() -> TestResult {
    let pairs: &[(TtyError, i32)] = &[
        (TtyError::InvalidIndex, -22),
        (TtyError::NotAllocated, -6),
        (TtyError::BackgroundRead, -5),
        (TtyError::BackgroundWrite, -5),
        (TtyError::HungUp, -5),
        (TtyError::WouldBlock, -11),
        (TtyError::PermissionDenied, -1),
        (TtyError::UnsupportedLineDiscipline, -22),
        (TtyError::CrossSessionDenied, -5),
        (TtyError::SignalInterrupt, -4),
        (TtyError::OrphanedProcessGroup, -5),
        (TtyError::InvalidArg, -22),
        (TtyError::Restart, -512),
    ];
    for &(err, expected) in pairs {
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

/// Non-blocking read on empty TTY returns WouldBlock, not Restart.
pub fn test_nonblock_empty_returns_wouldblock() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Non-blocking read on empty TTY should return WouldBlock.
    let mut buf = [0u8; 64];
    match tty::read(idx, &mut buf, true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - nonblock empty read expected WouldBlock, got {:?}",
                other
            );
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

/// Read with available data succeeds normally (no ERESTARTSYS).
pub fn test_read_with_data_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Push data + newline (canonical mode).
    for &c in b"hello\n" {
        tty::push_input(idx, c);
    }

    let mut buf = [0u8; 64];
    match tty::read(idx, &mut buf, true) {
        Ok(n) if n == 6 && &buf[..6] == b"hello\n" => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - read with data expected 6 bytes, got {:?}",
                other
            );
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

slopos_testing::define_test_suite!(
    tty_test_ldisc_signals,
    [
        test_ldisc_signal_ctrl_backslash,
        test_ldisc_signal_ctrl_z,
        test_canonical_eof_empty_no_phantom,
        test_canonical_eof_with_pending_text_no_phantom,
        test_isig_flush_no_noflsh,
        test_isig_flush_with_noflsh,
        test_isig_ctrl_c_clears_edit_buffer,
        test_isig_flush_sigquit,
        test_isig_flush_sigtstp,
        test_double_eof_no_phantom_accumulation,
        test_restart_error_to_errno,
        test_restart_distinct_from_signal_interrupt,
        test_erestartsys_constant_value,
        test_eintr_constant_value,
        test_sa_restart_flag_value,
        test_sa_restart_distinct,
        test_signal_interrupt_still_eintr,
        test_all_error_variants_preserved,
        test_nonblock_empty_returns_wouldblock,
        test_read_with_data_succeeds,
    ]
);
