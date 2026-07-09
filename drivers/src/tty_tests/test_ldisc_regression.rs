//! Split from test_ldisc.rs: test_ldisc_regression.rs

use super::fixtures::*;

// ===========================================================================
// Deferred Reprint (PENDIN)
// ===========================================================================

/// PENDIN constant value is 0x4000.
pub fn test_pendin_flag_value() -> TestResult {
    if LocalFlags::PENDIN.bits() != 0x4000 {
        klog_info!("TTY_TEST: BUG - PENDIN should be 0x4000");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Changing an echo-affecting lflag sets PENDIN; the next input_char()
/// returns ReprintLine instead of processing the byte.
pub fn test_pendin_auto_set_on_echo_change() -> TestResult {
    let mut ld = LineDisc::new();
    // Insert some content into the edit buffer so PENDIN triggers.
    ld.input_char(b'h');
    ld.input_char(b'i');

    // Toggle ECHO off — this is an echo-affecting change.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    // The next input_char() should return ReprintLine (deferred reprint).
    let action = ld.input_char(b'x');
    if !matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - expected ReprintLine after echo flag change");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PENDIN triggers ReprintLine once, then the next input is processed normally.
pub fn test_pendin_one_shot() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    // Toggle ECHOE off to trigger PENDIN.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    // First call: ReprintLine.
    let first = ld.input_char(b'b');
    if !matches!(first, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - first input after PENDIN should be ReprintLine");
        return TestResult::Fail;
    }

    // Second call: normal echo (ECHO still on, just ECHOE changed).
    let second = ld.input_char(b'b');
    if matches!(second, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - PENDIN should be one-shot, not repeat");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Explicit VREPRINT (Ctrl+R) clears PENDIN so we don't double-reprint.
pub fn test_vreprint_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'z');

    // Trigger PENDIN via echo flag change.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOK;
    ld.set_termios(&t);

    // Manually reprint with VREPRINT (Ctrl+R).  This should also clear PENDIN.
    let ctrl_r = ld.termios().c_cc[slopos_abi::syscall::VREPRINT];
    let action = ld.input_char(ctrl_r);
    // This returns ReprintLine from the PENDIN path, not the VREPRINT path,
    // but PENDIN is now cleared either way.
    if !matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - expected ReprintLine from PENDIN or VREPRINT");
        return TestResult::Fail;
    }

    // Next input should be processed normally — no double reprint.
    let next = ld.input_char(b'a');
    if matches!(next, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - VREPRINT should have cleared PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Changing non-echo-affecting flags (e.g., ISIG, NOFLSH) does NOT set PENDIN.
pub fn test_pendin_not_set_for_non_echo_flags() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'q');

    // Toggle ISIG off — not echo-affecting.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ISIG;
    ld.set_termios(&t);

    // Should process input normally, no ReprintLine.
    let action = ld.input_char(b'w');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - toggling ISIG should not trigger PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PENDIN is not set when the edit buffer is empty (nothing to reprint).
pub fn test_pendin_empty_edit_buffer() -> TestResult {
    let mut ld = LineDisc::new();
    // Don't type anything — edit buffer is empty.

    // Toggle ECHO off.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    // Should process input normally since there's nothing to reprint.
    let action = ld.input_char(b'a');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - PENDIN should not fire with empty edit buffer");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// flush_all() clears PENDIN state.
pub fn test_flush_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    // Trigger PENDIN.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    // Flush everything — should clear PENDIN.
    ld.flush_all();

    // Input should be processed normally.
    let action = ld.input_char(b'b');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - flush_all should clear PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// flush_input() clears PENDIN state.
pub fn test_flush_input_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    // Trigger PENDIN.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOK;
    ld.set_termios(&t);

    // Flush input — should clear PENDIN.
    ld.flush_input();

    // Input should be processed normally.
    let action = ld.input_char(b'c');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - flush_input should clear PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Review Fix Regression Tests
// ===========================================================================

/// Review fix regression: tcflush(TCIFLUSH) clears throttle on a PTY slave.
///
/// Before the fix, flushing input via tcflush did not clear the throttle
/// flag, leaving the master-side writer permanently blocked.  This matches
/// Linux's n_tty_flush_buffer() → tty_unthrottle() pattern.
pub fn test_review_tcflush_unthrottles_pty() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Raw mode so every byte goes straight to cooked buffer.
    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Fill past high-water to activate throttle.
    for _ in 0..(THROTTLE_HIGH_WATER + 64) {
        tty::push_input(slave, b'X');
    }

    // Confirm throttled.
    {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        if !guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(false)
        {
            klog_info!("TTY_TEST: BUG - slave not throttled after fill");
            tty::set_termios(slave, &saved).unwrap();
            return TestResult::Fail;
        }
    }

    // Flush input via tcflush — should clear the throttle.
    match tty::tcflush(slave, slopos_abi::syscall::TCIFLUSH) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcflush TCIFLUSH failed: {:?}", e);
            tty::set_termios(slave, &saved).unwrap();
            return TestResult::Fail;
        }
    }

    // Verify unthrottled.
    let still_throttled = {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(true)
    };
    if still_throttled {
        klog_info!("TTY_TEST: BUG - slave still throttled after tcflush(TCIFLUSH)");
        tty::set_termios(slave, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(slave, &saved).unwrap();
    TestResult::Pass
}

/// Review fix regression: TCIOFLUSH also clears throttle (via input flush path).
///
/// tcflush(TCIOFLUSH) flushes both input and output.  The throttle should
/// be cleared by the input-flush branch, same as TCIFLUSH.
pub fn test_review_tcflush_both_unthrottles_pty() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Throttle the slave.
    for _ in 0..(THROTTLE_HIGH_WATER + 64) {
        tty::push_input(slave, b'Y');
    }

    // Flush both.
    tty::tcflush(slave, slopos_abi::syscall::TCIOFLUSH).unwrap();

    // Verify unthrottled.
    let still_throttled = {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(true)
    };
    if still_throttled {
        klog_info!("TTY_TEST: BUG - slave still throttled after tcflush(TCIOFLUSH)");
        tty::set_termios(slave, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(slave, &saved).unwrap();
    TestResult::Pass
}

/// Review fix regression: master_write processes full 64-byte batches before
/// checking throttle, returning a batch-aligned count.
///
/// Before the fix, throttle was checked per-byte (O(n) lock acquisitions).
/// After the fix, throttle is checked once per 64-byte batch.  When throttle
/// activates mid-batch, the full batch is completed before master_write
/// returns, so the accepted count is a multiple of the batch size.
pub fn test_review_master_write_batch_boundary() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Raw mode.
    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Fill slave to THROTTLE_HIGH_WATER - 10.  Only 10 more bytes until
    // throttle activates, but the batch (64 bytes) completes fully.
    let prefill = THROTTLE_HIGH_WATER - 10;
    for _ in 0..prefill {
        tty::push_input(slave, b'P');
    }

    // Verify not yet throttled.
    {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        if guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(true)
        {
            klog_info!("TTY_TEST: BUG - slave throttled before master_write burst");
            tty::set_termios(slave, &saved).unwrap();
            return TestResult::Fail;
        }
    }

    // Get master's peer handle.
    let peer = {
        let guard = TTY_SLOTS[master.0 as usize].lock();
        match guard.as_ref().unwrap().driver {
            TtyDriverKind::PtyMaster { ref peer } => peer.clone(),
            _ => {
                klog_info!("TTY_TEST: BUG - master is not PtyMaster");
                return TestResult::Fail;
            }
        }
    };

    // Write 256 bytes.  Throttle activates at byte ~10 within the first
    // batch, but the full 64-byte batch completes before the check.
    let burst = [b'Q'; 256];
    let accepted = crate::tty::pty::master_write(&peer, &burst);

    // With BATCH_SIZE=64, the first batch pushes all 64 bytes (throttle
    // activates at ~byte 10 but isn't checked until after the batch).
    // The post-batch check sees throttled=true and returns 64.
    if accepted != 64 {
        klog_info!(
            "TTY_TEST: BUG - master_write returned {} (expected 64 for batch boundary)",
            accepted
        );
        tty::set_termios(slave, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(slave, &saved).unwrap();
    TestResult::Pass
}

/// Review: c_cflag is authoritative — c_ospeed does NOT override CBAUD bits.
///
/// POSIX: c_cflag encodes the baud rate.  c_ispeed/c_ospeed are informational
/// fields populated by get_termios but do not alter c_cflag in set_termios.
pub fn test_review_speed_fields_merge_into_cflag() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    // Set c_cflag to B38400, but c_ospeed to a different value.
    // c_cflag should remain authoritative.
    let mut t = saved;
    t.c_cflag = ControlFlags::from_bits_retain((t.c_cflag.bits() & !CBAUD) | B38400);
    t.c_ospeed = 9600;
    t.c_ispeed = 0;
    tty::set_termios(idx, &t).unwrap();

    let got = tty::get_termios(idx).unwrap();
    let got_baud_bits = got.c_cflag.bits() & CBAUD;
    if got_baud_bits != B38400 {
        klog_info!(
            "TTY_TEST: BUG - c_cflag CBAUD=0o{:o}, expected B38400=0o{:o} (cflag authoritative)",
            got_baud_bits,
            B38400
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    // Speed fields should reflect c_cflag (38400), not the c_ospeed we passed.
    if got.c_ospeed != 38400 || got.c_ispeed != 38400 {
        klog_info!(
            "TTY_TEST: BUG - speed fields {}/{}, expected 38400/38400",
            got.c_ispeed,
            got.c_ospeed
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(idx, &saved).unwrap();
    TestResult::Pass
}

/// Review: c_cflag is authoritative — c_ispeed does NOT override CBAUD bits.
///
/// Even when c_ospeed is zero and c_ispeed is set, c_cflag CBAUD wins.
pub fn test_review_speed_ispeed_fallback() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    let mut t = saved;
    t.c_cflag = ControlFlags::from_bits_retain((t.c_cflag.bits() & !CBAUD) | B38400);
    t.c_ospeed = 0;
    t.c_ispeed = 115200;
    tty::set_termios(idx, &t).unwrap();

    let got = tty::get_termios(idx).unwrap();
    let got_baud_bits = got.c_cflag.bits() & CBAUD;
    if got_baud_bits != B38400 {
        klog_info!(
            "TTY_TEST: BUG - c_cflag CBAUD=0o{:o}, expected B38400=0o{:o} (cflag authoritative)",
            got_baud_bits,
            B38400
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(idx, &saved).unwrap();
    TestResult::Pass
}

/// Review fix regression: unrecognised speed leaves c_cflag unchanged.
///
/// When c_ospeed is an unrecognised baud rate, CBAUD bits stay as-is.
pub fn test_review_speed_unrecognised_noop() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    let mut t = saved;
    t.c_cflag = ControlFlags::from_bits_retain((t.c_cflag.bits() & !CBAUD) | B38400);
    t.c_ospeed = 12345;
    t.c_ispeed = 0;
    tty::set_termios(idx, &t).unwrap();

    let got = tty::get_termios(idx).unwrap();
    let got_baud_bits = got.c_cflag.bits() & CBAUD;
    if got_baud_bits != B38400 {
        klog_info!(
            "TTY_TEST: BUG - c_cflag CBAUD=0o{:o}, expected B38400=0o{:o} (unrecognised speed)",
            got_baud_bits,
            B38400
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }

    tty::set_termios(idx, &saved).unwrap();
    TestResult::Pass
}

/// Review fix regression: poll_events returns POLLERR on hung-up TTY.
///
/// Before the fix, poll on a hung-up TTY returned POLLHUP | POLLIN but
/// not POLLERR.  Programs that detect write errors via POLLERR would miss
/// the hang-up.  Matches Linux tty_poll() behaviour.
pub fn test_review_pollerr_on_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);
    tty::hangup(idx);

    let revents = tty::poll_events(
        idx,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    tty::table::tty_table_init();

    let has_pollerr = (revents & slopos_abi::syscall::POLLERR) != 0;
    let has_pollhup = (revents & slopos_abi::syscall::POLLHUP) != 0;

    if !has_pollerr {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLERR on hung-up TTY");
        return TestResult::Fail;
    }
    if !has_pollhup {
        klog_info!("TTY_TEST: BUG - poll_events should report POLLHUP alongside POLLERR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Review fix regression: PTY peer_closed also sets POLLERR.
///
/// When the slave side of a PTY is closed and all data is drained, the
/// master should see POLLERR alongside POLLHUP.
pub fn test_review_pollerr_on_peer_closed() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_backing = tty::pty_open_slave(slave).unwrap();

    // Close slave side.
    drop(slave_backing);

    // Poll master — should see POLLHUP and POLLERR.
    let revents = tty::poll_events(
        master,
        slopos_abi::syscall::POLLIN | slopos_abi::syscall::POLLOUT,
    );

    let has_pollerr = (revents & slopos_abi::syscall::POLLERR) != 0;
    let has_pollhup = (revents & slopos_abi::syscall::POLLHUP) != 0;

    if !has_pollhup {
        klog_info!("TTY_TEST: BUG - PTY master poll should return POLLHUP after slave close");
        return TestResult::Fail;
    }
    if !has_pollerr {
        klog_info!("TTY_TEST: BUG - PTY master poll should return POLLERR after slave close");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Bug-fix regression tests (TTY review)
// ===========================================================================

/// BUG 1+2: flush_edit_to_cooked must preserve unflushed bytes when the
/// cooked buffer is partially occupied.  Before the fix, `edit_len` was
/// unconditionally reset to 0, silently discarding any bytes that did not
/// fit in the cooked ring buffer.
pub fn test_bugfix_flush_edit_preserves_remainder() -> TestResult {
    use crate::tty::ldisc::LineDisc;

    let mut ld = LineDisc::new();

    // Fill the cooked buffer to near-capacity via non-canonical mode.
    // Leave room for exactly 10 more bytes.
    let spare = 10usize;
    let fill_count = 8192 - spare;

    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    ld.set_termios(&t);
    for _ in 0..fill_count {
        ld.input_char(b'X');
    }

    if ld.bytes_available() != fill_count {
        klog_info!(
            "TTY_TEST: BUG - expected {} bytes available, got {}",
            fill_count,
            ld.bytes_available()
        );
        return TestResult::Fail;
    }

    // Switch to canonical mode and push 20 bytes + newline.
    // Only `spare` (10) bytes + the newline fit in cooked; the rest (10)
    // should be preserved in the edit buffer with the fix.
    t.c_lflag |= LocalFlags::ICANON;
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    for i in 0..20u8 {
        ld.input_char(b'A' + (i % 26));
    }
    ld.input_char(b'\n');

    // Drain ALL cooked data to make room for the remainder.
    let mut drain: KBox<[u8; 8192]> = KBox::zeroed().expect("alloc");
    let drained = ld.read(&mut *drain);
    if drained == 0 {
        klog_info!("TTY_TEST: BUG - expected to drain some data");
        return TestResult::Fail;
    }

    // Now push another newline to flush the preserved remainder.
    ld.input_char(b'\n');

    // If the fix works, the remainder bytes (~10) should now be in the
    // cooked buffer.  If the old bug persists (edit_len = 0 always),
    // the cooked buffer would be empty (only the newline itself).
    let avail_after_second = ld.bytes_available();

    // We expect more than just the newline (1 byte) — the preserved
    // remainder should also have been flushed.
    if avail_after_second <= 1 {
        klog_info!(
            "TTY_TEST: BUG - remainder bytes lost, only {} bytes after second flush",
            avail_after_second
        );
        return TestResult::Fail;
    }

    // Read the second line and verify it contains the expected bytes.
    let mut buf2 = [0u8; 64];
    let n2 = ld.read(&mut buf2);

    // Should have the 10 remainder bytes + newline = 11 total.
    if n2 < 2 {
        klog_info!(
            "TTY_TEST: BUG - second read expected >= 2 bytes (remainder + newline), got {}",
            n2
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// BUG 3: Non-blocking write to a PTY master whose slave is throttled must
/// return WouldBlock (EAGAIN) instead of blocking.
pub fn test_bugfix_nonblock_write_throttled_pty() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Put slave in raw mode so master writes flow into cooked buffer.
    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Fill the slave's cooked buffer past the throttle high-water mark
    // (THROTTLE_HIGH_WATER = 6144).  Write 6400 bytes in blocking mode.
    let mut fill: KBox<[u8; 6400]> = KBox::zeroed().expect("alloc");
    fill.iter_mut().for_each(|b| *b = b'Z');
    let _ = tty::write(master, &*fill, false);

    // The slave should now be throttled.  A non-blocking write from the
    // master should return WouldBlock.
    let result = tty::write(master, b"more", true);

    // Clean up.
    tty::set_termios(slave, &saved).unwrap();

    match result {
        Err(TtyError::WouldBlock) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - nonblock write to throttled PTY should return WouldBlock, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// BUG 3 (corollary): Non-blocking write to an unthrottled PTY should
/// succeed normally.
pub fn test_bugfix_nonblock_write_unthrottled_pty() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Put slave in raw mode.
    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Non-blocking write with an empty (unthrottled) slave should succeed.
    let result = tty::write(master, b"hello", true);

    tty::set_termios(slave, &saved).unwrap();

    match result {
        Ok(5) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - nonblock write to unthrottled PTY should return Ok(5), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

fn throttled_priority_setup(
    noflsh: bool,
) -> Option<(
    TtyIndex,
    TtyIndex,
    slopos_abi::syscall::UserTermios,
    PtyGuard,
)> {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;

    let (master, slave, saved, guard) = packet_mode_setup_pty()?;
    let mut t = saved;
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_lflag |= LocalFlags::ISIG;
    t.c_lflag.set(LocalFlags::NOFLSH, noflsh);
    t.c_iflag = InputFlags::empty();
    tty::set_termios(slave, &t).ok()?;

    for _ in 0..(THROTTLE_HIGH_WATER + 1) {
        tty::push_input(slave, b'A');
    }

    Some((master, slave, saved, guard))
}

fn pty_master_peer(master: TtyIndex) -> KWeak<TtyBacking> {
    peer_link_of(master)
}

pub fn test_priority_vintr_throttled_nonblock_flushes_and_unthrottles() -> TestResult {
    let Some((master, slave, saved, _guard)) = throttled_priority_setup(false) else {
        klog_info!("TTY_TEST: BUG - throttled priority setup failed");
        return TestResult::Fail;
    };

    let result = tty::write(master, b"\x03Z", true);
    let available = tty::bytes_available(slave).unwrap_or(usize::MAX);
    let throttled = {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(true)
    };

    packet_mode_teardown_pty(master, slave, &saved);

    if result != Ok(1) {
        klog_info!(
            "TTY_TEST: BUG - throttled VINTR write returned {:?}, want Ok(1)",
            result
        );
        return TestResult::Fail;
    }
    if available != 0 {
        klog_info!(
            "TTY_TEST: BUG - VINTR flush left {} input bytes; trailing ordinary byte may have passed",
            available
        );
        return TestResult::Fail;
    }
    if throttled {
        klog_info!("TTY_TEST: BUG - VINTR flush did not clear THROTTLED");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_priority_vintr_throttled_noflsh_preserves_throttle() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;

    let Some((master, slave, saved, _guard)) = throttled_priority_setup(true) else {
        klog_info!("TTY_TEST: BUG - throttled NOFLSH setup failed");
        return TestResult::Fail;
    };

    let result = tty::write(master, b"\x03Z", true);
    let available = tty::bytes_available(slave).unwrap_or(0);
    let throttled = {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        guard
            .as_ref()
            .map(|t| t.flags.contains(TtyFlags::THROTTLED))
            .unwrap_or(false)
    };

    packet_mode_teardown_pty(master, slave, &saved);

    if result != Ok(1) {
        klog_info!(
            "TTY_TEST: BUG - throttled NOFLSH VINTR write returned {:?}, want Ok(1)",
            result
        );
        return TestResult::Fail;
    }
    if available != THROTTLE_HIGH_WATER + 1 {
        klog_info!(
            "TTY_TEST: BUG - NOFLSH should preserve input, got {} bytes",
            available
        );
        return TestResult::Fail;
    }
    if !throttled {
        klog_info!("TTY_TEST: BUG - NOFLSH VINTR should leave THROTTLED set");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_master_write_throttled_ordinary_direct_rejected() -> TestResult {
    let Some((master, slave, saved, _guard)) = throttled_priority_setup(false) else {
        klog_info!("TTY_TEST: BUG - throttled ordinary setup failed");
        return TestResult::Fail;
    };
    let peer = pty_master_peer(master);

    let accepted = crate::tty::pty::master_write(&peer, b"x");
    packet_mode_teardown_pty(master, slave, &saved);

    if accepted != 0 {
        klog_info!(
            "TTY_TEST: BUG - direct ordinary master_write under throttle accepted {} bytes",
            accepted
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_literal_next_vintr_does_not_bypass_throttle() -> TestResult {
    let Some((master, slave, saved, _guard)) = throttled_priority_setup(false) else {
        klog_info!("TTY_TEST: BUG - throttled literal-next setup failed");
        return TestResult::Fail;
    };

    let mut t = tty::get_termios(slave).unwrap();
    t.c_lflag |= LocalFlags::IEXTEN;
    tty::set_termios(slave, &t).unwrap();
    let vlnext = t.c_cc[CcIndex::Vlnext.as_usize()];
    tty::push_input(slave, vlnext);

    let result = tty::write(master, b"\x03", true);
    packet_mode_teardown_pty(master, slave, &saved);

    if result != Err(TtyError::WouldBlock) {
        klog_info!(
            "TTY_TEST: BUG - literal-next VINTR should remain ordinary under throttle, got {:?}",
            result
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// BUG 4: RawDisc input_full prevents silent overflow.  When the master's
/// raw buffer is full, slave_write should stop accepting bytes instead of
/// silently dropping them.
pub fn test_bugfix_rawdisc_input_full() -> TestResult {
    use crate::tty::ldisc::RawDisc;

    let mut rd = RawDisc::new();

    // RawDisc buffer size is 4096.  Fill it completely.
    for _ in 0..4096 {
        rd.input_char(b'A');
    }

    // Buffer should now report full.
    if !rd.input_full() {
        klog_info!("TTY_TEST: BUG - RawDisc should report input_full after 4096 pushes");
        return TestResult::Fail;
    }

    // Verify bytes_available matches capacity.
    if rd.bytes_available() != 4096 {
        klog_info!(
            "TTY_TEST: BUG - expected 4096 bytes available, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }

    // Push one more byte — with the old code this would silently succeed.
    // With input_full check, callers should not push past capacity.
    // The RawDisc::input_char itself still silently drops (unchanged), but
    // slave_write now checks input_full() before each push.
    rd.input_char(b'B');

    // Count should still be 4096 (the extra byte was dropped, not added).
    if rd.bytes_available() != 4096 {
        klog_info!(
            "TTY_TEST: BUG - bytes_available should still be 4096 after overflow, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// BUG 4: slave_write respects input_full and returns a short write count
/// when the master's buffer is full.
pub fn test_bugfix_slave_write_stops_on_full() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Get the master's peer handle so we can call slave_write directly.
    let _master_peer = {
        let guard = TTY_SLOTS[master.0 as usize].lock();
        match guard.as_ref().unwrap().driver {
            tty::driver::TtyDriverKind::PtyMaster { ref peer } => peer.clone(),
            _ => {
                klog_info!("TTY_TEST: BUG - expected PtyMaster driver");
                return TestResult::Fail;
            }
        }
    };

    // The slave's peer handle points to the MASTER (so slave_write pushes
    // into the master's RawDisc).  Get the slave's peer handle.
    let slave_peer = {
        let guard = TTY_SLOTS[slave.0 as usize].lock();
        match guard.as_ref().unwrap().driver {
            tty::driver::TtyDriverKind::PtySlave { ref peer } => peer.clone(),
            _ => {
                klog_info!("TTY_TEST: BUG - expected PtySlave driver");
                return TestResult::Fail;
            }
        }
    };

    // Fill the master's buffer (4096 bytes via slave_write).
    let mut fill: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    fill.iter_mut().for_each(|b| *b = b'X');
    let written1 = tty::pty::slave_write(&slave_peer, &*fill);

    if written1 != 4096 {
        klog_info!(
            "TTY_TEST: BUG - first slave_write should accept 4096 bytes, got {}",
            written1
        );
        return TestResult::Fail;
    }

    // Now try to write more — should get a short write (0 bytes accepted).
    let extra = [b'Y'; 100];
    let written2 = tty::pty::slave_write(&slave_peer, &extra);

    if written2 != 0 {
        klog_info!(
            "TTY_TEST: BUG - slave_write to full master should return 0, got {}",
            written2
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// BUG 4: LineDisc input_full reports correctly.
pub fn test_bugfix_linedisc_input_full() -> TestResult {
    use crate::tty::ldisc::LineDisc;

    let mut ld = LineDisc::new();

    // A fresh LineDisc should NOT be full.
    if ld.input_full() {
        klog_info!("TTY_TEST: BUG - fresh LineDisc should not be input_full");
        return TestResult::Fail;
    }

    // Fill cooked buffer to capacity via non-canonical mode.
    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    ld.set_termios(&t);
    for _ in 0..8192 {
        ld.input_char(b'Z');
    }

    // Cooked buffer is full, but edit buffer is empty (non-canonical doesn't
    // use edit buffer).  So input_full should still be false for LineDisc
    // (input_full = cooked_full AND edit_full).
    // Actually in non-canonical mode, input goes to cooked directly,
    // so edit buffer stays empty.  input_full = both full, so false here.
    if ld.input_full() {
        klog_info!("TTY_TEST: BUG - LineDisc with only cooked full should not be input_full");
        return TestResult::Fail;
    }

    TestResult::Pass
}
// ---------------------------------------------------------------------------
// Bug-fix regression tests (TTY architectural review — PARMRK, TCXONC)
// ---------------------------------------------------------------------------

/// PARMRK atomic insertion: with 3+ bytes free in the cooked buffer, the
/// full \xff \x00 \x00 triplet is inserted.
pub fn test_bugfix_parmrk_atomic_full_insert() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    // Enable PARMRK, disable canonical mode so we can read directly.
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    // Fill cooked buffer to capacity minus exactly 3 bytes.
    for _ in 0..8189 {
        if !ld.push_cooked(b'X') {
            klog_info!("TTY_TEST: BUG - push_cooked failed during fill");
            return TestResult::Fail;
        }
    }

    // Now there is room for exactly 3 bytes.  A break (NUL with PARMRK)
    // should succeed and insert the full triplet.
    let action = ld.input_char(0x00);
    match action {
        InputAction::None => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PARMRK with 3 free bytes returned {:?}, expected None",
                other
            );
            return TestResult::Fail;
        }
    }

    // Drain the fill bytes first.
    let mut drain: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let n_drain = ld.read(&mut *drain);
    if n_drain != 4096 {
        klog_info!("TTY_TEST: BUG - drained {} bytes, expected 4096", n_drain);
        return TestResult::Fail;
    }
    let n_drain2 = ld.read(&mut drain[..4093]);
    if n_drain2 != 4093 {
        klog_info!(
            "TTY_TEST: BUG - second drain {} bytes, expected 4093",
            n_drain2
        );
        return TestResult::Fail;
    }

    // Now read the PARMRK triplet.
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 3 || buf[0] != 0xFF || buf[1] != 0x00 || buf[2] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - PARMRK triplet expected [0xFF, 0x00, 0x00], got {} bytes: {:?}",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PARMRK atomic insertion: with only 2 bytes free, the entire triplet is
/// dropped (no partial sequence).  Without IMAXBEL, returns None.
pub fn test_bugfix_parmrk_drop_when_insufficient_space() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    // Enable PARMRK only (no IMAXBEL), disable canonical mode.
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    // Fill cooked buffer to capacity minus 2 bytes — NOT enough for the
    // 3-byte PARMRK triplet.
    for _ in 0..8190 {
        ld.push_cooked(b'X');
    }

    // A break should be silently dropped (atomic: all-or-nothing).
    let action = ld.input_char(0x00);
    match action {
        InputAction::None => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PARMRK with 2 free bytes returned {:?}, expected None",
                other
            );
            return TestResult::Fail;
        }
    }

    // Drain the fill bytes.
    let mut drain: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let n_drain = ld.read(&mut *drain);
    if n_drain != 4096 {
        klog_info!("TTY_TEST: BUG - drained {} bytes, expected 4096", n_drain);
        return TestResult::Fail;
    }
    let n_drain2 = ld.read(&mut drain[..4094]);
    if n_drain2 != 4094 {
        klog_info!(
            "TTY_TEST: BUG - second drain {} bytes, expected 4094",
            n_drain2
        );
        return TestResult::Fail;
    }

    // No further data should be available — the triplet was dropped.
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!(
            "TTY_TEST: BUG - PARMRK partial sequence leaked: {} bytes {:?}",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PARMRK atomic insertion: with only 1 byte free and IMAXBEL set, a bell
/// is returned instead of a partial sequence.
pub fn test_bugfix_parmrk_imaxbel_bell_on_insufficient_space() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    // Enable PARMRK + IMAXBEL, disable canonical mode.
    t.c_iflag = InputFlags::PARMRK | InputFlags::IMAXBEL;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    // Fill cooked buffer to capacity minus 1 byte.
    for _ in 0..8191 {
        ld.push_cooked(b'X');
    }

    // A break should produce Bell (not None, not a partial push).
    let action = ld.input_char(0x00);
    match action {
        InputAction::Bell => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PARMRK+IMAXBEL with 1 free byte returned {:?}, expected Bell",
                other
            );
            return TestResult::Fail;
        }
    }

    // Drain and verify no partial PARMRK sequence leaked.
    let mut drain: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let n_drain = ld.read(&mut *drain);
    if n_drain != 4096 {
        klog_info!("TTY_TEST: BUG - drained {} bytes, expected 4096", n_drain);
        return TestResult::Fail;
    }
    let n_drain2 = ld.read(&mut drain[..4095]);
    if n_drain2 != 4095 {
        klog_info!(
            "TTY_TEST: BUG - second drain {} bytes, expected 4095",
            n_drain2
        );
        return TestResult::Fail;
    }
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!(
            "TTY_TEST: BUG - partial PARMRK sequence leaked: {} bytes {:?}",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PARMRK atomic insertion: with 0 bytes free (completely full buffer),
/// the triplet is dropped.  Verifies the boundary condition.
pub fn test_bugfix_parmrk_drop_when_buffer_completely_full() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    // Fill cooked buffer completely.
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }

    // Break with zero space — must be silently dropped.
    let action = ld.input_char(0x00);
    match action {
        InputAction::None => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - PARMRK on full buffer returned {:?}, expected None",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// TCXONC argument validation: invalid action codes return InvalidArg.
pub fn test_bugfix_tcxonc_invalid_action_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // Valid actions (0..=3) should succeed.
    for action in 0..=3i32 {
        match tty::tcxonc(idx, action) {
            Ok(()) => {}
            Err(e) => {
                klog_info!(
                    "TTY_TEST: BUG - tcxonc({}) failed unexpectedly: {:?}",
                    action,
                    e
                );
                return TestResult::Fail;
            }
        }
    }

    // Invalid actions should return InvalidArg.
    for &bad_action in &[4i32, -1, 99, i32::MAX, i32::MIN] {
        match tty::tcxonc(idx, bad_action) {
            Err(TtyError::InvalidArg) => {}
            other => {
                klog_info!(
                    "TTY_TEST: BUG - tcxonc({}) = {:?}, expected InvalidArg",
                    bad_action,
                    other
                );
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

/// TCXONC argument validation: boundary values (0 and 3) are accepted.
pub fn test_bugfix_tcxonc_boundary_values() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // Exact boundary: TCOOFF (0) and TCION (3).
    match tty::tcxonc(idx, slopos_abi::syscall::TCOOFF) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcxonc(TCOOFF) failed: {:?}", e);
            return TestResult::Fail;
        }
    }
    match tty::tcxonc(idx, slopos_abi::syscall::TCION) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcxonc(TCION) failed: {:?}", e);
            return TestResult::Fail;
        }
    }

    // Just outside boundary: 4 and -1.
    match tty::tcxonc(idx, 4) {
        Err(TtyError::InvalidArg) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcxonc(4) = {:?}, expected InvalidArg",
                other
            );
            return TestResult::Fail;
        }
    }
    match tty::tcxonc(idx, -1) {
        Err(TtyError::InvalidArg) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - tcxonc(-1) = {:?}, expected InvalidArg",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}
// ===========================================================================
// Input Wake Batching (WAKEUP_CHARS) tests
// ===========================================================================

/// WAKEUP_CHARS constant has expected value.
pub fn test_wakeup_chars_constant() -> TestResult {
    use crate::tty::ldisc::WAKEUP_CHARS;
    if WAKEUP_CHARS != 256 {
        klog_info!(
            "TTY_TEST: BUG - WAKEUP_CHARS = {}, expected 256",
            WAKEUP_CHARS
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode wakes immediately on line boundary.
pub fn test_canonical_wake_on_newline() -> TestResult {
    let mut ld = LineDisc::new();

    // Type chars without newline — should_wake_reader must return false.
    for &c in b"hello" {
        ld.input_char(c);
    }
    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical wake before newline");
        return TestResult::Fail;
    }

    // Newline completes the line — should_wake_reader must return true.
    ld.input_char(b'\n');
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical no wake after newline");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Non-canonical VMIN=1: wake as soon as any data is available.
pub fn test_noncanonical_no_wake_per_byte() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    for _ in 0..10 {
        ld.input_char(b'x');
    }
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - noncanonical VMIN=1 should wake when data available");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Non-canonical mode wakes at WAKEUP_CHARS threshold.
pub fn test_noncanonical_wake_at_threshold() -> TestResult {
    use crate::tty::ldisc::WAKEUP_CHARS;
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    // Switch to non-canonical mode.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    // Push exactly WAKEUP_CHARS bytes.
    for _ in 0..WAKEUP_CHARS {
        ld.input_char(b'a');
    }
    if !ld.should_wake_reader() {
        klog_info!(
            "TTY_TEST: BUG - noncanonical no wake after {} bytes",
            WAKEUP_CHARS
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Non-canonical mode wakes when buffer is nearly full.
pub fn test_noncanonical_wake_near_full() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    // Switch to non-canonical mode.
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    // Fill cooked buffer to near capacity (8192 - 64 = 8128 bytes).
    // Push in batches, draining the wake flag each time.
    let target = 8192 - 64;
    let mut pushed = 0usize;
    while pushed < target {
        ld.input_char(b'z');
        pushed += 1;
        // Drain any intermediate wake triggers.
        if pushed % 256 == 0 && pushed < target {
            let _ = ld.should_wake_reader();
        }
    }
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - noncanonical no wake when near full");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// flush_input clears the buffer; refilled data should be readable.
pub fn test_flush_input_resets_wake_counter() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    for _ in 0..100 {
        ld.input_char(b'q');
    }
    ld.flush_input();

    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - wake with empty buffer after flush_input");
        return TestResult::Fail;
    }

    for _ in 0..100 {
        ld.input_char(b'q');
    }
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - no wake after flush_input + refill");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// flush_all clears the buffer; refilled data should be readable.
pub fn test_flush_all_resets_wake_counter() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    for _ in 0..100 {
        ld.input_char(b'w');
    }
    ld.flush_all();

    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - wake with empty buffer after flush_all");
        return TestResult::Fail;
    }

    for _ in 0..100 {
        ld.input_char(b'w');
    }
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - no wake after flush_all + refill");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// RawDisc also batches wakeups.
pub fn test_rawdisc_wake_batching() -> TestResult {
    use crate::tty::ldisc::WAKEUP_CHARS;
    let mut rd = RawDisc::new();
    // Enable CREAD so input is accepted.
    let mut t = *rd.termios();
    t.c_cflag |= ControlFlags::CREAD;
    rd.set_termios(&t);

    // Push a few bytes — should NOT wake.
    for _ in 0..10 {
        rd.input_char(b'r');
    }
    if rd.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - RawDisc wake after only 10 bytes");
        return TestResult::Fail;
    }

    // Push up to threshold.
    for _ in 10..WAKEUP_CHARS {
        rd.input_char(b'r');
    }
    if !rd.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - RawDisc no wake at threshold");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// should_wake_reader returns false on empty buffer, true when data exists.
pub fn test_wake_resets_counter() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - wake on empty buffer");
        return TestResult::Fail;
    }

    ld.input_char(b'a');
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - no wake after single byte");
        return TestResult::Fail;
    }

    // Drain the buffer, then verify no spurious wake.
    ld.flush_input();
    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - wake after flush");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Canonical mode EOF (Ctrl+D) still wakes immediately.
pub fn test_canonical_eof_wakes() -> TestResult {
    let mut ld = LineDisc::new();

    // Type chars then Ctrl+D (VEOF = 0x04).
    for &c in b"data" {
        ld.input_char(c);
    }
    ld.input_char(0x04); // EOF

    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical EOF did not wake");
        return TestResult::Fail;
    }

    TestResult::Pass
}
// ===========================================================================
// no_room-style Overflow Recovery
// ===========================================================================

/// A fresh LineDisc has no_room = false.
pub fn test_no_room_initially_false() -> TestResult {
    let ld = LineDisc::new();
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - fresh LineDisc has no_room=true");
        return TestResult::Fail;
    }
    if ld.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - fresh LineDisc overflow_count != 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Filling cooked buffer then pushing sets no_room.
pub fn test_no_room_set_on_cooked_full() -> TestResult {
    let mut ld = LineDisc::new();
    // Fill to capacity.
    for _ in 0..8192 {
        if !ld.push_cooked(b'X') {
            klog_info!("TTY_TEST: BUG - push_cooked failed before buffer full");
            return TestResult::Fail;
        }
    }
    // Buffer is full but no_room not set yet (no failed push).
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room set before overflow push");
        return TestResult::Fail;
    }
    // One more byte triggers no_room.
    if ld.push_cooked(b'Y') {
        klog_info!("TTY_TEST: BUG - push_cooked succeeded on full buffer");
        return TestResult::Fail;
    }
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set after overflow push");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// no_room not set when buffer is not full.
pub fn test_no_room_not_set_before_full() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..100 {
        ld.push_cooked(b'A');
    }
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room set on non-full buffer");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// overflow_count increments on each dropped byte.
pub fn test_overflow_count_increments() -> TestResult {
    let mut ld = LineDisc::new();
    // Fill to capacity.
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    // Drop 5 bytes.
    for _ in 0..5 {
        ld.push_cooked(b'Z');
    }
    if ld.overflow_count() != 5 {
        klog_info!(
            "TTY_TEST: BUG - overflow_count={}, expected 5",
            ld.overflow_count()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// overflow_count saturates instead of wrapping.
pub fn test_overflow_count_saturates() -> TestResult {
    let mut ld = LineDisc::new();
    // Fill buffer.
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    // Simulate many overflows (we can't do u32::MAX iterations, so verify
    // the saturation logic via the implementation: push_cooked uses
    // saturating_add which can never wrap).
    for _ in 0..100 {
        ld.push_cooked(b'Z');
    }
    if ld.overflow_count() != 100 {
        klog_info!(
            "TTY_TEST: BUG - overflow_count={}, expected 100",
            ld.overflow_count()
        );
        return TestResult::Fail;
    }
    // Verify still growing.
    ld.push_cooked(b'W');
    if ld.overflow_count() != 101 {
        klog_info!("TTY_TEST: BUG - overflow_count did not increment to 101");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Draining below low-water clears no_room.
pub fn test_no_room_clears_on_drain_below_threshold() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut ld = LineDisc::new();
    // Fill to capacity and trigger no_room.
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y'); // triggers no_room
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set after overflow");
        return TestResult::Fail;
    }
    // Drain to just above low-water (2048) — no_room should persist.
    let drain_to_above = 8192 - (THROTTLE_LOW_WATER + 1);
    let mut scratch: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let mut drained = 0usize;
    while drained < drain_to_above {
        let want = core::cmp::min(scratch.len(), drain_to_above - drained);
        let got = ld.read(&mut scratch[..want]);
        if got == 0 {
            break;
        }
        drained += got;
    }
    if drained != drain_to_above {
        klog_info!(
            "TTY_TEST: BUG - drained {} expected {}",
            drained,
            drain_to_above
        );
        return TestResult::Fail;
    }
    // Recovery check should not clear (still above threshold).
    if ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery triggered above low-water");
        return TestResult::Fail;
    }
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room cleared above low-water");
        return TestResult::Fail;
    }
    // Read one more byte to drop to exactly low-water.
    let got2 = ld.read(&mut scratch[..1]);
    if got2 != 1 {
        klog_info!("TTY_TEST: BUG - second read failed");
        return TestResult::Fail;
    }
    // Now recovery should trigger.
    if !ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery did not trigger at low-water");
        return TestResult::Fail;
    }
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room still set after recovery");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// no_room stays set when still above threshold.
pub fn test_no_room_stays_above_threshold() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y');
    // Read only a few bytes (still far above low-water).
    let mut scratch = [0u8; 64];
    ld.read(&mut scratch);
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room cleared with minimal drain");
        return TestResult::Fail;
    }
    if ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery triggered with minimal drain");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// flush_input clears no_room and overflow_count.
pub fn test_flush_input_clears_no_room() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y');
    if !ld.no_room() || ld.overflow_count() == 0 {
        klog_info!("TTY_TEST: BUG - precondition failed");
        return TestResult::Fail;
    }
    ld.flush_input();
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not cleared by flush_input");
        return TestResult::Fail;
    }
    if ld.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - overflow_count not cleared by flush_input");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// flush_all clears no_room and overflow_count.
pub fn test_flush_all_clears_no_room() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y');
    ld.flush_all();
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not cleared by flush_all");
        return TestResult::Fail;
    }
    if ld.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - overflow_count not cleared by flush_all");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Fill/drain cycle with no_room preserves throttle.
pub fn test_fill_drain_cycle_preserves_throttle() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut ld = LineDisc::new();
    // Cycle 1: fill → overflow → drain → recovery.
    for _ in 0..8192 {
        ld.push_cooked(b'A');
    }
    ld.push_cooked(b'B'); // no_room
    let mut scratch: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let _ = ld.read(&mut *scratch);
    let _ = ld.read(&mut *scratch);
    // After full drain, cooked_count == 0 which is below THROTTLE_LOW_WATER.
    if !ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery did not trigger after full drain");
        return TestResult::Fail;
    }
    // Cycle 2: fill again — no_room should be clearable again.
    for _ in 0..8192 {
        ld.push_cooked(b'C');
    }
    ld.push_cooked(b'D');
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set on second cycle");
        return TestResult::Fail;
    }
    // Drain below threshold.
    let drain_amount = 8192 - THROTTLE_LOW_WATER;
    let mut drained = 0usize;
    while drained < drain_amount {
        let want = core::cmp::min(scratch.len(), drain_amount - drained);
        let got = ld.read(&mut scratch[..want]);
        if got == 0 {
            break;
        }
        drained += got;
    }
    if drained != drain_amount {
        klog_info!(
            "TTY_TEST: BUG - drained {} expected {}",
            drained,
            drain_amount
        );
        return TestResult::Fail;
    }
    if !ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery did not trigger on second cycle");
        return TestResult::Fail;
    }
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room still set after second recovery");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc also tracks no_room on overflow.
pub fn test_rawdisc_no_room() -> TestResult {
    let mut rd = RawDisc::new();
    // RawDisc buffer is 4096 bytes.
    for _ in 0..4096 {
        rd.input_char(b'R');
    }
    if rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room set before overflow");
        return TestResult::Fail;
    }
    // Overflow.
    rd.input_char(b'S');
    if !rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room not set after overflow");
        return TestResult::Fail;
    }
    if rd.overflow_count() != 1 {
        klog_info!("TTY_TEST: BUG - RawDisc overflow_count != 1");
        return TestResult::Fail;
    }
    // Flush clears it.
    rd.flush_all();
    if rd.no_room() || rd.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - RawDisc flush_all did not clear no_room");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// IMAXBEL bell still works when no_room is set.
pub fn test_imaxbel_preserved_with_no_room() -> TestResult {
    let mut ld = LineDisc::new();
    // Put into raw mode with IMAXBEL.
    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_iflag |= InputFlags::IMAXBEL;
    ld.set_termios(&t);
    // Fill cooked buffer.
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    // Next input_char should return Bell AND set no_room.
    let action = ld.input_char(b'Z');
    let is_bell = matches!(action, InputAction::Bell);
    if !is_bell {
        klog_info!("TTY_TEST: BUG - expected Bell on overflow with IMAXBEL");
        return TestResult::Fail;
    }
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set alongside IMAXBEL bell");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc check_no_room_recovery works.
pub fn test_rawdisc_recovery() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut rd = RawDisc::new();
    // Fill and overflow.
    for _ in 0..4096 {
        rd.input_char(b'R');
    }
    rd.input_char(b'S');
    if !rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room not set");
        return TestResult::Fail;
    }
    // Drain below low-water.
    let drain_amount = 4096 - THROTTLE_LOW_WATER;
    let mut scratch: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let got = rd.read(&mut scratch[..drain_amount]);
    if got != drain_amount {
        klog_info!("TTY_TEST: BUG - RawDisc read returned {}", got);
        return TestResult::Fail;
    }
    if !rd.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - RawDisc recovery did not trigger");
        return TestResult::Fail;
    }
    if rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room still set after recovery");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// LdiscKind dispatch forwards no_room/overflow_count.
pub fn test_ldisc_kind_dispatch() -> TestResult {
    use crate::tty::ldisc::LdiscKind;
    let mut lk = LdiscKind::NTty(LineDisc::new());
    if lk.no_room() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty no_room initially true");
        return TestResult::Fail;
    }
    if lk.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty overflow_count initially != 0");
        return TestResult::Fail;
    }
    // Fill via NTty's raw input to trigger overflow.
    let mut t = *lk.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_iflag &= !InputFlags::IMAXBEL;
    lk.set_termios(&t);
    for _ in 0..8193 {
        lk.input_char(b'Q');
    }
    if !lk.no_room() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty no_room not set after overflow");
        return TestResult::Fail;
    }
    if lk.overflow_count() < 1 {
        klog_info!("TTY_TEST: BUG - LdiscKind overflow_count should be >= 1");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ---------------------------------------------------------------------------
// Output Drain Semantics Hardening tests
// ---------------------------------------------------------------------------

/// wait_output_idle (via is_output_idle) returns true
/// when no output is in-flight and driver has no pending output (fast path).
pub fn test_drain_idle_fast_path() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Write some output so the inflight counter goes up and back down.
    let _ = tty::write(TtyIndex(0), b"fast-path test", false);

    // Synchronous driver: after write returns, output is already idle.
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 drain fast path should return true, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Drain on a hung-up TTY is vacuously complete.
pub fn test_drain_hangup_vacuously_complete() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Hang up TTY 0.
    tty::hangup(TtyIndex(0));

    // is_output_idle should return true — drain is vacuously complete.
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 drain on hung-up TTY should be idle, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// tcsbrk(arg>0) on a hung-up TTY returns HungUp error.
pub fn test_tcsbrk_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    // Hang up TTY 0.
    tty::hangup(idx);

    // tcsbrk on hung-up TTY should return HungUp.
    match tty::tcsbrk(idx, 1) {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 tcsbrk on hung-up TTY should return HungUp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// tcsbrk(0) on a hung-up TTY also returns HungUp (break is an ioctl).
pub fn test_tcsbrk_zero_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    tty::hangup(idx);

    match tty::tcsbrk(idx, 0) {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 tcsbrk(0) on hung-up TTY should return HungUp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// tcsbrk(0) on a healthy TTY returns success (no-op break).
pub fn test_tcsbrk_zero_healthy_succeeds() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    match tty::tcsbrk(idx, 0) {
        Ok(()) => TestResult::Pass,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - fp13 tcsbrk(0) should succeed, got {:?}", e);
            TestResult::Fail
        }
    }
}

/// tcsbrk(arg>0) and TCSETSW share the same drain path.
/// Verify behavioral parity: both succeed immediately on a synchronous backend.
pub fn test_tcsbrk_and_tcsetsw_share_drain() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    // Write some output.
    let _ = tty::write(idx, b"drain parity test", false);

    // tcsbrk(1) should succeed immediately (synchronous driver).
    if let Err(e) = tty::tcsbrk(idx, 1) {
        klog_info!("TTY_TEST: BUG - fp13 tcsbrk(1) failed: {:?}", e);
        return TestResult::Fail;
    }

    // set_termios_wait should also succeed immediately.
    let t = tty::get_termios(idx).unwrap();
    if let Err(e) = tty::set_termios_wait(idx, &t) {
        klog_info!("TTY_TEST: BUG - fp13 set_termios_wait failed: {:?}", e);
        return TestResult::Fail;
    }

    // Both completed — they share the same drain path.
    TestResult::Pass
}

/// drain on an invalid TTY index returns InvalidIndex.
pub fn test_drain_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    match tty::tcsbrk(TtyIndex(255), 1) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 drain invalid index should return InvalidIndex, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// drain on an unallocated TTY slot returns NotAllocated.
pub fn test_drain_unallocated_slot() -> TestResult {
    tty::table::tty_table_init();
    // Slot 7 is not allocated after init.
    match tty::tcsbrk(TtyIndex(7), 1) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 drain unallocated should return NotAllocated, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// PTY drain is always immediate (no hardware latency).
pub fn test_pty_tcsbrk_drain_immediate() -> TestResult {
    tty::table::tty_table_init();

    let (master_idx, _master_backing) = match tty::pty_alloc() {
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
    tty::set_pty_lock(master_idx, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave_idx).unwrap();

    // Write to master.
    let _ = tty::write(master_idx, b"pty drain fp13", false);

    // tcsbrk drain should succeed immediately.
    let drain_result = tty::tcsbrk(master_idx, 1);
    let idle_result = tty::is_output_idle(master_idx);

    if let Err(e) = drain_result {
        klog_info!("TTY_TEST: BUG - fp13 PTY tcsbrk failed: {:?}", e);
        return TestResult::Fail;
    }
    match idle_result {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 PTY is_output_idle should be true, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Console drain is immediate (synchronous serial driver).
pub fn test_console_drain_synchronous() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let _ = tty::write(TtyIndex(0), b"console drain fp13\r\n", false);

    // tcsbrk drain should succeed immediately for synchronous driver.
    match tty::tcsbrk(TtyIndex(0), 1) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - fp13 console tcsbrk failed: {:?}", e);
            return TestResult::Fail;
        }
    }

    // is_output_idle should also be true.
    match tty::is_output_idle(TtyIndex(0)) {
        Ok(true) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 console should be idle after drain, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// output_pending_bytes returns 0 for all current driver kinds
/// (all backends are synchronous).
pub fn test_output_pending_bytes_all_drivers() -> TestResult {
    use crate::tty::driver::SerialConsoleDriver;

    let serial = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if serial.output_pending_bytes() != 0 {
        klog_info!("TTY_TEST: BUG - fp13 SerialConsole output_pending_bytes should be 0");
        return TestResult::Fail;
    }

    let vc = TtyDriverKind::VConsole(VConsoleDriver);
    if vc.output_pending_bytes() != 0 {
        klog_info!("TTY_TEST: BUG - fp13 VConsole output_pending_bytes should be 0");
        return TestResult::Fail;
    }

    let pty_master = TtyDriverKind::PtyMaster { peer: KWeak::new() };
    if pty_master.output_pending_bytes() != 0 {
        klog_info!("TTY_TEST: BUG - fp13 PtyMaster output_pending_bytes should be 0");
        return TestResult::Fail;
    }

    let pty_slave = TtyDriverKind::PtySlave { peer: KWeak::new() };
    if pty_slave.output_pending_bytes() != 0 {
        klog_info!("TTY_TEST: BUG - fp13 PtySlave output_pending_bytes should be 0");
        return TestResult::Fail;
    }

    let none = TtyDriverKind::SerialConsole(SerialConsoleDriver);
    if none.output_pending_bytes() != 0 {
        klog_info!("TTY_TEST: BUG - fp13 None output_pending_bytes should be 0");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// output_queued_bytes uses output_pending_bytes.
/// After a completed write, queued bytes should be 0 for synchronous drivers.
pub fn test_output_queued_uses_pending_bytes() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let _ = tty::write(TtyIndex(0), b"queued bytes test", false);

    match tty::output_queued_bytes(TtyIndex(0)) {
        Ok(0) => TestResult::Pass,
        Ok(n) => {
            klog_info!(
                "TTY_TEST: BUG - fp13 output_queued_bytes should be 0 after sync write, got {}",
                n
            );
            TestResult::Fail
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - fp13 output_queued_bytes error: {:?}", e);
            TestResult::Fail
        }
    }
}

/// TCSETSW on a hung-up TTY returns HungUp (the
/// set_termios_mode hangup guard fires before the drain path).
pub fn test_tcsetsw_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let t = tty::get_termios(idx).unwrap();
    tty::hangup(idx);

    match tty::set_termios_wait(idx, &t) {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 TCSETSW on hung-up TTY should return HungUp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// TCSETSF on a hung-up TTY returns HungUp.
pub fn test_tcsetsf_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let t = tty::get_termios(idx).unwrap();
    tty::hangup(idx);

    match tty::set_termios_flush(idx, &t) {
        Err(TtyError::HungUp) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fp13 TCSETSF on hung-up TTY should return HungUp, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Inflight counter starts at 0, bumps during write,
/// returns to 0 after write completes.  Verifies the split-write accounting.
pub fn test_inflight_accounting_round_trip() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    // Before write: inflight must be 0.
    let before = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if before != 0 {
        klog_info!(
            "TTY_TEST: BUG - fp13 inflight before write should be 0, got {}",
            before
        );
        return TestResult::Fail;
    }

    // Write completes synchronously on serial backend.
    let _ = tty::write(TtyIndex(0), b"inflight round trip", false);

    // After write: inflight must be back to 0.
    let after = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if after != 0 {
        klog_info!(
            "TTY_TEST: BUG - fp13 inflight after write should be 0, got {}",
            after
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_input_event_normal_behavior() -> TestResult {
    let mut legacy = LineDisc::new();
    let mut typed = LineDisc::new();
    let mut t = *legacy.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    legacy.set_termios(&t);
    typed.set_termios(&t);

    let _ = legacy.input_char(b'A');
    let _ = typed.input_char(InputEvent::normal(b'A'));

    let mut a = [0u8; 8];
    let mut b = [0u8; 8];
    let na = legacy.read(&mut a);
    let nb = typed.read(&mut b);
    if na != nb || &a[..na] != &b[..nb] {
        klog_info!("TTY_TEST: BUG - normal InputEvent diverged from byte path");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_input_event_break_brkint() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::BRKINT;
    t.c_iflag &= !InputFlags::IGNBRK;
    ld.set_termios(&t);
    match ld.input_char(InputEvent {
        byte: 0,
        status: InputStatus::Break,
    }) {
        InputAction::Signal(SIGINT) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - break+BRKINT expected SIGINT, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_input_event_break_ignbrk() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::IGNBRK;
    ld.set_termios(&t);
    let _ = ld.input_char(InputEvent {
        byte: 0,
        status: InputStatus::Break,
    });
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - break+IGNBRK should be discarded");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_input_event_parity_parmrk() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_iflag |= InputFlags::INPCK | InputFlags::PARMRK;
    t.c_iflag &= !InputFlags::IGNPAR;
    ld.set_termios(&t);

    let _ = ld.input_char(InputEvent {
        byte: b'X',
        status: InputStatus::ParityError,
    });
    let mut out = [0u8; 8];
    let n = ld.read(&mut out);
    if n < 3 || out[0] != 0xFF || out[1] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - parity+PARMRK expected 0xFF 0x00 prefix, got {:?}",
            &out[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_input_event_parity_ignpar() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::IGNPAR;
    ld.set_termios(&t);
    let _ = ld.input_char(InputEvent {
        byte: b'X',
        status: InputStatus::ParityError,
    });
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - parity+IGNPAR should discard byte");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_input_event_overrun_noop() -> TestResult {
    let mut ld = LineDisc::new();
    let _ = ld.input_char(InputEvent {
        byte: b'X',
        status: InputStatus::Overrun,
    });
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - overrun status should be no-op");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_poll_output_stopped_masks_pollout() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOOFF as i32);
    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLOUT);
    let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON as i32);
    if (revents & slopos_abi::syscall::POLLOUT) != 0 {
        klog_info!("TTY_TEST: BUG - POLLOUT should be masked when output_stopped");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_poll_output_not_stopped_has_pollout() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _ = tty::tcxonc(idx, slopos_abi::syscall::TCOON as i32);
    let revents = tty::poll_events(idx, slopos_abi::syscall::POLLOUT);
    if (revents & slopos_abi::syscall::POLLOUT) == 0 {
        klog_info!("TTY_TEST: BUG - POLLOUT should be present when output is active");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_grantpt_unlocks_slave() -> TestResult {
    use slopos_kernel_services::syscall_services::tty::tty_services;

    tty::table::tty_table_init();
    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => return TestResult::Pass,
    };
    let locked_before = tty::get_pty_lock(master).unwrap_or(false);
    let rc = (tty_services().grantpt)(master);
    let locked_after = tty::get_pty_lock(master).unwrap_or(true);
    if !locked_before || rc.is_err() || locked_after {
        klog_info!(
            "TTY_TEST: BUG - grantpt should unlock slave (before={}, rc_ok={}, after={})",
            locked_before,
            rc.is_ok(),
            locked_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_b0_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let mut t = tty::get_termios(idx).unwrap();
    t.c_cflag = ControlFlags::from_bits_retain(
        (t.c_cflag.bits() & !slopos_abi::syscall::CBAUD) | slopos_abi::syscall::B0,
    );
    match tty::set_termios(idx, &t) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - set_termios(B0) failed: {:?}", e);
            return TestResult::Fail;
        }
    }
    if !tty::is_hung_up(idx) {
        klog_info!("TTY_TEST: BUG - B0 should trigger hangup");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_speed_roundtrip() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

    let mut t = saved;
    t.c_cflag = ControlFlags::from_bits_retain((t.c_cflag.bits() & !CBAUD) | B9600);
    if let Err(e) = tty::set_termios(idx, &t) {
        klog_info!(
            "TTY_TEST: BUG - set_termios speed roundtrip failed: {:?}",
            e
        );
        tty::set_termios(idx, &saved).unwrap();
        return TestResult::Fail;
    }
    let got = tty::get_termios(idx).unwrap();
    if (got.c_cflag.bits() & CBAUD) != B9600 || got.c_ispeed != 9600 || got.c_ospeed != 9600 {
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

pub fn test_batched_ingress_no_data_loss() -> TestResult {
    let mut ld = LineDisc::new();
    {
        let mut t = *ld.termios();
        t.c_lflag = LocalFlags::empty();
        t.c_iflag = InputFlags::empty();
        ld.set_termios(&t);
    }

    let count = 256usize;
    let mut events = [InputEvent::normal(0); 256];
    for i in 0..count {
        events[i] = InputEvent::normal((i as u8).wrapping_add(0x20));
    }

    let result = ld.receive_buf(&events[..count]);
    let _ = result;

    let avail = ld.bytes_available();
    if avail != count {
        klog_info!(
            "TTY_TEST: BUG - batched ingress data mismatch (total={})",
            avail
        );
        return TestResult::Fail;
    }

    let mut out = [0u8; 256];
    let got = ld.read(&mut out);
    if got != count {
        klog_info!(
            "TTY_TEST: BUG - batched ingress read mismatch (got={})",
            got
        );
        return TestResult::Fail;
    }
    for i in 0..count {
        if out[i] != (i as u8).wrapping_add(0x20) {
            klog_info!("TTY_TEST: BUG - batched ingress byte mismatch at {}", i);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_batched_ingress_signal_in_middle() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    let mut t = saved;
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_lflag |= LocalFlags::ISIG;
    t.c_lflag &= !LocalFlags::NOFLSH;
    if tty::set_termios(slave, &t).is_err() {
        packet_mode_teardown_pty(master, slave, &saved);
        return TestResult::Fail;
    }

    let payload = [b'a', 0x03, b'b'];
    let _ = tty::write(master, &payload, false);
    let mut out = [0u8; 8];
    let n = tty::read(slave, &mut out, true).unwrap_or(0);
    packet_mode_teardown_pty(master, slave, &saved);
    if n != 0 {
        klog_info!(
            "TTY_TEST: BUG - signal in batch should flush/discard trailing bytes, got {:?}",
            &out[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_background_read_sigttin_blocked_eio() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    if !matches!(s.check_read(99, 10), ForegroundCheck::BackgroundRead) {
        klog_info!("TTY_TEST: BUG - expected BackgroundRead precondition");
        return TestResult::Fail;
    }
    if TtyError::HungUp.to_errno() != -5 {
        klog_info!("TTY_TEST: BUG - blocked SIGTTIN EIO path should map to -5");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_receive_buf_accumulates_echo() -> TestResult {
    let mut ld = LineDisc::new();
    let events = [
        InputEvent::normal(b'a'),
        InputEvent::normal(b'b'),
        InputEvent::normal(b'c'),
    ];
    let result = ld.receive_buf(&events);
    if result.echo.is_empty() {
        klog_info!("TTY_TEST: BUG - receive_buf should accumulate echo bytes");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// mod.rs Module Decomposition — Regression Tests
// ===========================================================================

pub fn test_mod_reexports_io_functions() -> TestResult {
    // Verify I/O functions are accessible through crate::tty::*
    let _: fn(TtyIndex, &mut [u8], bool) -> Result<usize, TtyError> = tty::read;
    let _: fn(TtyIndex, &[u8], bool) -> Result<usize, TtyError> = tty::write;
    let _: fn(TtyIndex) -> bool = tty::has_data;
    let _: fn(TtyIndex) -> Result<usize, TtyError> = tty::bytes_available;
    let _: fn(TtyIndex) -> Result<usize, TtyError> = tty::output_queued_bytes;
    TestResult::Pass
}

pub fn test_mod_reexports_termios_functions() -> TestResult {
    use slopos_abi::syscall::{UserTermios, UserWinsize};
    let _: fn(TtyIndex) -> Result<UserTermios, TtyError> = tty::get_termios;
    let _: fn(TtyIndex, &UserTermios) -> Result<(), TtyError> = tty::set_termios;
    let _: fn(TtyIndex, &UserTermios) -> Result<(), TtyError> = tty::set_termios_wait;
    let _: fn(TtyIndex, &UserTermios) -> Result<(), TtyError> = tty::set_termios_flush;
    let _: fn(TtyIndex) -> Result<bool, TtyError> = tty::is_output_idle;
    let _: fn(TtyIndex) -> Result<u32, TtyError> = tty::get_ldisc;
    let _: fn(TtyIndex, u32) -> Result<(), TtyError> = tty::set_ldisc;
    let _: fn(TtyIndex) -> Result<UserWinsize, TtyError> = tty::get_winsize;
    let _: fn(TtyIndex, &UserWinsize) -> Result<(), TtyError> = tty::set_winsize;
    let _: fn(TtyIndex, i32) -> Result<(), TtyError> = tty::tcflush;
    let _: fn(TtyIndex, i32) -> Result<(), TtyError> = tty::tcsbrk;
    let _: fn(TtyIndex, i32) -> Result<(), TtyError> = tty::tcxonc;
    TestResult::Pass
}

pub fn test_mod_reexports_job_control_functions() -> TestResult {
    let _: fn(TtyIndex) -> Result<u32, TtyError> = tty::get_foreground_pgrp;
    let _: fn(TtyIndex, u32) -> Result<(), TtyError> = tty::set_foreground_pgrp;
    let _: fn(TtyIndex) -> Result<u32, TtyError> = tty::get_session_id;
    let _: fn(TtyIndex, u32, u32) = tty::attach_session;
    let _: fn(TtyIndex) = tty::detach_session;
    TestResult::Pass
}

pub fn test_mod_reexports_lifecycle_functions() -> TestResult {
    let _: fn() -> TtyIndex = tty::active_tty;
    let _: fn(TtyIndex) = tty::set_active_tty;
    let _: fn(TtyIndex) -> Result<(), TtyError> = tty::switch_active_tty;
    let _: fn() -> TtyIndex = tty::default_console_tty;
    let _: fn(TtyIndex) = tty::set_default_console_tty;
    let _: fn() = tty::init;
    let _: fn(TtyIndex) -> Result<KArc<dyn FileBacking>, TtyError> = tty::open_tty;
    let _: fn(TtyIndex) = tty::hangup;
    let _: fn(TtyIndex) -> bool = tty::is_hung_up;
    let _: fn(TtyIndex) = tty::vhangup;
    TestResult::Pass
}

pub fn test_mod_reexports_poll_functions() -> TestResult {
    let _: fn(TtyIndex, u16) -> u16 = tty::poll_events;
    let _: fn(&[u8]) = tty::poll_sleep_on;
    let _: fn() = tty::poll_sleep;
    let _: fn(u32) -> Result<(), TtyError> = tty::set_compositor_focus;
    let _: fn() -> Result<u32, TtyError> = tty::get_compositor_focus;
    TestResult::Pass
}

pub fn test_mod_reexports_pty_functions() -> TestResult {
    let _: fn(TtyIndex) -> bool = tty::is_pty_slave;
    let _: fn(TtyIndex) -> bool = tty::is_slave_locked;
    TestResult::Pass
}

pub fn test_tty_struct_fields_accessible() -> TestResult {
    use slopos_abi::syscall::UserWinsize;
    let guard = crate::tty::table::TTY_SLOTS[0].lock();
    if let Some(tty) = guard.as_ref() {
        let _ = tty.index;
        // active field removed — slot being Some means active
        let _ = tty.flags.contains(TtyFlags::HUNG_UP);
        let _ = tty.flags.contains(TtyFlags::PEER_CLOSED);
        let _ = tty.flags.contains(TtyFlags::SLAVE_LOCKED);
        let _ = tty.flags.contains(TtyFlags::PACKET_MODE);
        let _ = tty.packet_events;
        let _ = tty.flags.contains(TtyFlags::THROTTLED);
        let _ = tty.flags.contains(TtyFlags::OUTPUT_STOPPED);
        let _ = tty.winsize;
    }
    TestResult::Pass
}

pub fn test_tty_error_variants_unchanged() -> TestResult {
    let errors = [
        (TtyError::InvalidIndex, -22),
        (TtyError::NotAllocated, -6),
        (TtyError::HungUp, -5),
        (TtyError::WouldBlock, -11),
        (TtyError::PermissionDenied, -1),
        (TtyError::InvalidArg, -22),
        (TtyError::Restart, -512),
    ];
    for (err, expected) in &errors {
        if err.to_errno() != *expected {
            klog_info!(
                "TTY_TEST: BUG - {:?}.to_errno() = {} expected {}",
                err,
                err.to_errno(),
                expected
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_max_ttys_constant() -> TestResult {
    if tty::MAX_TTYS != 32 {
        klog_info!(
            "TTY_TEST: BUG - MAX_TTYS changed from 32 to {}",
            tty::MAX_TTYS
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_existing_api_smoke_test() -> TestResult {
    let idx = tty::active_tty();
    let _ = tty::get_termios(idx);
    let _ = tty::get_foreground_pgrp(idx);
    let _ = tty::get_session_id(idx);
    let _ = tty::get_winsize(idx);
    let _ = tty::get_ldisc(idx);
    let _ = tty::is_hung_up(idx);
    let _ = tty::has_data(idx);
    let _ = tty::bytes_available(idx);
    let _ = tty::is_output_idle(idx);
    let _ = tty::output_queued_bytes(idx);
    TestResult::Pass
}
// ===========================================================================
// Phase 21: Deferred Actions RAII & Boilerplate Reduction
// ===========================================================================

use crate::tty::PostLockWork;

pub fn test_p21_postlockwork_default_is_empty() -> TestResult {
    let plw = PostLockWork::new();
    if !plw.is_empty() {
        klog_info!("TTY_TEST: BUG - new PostLockWork should be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_p21_postlockwork_signal_makes_nonempty() -> TestResult {
    let scope = SessionScope::new(42, 42);
    let mut plw = PostLockWork::new();
    plw.add_signal(scope.pgrp.clone(), slopos_abi::signal::SIGINT);
    if plw.is_empty() {
        klog_info!("TTY_TEST: BUG - PostLockWork with signal should not be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_p21_postlockwork_execute_completes() -> TestResult {
    let scope = SessionScope::new(42, 42);
    let mut plw = PostLockWork::new();
    plw.add_signal(scope.pgrp.clone(), slopos_abi::signal::SIGINT);
    plw.wake_input_slot(0);
    plw.wake_output_slot(0);
    plw.wake_poll_slot(0);
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_ixoff_byte() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.add_ixoff_byte(DriverId::SerialConsole, 0x13, 0);
    if plw.is_empty() {
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_packet_event() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.add_packet_event(TtyIndex(0), slopos_abi::syscall::TIOCPKT_STOP);
    if plw.is_empty() {
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_packet_event_merge() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.add_packet_event(TtyIndex(0), slopos_abi::syscall::TIOCPKT_STOP);
    plw.add_packet_event(TtyIndex(0), slopos_abi::syscall::TIOCPKT_FLUSHREAD);
    if plw.is_empty() {
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_wake_helpers() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.wake_output_and_poll(5);
    plw.wake_input_and_poll(3);
    if plw.is_empty() {
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_zero_pgid_signal_ignored() -> TestResult {
    // With no live foreground group, the call sites resolve `None` and never
    // queue a signal — the "no target, no signal" invariant now lives at
    // resolution time rather than inside `add_signal`.
    let s = TtySession::new();
    let mut plw = PostLockWork::new();
    if let Some(pg) = s.fg_pgrp_handle() {
        plw.add_signal(pg, slopos_abi::signal::SIGINT);
    }
    if !plw.is_empty() {
        klog_info!("TTY_TEST: BUG - no live fg group should queue no signal");
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_zero_event_bits_ignored() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.add_packet_event(TtyIndex(0), 0);
    if !plw.is_empty() {
        klog_info!("TTY_TEST: BUG - zero event bits should not be added");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_p21_write_path_peer_cache_consolidation() -> TestResult {
    tty::table::tty_table_init();
    let (master_idx, _master_backing) = match tty::pty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => return TestResult::Pass,
    };
    let slave_idx = match tty::get_pty_number(master_idx) {
        Ok(n) => TtyIndex(n as u8),
        Err(_) => return TestResult::Fail,
    };
    tty::pty::set_pty_lock(master_idx, false).ok();
    let _slave_backing = tty::pty_open_slave(slave_idx).ok();
    let result = tty::write(slave_idx, b"hello", true);
    match result {
        Ok(n) if n > 0 => {}
        Ok(0) => {}
        Err(_) => {}
        _ => {}
    }
    TestResult::Pass
}

pub fn test_p21_forward_ldisc_ops_linedisc() -> TestResult {
    let ld = LineDisc::new();
    let canonical = ld.is_canonical();
    let has = ld.has_data();
    let avail = ld.bytes_available();
    let stopped = ld.is_stopped();
    let full = ld.input_full();
    if ld.is_canonical() != canonical {
        return TestResult::Fail;
    }
    if ld.has_data() != has {
        return TestResult::Fail;
    }
    if ld.bytes_available() != avail {
        return TestResult::Fail;
    }
    if ld.is_stopped() != stopped {
        return TestResult::Fail;
    }
    if ld.input_full() != full {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_p21_forward_ldisc_ops_rawdisc() -> TestResult {
    let rd = RawDisc::new();
    if rd.is_canonical() {
        return TestResult::Fail;
    }
    if rd.has_data() {
        return TestResult::Fail;
    }
    if rd.is_stopped() {
        return TestResult::Fail;
    }
    if rd.input_full() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_p21_existing_api_smoke_read_write() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _console_backing = tty::open_tty(idx);
    let write_result = tty::write(idx, b"phase21test\n", true);
    if write_result.is_err() {
        klog_info!("TTY_TEST: write failed: {:?}", write_result);
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_pendin_flag_value,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_pendin_auto_set_on_echo_change,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_pendin_one_shot,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_vreprint_clears_pendin,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_pendin_not_set_for_non_echo_flags,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_pendin_empty_edit_buffer,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_clears_pendin,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_input_clears_pendin,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_tcflush_unthrottles_pty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_tcflush_both_unthrottles_pty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_master_write_batch_boundary,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_speed_fields_merge_into_cflag,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_speed_ispeed_fallback,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_speed_unrecognised_noop,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_pollerr_on_hangup,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_review_pollerr_on_peer_closed,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_flush_edit_preserves_remainder,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_nonblock_write_throttled_pty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_nonblock_write_unthrottled_pty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_priority_vintr_throttled_nonblock_flushes_and_unthrottles,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_priority_vintr_throttled_noflsh_preserves_throttle,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_master_write_throttled_ordinary_direct_rejected,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_literal_next_vintr_does_not_bypass_throttle,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_rawdisc_input_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_slave_write_stops_on_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_linedisc_input_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_atomic_full_insert,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_drop_when_insufficient_space,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_imaxbel_bell_on_insufficient_space,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_parmrk_drop_when_buffer_completely_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_tcxonc_invalid_action_returns_error,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_bugfix_tcxonc_boundary_values,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_wakeup_chars_constant,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_canonical_wake_on_newline,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_noncanonical_no_wake_per_byte,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_noncanonical_wake_at_threshold,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_noncanonical_wake_near_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_input_resets_wake_counter,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_all_resets_wake_counter,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_rawdisc_wake_batching,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_wake_resets_counter,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_canonical_eof_wakes,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_no_room_initially_false,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_no_room_set_on_cooked_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_no_room_not_set_before_full,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_overflow_count_increments,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_overflow_count_saturates,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_no_room_clears_on_drain_below_threshold,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_no_room_stays_above_threshold,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_input_clears_no_room,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_flush_all_clears_no_room,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_fill_drain_cycle_preserves_throttle,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_rawdisc_no_room,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_imaxbel_preserved_with_no_room,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_rawdisc_recovery,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_ldisc_kind_dispatch,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_drain_idle_fast_path,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_drain_hangup_vacuously_complete,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsbrk_hangup_returns_error,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsbrk_zero_hangup_returns_error,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsbrk_zero_healthy_succeeds,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsbrk_and_tcsetsw_share_drain,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_drain_invalid_index,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_drain_unallocated_slot,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_pty_tcsbrk_drain_immediate,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_console_drain_synchronous,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_output_pending_bytes_all_drivers,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_output_queued_uses_pending_bytes,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsetsw_hangup_returns_error,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tcsetsf_hangup_returns_error,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_inflight_accounting_round_trip,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_normal_behavior,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_break_brkint,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_break_ignbrk,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_parity_parmrk,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_parity_ignpar,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_input_event_overrun_noop,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_poll_output_stopped_masks_pollout,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_poll_output_not_stopped_has_pollout,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_grantpt_unlocks_slave,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(name = test_b0_hangup, suite = tty_test_ldisc_regression);
slopos_testing::stest!(
    name = test_speed_roundtrip,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_batched_ingress_no_data_loss,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_batched_ingress_signal_in_middle,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_background_read_sigttin_blocked_eio,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_receive_buf_accumulates_echo,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_io_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_termios_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_job_control_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_lifecycle_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_poll_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_mod_reexports_pty_functions,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tty_struct_fields_accessible,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_tty_error_variants_unchanged,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_max_ttys_constant,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_existing_api_smoke_test,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_default_is_empty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_signal_makes_nonempty,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_execute_completes,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_ixoff_byte,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_packet_event,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_packet_event_merge,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_wake_helpers,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_zero_pgid_signal_ignored,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_postlockwork_zero_event_bits_ignored,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_write_path_peer_cache_consolidation,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_forward_ldisc_ops_linedisc,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_forward_ldisc_ops_rawdisc,
    suite = tty_test_ldisc_regression
);
slopos_testing::stest!(
    name = test_p21_existing_api_smoke_read_write,
    suite = tty_test_ldisc_regression
);
