use super::*;

/// A PTY master write that pushes the slave past its IXOFF high-water mark
/// emits XOFF through the *slave's* driver while the master's write lock is
/// still held. Both ends draw their write lock from one `TTY_WRITE_LOCKS`
/// declaration, so the pair is a same-class nesting: without a subclass naming
/// the direction the validator reports it and, in the default mode, panics.
/// Any process that can open a PTY reaches it with three syscalls.
///
/// `test_ixoff_high_water_sends_xoff` drives `LineDisc` directly and takes
/// neither lock, which is how this path stayed uncovered.
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

    // The window is narrow. IXOFF fires when edit_len + cooked reaches 9830
    // (80% of 4096+8192) and edit_len caps at 4096, so cooked has to sit above
    // 5734 — and below the 6144 throttle mark, or the peer stops accepting
    // input before the edit buffer fills. Canonical mode is what separates
    // them: a line moves to `cooked` only at its newline, so newline-
    // terminated writes fill `cooked` and unterminated ones stay in `edit_len`.
    //
    // Written in small chunks because a buffer big enough to cross the mark in
    // one call would not fit a kernel stack frame.
    let mut line = [b'x'; 256];
    line[255] = b'\n';
    for _ in 0..23 {
        if tty::write(pair.master, &line, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }

    // Unterminated: these stay in the edit buffer and push the sum past the
    // mark, so the write that crosses it is the one that emits XOFF.
    let unterminated = [b'y'; 256];
    for _ in 0..16 {
        if tty::write(pair.master, &unterminated, false) != Ok(256) {
            let _ = tty::set_termios(pair.slave, &saved);
            return TestResult::Fail;
        }
    }

    // Reading the byte back is what proves the path ran; without it the test
    // passes whether or not the flag ever reached the driver. The read is
    // non-blocking because a lone flow-control byte is under `WAKEUP_CHARS`, so
    // its arrival publishes no input event a blocking read could park on.
    let arrived = fixtures::drain_then_read_byte(pair.slave, pair.master, 0x13);

    let _ = tty::set_termios(pair.slave, &saved);

    if !arrived {
        klog_info!("TTY_TEST: BUG - IXOFF never reached the peer write path");
        return TestResult::Fail;
    }
    TestResult::Pass
}
