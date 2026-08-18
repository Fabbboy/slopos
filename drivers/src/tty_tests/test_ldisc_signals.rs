//! Line-discipline tests for ISIG signal generation, buffer flush, caret echo
//! and TtyError errno mappings.

use super::fixtures::*;

/// SIGQUIT: Ctrl+\ generates SIGQUIT (signal 3).
pub fn test_ldisc_signal_ctrl_backslash() -> TestResult {
    let mut ld = LineDisc::new();
    let action = ld.input_char(0x1C);
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

/// Ctrl+D on empty buffer produces EOF (0 bytes) without leaving has_data()
/// stuck true.
pub fn test_canonical_eof_empty_no_phantom() -> TestResult {
    let mut ld = LineDisc::new();

    ld.input_char(0x04);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data should be true immediately after empty EOF");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - empty EOF read should return 0, got {}", n);
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data still true after consuming empty EOF (phantom state)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Ctrl+D after text returns text without newline, then no phantom.
pub fn test_canonical_eof_with_pending_text_no_phantom() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x04);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false after text+EOF");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - text+EOF read mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after reading text+EOF chunk (phantom)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// ISIG flush — Ctrl+C without NOFLSH clears edit and cooked buffers.
pub fn test_isig_flush_no_noflsh() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }

    if ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should have content before signal");
        return TestResult::Fail;
    }

    let action = ld.input_char(0x03);
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected Signal(SIGINT), got {:?}", other);
            return TestResult::Fail;
        }
    }

    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after ISIG flush");
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after ISIG flush (should be clear)");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// ISIG with NOFLSH set — Ctrl+C does NOT flush buffers.
pub fn test_isig_flush_with_noflsh() -> TestResult {
    let mut ld = LineDisc::new();

    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::NOFLSH;
    ld.set_termios(&t);

    for &c in b"abc" {
        ld.input_char(c);
    }

    let action = ld.input_char(0x03);
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

    if ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - NOFLSH should preserve edit buffer on ISIG");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// After Ctrl+C (without NOFLSH), subsequent newline produces empty line.
pub fn test_isig_ctrl_c_clears_edit_buffer() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    let _ = ld.input_char(0x03);
    ld.input_char(b'\n');

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false after flush+newline");
        return TestResult::Fail;
    }

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

