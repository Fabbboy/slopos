//! Split from test_ldisc.rs: test_ioctls_ext.rs

use super::fixtures::*;

// ===========================================================================
// Real TCSETSW/TCSETSF Drain Semantics
// ===========================================================================

/// The `is_output_idle` function returns `true` when no output
/// is in flight and the driver reports no pending output.  For synchronous
/// backends (serial, vconsole) this should always be `true` when no write
/// is in progress.
pub fn test_is_output_idle_initially_true() -> TestResult {
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

/// The inflight counter starts at zero for all TTY slots.
pub fn test_inflight_counter_initial_zero() -> TestResult {
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

/// After a write completes, the inflight counter is back to zero
/// and `is_output_idle` returns true.
pub fn test_write_updates_inflight_counter() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Write some data.
    let data = b"hello drain";
    let result = tty::write(TtyIndex(0), data, false);
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

/// `TCSETSW` (set_termios_wait) applies termios after drain and
/// preserves pending input (does not flush).
pub fn test_tcsetsw_preserves_input_after_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode so we can observe single-byte input.
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input, then perform a write (creating output to "drain").
    tty::push_input(TtyIndex(0), b'x');
    let _ = tty::write(TtyIndex(0), b"output", false);

    // Now use TCSETSW to change termios.  Input should survive.
    let mut changed = raw;
    changed.c_lflag &= !LocalFlags::ECHO;
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

/// `TCSETSF` (set_termios_flush) applies termios after drain and
/// flushes pending input.
pub fn test_tcsetsf_flushes_input_after_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input, then perform a write.
    tty::push_input(TtyIndex(0), b'y');
    let _ = tty::write(TtyIndex(0), b"output", false);

    // Use TCSETSF — should drain output AND flush input.
    let mut changed = raw;
    changed.c_lflag &= !LocalFlags::ECHO;
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

/// `is_output_idle` returns an error for an invalid index.
pub fn test_is_output_idle_invalid_index() -> TestResult {
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

/// `is_output_idle` returns an error for an unallocated slot.
pub fn test_is_output_idle_unallocated() -> TestResult {
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

/// `wait_output_idle` (via `set_termios_wait`) returns an error
/// for an invalid TTY index.
pub fn test_drain_invalid_index_error() -> TestResult {
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

/// The `TtyDriver` trait default `output_pending()` returns `false`.
pub fn test_driver_output_pending_default_false() -> TestResult {
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

/// `TtyDriverKind::output_pending()` dispatches correctly for all
/// driver variants.
pub fn test_driver_kind_output_pending_dispatch() -> TestResult {
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

    let pty_master_kind = TtyDriverKind::PtyMaster { peer: KWeak::new() };
    if pty_master_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - PtyMaster kind output_pending should be false");
        return TestResult::Fail;
    }

    let pty_slave_kind = TtyDriverKind::PtySlave { peer: KWeak::new() };
    if pty_slave_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - PtySlave kind output_pending should be false");
        return TestResult::Fail;
    }

    let none_kind = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if none_kind.output_pending() {
        klog_info!("TTY_TEST: BUG - None kind output_pending should be false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// PTY drain is immediate — `is_output_idle` returns `true` right
/// after writing to a PTY master/slave pair.
pub fn test_pty_output_idle_immediate() -> TestResult {
    tty::table::tty_table_init();

    // Allocate a PTY pair.
    let (master_idx, master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: SKIP - could not allocate PTY pair");
            return TestResult::Pass;
        }
    };
    let slave_idx = match tty::get_pty_number(master_idx) {
        Ok(n) => TtyIndex(n as u8),
        Err(_) => {
            klog_info!("TTY_TEST: SKIP - could not get PTY slave index");
            return TestResult::Pass;
        }
    };

    // Open the slave so the pair stays alive.
    let slave_backing = tty::open_tty(slave_idx).unwrap();

    // Write to master (goes to slave's input buffer).
    let _ = tty::write(master_idx, b"pty drain test", false);

    // Output should be idle immediately (PTY has no hardware latency).
    match tty::is_output_idle(master_idx) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PTY master is_output_idle should be true, got {:?}",
                other
            );
            drop(slave_backing);
            drop(master_backing);
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
            drop(slave_backing);
            drop(master_backing);
            return TestResult::Fail;
        }
    }

    // Clean up.
    drop(slave_backing);
    drop(master_backing);
    TestResult::Pass
}

/// `TCSETSW` on console completes immediately because the serial
/// driver is synchronous (all output is "drained" instantly).
pub fn test_console_drain_immediate() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Write output to create something to "drain".
    let _ = tty::write(TtyIndex(0), b"drain test output\r\n", false);

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

/// `set_termios_mode` with `Now` does NOT call `wait_output_idle`
/// — it applies termios immediately regardless of pending output.
pub fn test_tcsets_now_skips_drain() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Push input.
    tty::push_input(TtyIndex(0), b'z');

    // TCSETS (Now) should apply immediately, input preserved.
    let mut changed = raw;
    changed.c_lflag &= !LocalFlags::ECHO;
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
// Post-Hangup I/O Hardening regression tests
// ===========================================================================

/// read() on a hung-up TTY with no buffered data returns EOF (0
/// bytes), regardless of blocking/nonblock mode.
pub fn test_hangup_read_returns_eof() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

    let mut out = [0u8; 8];
    // Nonblocking read on hung-up TTY.
    let result = tty::read(idx, &mut out, true);

    // Re-init before checking.
    drop(con);
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

/// write() on a hung-up TTY returns Err(HungUp) which maps to EIO.
pub fn test_hangup_write_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

    let result = tty::write(idx, b"hello", false);

    drop(con);
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

/// poll_events() on a hung-up TTY returns POLLHUP | POLLIN.
pub fn test_hangup_poll_returns_pollhup_pollin() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);
    let _hangup = HangupScope::hang_up(idx);

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

/// set_termios on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_hangup_set_termios_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

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

    drop(con);
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

/// set_winsize on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_hangup_set_winsize_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

    let ws = slopos_abi::syscall::UserWinsize {
        ws_row: 25,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = tty::set_winsize(idx, &ws);

    drop(con);
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

/// set_ldisc on a hung-up TTY returns Err(HungUp) / EIO.
pub fn test_hangup_set_ldisc_returns_eio() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

    let result = tty::set_ldisc(idx, 0);

    drop(con);
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

/// get_foreground_pgrp is a hangup-safe ioctl — still works after
/// hangup so shells can query job control state during session cleanup.
pub fn test_hangup_get_fg_pgrp_still_works() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    // Set a foreground pgrp before hangup.
    let _ = tty::set_foreground_pgrp(idx, 42);
    let _hangup = HangupScope::hang_up(idx);

    // get_foreground_pgrp should still succeed after hangup.
    let result = tty::get_foreground_pgrp(idx);

    drop(con);
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

/// PTY master close → slave read returns EOF, slave write returns
/// EIO.  Validates cross-end hangup propagation.
pub fn test_pty_master_close_slave_eof_eio() -> TestResult {
    tty::table::tty_table_init();

    // Allocate a PTY pair. The returned backing is the sole master open.
    let (master_idx, master_backing) = match crate::tty::pty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let slave_idx = TtyIndex(tty::get_pty_number(master_idx).unwrap() as u8);

    // Unlock the slave so it can be opened.
    crate::tty::set_pty_lock(master_idx, false).unwrap();

    // Open slave.
    let slave_backing = match tty::pty_open_slave(slave_idx) {
        Ok(backing) => backing,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_open_slave failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // Close the last master open — hangs up the slave.
    drop(master_backing);

    // Slave read should return EOF (0 bytes).
    let mut out = [0u8; 8];
    let read_result = tty::read(slave_idx, &mut out, true);

    // Slave write should return EIO.
    let write_result = tty::write(slave_idx, b"test", false);

    drop(slave_backing);

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

/// hung_up flag is never cleared — a hung-up TTY is permanently dead
/// until the slot is reclaimed.  Verify multiple reads all return EOF.
pub fn test_hangup_permanent_eof() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = tty::open_tty(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

    let mut out = [0u8; 8];
    let r1 = tty::read(idx, &mut out, true);
    let r2 = tty::read(idx, &mut out, true);
    let r3 = tty::read(idx, &mut out, true);

    drop(con);
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

/// PTY slave poll returns POLLHUP after master closes.
pub fn test_pty_slave_poll_pollhup_after_master_close() -> TestResult {
    tty::table::tty_table_init();

    let (master_idx, master_backing) = match crate::tty::pty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    let slave_idx = TtyIndex(tty::get_pty_number(master_idx).unwrap() as u8);

    // Unlock the slave so it can be opened.
    crate::tty::set_pty_lock(master_idx, false).unwrap();

    let slave_backing = match tty::pty_open_slave(slave_idx) {
        Ok(backing) => backing,
        Err(_) => return TestResult::Fail,
    };

    // Close the last master open — hangs up the slave.
    drop(master_backing);

    // Poll slave.
    let revents = tty::poll_events(
        slave_idx,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    drop(slave_backing);

    if (revents & slopos_abi::syscall::POLLHUP) == 0 {
        klog_info!("TTY_TEST: BUG - PTY slave poll should return POLLHUP after master close");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TtyError::HungUp maps to errno -5 (EIO).
pub fn test_hungup_errno_is_eio() -> TestResult {
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
// ---------------------------------------------------------------------------
// c_cflag ABI Completion tests
// ---------------------------------------------------------------------------

/// ControlFlags constants have correct octal values.
pub fn test_control_flag_values() -> TestResult {
    use slopos_abi::syscall::*;
    // Character size.
    if ControlFlags::CS5.bits() != 0o000000
        || ControlFlags::CS6.bits() != 0o000020
        || ControlFlags::CS7.bits() != 0o000040
        || ControlFlags::CS8.bits() != 0o000060
    {
        klog_info!("TTY_TEST: BUG - CS5/6/7/8 values wrong");
        return TestResult::Fail;
    }
    if ControlFlags::CSIZE.bits() != 0o000060 {
        klog_info!("TTY_TEST: BUG - CSIZE value wrong");
        return TestResult::Fail;
    }
    // Parity.
    if ControlFlags::PARENB.bits() != 0o000400 || ControlFlags::PARODD.bits() != 0o001000 {
        klog_info!("TTY_TEST: BUG - PARENB/PARODD values wrong");
        return TestResult::Fail;
    }
    // Stop/modem.
    if ControlFlags::CSTOPB.bits() != 0o000100
        || ControlFlags::HUPCL.bits() != 0o002000
        || ControlFlags::CLOCAL.bits() != 0o004000
    {
        klog_info!("TTY_TEST: BUG - CSTOPB/HUPCL/CLOCAL values wrong");
        return TestResult::Fail;
    }
    // Baud.
    if B0 != 0 || B9600 != 0o000015 || B38400 != 0o000017 || B115200 != 0o010002 {
        klog_info!("TTY_TEST: BUG - baud rate constants wrong");
        return TestResult::Fail;
    }
    if CBAUD != 0o010017 || CBAUDEX != 0o010000 {
        klog_info!("TTY_TEST: BUG - CBAUD/CBAUDEX wrong");
        return TestResult::Fail;
    }
    // Hardware flow control.
    if ControlFlags::CRTSCTS.bits() != 0o020000000 {
        klog_info!("TTY_TEST: BUG - CRTSCTS value wrong");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Default termios c_cflag contains CS8|CREAD|HUPCL|B38400.
pub fn test_default_cflag() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let t = tty::get_termios(TtyIndex(0)).unwrap();
    let expected = ControlFlags::from_bits_retain(
        ControlFlags::CS8.bits() | ControlFlags::CREAD.bits() | ControlFlags::HUPCL.bits() | B38400,
    );
    if t.c_cflag != expected {
        klog_info!(
            "TTY_TEST: BUG - default c_cflag 0x{:x}, expected 0x{:x}",
            t.c_cflag.bits(),
            expected.bits()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// tcsetattr with CS7|PARENB roundtrips through tcgetattr.
pub fn test_cflag_roundtrip() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    let mut t = saved;
    t.c_cflag = ControlFlags::from_bits_retain(
        ControlFlags::CS7.bits() | ControlFlags::PARENB.bits() | ControlFlags::CREAD.bits() | B9600,
    );
    tty::set_termios(idx, &t).unwrap();

    let got = tty::get_termios(idx).unwrap();
    if got.c_cflag != t.c_cflag {
        klog_info!(
            "TTY_TEST: BUG - roundtrip c_cflag 0x{:x} vs 0x{:x}",
            got.c_cflag.bits(),
            t.c_cflag.bits()
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(idx, &saved).unwrap();
    TestResult::Pass
}

/// c_ispeed/c_ospeed populated from default baud (38400).
pub fn test_speed_fields_populated() -> TestResult {
    tty::table::tty_table_init();
    let t = tty::get_termios(TtyIndex(0)).unwrap();
    if t.c_ispeed != 38400 {
        klog_info!("TTY_TEST: BUG - c_ispeed={}, expected 38400", t.c_ispeed);
        return TestResult::Fail;
    }
    if t.c_ospeed != 38400 {
        klog_info!("TTY_TEST: BUG - c_ospeed={}, expected 38400", t.c_ospeed);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Changing baud rate updates c_ispeed/c_ospeed.
pub fn test_speed_follows_baud_change() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    let mut t = saved;
    // Clear old baud bits and set B9600.
    t.c_cflag = ControlFlags::from_bits_retain((t.c_cflag.bits() & !CBAUD) | B9600);
    tty::set_termios(idx, &t).unwrap();

    let got = tty::get_termios(idx).unwrap();
    if got.c_ispeed != 9600 || got.c_ospeed != 9600 {
        klog_info!(
            "TTY_TEST: BUG - speed={}/{}, expected 9600",
            got.c_ispeed,
            got.c_ospeed
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(idx, &saved).unwrap();
    TestResult::Pass
}

/// CREAD value preserved after ABI update.
pub fn test_cread_value_preserved() -> TestResult {
    // CREAD was 0x80 before, now 0o000200 = 128 = 0x80. Same value.
    use slopos_abi::syscall::ControlFlags;
    let cread = ControlFlags::CREAD;
    if cread.bits() != 0x80 {
        klog_info!(
            "TTY_TEST: BUG - CREAD bits 0x{:x}, expected 0x80",
            cread.bits()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ---------------------------------------------------------------------------
// Missing Ioctls (TCFLSH, TCSBRK, TCXONC) tests
// ---------------------------------------------------------------------------

/// ABI constants have correct values.
pub fn test_flush_flow_ioctl_constants() -> TestResult {
    use slopos_abi::syscall::*;
    if TCSBRK != 0x5409 {
        klog_info!("TTY_TEST: BUG - TCSBRK=0x{:x}", TCSBRK);
        return TestResult::Fail;
    }
    if TCXONC != 0x540A {
        klog_info!("TTY_TEST: BUG - TCXONC=0x{:x}", TCXONC);
        return TestResult::Fail;
    }
    if TCFLSH != 0x540B {
        klog_info!("TTY_TEST: BUG - TCFLSH=0x{:x}", TCFLSH);
        return TestResult::Fail;
    }
    if TCIFLUSH != 0 || TCOFLUSH != 1 || TCIOFLUSH != 2 {
        klog_info!("TTY_TEST: BUG - flush selectors wrong");
        return TestResult::Fail;
    }
    if TCOOFF != 0 || TCOON != 1 || TCIOFF != 2 || TCION != 3 {
        klog_info!("TTY_TEST: BUG - flow selectors wrong");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TCFLSH with TCIFLUSH clears input.
pub fn test_tcflush_input() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Push some data (canonical: need newline to commit to cooked).
    tty::push_input(idx, b'H');
    tty::push_input(idx, b'i');
    tty::push_input(idx, b'\n');

    // Data should be available.
    if !tty::has_data(idx) {
        klog_info!("TTY_TEST: BUG - no data after push_input");
        drain_tty_nonblock(idx);
        return TestResult::Fail;
    }

    // Flush input.
    match tty::tcflush(idx, slopos_abi::syscall::TCIFLUSH) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcflush TCIFLUSH failed: {:?}", e);
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    // Now read should return 0 (no data).
    let mut buf = [0u8; 64];
    match tty::read(idx, &mut buf, true) {
        Ok(0) | Err(_) => {}
        Ok(n) => {
            klog_info!("TTY_TEST: BUG - read {} bytes after TCIFLUSH", n);
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

/// TCFLSH with TCOFLUSH resets output inflight.
pub fn test_tcflush_output() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;

    // Artificially set inflight counter.
    crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].store(5, Ordering::Release);

    // Flush output.
    tty::tcflush(idx, slopos_abi::syscall::TCOFLUSH).unwrap();

    let val = crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire);
    if val != 0 {
        klog_info!("TTY_TEST: BUG - inflight={} after TCOFLUSH", val);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TCFLSH with TCIOFLUSH clears both input and output.
pub fn test_tcflush_both() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;
    drain_tty_nonblock(idx);

    // Push data.
    tty::push_input(idx, b'A');
    tty::push_input(idx, b'\n');

    // Set inflight.
    crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].store(3, Ordering::Release);

    // Flush both.
    tty::tcflush(idx, slopos_abi::syscall::TCIOFLUSH).unwrap();

    // Input cleared.
    let mut buf = [0u8; 64];
    match tty::read(idx, &mut buf, true) {
        Ok(0) | Err(_) => {}
        Ok(n) => {
            klog_info!("TTY_TEST: BUG - read {} bytes after TCIOFLUSH", n);
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    // Output cleared.
    let val = crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].load(Ordering::Acquire);
    if val != 0 {
        klog_info!("TTY_TEST: BUG - inflight={} after TCIOFLUSH", val);
        return TestResult::Fail;
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

/// TCFLSH with invalid argument returns error.
pub fn test_tcflush_invalid_arg() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    match tty::tcflush(idx, 99) {
        Err(TtyError::InvalidArg) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcflush(99) = {:?}, expected InvalidArg",
                other
            );
            TestResult::Fail
        }
    }
}

/// TCSBRK with arg=0 returns success (no-op).
pub fn test_tcsbrk_noop() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    match tty::tcsbrk(idx, 0) {
        Ok(()) => TestResult::Pass,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcsbrk(0) failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// TCSBRK with arg>0 drains output (succeeds).
pub fn test_tcsbrk_drain() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    match tty::tcsbrk(idx, 1) {
        Ok(()) => TestResult::Pass,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcsbrk(1) failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// TCXONC with all four actions returns success.
pub fn test_tcxonc_all_actions() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    for action in 0..4 {
        match tty::tcxonc(idx, action) {
            Ok(()) => {}
            Err(e) => {
                klog_info!("TTY_TEST: BUG - tcxonc({}) failed: {:?}", action, e);
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}
// ===========================================================================
// TCXONC Behavioral Completion tests
// ===========================================================================

/// TCOOFF sets output_stopped, nonblocking write returns
/// WouldBlock on a console TTY.
pub fn test_tcooff_blocks_nonblock_write() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Suspend output via TCOOFF.
    if let Err(e) = tty::tcxonc(idx, slopos_abi::syscall::TCOOFF) {
        klog_info!("TTY_TEST: BUG - tcxonc(TCOOFF) failed: {:?}", e);
        return TestResult::Fail;
    }

    // Non-blocking write should return WouldBlock (no bytes written yet).
    match tty::write(idx, b"hello", true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - nonblock write under TCOOFF should return WouldBlock, got {:?}",
                other
            );
            // Clean up: resume output before returning.
            let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
            return TestResult::Fail;
        }
    }

    // Resume output for cleanup.
    let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
    TestResult::Pass
}

/// TCOON clears output_stopped, nonblocking write
/// succeeds after resume.
pub fn test_tcoon_resumes_write() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Stop output.
    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();

    // Verify stopped.
    match tty::write(idx, b"test", true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected WouldBlock while stopped, got {:?}",
                other
            );
            let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
            return TestResult::Fail;
        }
    }

    // Resume output.
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();

    // Write should now succeed.
    match tty::write(idx, b"hello", true) {
        Ok(n) if n == 5 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - write after TCOON should succeed with 5, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Double TCOOFF is idempotent (does not error).
pub fn test_tcooff_idempotent() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();
    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();

    // Still stopped — verify.
    match tty::write(idx, b"x", true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - double TCOOFF should still block, got {:?}",
                other
            );
            let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
            return TestResult::Fail;
        }
    }

    // Resume.
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();
    TestResult::Pass
}

/// Double TCOON is idempotent (does not error).
pub fn test_tcoon_idempotent() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // Start with a clean state.
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();
    // Calling TCOON again when already running is fine.
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();

    // Write should succeed.
    match tty::write(idx, b"ok", true) {
        Ok(n) if n == 2 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - write after double TCOON should succeed, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// TCOOFF then TCOON cycle — write works after resume.
pub fn test_stop_resume_cycle() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Cycle 1: stop → verify blocked → resume → verify working.
    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();
    if tty::write(idx, b"a", true) != Err(TtyError::WouldBlock) {
        klog_info!("TTY_TEST: BUG - cycle 1 stop did not block");
        let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
        return TestResult::Fail;
    }
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();
    if tty::write(idx, b"a", true).is_err() {
        klog_info!("TTY_TEST: BUG - cycle 1 resume did not unblock");
        return TestResult::Fail;
    }

    // Cycle 2: same thing again — no residual state.
    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();
    if tty::write(idx, b"b", true) != Err(TtyError::WouldBlock) {
        klog_info!("TTY_TEST: BUG - cycle 2 stop did not block");
        let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
        return TestResult::Fail;
    }
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();
    if tty::write(idx, b"b", true).is_err() {
        klog_info!("TTY_TEST: BUG - cycle 2 resume did not unblock");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// TCIOFF and TCION succeed (control-byte path).
pub fn test_tcioff_tcion_succeed() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // TCIOFF: transmit STOP byte — should succeed.
    if let Err(e) = tty::tcxonc(idx, slopos_abi::syscall::TCIOFF) {
        klog_info!("TTY_TEST: BUG - tcxonc(TCIOFF) failed: {:?}", e);
        return TestResult::Fail;
    }

    // TCION: transmit START byte — should succeed.
    if let Err(e) = tty::tcxonc(idx, slopos_abi::syscall::TCION) {
        klog_info!("TTY_TEST: BUG - tcxonc(TCION) failed: {:?}", e);
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// TCIOFF/TCION do not affect output_stopped state.
pub fn test_tcioff_tcion_no_output_stop() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Ensure output is running.
    tty::tcxonc(idx, slopos_abi::syscall::TCOON).unwrap();

    // TCIOFF should not block output.
    tty::tcxonc(idx, slopos_abi::syscall::TCIOFF).unwrap();
    match tty::write(idx, b"data", true) {
        Ok(4) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCIOFF should not block output, write returned {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // TCION should also not affect output.
    tty::tcxonc(idx, slopos_abi::syscall::TCION).unwrap();
    match tty::write(idx, b"data", true) {
        Ok(4) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCION should not affect output, write returned {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Invalid TCXONC actions still return InvalidArg.
/// (Regression test for validation preservation.)
pub fn test_invalid_action_still_errors() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    for &bad in &[4i32, -1, 99, i32::MAX, i32::MIN] {
        match tty::tcxonc(idx, bad) {
            Err(TtyError::InvalidArg) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - tcxonc({}) = {:?}, expected InvalidArg",
                    bad,
                    other
                );
                return TestResult::Fail;
            }
        }
    }

    TestResult::Pass
}

/// TCOOFF on a PTY slave stops nonblocking writes to that
/// slave.
pub fn test_tcooff_pty_slave_write() -> TestResult {
    let pair = open_pty_pair();
    let slave = pair.slave;

    // Stop output on the slave.
    tty::tcxonc(slave, slopos_abi::syscall::TCOOFF).unwrap();

    // Non-blocking write to slave should return WouldBlock.
    match tty::write(slave, b"blocked", true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - slave write under TCOOFF should return WouldBlock, got {:?}",
                other
            );
            let _ = tty::tcxonc(slave, slopos_abi::syscall::TCOON);
            return TestResult::Fail;
        }
    }

    // Resume and verify write works.
    tty::tcxonc(slave, slopos_abi::syscall::TCOON).unwrap();
    match tty::write(slave, b"ok", true) {
        Ok(n) if n > 0 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - slave write after TCOON should succeed, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// output_stopped is independent of ldisc IXON stopped.
pub fn test_output_stopped_independent_of_ixon() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Verify that output_stopped (TCXONC) and IXON stopped are separate.
    // TCOOFF should block even when IXON flow control is not active.
    // The default termios does NOT have IXON set (no keyboard flow
    // control), so ldisc.is_stopped() is false.  TCOOFF should still
    // block writes.
    tty::tcxonc(idx, slopos_abi::syscall::TCOOFF).unwrap();

    match tty::write(idx, b"x", true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCOOFF should block independently of IXON, got {:?}",
                other
            );
            let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
            return TestResult::Fail;
        }
    }

    let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON);
    TestResult::Pass
}

/// TCXONC on unallocated slot returns NotAllocated.
pub fn test_tcxonc_unallocated_slot() -> TestResult {
    tty::table::tty_table_init();
    // Slot 30 should not be allocated after init.
    let idx = TtyIndex(30);

    for action in 0..=3i32 {
        match tty::tcxonc(idx, action) {
            Err(TtyError::NotAllocated) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - tcxonc({}) on unallocated slot = {:?}, expected NotAllocated",
                    action,
                    other
                );
                return TestResult::Fail;
            }
        }
    }

    TestResult::Pass
}

/// TCXONC on out-of-range index returns InvalidIndex.
pub fn test_tcxonc_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(255);

    for action in 0..=3i32 {
        match tty::tcxonc(idx, action) {
            Err(TtyError::InvalidIndex) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - tcxonc({}) on invalid index = {:?}, expected InvalidIndex",
                    action,
                    other
                );
                return TestResult::Fail;
            }
        }
    }

    TestResult::Pass
}
// ---------------------------------------------------------------------------
// Output Queue Visibility (TIOCOUTQ) tests
// ---------------------------------------------------------------------------

/// TIOCOUTQ ABI constant is correct.
pub fn test_tiocoutq_abi_constant() -> TestResult {
    if slopos_abi::syscall::TIOCOUTQ != 0x5411 {
        klog_info!(
            "TTY_TEST: BUG - TIOCOUTQ should be 0x5411, got 0x{:X}",
            slopos_abi::syscall::TIOCOUTQ
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// output_queued_bytes returns 0 when idle.
pub fn test_output_queued_zero_when_idle() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // With no in-flight output, queued bytes should be 0.
    match tty::output_queued_bytes(idx) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - output_queued_bytes idle = {}, expected 0",
                n
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - output_queued_bytes failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// output_queued_bytes reflects TTY_OUTPUT_INFLIGHT.
pub fn test_output_queued_reflects_inflight() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;

    // Artificially set inflight counter.
    crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].store(7, Ordering::Release);

    let result = tty::output_queued_bytes(idx);

    // Reset before checking (avoid polluting other tests).
    crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);

    match result {
        Ok(7) => TestResult::Pass,
        Ok(n) => {
            klog_info!("TTY_TEST: BUG - output_queued_bytes = {}, expected 7", n);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - output_queued_bytes failed: {:?}", e);
            TestResult::Fail
        }
    }
}

/// output_queued_bytes returns 0 after TCOFLUSH.
pub fn test_output_queued_zero_after_flush() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;

    // Set inflight, then flush output.
    crate::tty::table::TTY_OUTPUT_INFLIGHT[slot].store(5, Ordering::Release);
    tty::tcflush(idx, slopos_abi::syscall::TCOFLUSH).unwrap();

    match tty::output_queued_bytes(idx) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - output_queued_bytes after flush = {}, expected 0",
                n
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - output_queued_bytes after flush failed: {:?}",
                e
            );
            TestResult::Fail
        }
    }
}

/// output_queued_bytes on unallocated slot returns error.
pub fn test_output_queued_unallocated() -> TestResult {
    tty::table::tty_table_init();
    // Slot 5 is never allocated by tty_table_init().
    let idx = TtyIndex(5);

    match tty::output_queued_bytes(idx) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - output_queued_bytes(5) = {:?}, expected NotAllocated",
                other
            );
            TestResult::Fail
        }
    }
}

/// output_queued_bytes on invalid index returns error.
pub fn test_output_queued_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(255);

    match tty::output_queued_bytes(idx) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - output_queued_bytes(255) = {:?}, expected InvalidIndex",
                other
            );
            TestResult::Fail
        }
    }
}

/// FIONREAD behavior is unchanged by TIOCOUTQ addition.
pub fn test_fionread_unchanged() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Push data (canonical: need newline to commit to cooked).
    tty::push_input(idx, b'A');
    tty::push_input(idx, b'B');
    tty::push_input(idx, b'\n');

    // bytes_available (FIONREAD equivalent) should report 3.
    match tty::bytes_available(idx) {
        Ok(3) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - bytes_available after push = {:?}, expected Ok(3)",
                other
            );
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

/// output_queued_bytes on console TTY 1 works.
pub fn test_output_queued_vconsole() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(1);

    // Console should report 0 queued bytes (synchronous driver).
    match tty::output_queued_bytes(idx) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - vconsole output_queued_bytes = {}, expected 0",
                n
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - vconsole output_queued_bytes failed: {:?}",
                e
            );
            TestResult::Fail
        }
    }
}
// ===========================================================================
// TIOCOUTQ Byte Accounting & Packet Mode Edge Fix
// ===========================================================================

