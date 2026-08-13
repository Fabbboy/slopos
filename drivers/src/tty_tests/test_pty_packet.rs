//! Split from test_ldisc.rs: test_pty_packet.rs

use super::fixtures::*;

// ===========================================================================
// PTY Packet Mode (TIOCPKT)
// ===========================================================================

/// Verify TIOCPKT and TIOCPKT_* ABI constant values.
pub fn test_abi_constants() -> TestResult {
    use slopos_abi::syscall::{
        TIOCPKT, TIOCPKT_DATA, TIOCPKT_DOSTOP, TIOCPKT_FLUSHREAD, TIOCPKT_FLUSHWRITE,
        TIOCPKT_NOSTOP, TIOCPKT_START, TIOCPKT_STOP,
    };
    if TIOCPKT != 0x5420 {
        klog_info!(
            "TTY_TEST: BUG - TIOCPKT should be 0x5420, got 0x{:X}",
            TIOCPKT
        );
        return TestResult::Fail;
    }
    if TIOCPKT_DATA != 0x00 {
        return TestResult::Fail;
    }
    if TIOCPKT_FLUSHREAD != 0x01 {
        return TestResult::Fail;
    }
    if TIOCPKT_FLUSHWRITE != 0x02 {
        return TestResult::Fail;
    }
    if TIOCPKT_STOP != 0x04 {
        return TestResult::Fail;
    }
    if TIOCPKT_START != 0x08 {
        return TestResult::Fail;
    }
    if TIOCPKT_NOSTOP != 0x10 {
        return TestResult::Fail;
    }
    if TIOCPKT_DOSTOP != 0x20 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// With packet mode ON, master read gets TIOCPKT_DATA prefix.
pub fn test_tiocpkt_on_data_prefixed() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    // Enable packet mode on the master.
    if let Err(e) = tty::set_packet_mode(master, true) {
        klog_info!("TTY_TEST: BUG - set_packet_mode failed: {:?}", e);
        packet_mode_teardown_pty(master, slave, &saved);
        return TestResult::Fail;
    }

    // Slave write -> master read should get TIOCPKT_DATA prefix.
    let _ = tty::write(slave, b"hi", false);
    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(n)
            if n >= 3
                && buf[0] == slopos_abi::syscall::TIOCPKT_DATA
                && buf[1] == b'h'
                && buf[2] == b'i' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - packet mode read expected [0x00, 'h', 'i'], got {:?}, buf={:?}",
                other,
                &buf[..8]
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

/// With packet mode OFF, master read has no prefix.
pub fn test_tiocpkt_off_normal_read() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    // Packet mode is OFF by default.
    let _ = tty::write(slave, b"AB", false);
    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(n) if n >= 2 && buf[0] == b'A' && buf[1] == b'B' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - non-packet read expected ['A', 'B'], got {:?}, buf={:?}",
                other,
                &buf[..4]
            );
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

/// Slave input flush sets TIOCPKT_FLUSHREAD on master.
pub fn test_tiocpkt_slave_flush_read() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    // Use TCSETSF (set_termios_flush) on the slave to trigger FLUSHREAD.
    let t = tty::get_termios(slave).unwrap();
    tty::set_termios_flush(slave, &t).unwrap();

    // Master read should return the FLUSHREAD packet event.
    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(1) if (buf[0] & slopos_abi::syscall::TIOCPKT_FLUSHREAD) != 0 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected TIOCPKT_FLUSHREAD event, got {:?}, buf[0]=0x{:02X}",
                other,
                buf[0]
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