pub fn test_isig_flush_sigquit() -> TestResult {
    let mut ld = LineDisc::new();

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

pub fn test_isig_flush_sigtstp() -> TestResult {
    let mut ld = LineDisc::new();

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

/// VINTR (Ctrl+C) with ECHO|ECHOCTL echoes `^C` to the output, then signals.
pub fn test_isig_vintr_echoes_caret() -> TestResult {
    let mut ld = LineDisc::new();
    let batch = ld.receive_buf(&[InputEvent::normal(0x03)]);
    let echo = EchoScratch::drain(&mut ld);

    match batch.signal {
        Some((sig, _)) if sig == SIGINT => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected SIGINT signal, got {:?}", other);
            return TestResult::Fail;
        }
    }

    if echo.as_slice() != b"^C" {
        klog_info!(
            "TTY_TEST: BUG - expected echo \"^C\", got {:?}",
            echo.as_slice()
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_isig_vquit_vsusp_echo_caret() -> TestResult {
    let mut ld = LineDisc::new();
    let quit = ld.receive_buf(&[InputEvent::normal(0x1C)]);
    let quit_echo = EchoScratch::drain(&mut ld);
    if quit_echo.as_slice() != b"^\\" {
        klog_info!(
            "TTY_TEST: BUG - VQUIT expected echo \"^\\\", got {:?}",
            quit_echo.as_slice()
        );
        return TestResult::Fail;
    }
    match quit.signal {
        Some((sig, _)) if sig == SIGQUIT => {}
        other => {
            klog_info!("TTY_TEST: BUG - VQUIT expected SIGQUIT, got {:?}", other);
            return TestResult::Fail;
        }
    }

    let mut ld = LineDisc::new();
    let susp = ld.receive_buf(&[InputEvent::normal(0x1A)]);
    let susp_echo = EchoScratch::drain(&mut ld);
    if susp_echo.as_slice() != b"^Z" {
        klog_info!(
            "TTY_TEST: BUG - VSUSP expected echo \"^Z\", got {:?}",
            susp_echo.as_slice()
        );
        return TestResult::Fail;
    }
    match susp.signal {
        Some((sig, _)) if sig == SIGTSTP => {}
        other => {
            klog_info!("TTY_TEST: BUG - VSUSP expected SIGTSTP, got {:?}", other);
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_isig_no_echo_without_echoctl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOCTL;
    ld.set_termios(&t);

    let batch = ld.receive_buf(&[InputEvent::normal(0x03)]);

    match batch.signal {
        Some((sig, _)) if sig == SIGINT => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGINT even without ECHOCTL, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    let echo = EchoScratch::drain(&mut ld);
    if !echo.is_empty() {
        klog_info!(
            "TTY_TEST: BUG - no caret should echo without ECHOCTL, got {:?}",
            echo.as_slice()
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// With NOFLSH the caret still echoes and the signal is reported as
/// non-flushing.
pub fn test_isig_caret_echo_with_noflsh() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::NOFLSH;
    ld.set_termios(&t);

    for &c in b"abc" {
        ld.input_char(c);
    }

    let batch = ld.receive_buf(&[InputEvent::normal(0x03)]);

    let echo = EchoScratch::drain(&mut ld);
    if echo.as_slice() != b"^C" {
        klog_info!(
            "TTY_TEST: BUG - NOFLSH should not suppress caret echo, got {:?}",
            echo.as_slice()
        );
        return TestResult::Fail;
    }

    match batch.signal {
        Some((sig, flush)) if sig == SIGINT => {
            if flush {
                klog_info!("TTY_TEST: BUG - NOFLSH should report flush=false");
                return TestResult::Fail;
            }
        }
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGINT with NOFLSH, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    if ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - NOFLSH should preserve edit buffer on ISIG");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_double_eof_no_phantom_accumulation() -> TestResult {
    let mut ld = LineDisc::new();

    ld.input_char(0x04);
    ld.input_char(0x04);

    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 0 {
        klog_info!(
            "TTY_TEST: BUG - first empty EOF should return 0, got {}",
            n1
        );
        return TestResult::Fail;
    }

    let n2 = ld.read(&mut buf);
    if n2 != 0 {
        klog_info!(
            "TTY_TEST: BUG - second empty EOF should return 0, got {}",
            n2
        );
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after consuming both EOFs");
        return TestResult::Fail;
    }

    TestResult::Pass
}

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

slopos_testing::stest!(
    name = test_ldisc_signal_ctrl_backslash,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_ldisc_signal_ctrl_z,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_canonical_eof_empty_no_phantom,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_canonical_eof_with_pending_text_no_phantom,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_flush_no_noflsh,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_flush_with_noflsh,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_ctrl_c_clears_edit_buffer,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_flush_sigquit,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_flush_sigtstp,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_vintr_echoes_caret,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_vquit_vsusp_echo_caret,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_no_echo_without_echoctl,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_isig_caret_echo_with_noflsh,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_double_eof_no_phantom_accumulation,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_restart_error_to_errno,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_restart_distinct_from_signal_interrupt,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_erestartsys_constant_value,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_eintr_constant_value,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_sa_restart_flag_value,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_sa_restart_distinct,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_signal_interrupt_still_eintr,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_all_error_variants_preserved,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_nonblock_empty_returns_wouldblock,
    suite = tty_test_ldisc_signals
);
slopos_testing::stest!(
    name = test_read_with_data_succeeds,
    suite = tty_test_ldisc_signals
);
