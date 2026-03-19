use super::*;

pub fn test_pty_data_roundtrip() -> TestResult {
    tty::table::tty_table_init();
    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => return TestResult::Fail,
    };
    let slave = TtyIndex(match tty::get_pty_number(master) {
        Ok(n) => n as u8,
        Err(_) => return TestResult::Fail,
    });

    if tty::open_ref(master).is_err() || tty::open_ref(slave).is_err() {
        return TestResult::Fail;
    }

    let saved = match tty::get_termios(slave) {
        Ok(t) => t,
        Err(_) => return TestResult::Fail,
    };
    let mut raw = saved;
    raw.c_lflag &= !(slopos_abi::syscall::ICANON | slopos_abi::syscall::ECHO);
    if tty::set_termios(slave, &raw).is_err() {
        return TestResult::Fail;
    }

    let write_rc = tty::write(master, b"roundtrip", false);
    let mut out = [0u8; 16];
    let read_rc = tty::read(slave, &mut out, true);

    let _ = tty::set_termios(slave, &saved);
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    if write_rc != Ok(9) || read_rc != Ok(9) || &out[..9] != b"roundtrip" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_pty_hangup_propagation() -> TestResult {
    tty::table::tty_table_init();
    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => return TestResult::Fail,
    };
    let slave = TtyIndex(match tty::get_pty_number(master) {
        Ok(n) => n as u8,
        Err(_) => return TestResult::Fail,
    });

    if tty::open_ref(master).is_err() || tty::open_ref(slave).is_err() {
        return TestResult::Fail;
    }

    let _ = tty::close_ref(master);
    let events = tty::poll_events(
        slave,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLHUP,
    );
    let _ = tty::close_ref(slave);

    if (events & slopos_abi::syscall::POLLHUP) == 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_errno_background_maps_to_eio() -> TestResult {
    use slopos_abi::syscall::ERRNO_EIO;

    if TtyError::BackgroundRead.to_errno() != ERRNO_EIO as i32 {
        return TestResult::Fail;
    }
    if TtyError::BackgroundWrite.to_errno() != ERRNO_EIO as i32 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_ringbuf_integration() -> TestResult {
    let mut ld = LineDisc::new();
    let mut termios = *ld.termios();
    termios.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&termios);

    for &b in b"ringbuf" {
        let _ = ld.input_char(b);
    }

    let mut out = [0u8; 16];
    let n = ld.read(&mut out);
    if n != 7 || &out[..7] != b"ringbuf" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_echo_batching_correctness() -> TestResult {
    let mut ld = LineDisc::new();
    let mut echoed = [0u8; 16];
    let mut len = 0usize;

    for &b in b"ab" {
        if let InputAction::Echo { buf, len: n } = ld.input_char(b) {
            let n = n as usize;
            echoed[len..len + n].copy_from_slice(&buf[..n]);
            len += n;
        }
    }

    if let InputAction::Echo { buf, len: n } = ld.input_char(0x08) {
        let n = n as usize;
        echoed[len..len + n].copy_from_slice(&buf[..n]);
        len += n;
    } else {
        return TestResult::Fail;
    }

    if let InputAction::Echo { buf, len: n } = ld.input_char(b'\n') {
        let n = n as usize;
        echoed[len..len + n].copy_from_slice(&buf[..n]);
        len += n;
    } else {
        return TestResult::Fail;
    }

    if &echoed[..len] != b"ab\x08 \x08\n" {
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

/// set_compositor_focus / get_compositor_focus round-trip.
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
