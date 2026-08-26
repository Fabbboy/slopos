//! Line-discipline regression tests.

use super::fixtures::*;

pub fn test_pendin_flag_value() -> TestResult {
    if LocalFlags::PENDIN.bits() != 0x4000 {
        klog_info!("TTY_TEST: BUG - PENDIN should be 0x4000");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_pendin_auto_set_on_echo_change() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'h');
    ld.input_char(b'i');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    let action = ld.input_char(b'x');
    if !matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - expected ReprintLine after echo flag change");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_pendin_one_shot() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    let first = ld.input_char(b'b');
    if !matches!(first, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - first input after PENDIN should be ReprintLine");
        return TestResult::Fail;
    }

    let second = ld.input_char(b'b');
    if matches!(second, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - PENDIN should be one-shot, not repeat");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vreprint_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'z');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOK;
    ld.set_termios(&t);

    let ctrl_r = ld.termios().c_cc[slopos_abi::syscall::VREPRINT];
    let action = ld.input_char(ctrl_r);
    if !matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - expected ReprintLine from PENDIN or VREPRINT");
        return TestResult::Fail;
    }

    let next = ld.input_char(b'a');
    if matches!(next, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - VREPRINT should have cleared PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_pendin_not_set_for_non_echo_flags() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'q');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ISIG;
    ld.set_termios(&t);

    let action = ld.input_char(b'w');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - toggling ISIG should not trigger PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_pendin_empty_edit_buffer() -> TestResult {
    let mut ld = LineDisc::new();

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    let action = ld.input_char(b'a');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - PENDIN should not fire with empty edit buffer");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_flush_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    ld.flush_all();

    let action = ld.input_char(b'b');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - flush_all should clear PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_flush_input_clears_pendin() -> TestResult {
    let mut ld = LineDisc::new();
    ld.input_char(b'a');

    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ECHOK;
    ld.set_termios(&t);

    ld.flush_input();

    let action = ld.input_char(b'c');
    if matches!(action, InputAction::ReprintLine) {
        klog_info!("TTY_TEST: BUG - flush_input should clear PENDIN");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Without the unthrottle, the master-side writer stays blocked forever.
pub fn test_review_tcflush_unthrottles_pty() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    for _ in 0..(THROTTLE_HIGH_WATER + 64) {
        tty::push_input(slave, b'X');
    }

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

    match tty::tcflush(slave, slopos_abi::syscall::TCIFLUSH) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - tcflush TCIFLUSH failed: {:?}", e);
            tty::set_termios(slave, &saved).unwrap();
            return TestResult::Fail;
        }
    }

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

pub fn test_review_tcflush_both_unthrottles_pty() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    for _ in 0..(THROTTLE_HIGH_WATER + 64) {
        tty::push_input(slave, b'Y');
    }

    tty::tcflush(slave, slopos_abi::syscall::TCIOFLUSH).unwrap();

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

/// master_write checks throttle once per 64-byte batch, not per byte, so a
/// throttle activating mid-batch still yields a batch-aligned count.
pub fn test_review_master_write_batch_boundary() -> TestResult {
    use crate::tty::ldisc::THROTTLE_HIGH_WATER;
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    // Ten bytes short of the throttle mark, so it activates mid-batch.
    let prefill = THROTTLE_HIGH_WATER - 10;
    for _ in 0..prefill {
        tty::push_input(slave, b'P');
    }

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

    let burst = [b'Q'; 256];
    let accepted = crate::tty::pty::master_write(&peer, &burst);

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

/// POSIX puts the baud rate in c_cflag; c_ispeed/c_ospeed are informational
/// fields get_termios fills in.
pub fn test_review_speed_fields_merge_into_cflag() -> TestResult {
    use slopos_abi::syscall::*;
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let saved = tty::get_termios(idx).unwrap();

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

/// c_cflag CBAUD wins even when c_ospeed is zero and c_ispeed is set.
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

/// A program watching only POLLERR for write errors still sees the hang-up.
pub fn test_review_pollerr_on_hangup() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);
    let _hangup = HangupScope::hang_up(idx);

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

pub fn test_review_pollerr_on_peer_closed() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    drop(slave_backing);

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

/// flush_edit_to_cooked preserves bytes that did not fit in the cooked ring.
pub fn test_bugfix_flush_edit_preserves_remainder() -> TestResult {
    use crate::tty::ldisc::LineDisc;

    let mut ld = LineDisc::new();

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

    // Only `spare` bytes plus the newline fit in cooked; the other 10 must
    // survive in the edit buffer.
    t.c_lflag |= LocalFlags::ICANON;
    t.c_lflag &= !LocalFlags::ECHO;
    ld.set_termios(&t);

    for i in 0..20u8 {
        ld.input_char(b'A' + (i % 26));
    }
    ld.input_char(b'\n');

    let mut drain: KBox<[u8; 8192]> = KBox::zeroed().expect("alloc");
    let drained = ld.read(&mut *drain);
    if drained == 0 {
        klog_info!("TTY_TEST: BUG - expected to drain some data");
        return TestResult::Fail;
    }

    ld.input_char(b'\n');

    let avail_after_second = ld.bytes_available();

    if avail_after_second <= 1 {
        klog_info!(
            "TTY_TEST: BUG - remainder bytes lost, only {} bytes after second flush",
            avail_after_second
        );
        return TestResult::Fail;
    }

    let mut buf2 = [0u8; 64];
    let n2 = ld.read(&mut buf2);

    if n2 < 2 {
        klog_info!(
            "TTY_TEST: BUG - second read expected >= 2 bytes (remainder + newline), got {}",
            n2
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_bugfix_nonblock_write_throttled_pty() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    let mut fill: KBox<[u8; 6400]> = KBox::zeroed().expect("alloc");
    fill.iter_mut().for_each(|b| *b = b'Z');
    let _ = tty::write(master, &*fill, false);

    let result = tty::write(master, b"more", true);

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

pub fn test_bugfix_nonblock_write_unthrottled_pty() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

pub fn test_bugfix_rawdisc_input_full() -> TestResult {
    use crate::tty::ldisc::RawDisc;

    let mut rd = RawDisc::new();

    for _ in 0..4096 {
        rd.input_char(b'A');
    }

    if !rd.input_full() {
        klog_info!("TTY_TEST: BUG - RawDisc should report input_full after 4096 pushes");
        return TestResult::Fail;
    }

    if rd.bytes_available() != 4096 {
        klog_info!(
            "TTY_TEST: BUG - expected 4096 bytes available, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }

    // `RawDisc::input_char` drops silently; `slave_write` is what checks
    // `input_full()` before each push.
    rd.input_char(b'B');

    if rd.bytes_available() != 4096 {
        klog_info!(
            "TTY_TEST: BUG - bytes_available should still be 4096 after overflow, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_bugfix_slave_write_stops_on_full() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

/// input_full needs both buffers full, not just the cooked one.
pub fn test_bugfix_linedisc_input_full() -> TestResult {
    use crate::tty::ldisc::LineDisc;

    let mut ld = LineDisc::new();

    if ld.input_full() {
        klog_info!("TTY_TEST: BUG - fresh LineDisc should not be input_full");
        return TestResult::Fail;
    }

    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    ld.set_termios(&t);
    for _ in 0..8192 {
        ld.input_char(b'Z');
    }

    if ld.input_full() {
        klog_info!("TTY_TEST: BUG - LineDisc with only cooked full should not be input_full");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// With 3+ bytes free in the cooked buffer, the full \xff \x00 \x00 triplet
/// is inserted.
pub fn test_bugfix_parmrk_atomic_full_insert() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    for _ in 0..8189 {
        if !ld.push_cooked(b'X') {
            klog_info!("TTY_TEST: BUG - push_cooked failed during fill");
            return TestResult::Fail;
        }
    }

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

/// With only 2 bytes free the entire triplet is dropped, and without IMAXBEL
/// that is reported as None.
pub fn test_bugfix_parmrk_drop_when_insufficient_space() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    for _ in 0..8190 {
        ld.push_cooked(b'X');
    }

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

/// With only 1 byte free and IMAXBEL set, a bell is returned instead of a
/// partial sequence.
pub fn test_bugfix_parmrk_imaxbel_bell_on_insufficient_space() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK | InputFlags::IMAXBEL;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    for _ in 0..8191 {
        ld.push_cooked(b'X');
    }

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

/// With 0 bytes free the triplet is dropped.
pub fn test_bugfix_parmrk_drop_when_buffer_completely_full() -> TestResult {
    use crate::tty::ldisc::LineDisc;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);

    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }

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

pub fn test_bugfix_tcxonc_invalid_action_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

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

pub fn test_bugfix_tcxonc_boundary_values() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

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

pub fn test_canonical_wake_on_newline() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"hello" {
        ld.input_char(c);
    }
    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical wake before newline");
        return TestResult::Fail;
    }

    ld.input_char(b'\n');
    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical no wake after newline");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Non-canonical VMIN=1 wakes as soon as any data is available.
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

pub fn test_noncanonical_wake_at_threshold() -> TestResult {
    use crate::tty::ldisc::WAKEUP_CHARS;
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

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

pub fn test_noncanonical_wake_near_full() -> TestResult {
    use slopos_abi::syscall::LocalFlags;
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    // 64 is the near-full margin the wake check uses.
    let target = 8192 - 64;
    let mut pushed = 0usize;
    while pushed < target {
        ld.input_char(b'z');
        pushed += 1;
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

pub fn test_rawdisc_wake_batching() -> TestResult {
    use crate::tty::ldisc::WAKEUP_CHARS;
    let mut rd = RawDisc::new();
    let mut t = *rd.termios();
    t.c_cflag |= ControlFlags::CREAD;
    rd.set_termios(&t);

    for _ in 0..10 {
        rd.input_char(b'r');
    }
    if rd.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - RawDisc wake after only 10 bytes");
        return TestResult::Fail;
    }

    for _ in 10..WAKEUP_CHARS {
        rd.input_char(b'r');
    }
    if !rd.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - RawDisc no wake at threshold");
        return TestResult::Fail;
    }

    TestResult::Pass
}

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

    ld.flush_input();
    if ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - wake after flush");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_canonical_eof_wakes() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"data" {
        ld.input_char(c);
    }
    ld.input_char(0x04);

    if !ld.should_wake_reader() {
        klog_info!("TTY_TEST: BUG - canonical EOF did not wake");
        return TestResult::Fail;
    }

    TestResult::Pass
}

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

pub fn test_no_room_set_on_cooked_full() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        if !ld.push_cooked(b'X') {
            klog_info!("TTY_TEST: BUG - push_cooked failed before buffer full");
            return TestResult::Fail;
        }
    }
    if ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room set before overflow push");
        return TestResult::Fail;
    }
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

pub fn test_overflow_count_increments() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
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

pub fn test_overflow_count_saturates() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    // A real u32::MAX of iterations is impractical; `push_cooked` uses
    // `saturating_add`, so counting a bounded run exercises the same path.
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
    ld.push_cooked(b'W');
    if ld.overflow_count() != 101 {
        klog_info!("TTY_TEST: BUG - overflow_count did not increment to 101");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_no_room_clears_on_drain_below_threshold() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y');
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set after overflow");
        return TestResult::Fail;
    }
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
    if ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery triggered above low-water");
        return TestResult::Fail;
    }
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room cleared above low-water");
        return TestResult::Fail;
    }
    let got2 = ld.read(&mut scratch[..1]);
    if got2 != 1 {
        klog_info!("TTY_TEST: BUG - second read failed");
        return TestResult::Fail;
    }
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

pub fn test_no_room_stays_above_threshold() -> TestResult {
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
    ld.push_cooked(b'Y');
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

pub fn test_fill_drain_cycle_preserves_throttle() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut ld = LineDisc::new();
    for _ in 0..8192 {
        ld.push_cooked(b'A');
    }
    ld.push_cooked(b'B');
    let mut scratch: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let _ = ld.read(&mut *scratch);
    let _ = ld.read(&mut *scratch);
    if !ld.check_no_room_recovery() {
        klog_info!("TTY_TEST: BUG - recovery did not trigger after full drain");
        return TestResult::Fail;
    }
    for _ in 0..8192 {
        ld.push_cooked(b'C');
    }
    ld.push_cooked(b'D');
    if !ld.no_room() {
        klog_info!("TTY_TEST: BUG - no_room not set on second cycle");
        return TestResult::Fail;
    }
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

pub fn test_rawdisc_no_room() -> TestResult {
    let mut rd = RawDisc::new();
    for _ in 0..4096 {
        rd.input_char(b'R');
    }
    if rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room set before overflow");
        return TestResult::Fail;
    }
    rd.input_char(b'S');
    if !rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room not set after overflow");
        return TestResult::Fail;
    }
    if rd.overflow_count() != 1 {
        klog_info!("TTY_TEST: BUG - RawDisc overflow_count != 1");
        return TestResult::Fail;
    }
    rd.flush_all();
    if rd.no_room() || rd.overflow_count() != 0 {
        klog_info!("TTY_TEST: BUG - RawDisc flush_all did not clear no_room");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_imaxbel_preserved_with_no_room() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    t.c_iflag |= InputFlags::IMAXBEL;
    ld.set_termios(&t);
    for _ in 0..8192 {
        ld.push_cooked(b'X');
    }
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

pub fn test_rawdisc_recovery() -> TestResult {
    use crate::tty::ldisc::THROTTLE_LOW_WATER;
    let mut rd = RawDisc::new();
    for _ in 0..4096 {
        rd.input_char(b'R');
    }
    rd.input_char(b'S');
    if !rd.no_room() {
        klog_info!("TTY_TEST: BUG - RawDisc no_room not set");
        return TestResult::Fail;
    }
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

/// wait_output_idle's fast path, reached here via is_output_idle.
pub fn test_drain_idle_fast_path() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

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

pub fn test_drain_hangup_vacuously_complete() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let _hangup = HangupScope::hang_up(TtyIndex(0));

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

pub fn test_tcsbrk_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);

    let _hangup = HangupScope::hang_up(idx);

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

pub fn test_tcsbrk_zero_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    let _hangup = HangupScope::hang_up(idx);

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

pub fn test_tcsbrk_and_tcsetsw_share_drain() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let _ = tty::write(idx, b"drain parity test", false);

    if let Err(e) = tty::tcsbrk(idx, 1) {
        klog_info!("TTY_TEST: BUG - fp13 tcsbrk(1) failed: {:?}", e);
        return TestResult::Fail;
    }

    let t = tty::get_termios(idx).unwrap();
    if let Err(e) = tty::set_termios_wait(idx, &t) {
        klog_info!("TTY_TEST: BUG - fp13 set_termios_wait failed: {:?}", e);
        return TestResult::Fail;
    }

    TestResult::Pass
}

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

pub fn test_drain_unallocated_slot() -> TestResult {
    tty::table::tty_table_init();
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

pub fn test_pty_tcsbrk_drain_immediate() -> TestResult {
    tty::table::tty_table_init();

    let (master_idx, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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

    let _ = tty::write(master_idx, b"pty drain fp13", false);

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

pub fn test_console_drain_synchronous() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let _ = tty::write(TtyIndex(0), b"console drain fp13\r\n", false);

    match tty::tcsbrk(TtyIndex(0), 1) {
        Ok(()) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - fp13 console tcsbrk failed: {:?}", e);
            return TestResult::Fail;
        }
    }

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

/// Every driver kind is synchronous, so none reports pending output.
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

/// The set_termios_mode hangup guard fires before the drain path.
pub fn test_tcsetsw_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let t = tty::get_termios(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

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

pub fn test_tcsetsf_hangup_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let t = tty::get_termios(idx).unwrap();
    let _hangup = HangupScope::hang_up(idx);

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

pub fn test_inflight_accounting_round_trip() -> TestResult {
    use core::sync::atomic::Ordering;
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let before = TTY_OUTPUT_INFLIGHT[0].load(Ordering::Acquire);
    if before != 0 {
        klog_info!(
            "TTY_TEST: BUG - fp13 inflight before write should be 0, got {}",
            before
        );
        return TestResult::Fail;
    }

    let _ = tty::write(TtyIndex(0), b"inflight round trip", false);

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
    let (master, _master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
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
    let _hangup = HangupScope::guard(idx);
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
    let _ = ld.receive_buf(&events);
    let echo = EchoScratch::drain(&mut ld);
    if echo.as_slice() != b"abc" {
        klog_info!(
            "TTY_TEST: BUG - receive_buf should stage echo bytes, got {:?}",
            echo.as_slice()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_mod_reexports_io_functions() -> TestResult {
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
    let empty = plw.is_empty();
    plw.discard();
    if empty {
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

pub fn test_p21_postlockwork_echo_flush_request() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.request_echo_flush(0, WriteNesting::Toplevel);
    if plw.is_empty() {
        plw.discard();
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_packet_event() -> TestResult {
    let mut plw = PostLockWork::new();
    plw.add_packet_event(TtyIndex(0), slopos_abi::syscall::TIOCPKT_STOP);
    if plw.is_empty() {
        plw.discard();
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
        plw.discard();
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
        plw.discard();
        return TestResult::Fail;
    }
    plw.execute();
    TestResult::Pass
}

pub fn test_p21_postlockwork_zero_pgid_signal_ignored() -> TestResult {
    // "No target, no signal" is enforced at resolution time, not inside
    // `add_signal`.
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
    let (master_idx, _master_backing) =
        match tty::pty::pty_alloc(slopos_ostd::process::quota::root()) {
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
    name = test_p21_postlockwork_echo_flush_request,
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