/// Slave IXON toggle triggers TIOCPKT_DOSTOP / TIOCPKT_NOSTOP.
pub fn test_tiocpkt_ixon_toggle() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    // Enable IXON on the slave -> should produce DOSTOP.
    let mut t = tty::get_termios(slave).unwrap();
    t.c_iflag |= InputFlags::IXON;
    tty::set_termios(slave, &t).unwrap();

    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(1) if (buf[0] & slopos_abi::syscall::TIOCPKT_DOSTOP) != 0 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected TIOCPKT_DOSTOP, got {:?}, buf[0]=0x{:02X}",
                other,
                buf[0]
            );
            let _ = tty::set_packet_mode(master, false);
            packet_mode_teardown_pty(master, slave, &saved);
            return TestResult::Fail;
        }
    }

    // Clear IXON -> should produce NOSTOP.
    t.c_iflag &= !InputFlags::IXON;
    tty::set_termios(slave, &t).unwrap();

    match tty::read(master, &mut buf, true) {
        Ok(1) if (buf[0] & slopos_abi::syscall::TIOCPKT_NOSTOP) != 0 => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - expected TIOCPKT_NOSTOP, got {:?}, buf[0]=0x{:02X}",
                other,
                buf[0]
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

/// Disabling packet mode clears pending events.
pub fn test_tiocpkt_disable_clears_events() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    // Trigger an event.
    let mut t = tty::get_termios(slave).unwrap();
    t.c_iflag |= InputFlags::IXON;
    tty::set_termios(slave, &t).unwrap();

    // Disable packet mode — should clear pending events.
    tty::set_packet_mode(master, false).unwrap();

    // Re-enable and check there are no stale events.
    tty::set_packet_mode(master, true).unwrap();

    // Write data so there IS something to read.
    let _ = tty::write(slave, b"X", false);
    let mut buf = [0u8; 16];
    match tty::read(master, &mut buf, true) {
        Ok(n) if n >= 2 && buf[0] == slopos_abi::syscall::TIOCPKT_DATA && buf[1] == b'X' => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - after disable/re-enable expected data, got {:?}, buf={:?}",
                other,
                &buf[..4]
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

/// poll_events reports POLLIN when packet events are pending.
pub fn test_poll_packet_events_pollin() -> TestResult {
    let Some((master, slave, saved, _guard)) = packet_mode_setup_pty() else {
        klog_info!("TTY_TEST: BUG - packet_mode setup failed");
        return TestResult::Fail;
    };

    tty::set_packet_mode(master, true).unwrap();

    // No events, no data -> POLLIN should NOT be set.
    let revents = tty::poll_events(master, slopos_abi::syscall::POLLIN);
    if (revents & slopos_abi::syscall::POLLIN) != 0 {
        klog_info!("TTY_TEST: BUG - POLLIN should not be set with no data and no events");
        let _ = tty::set_packet_mode(master, false);
        packet_mode_teardown_pty(master, slave, &saved);
        return TestResult::Fail;
    }

    // Trigger a packet event.
    let mut t = tty::get_termios(slave).unwrap();
    t.c_iflag |= InputFlags::IXON;
    tty::set_termios(slave, &t).unwrap();

    // Now POLLIN should be set.
    let revents = tty::poll_events(master, slopos_abi::syscall::POLLIN);
    if (revents & slopos_abi::syscall::POLLIN) == 0 {
        klog_info!("TTY_TEST: BUG - POLLIN should be set with pending packet events");
        let _ = tty::set_packet_mode(master, false);
        packet_mode_teardown_pty(master, slave, &saved);
        return TestResult::Fail;
    }

    // Consume the event.
    let mut buf = [0u8; 16];
    let _ = tty::read(master, &mut buf, true);

    // POLLIN should no longer be set.
    let revents = tty::poll_events(master, slopos_abi::syscall::POLLIN);
    if (revents & slopos_abi::syscall::POLLIN) != 0 {
        klog_info!("TTY_TEST: BUG - POLLIN should not be set after consuming events");
        let _ = tty::set_packet_mode(master, false);
        packet_mode_teardown_pty(master, slave, &saved);
        return TestResult::Fail;
    }

    let _ = tty::set_packet_mode(master, false);
    packet_mode_teardown_pty(master, slave, &saved);
    TestResult::Pass
}

/// set_packet_mode on non-master returns error.
pub fn test_set_packet_mode_non_master() -> TestResult {
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

    // set_packet_mode on the slave should fail.
    match tty::set_packet_mode(slave, true) {
        Err(TtyError::NotAllocated) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - set_packet_mode on slave should return NotAllocated, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // set_packet_mode on the console (TtyIndex(0)) should also fail.
    match tty::set_packet_mode(TtyIndex(0), true) {
        Err(TtyError::NotAllocated) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - set_packet_mode on console should return NotAllocated, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

slopos_testing::stest!(name = test_abi_constants, suite = tty_test_pty_packet);
slopos_testing::stest!(
    name = test_tiocpkt_on_data_prefixed,
    suite = tty_test_pty_packet
);
slopos_testing::stest!(
    name = test_tiocpkt_off_normal_read,
    suite = tty_test_pty_packet
);
slopos_testing::stest!(
    name = test_tiocpkt_slave_flush_read,
    suite = tty_test_pty_packet
);
slopos_testing::stest!(name = test_tiocpkt_ixon_toggle, suite = tty_test_pty_packet);
slopos_testing::stest!(
    name = test_tiocpkt_disable_clears_events,
    suite = tty_test_pty_packet
);
slopos_testing::stest!(
    name = test_poll_packet_events_pollin,
    suite = tty_test_pty_packet
);
slopos_testing::stest!(
    name = test_set_packet_mode_non_master,
    suite = tty_test_pty_packet
);