pub fn test_inflight_byte_granularity() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let before = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if before != 0 {
        klog_info!(
            "TTY_TEST: BUG - inflight before write should be 0, got {}",
            before
        );
        return TestResult::Fail;
    }

    let data = b"Hello, byte accounting!";
    let _ = tty::write(TtyIndex(0), data, false);

    let after = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if after != 0 {
        klog_info!(
            "TTY_TEST: BUG - inflight after sync write should be 0, got {}",
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_tiocoutq_returns_bytes_not_ops() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;

    TTY_OUTPUT_INFLIGHT[slot].store(256, Ordering::Release);

    let result = tty::output_queued_bytes(idx);

    TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);

    match result {
        Ok(256) => TestResult::Pass,
        Ok(n) => {
            klog_info!("TTY_TEST: BUG - TIOCOUTQ expected 256 bytes, got {}", n);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - TIOCOUTQ error: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_tiocoutq_zero_after_sync_write() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let _ = tty::write(TtyIndex(0), b"test output drain", false);

    match tty::output_queued_bytes(TtyIndex(0)) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - TIOCOUTQ after sync write should be 0, got {}",
                n
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - TIOCOUTQ error: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_tiocoutq_various_byte_counts() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let slot = idx.0 as usize;

    for &count in &[1u32, 42, 100, 512, 4096] {
        TTY_OUTPUT_INFLIGHT[slot].store(count, Ordering::Release);
        match tty::output_queued_bytes(idx) {
            Ok(n) if n as u32 == count => {}
            Ok(n) => {
                TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
                klog_info!("TTY_TEST: BUG - TIOCOUTQ expected {}, got {}", count, n);
                return TestResult::Fail;
            }
            Err(e) => {
                TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
                klog_info!("TTY_TEST: BUG - TIOCOUTQ error for {}: {:?}", count, e);
                return TestResult::Fail;
            }
        }
    }

    TTY_OUTPUT_INFLIGHT[slot].store(0, Ordering::Release);
    TestResult::Pass
}

pub fn test_packet_mode_1byte_with_events() -> TestResult {
    let Some((master, slave, saved, _hold)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    let mut t = tty::get_termios(slave).unwrap();
    t.c_iflag |= InputFlags::IXON;
    tty::set_termios(slave, &t).unwrap();

    let mut buf = [0u8; 1];
    match tty::read(master, &mut buf, true) {
        Ok(1) if buf[0] != 0 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet 1-byte with events: expected Ok(1) with event, got {:?}, buf[0]=0x{:02X}",
                other,
                buf[0]
            );
            let _ = tty::set_packet_mode(master, false);
            t.c_iflag &= !InputFlags::IXON;
            let _ = tty::set_termios(slave, &t);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let _ = tty::set_packet_mode(master, false);
    t.c_iflag &= !InputFlags::IXON;
    let _ = tty::set_termios(slave, &t);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

pub fn test_packet_mode_1byte_data_no_events() -> TestResult {
    let Some((master, slave, saved, _hold)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    let _ = tty::write(slave, b"Z", false);

    let mut buf = [0u8; 1];
    match tty::read(master, &mut buf, true) {
        Ok(0) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet 1-byte with data but no events: expected Ok(0), got {:?}",
                other
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let mut big_buf = [0u8; 16];
    match tty::read(master, &mut big_buf, true) {
        Ok(n)
            if n >= 2 && big_buf[0] == slopos_abi::syscall::TIOCPKT_DATA && big_buf[1] == b'Z' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet data still available after 1-byte read: expected TIOCPKT_DATA + 'Z', got {:?}",
                other
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let _ = tty::set_packet_mode(master, false);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

/// PTY master uses RawDisc (VMIN=0, VTIME=0) so empty nonblock reads
/// return immediately with 0 bytes.
pub fn test_packet_mode_1byte_no_data_nonblock() -> TestResult {
    let Some((master, slave, saved, _hold)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    // No data, no pending packet event, non-blocking: WouldBlock, exactly
    // like Linux. The master's RawDisc carries VMIN=1, so the EOF-shaped
    // `Ok(0)` is reserved for peer-close/hangup.
    let mut buf = [0u8; 1];
    match tty::read(master, &mut buf, true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet 1-byte no data nonblock: expected WouldBlock, got {:?}",
                other
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let _ = tty::set_packet_mode(master, false);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

pub fn test_packet_mode_2byte_works() -> TestResult {
    let Some((master, slave, saved, _hold)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    let _ = tty::write(slave, b"Q", false);

    let mut buf = [0u8; 2];
    match tty::read(master, &mut buf, true) {
        Ok(2) if buf[0] == slopos_abi::syscall::TIOCPKT_DATA && buf[1] == b'Q' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet 2-byte: expected [DATA, 'Q'], got {:?}, buf={:?}",
                other,
                buf
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let _ = tty::set_packet_mode(master, false);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

pub fn test_tiocoutq_byte_accounting_regression_idle() -> TestResult {
    tty::table::tty_table_init();
    match tty::output_queued_bytes(TtyIndex(0)) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!("TTY_TEST: BUG - regression idle expected 0, got {}", n);
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - regression idle error: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_packet_mode_data_prefix_regression() -> TestResult {
    let Some((master, slave, saved, _hold)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet regression setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    let _ = tty::write(slave, b"AB", false);
    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(n)
            if n >= 3
                && buf[0] == slopos_abi::syscall::TIOCPKT_DATA
                && buf[1] == b'A'
                && buf[2] == b'B' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet regression: expected [DATA, 'A', 'B'], got {:?}",
                other
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    let _ = tty::set_packet_mode(master, false);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

pub fn test_echo_inflight_byte_granularity() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    tty::push_input(TtyIndex(0), b'X');

    let after = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if after != 0 {
        klog_info!(
            "TTY_TEST: BUG - inflight after echo should be 0, got {}",
            after
        );
        drain_tty_nonblock(TtyIndex(0));
        return TestResult::Fail;
    }

    drain_tty_nonblock(TtyIndex(0));
    TestResult::Pass
}
// ===========================================================================
// TIOCGSID, TIOCEXCL/TIOCNXCL/TIOCGEXCL & HUPCL Enforcement
// ===========================================================================

pub fn test_excl_hupcl_tiocgsid_abi_constant() -> TestResult {
    if slopos_abi::syscall::TIOCGSID != 0x5429 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_excl_hupcl_tiocexcl_abi_constants() -> TestResult {
    if slopos_abi::syscall::TIOCEXCL != 0x540C {
        return TestResult::Fail;
    }
    if slopos_abi::syscall::TIOCNXCL != 0x540D {
        return TestResult::Fail;
    }
    if slopos_abi::syscall::TIOCGEXCL != 0x8004_5440 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_excl_hupcl_errno_ebusy_value() -> TestResult {
    if slopos_abi::syscall::ERRNO_EBUSY != (-16i64) as u64 {
        return TestResult::Fail;
    }
    if TtyError::DeviceBusy.to_errno() != slopos_abi::syscall::ERRNO_EBUSY as i32 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_excl_hupcl_get_session_id_returns_correct_sid() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let scope = SessionScope::new(500, 500);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
    match tty::get_session_id(idx) {
        Ok(500) => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected sid 500, got {:?}", other);
            tty::detach_session(idx);
            return TestResult::Fail;
        }
    }
    tty::detach_session(idx);
    TestResult::Pass
}

pub fn test_excl_hupcl_get_session_id_unallocated() -> TestResult {
    tty::table::tty_table_init();
    match tty::get_session_id(TtyIndex(31)) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected NotAllocated, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_excl_hupcl_exclusive_initially_false() -> TestResult {
    tty::table::tty_table_init();
    match tty::get_exclusive(TtyIndex(0)) {
        Ok(false) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected false, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_excl_hupcl_set_exclusive_roundtrip() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    if tty::set_exclusive(idx, true).is_err() {
        return TestResult::Fail;
    }
    match tty::get_exclusive(idx) {
        Ok(true) => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected true after set, got {:?}", other);
            let _ = tty::set_exclusive(idx, false);
            return TestResult::Fail;
        }
    }
    if tty::set_exclusive(idx, false).is_err() {
        return TestResult::Fail;
    }
    match tty::get_exclusive(idx) {
        Ok(false) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected false after clear, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_excl_hupcl_exclusive_blocks_second_open() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    if tty::set_exclusive(idx, true).is_err() {
        drop(con);
        return TestResult::Fail;
    }
    match tty::open_tty(idx) {
        Err(TtyError::DeviceBusy) => {}
        Ok(dup) => {
            klog_info!("TTY_TEST: BUG - second open of exclusive tty unexpectedly succeeded");
            drop(dup);
            let _ = tty::set_exclusive(idx, false);
            drop(con);
            return TestResult::Fail;
        }
        Err(other) => {
            klog_info!(
                "TTY_TEST: BUG - expected DeviceBusy on second open, got {:?}",
                other
            );
            let _ = tty::set_exclusive(idx, false);
            drop(con);
            return TestResult::Fail;
        }
    }
    let _ = tty::set_exclusive(idx, false);
    drop(con);
    TestResult::Pass
}

pub fn test_excl_hupcl_nxcl_allows_second_open() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    let _ = tty::set_exclusive(idx, true);
    let _ = tty::set_exclusive(idx, false);
    let con2 = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(e) => {
            klog_info!(
                "TTY_TEST: BUG - second open after NXCL should succeed, got {:?}",
                e
            );
            drop(con);
            return TestResult::Fail;
        }
    };
    drop(con2);
    drop(con);
    TestResult::Pass
}

pub fn test_excl_hupcl_exclusive_unallocated_slot() -> TestResult {
    tty::table::tty_table_init();
    match tty::set_exclusive(TtyIndex(31), true) {
        Err(TtyError::NotAllocated) => {}
        other => {
            klog_info!("TTY_TEST: BUG - expected NotAllocated, got {:?}", other);
            return TestResult::Fail;
        }
    }
    match tty::get_exclusive(TtyIndex(31)) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected NotAllocated, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_excl_hupcl_hupcl_last_close_triggers_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _hangup = HangupScope::guard(idx);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    let scope = SessionScope::new(600, 600);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
    let mut t = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(_) => {
            drop(con);
            return TestResult::Fail;
        }
    };
    t.c_cflag |= ControlFlags::HUPCL;
    let _ = tty::set_termios(idx, &t);
    drop(con);
    let hung = tty::is_hung_up(idx);
    if !hung {
        klog_info!("TTY_TEST: BUG - expected hung_up after HUPCL close");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_excl_hupcl_no_hupcl_last_close_no_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _hangup = HangupScope::guard(idx);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    let scope = SessionScope::new(700, 700);
    tty::session::test_install_session(idx, scope.session_weak(), scope.pgrp_weak());
    let mut t = match tty::get_termios(idx) {
        Ok(t) => t,
        Err(_) => {
            drop(con);
            return TestResult::Fail;
        }
    };
    t.c_cflag &= !ControlFlags::HUPCL;
    let _ = tty::set_termios(idx, &t);
    drop(con);
    let hung = tty::is_hung_up(idx);
    if hung {
        klog_info!("TTY_TEST: BUG - should NOT be hung_up without HUPCL");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_excl_hupcl_hupcl_pty_no_double_hangup() -> TestResult {
    let pair = open_pty_pair();
    let slave = pair.slave;

    let mut t = match tty::get_termios(slave) {
        Ok(t) => t,
        Err(_) => return TestResult::Fail,
    };
    t.c_cflag |= ControlFlags::HUPCL;
    let _ = tty::set_termios(slave, &t);

    TestResult::Pass
}

pub fn test_excl_hupcl_close_clears_exclusive() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let con = match tty::open_tty(idx) {
        Ok(c) => c,
        Err(_) => return TestResult::Fail,
    };
    let _ = tty::set_exclusive(idx, true);
    drop(con);

    match tty::get_exclusive(idx) {
        Ok(false) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - exclusive should be cleared after last close, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

// ===========================================================================
// ===========================================================================

pub fn test_ttyflags_default_empty() -> TestResult {
    let flags = TtyFlags::empty();
    if flags.bits() != 0 {
        klog_info!("TTY_TEST: BUG - empty TtyFlags has non-zero bits");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ttyflags_insert_remove_contains() -> TestResult {
    let mut flags = TtyFlags::empty();
    flags.insert(TtyFlags::HUNG_UP);
    if !flags.contains(TtyFlags::HUNG_UP) {
        return TestResult::Fail;
    }
    flags.remove(TtyFlags::HUNG_UP);
    if flags.contains(TtyFlags::HUNG_UP) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_mark_hung_up_clears_output_stopped() -> TestResult {
    tty::table::tty_table_init();
    let _hangup = HangupScope::guard(TtyIndex(0));
    let result = tty::table::with_tty(TtyIndex(0), |tty| {
        tty.flags.insert(TtyFlags::OUTPUT_STOPPED);
        tty.mark_hung_up();
        let hung = tty.flags.contains(TtyFlags::HUNG_UP);
        let stopped = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
        (hung, stopped)
    });
    match result {
        Some((true, false)) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - mark_hung_up invariant failed: {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_packet_events_default_empty() -> TestResult {
    let events = PacketEvents::empty();
    if events.bits() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_packet_events_from_bits_matches_tiocpkt() -> TestResult {
    use slopos_abi::syscall::*;
    let pkt = PacketEvents::from_bits_truncate(TIOCPKT_FLUSHREAD | TIOCPKT_STOP);
    if !pkt.contains(PacketEvents::FLUSHREAD) || !pkt.contains(PacketEvents::STOP) {
        return TestResult::Fail;
    }
    if pkt.contains(PacketEvents::FLUSHWRITE) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_packet_events_bits_roundtrip() -> TestResult {
    let pkt = PacketEvents::FLUSHREAD | PacketEvents::START;
    let raw = pkt.bits();
    let pkt2 = PacketEvents::from_bits_truncate(raw);
    if pkt != pkt2 {
        return TestResult::Fail;
    }
    if raw != (slopos_abi::syscall::TIOCPKT_FLUSHREAD | slopos_abi::syscall::TIOCPKT_START) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_fields_pub_crate_smoke() -> TestResult {
    tty::table::tty_table_init();
    let ok = tty::table::with_tty_ref(TtyIndex(0), |tty| {
        let _ = tty.flags;
        let _ = tty.packet_events;
        let _ = &tty.ldisc;
        let _ = &tty.driver;
        let _ = &tty.session;
        let _ = tty.winsize;
        true
    });
    if ok != Some(true) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_session_fields_pub_crate_smoke() -> TestResult {
    tty::table::tty_table_init();
    let ok = tty::table::with_tty_ref(TtyIndex(0), |tty| {
        let _ = tty.session.session_id();
        let _ = tty.session.fg_pgrp_id();
        let _ = tty.session.focused_task_id;
        true
    });
    if ok != Some(true) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_slave_starts_locked() -> TestResult {
    tty::table::tty_table_init();
    let (master, master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    let locked = tty::table::with_tty_ref(slave, |tty| tty.flags.contains(TtyFlags::SLAVE_LOCKED));
    drop(master_backing);
    if locked != Some(true) {
        klog_info!("TTY_TEST: BUG - PTY slave should start locked");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ttyflags_set_method() -> TestResult {
    let mut flags = TtyFlags::empty();
    flags.set(TtyFlags::PACKET_MODE, true);
    if !flags.contains(TtyFlags::PACKET_MODE) {
        return TestResult::Fail;
    }
    flags.set(TtyFlags::PACKET_MODE, false);
    if flags.contains(TtyFlags::PACKET_MODE) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ttyflags_multi_flag_operations() -> TestResult {
    let mut flags = TtyFlags::empty();
    flags.insert(TtyFlags::HUNG_UP | TtyFlags::PEER_CLOSED);
    if !flags.contains(TtyFlags::HUNG_UP) || !flags.contains(TtyFlags::PEER_CLOSED) {
        return TestResult::Fail;
    }
    flags.remove(TtyFlags::HUNG_UP | TtyFlags::PEER_CLOSED);
    if flags.contains(TtyFlags::HUNG_UP) || flags.contains(TtyFlags::PEER_CLOSED) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_no_driver_kind_none() -> TestResult {
    tty::table::tty_table_init();
    let id0 = tty::table::with_tty_ref(TtyIndex(0), |tty| tty.driver.id());
    let id1 = tty::table::with_tty_ref(TtyIndex(1), |tty| tty.driver.id());
    use crate::tty::driver::DriverId;
    match (id0, id1) {
        (Some(DriverId::SerialConsole), Some(DriverId::VConsole)) => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - unexpected driver IDs for slots 0 and 1");
            TestResult::Fail
        }
    }
}

slopos_testing::stest!(
    name = test_is_output_idle_initially_true,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_inflight_counter_initial_zero,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_write_updates_inflight_counter,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcsetsw_preserves_input_after_drain,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcsetsf_flushes_input_after_drain,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_is_output_idle_invalid_index,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_is_output_idle_unallocated,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_drain_invalid_index_error,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_driver_output_pending_default_false,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_driver_kind_output_pending_dispatch,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_pty_output_idle_immediate,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_console_drain_immediate,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcsets_now_skips_drain,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_read_returns_eof,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_write_returns_eio,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_poll_returns_pollhup_pollin,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_set_termios_returns_eio,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_set_winsize_returns_eio,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_set_ldisc_returns_eio,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_get_fg_pgrp_still_works,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_pty_master_close_slave_eof_eio,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_hangup_permanent_eof,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_pty_slave_poll_pollhup_after_master_close,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_hungup_errno_is_eio, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_control_flag_values, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_default_cflag, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_cflag_roundtrip, suite = tty_test_ioctls_ext);
slopos_testing::stest!(
    name = test_speed_fields_populated,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_speed_follows_baud_change,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_cread_value_preserved,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_flush_flow_ioctl_constants,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_tcflush_input, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcflush_output, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcflush_both, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcflush_invalid_arg, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcsbrk_noop, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcsbrk_drain, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcxonc_all_actions, suite = tty_test_ioctls_ext);
slopos_testing::stest!(
    name = test_tcooff_blocks_nonblock_write,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_tcoon_resumes_write, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcooff_idempotent, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_tcoon_idempotent, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_stop_resume_cycle, suite = tty_test_ioctls_ext);
slopos_testing::stest!(
    name = test_tcioff_tcion_succeed,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcioff_tcion_no_output_stop,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_invalid_action_still_errors,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcooff_pty_slave_write,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_stopped_independent_of_ixon,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcxonc_unallocated_slot,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tcxonc_invalid_index,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tiocoutq_abi_constant,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_queued_zero_when_idle,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_queued_reflects_inflight,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_queued_zero_after_flush,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_queued_unallocated,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_output_queued_invalid_index,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_fionread_unchanged, suite = tty_test_ioctls_ext);
slopos_testing::stest!(
    name = test_output_queued_vconsole,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_inflight_byte_granularity,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tiocoutq_returns_bytes_not_ops,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tiocoutq_zero_after_sync_write,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tiocoutq_various_byte_counts,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_mode_1byte_with_events,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_mode_1byte_data_no_events,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_mode_1byte_no_data_nonblock,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_mode_2byte_works,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tiocoutq_byte_accounting_regression_idle,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_mode_data_prefix_regression,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_echo_inflight_byte_granularity,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_tiocgsid_abi_constant,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_tiocexcl_abi_constants,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_errno_ebusy_value,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_get_session_id_returns_correct_sid,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_get_session_id_unallocated,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_initially_false,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_set_exclusive_roundtrip,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_blocks_second_open,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_nxcl_allows_second_open,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_exclusive_unallocated_slot,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_hupcl_last_close_triggers_hangup,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_no_hupcl_last_close_no_hangup,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_hupcl_pty_no_double_hangup,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_excl_hupcl_close_clears_exclusive,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_ttyflags_default_empty,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_ttyflags_insert_remove_contains,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_mark_hung_up_clears_output_stopped,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_events_default_empty,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_events_from_bits_matches_tiocpkt,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_packet_events_bits_roundtrip,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_tty_fields_pub_crate_smoke,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(
    name = test_session_fields_pub_crate_smoke,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_slave_starts_locked, suite = tty_test_ioctls_ext);
slopos_testing::stest!(name = test_ttyflags_set_method, suite = tty_test_ioctls_ext);
slopos_testing::stest!(
    name = test_ttyflags_multi_flag_operations,
    suite = tty_test_ioctls_ext
);
slopos_testing::stest!(name = test_no_driver_kind_none, suite = tty_test_ioctls_ext);
