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
