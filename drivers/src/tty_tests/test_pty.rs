use super::*;

/// A PTY master write past the slave's IXOFF high-water mark emits XOFF through
/// the *slave's* driver while the master's write lock is held. Both ends draw
/// their lock from one `TTY_WRITE_LOCKS` declaration, so this is a same-class
/// nesting the validator panics on unless a subclass names the direction.
pub fn test_pty_ixoff_nests_peer_write_lock() -> TestResult {
    let pair = fixtures::open_pty_pair();

    let Ok(saved) = tty::get_termios(pair.slave) else {
        return TestResult::Fail;
    };
    let mut t = saved;
    t.c_lflag = LocalFlags::ICANON;
    t.c_iflag |= InputFlags::IXOFF;
    t.c_cc[CcIndex::Vstop.as_usize()] = 0x13;
    if tty::set_termios(pair.slave, &t).is_err() {
        return TestResult::Fail;
    }

    // IXOFF fires when edit_len + cooked reaches 9830 (80% of 4096+8192) and
    // edit_len caps at 4096, so cooked must sit between 5734 and the 6144
    // throttle mark. Canonical mode separates the two: a line reaches `cooked`
    // only at its newline. Small chunks because a buffer big enough to cross the
    // mark in one call would not fit a kernel stack frame.
    let mut line = [b'x'; 256];
    line[255] = b'\n';
    for _ in 0..23 {
        if tty::write(pair.master, &line, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }

    // Unterminated: these stay in the edit buffer and push the sum past the mark.
    let unterminated = [b'y'; 256];
    for _ in 0..16 {
        if tty::write(pair.master, &unterminated, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }

    // Non-blocking: a lone flow-control byte is under `WAKEUP_CHARS`, so its
    // arrival publishes no input event a blocking read could park on.
    let arrived = fixtures::drain_then_read_byte(pair.slave, pair.master, 0x13);

    let _ = tty::set_termios(pair.slave, &saved);

    if !arrived {
        klog_info!("TTY_TEST: BUG - IXOFF never reached the peer write path");
        return TestResult::Fail;
    }
    TestResult::Pass
}
