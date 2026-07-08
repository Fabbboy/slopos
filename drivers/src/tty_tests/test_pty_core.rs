//! Split from test_ldisc.rs: test_pty_core.rs

use super::fixtures::*;

// ===========================================================================
// Responsibility Split — PTY Foundation
// ===========================================================================

// -- 18.4: SessionId / ProcessGroupId newtype tests --

/// SessionId::new(0) returns None (zero is the "no session" sentinel).
pub fn test_session_id_zero_is_none() -> TestResult {
    if SessionId::new(0).is_some() {
        klog_info!("TTY_TEST: BUG - SessionId::new(0) should be None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SessionId::new(non-zero) returns Some and round-trips through get().
pub fn test_session_id_round_trip() -> TestResult {
    match SessionId::new(42) {
        Some(sid) => {
            if sid.get() != 42 {
                klog_info!(
                    "TTY_TEST: BUG - SessionId(42).get() = {}, expected 42",
                    sid.get()
                );
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        None => {
            klog_info!("TTY_TEST: BUG - SessionId::new(42) returned None");
            TestResult::Fail
        }
    }
}

/// ProcessGroupId::new(0) returns None.
pub fn test_pgrp_id_zero_is_none() -> TestResult {
    if ProcessGroupId::new(0).is_some() {
        klog_info!("TTY_TEST: BUG - ProcessGroupId::new(0) should be None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ProcessGroupId::new(non-zero) round-trips through get().
pub fn test_pgrp_id_round_trip() -> TestResult {
    match ProcessGroupId::new(99) {
        Some(pgid) => {
            if pgid.get() != 99 {
                klog_info!(
                    "TTY_TEST: BUG - ProcessGroupId(99).get() = {}, expected 99",
                    pgid.get()
                );
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        None => {
            klog_info!("TTY_TEST: BUG - ProcessGroupId::new(99) returned None");
            TestResult::Fail
        }
    }
}

/// TtySession uses Option-based fields: new() has None for all IDs.
pub fn test_session_option_fields() -> TestResult {
    let s = TtySession::new();
    if s.session_leader.is_some() || s.session_id.is_some() || s.fg_pgrp.is_some() {
        klog_info!("TTY_TEST: BUG - new TtySession should have None for all Option fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// After attach(), Option fields are Some; after detach(), they are None.
pub fn test_session_option_attach_detach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 20);
    if s.session_leader.is_none() || s.session_id.is_none() || s.fg_pgrp.is_none() {
        klog_info!("TTY_TEST: BUG - Option fields should be Some after attach");
        return TestResult::Fail;
    }
    s.detach();
    if s.session_leader.is_some() || s.session_id.is_some() || s.fg_pgrp.is_some() {
        klog_info!("TTY_TEST: BUG - Option fields should be None after detach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// -- 18.2: RawDisc / LdiscKind tests --

/// RawDisc: new instance has no data.
pub fn test_raw_disc_new_empty() -> TestResult {
    let rd = RawDisc::new();
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - new RawDisc has data");
        return TestResult::Fail;
    }
    if rd.is_canonical() {
        klog_info!("TTY_TEST: BUG - RawDisc should not be canonical");
        return TestResult::Fail;
    }
    if rd.is_stopped() {
        klog_info!("TTY_TEST: BUG - RawDisc should not be stopped");
        return TestResult::Fail;
    }
    if !rd.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - RawDisc edit_content should be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc: input_char pushes byte, read retrieves it.
pub fn test_raw_disc_input_read() -> TestResult {
    let mut rd = RawDisc::new();
    let _ = rd.input_char(b'A');
    let _ = rd.input_char(b'B');
    if !rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should have data after input_char");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = rd.read(&mut buf);
    if n != 2 || buf[0] != b'A' || buf[1] != b'B' {
        klog_info!(
            "TTY_TEST: BUG - RawDisc read got {} bytes [{}, {}]",
            n,
            buf[0],
            buf[1]
        );
        return TestResult::Fail;
    }
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should be empty after reading all");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// RawDisc: process_output_byte passes through unchanged.
pub fn test_raw_disc_output_passthrough() -> TestResult {
    let mut rd = RawDisc::new();
    match rd.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!(
                    "TTY_TEST: BUG - RawDisc output should passthrough, got len={} buf[0]={}",
                    len,
                    buf[0]
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - RawDisc output should emit, got other action");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// RawDisc: flush_all clears buffer.
pub fn test_raw_disc_flush() -> TestResult {
    let mut rd = RawDisc::new();
    let _ = rd.input_char(b'X');
    rd.flush_all();
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should be empty after flush_all");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// LdiscKind::NTty delegates to LineDisc correctly.
pub fn test_ldisc_kind_ntty_delegation() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    // NTty should be canonical by default.
    if !lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should be canonical by default");
        return TestResult::Fail;
    }
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have no data initially");
        return TestResult::Fail;
    }
    // Feed a character + newline to flush to cooked buffer.
    let _ = lk.input_char(b'A');
    let _ = lk.input_char(b'\n');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have data after newline");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 8];
    let n = lk.read(&mut buf);
    // Canonical: 'A' + '\n' = 2 bytes.
    if n != 2 || buf[0] != b'A' || buf[1] != b'\n' {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty read got {} bytes", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// LdiscKind::Raw delegates to RawDisc correctly.
pub fn test_ldisc_kind_raw_delegation() -> TestResult {
    let mut lk = LdiscKind::Raw(RawDisc::new());
    // Raw should NOT be canonical.
    if lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should not be canonical");
        return TestResult::Fail;
    }
    // Input bytes should go directly to buffer.
    let _ = lk.input_char(b'Z');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have data after input_char");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 1 || buf[0] != b'Z' {
        klog_info!(
            "TTY_TEST: BUG - LdiscKind::Raw read got {} bytes, buf[0]={}",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// -- 18.3: PTY driver stub tests --

/// PtyMaster and PtySlave DriverId variants exist and are distinct.
pub fn test_pty_driver_id_variants() -> TestResult {
    let master_id = DriverId::PtyMaster { peer: KWeak::new() };
    let slave_id = DriverId::PtySlave { peer: KWeak::new() };
    if !matches!(master_id, DriverId::PtyMaster { .. }) {
        klog_info!("TTY_TEST: BUG - PtyMaster DriverId should be the PtyMaster variant");
        return TestResult::Fail;
    }
    if !matches!(slave_id, DriverId::PtySlave { .. }) {
        klog_info!("TTY_TEST: BUG - PtySlave DriverId should be the PtySlave variant");
        return TestResult::Fail;
    }
    // The PTY variants are distinct from each other and from the consoles.
    if matches!(
        master_id,
        DriverId::SerialConsole | DriverId::VConsole | DriverId::PtySlave { .. }
    ) {
        klog_info!("TTY_TEST: BUG - PtyMaster should differ from console and slave variants");
        return TestResult::Fail;
    }
    if matches!(
        slave_id,
        DriverId::SerialConsole | DriverId::VConsole | DriverId::PtyMaster { .. }
    ) {
        klog_info!("TTY_TEST: BUG - PtySlave should differ from console and master variants");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyMaster driver kind returns correct DriverId.
pub fn test_pty_master_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtyMaster { peer: KWeak::new() };
    if !matches!(drv.id(), DriverId::PtyMaster { .. }) {
        klog_info!("TTY_TEST: BUG - PtyMaster TtyDriverKind should return DriverId::PtyMaster");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtySlave driver kind returns correct DriverId.
pub fn test_pty_slave_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtySlave { peer: KWeak::new() };
    if !matches!(drv.id(), DriverId::PtySlave { .. }) {
        klog_info!("TTY_TEST: BUG - PtySlave TtyDriverKind should return DriverId::PtySlave");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// PTY Pair Atomicity & Lifecycle Hardening
// ===========================================================================

/// pty_alloc initialises both master and slave slots atomically.
pub fn test_pty_alloc_pair_both_initialized() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master) {
        Ok(n) => n,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(slave_num as u8);

    // Both slots should be Some (initialised).
    let master_exists =
        tty::table::with_tty_ref(master, |tty| tty.index == master).unwrap_or(false);
    let slave_exists = tty::table::with_tty_ref(slave, |tty| tty.index == slave).unwrap_or(false);

    if !master_exists || !slave_exists {
        klog_info!(
            "TTY_TEST: BUG - pair not fully initialised (master={}, slave={})",
            master_exists,
            slave_exists
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// An empty non-blocking master read reports WouldBlock — never `Ok(0)`.
///
/// The master's `RawDisc` defaults to VMIN=1 so that `read() == 0` is
/// reserved for "peer closed" (true EOF). With VMIN=0 the empty read would
/// return the polling-read `Ok(0)`, which a terminal emulator cannot
/// distinguish from EOF — it would tear the session down the moment the
/// slave went quiet. Only closing the last slave open flips the master to
/// EOF, letting the emulator detect the shell exiting.
pub fn test_pty_master_empty_read_would_block_not_eof() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_backing = tty::pty_open_slave(slave).unwrap();

    // Empty + non-blocking: must be WouldBlock, not the EOF-shaped Ok(0).
    let mut buf = [0u8; 8];
    let empty = tty::read(master, &mut buf, true);
    if empty != Err(TtyError::WouldBlock) {
        klog_info!(
            "TTY_TEST: BUG - empty nonblock master read returned {:?}, want WouldBlock",
            empty
        );
        return TestResult::Fail;
    }

    // Data still flows: slave output is readable from the master.
    let _ = tty::write(slave, b"x", false);
    let got = tty::read(master, &mut buf, true);
    if !matches!(got, Ok(n) if n >= 1) {
        klog_info!(
            "TTY_TEST: BUG - master read after slave write returned {:?}",
            got
        );
        return TestResult::Fail;
    }

    // Closing the last slave open flips the drained master to EOF
    // (peer-closed latched), which is how the emulator sees shell exit.
    drop(slave_backing);
    let eof = tty::read(master, &mut buf, true);
    if eof != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - master read after last slave close returned {:?}, want Ok(0)",
            eof
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// TIOCSWINSZ on either PTY end updates BOTH views: the window size is a
/// property of the pair, and the slave-side reader (the shell asking for
/// its columns) must see the geometry the terminal emulator pushed onto
/// the master — otherwise line-wrap arithmetic diverges from the render.
pub fn test_pty_winsize_shared_across_pair() -> TestResult {
    use slopos_abi::syscall::UserWinsize;

    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    let ws = UserWinsize {
        ws_row: 21,
        ws_col: 64,
        ws_xpixel: 640,
        ws_ypixel: 480,
    };
    tty::set_winsize(master, &ws).unwrap();
    let slave_view = tty::get_winsize(slave).unwrap();
    let master_to_slave = slave_view.ws_row == 21 && slave_view.ws_col == 64;

    let ws2 = UserWinsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    tty::set_winsize(slave, &ws2).unwrap();
    let master_view = tty::get_winsize(master).unwrap();
    let slave_to_master = master_view.ws_row == 30 && master_view.ws_col == 100;

    if !master_to_slave || !slave_to_master {
        klog_info!(
            "TTY_TEST: BUG - winsize not shared across pair (m->s ok={}, s->m ok={})",
            master_to_slave,
            slave_to_master
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// closing master then slave frees both slots.
pub fn test_pty_close_master_first_frees_pair() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_backing = tty::pty_open_slave(slave).unwrap();

    // Close master first (triggers hangup on slave), then slave.
    drop(master_backing);
    drop(slave_backing);

    // Both slots should now be None (freed).
    let master_freed = TTY_SLOTS[master.0 as usize].lock().is_none();
    let slave_freed = TTY_SLOTS[slave.0 as usize].lock().is_none();

    if !master_freed || !slave_freed {
        klog_info!(
            "TTY_TEST: BUG - pair not freed after master-first close (master_freed={}, slave_freed={})",
            master_freed,
            slave_freed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// closing slave then master frees both slots (order independence).
pub fn test_pty_close_slave_first_frees_pair() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_backing = tty::pty_open_slave(slave).unwrap();

    // Close slave first, then master (order independence).
    drop(slave_backing);
    drop(master_backing);

    let master_freed = TTY_SLOTS[master.0 as usize].lock().is_none();
    let slave_freed = TTY_SLOTS[slave.0 as usize].lock().is_none();

    if !master_freed || !slave_freed {
        klog_info!(
            "TTY_TEST: BUG - pair not freed after slave-first close (master_freed={}, slave_freed={})",
            master_freed,
            slave_freed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// freed pair can be reallocated with fresh state.
pub fn test_pty_reallocation_after_free() -> TestResult {
    tty::table::tty_table_init();

    // Allocate + open + close a pair to return slots to the free pool.
    let (master1, master1_backing) = tty::pty_alloc().unwrap();
    let slave1 = TtyIndex(tty::get_pty_number(master1).unwrap() as u8);
    tty::set_pty_lock(master1, false).unwrap();
    let slave1_backing = tty::pty_open_slave(slave1).unwrap();
    drop(slave1_backing);
    drop(master1_backing);

    // Reallocate — should succeed and return valid indices.
    let (master2, _master2_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - reallocation failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave2 = TtyIndex(tty::get_pty_number(master2).unwrap() as u8);

    // Verify the reallocated pair is functional.
    tty::set_pty_lock(master2, false).unwrap();
    let _slave2_backing = tty::pty_open_slave(slave2).unwrap();

    let slave_is_pty = tty::is_pty_slave(slave2);
    let master_is_not_slave = !tty::is_pty_slave(master2);

    if !slave_is_pty || !master_is_not_slave {
        klog_info!(
            "TTY_TEST: BUG - reallocated pair has wrong types (slave_is_pty={}, master_is_not_slave={})",
            slave_is_pty,
            master_is_not_slave
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// pty_open_slave validates that the slot is actually a PTY slave.
pub fn test_pty_open_slave_validates_type() -> TestResult {
    tty::table::tty_table_init();

    // Try to open a serial console slot (index 0) as a PTY slave — should fail.
    let result = tty::pty_open_slave(TtyIndex(0));
    if result.is_ok() {
        klog_info!("TTY_TEST: BUG - pty_open_slave should reject non-slave index 0");
        drop(result);
        return TestResult::Fail;
    }

    // Try to open a non-existent slot — should fail.
    let result = tty::pty_open_slave(TtyIndex(5));
    if result.is_ok() {
        klog_info!("TTY_TEST: BUG - pty_open_slave should reject empty slot 5");
        drop(result);
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// An open slave keeps its slot alive after the master closes.
pub fn test_pty_open_slave_prevents_free() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);

    // Unlock slave so it can be opened (lock guard).
    tty::set_pty_lock(master, false).unwrap();

    // Open slave via the validated path.
    let slave_backing = match tty::pty_open_slave(slave) {
        Ok(b) => b,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_open_slave failed on valid slave");
            return TestResult::Fail;
        }
    };

    // Close master — the still-open slave keeps its slot alive.
    drop(master_backing);

    let slave_still_exists = TTY_SLOTS[slave.0 as usize].lock().is_some();

    // Cleanup.
    drop(slave_backing);

    if !slave_still_exists {
        klog_info!("TTY_TEST: BUG - slave freed while still open");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Closing the master frees the master slot at once, while any remaining
/// slave open keeps the slave slot alive until its last open drops.
pub fn test_extra_slave_open_keeps_slave_alive() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_open1 = tty::pty_open_slave(slave).unwrap();
    let slave_open2 = tty::pty_open_slave(slave).unwrap();

    // Closing the master frees the master slot and hangs up the slave, but
    // the two slave opens keep the slave slot alive.
    drop(master_backing);
    let master_freed = TTY_SLOTS[master.0 as usize].lock().is_none();
    let slave_alive_after_master = TTY_SLOTS[slave.0 as usize].lock().is_some();

    // One slave open remains: still alive.
    drop(slave_open1);
    let slave_alive_after_one = TTY_SLOTS[slave.0 as usize].lock().is_some();

    // Last slave open closes: slot freed.
    drop(slave_open2);
    let slave_freed = TTY_SLOTS[slave.0 as usize].lock().is_none();

    if !master_freed {
        klog_info!("TTY_TEST: BUG - master slot not freed on last master close");
        return TestResult::Fail;
    }
    if !slave_alive_after_master || !slave_alive_after_one {
        klog_info!("TTY_TEST: BUG - slave freed while a slave open remained");
        return TestResult::Fail;
    }
    if !slave_freed {
        klog_info!("TTY_TEST: BUG - slave slot not freed after its last open closed");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// rapid allocate/free/reallocate cycles produce valid pairs.
pub fn test_rapid_alloc_free_realloc() -> TestResult {
    tty::table::tty_table_init();

    for i in 0..3u8 {
        let (master, _master_backing) = match tty::pty_alloc() {
            Ok(pair) => pair,
            Err(err) => {
                klog_info!("TTY_TEST: BUG - rapid alloc cycle {} failed: {:?}", i, err);
                return TestResult::Fail;
            }
        };
        let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);

        tty::set_pty_lock(master, false).unwrap();
        let _slave_backing = tty::pty_open_slave(slave).unwrap();

        // Verify data flows correctly on this pair.
        let saved = tty::get_termios(slave).unwrap();
        let mut raw = saved;
        raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
        tty::set_termios(slave, &raw).unwrap();

        let write_ok = tty::write(master, b"x", false).is_ok();
        let mut buf = [0u8; 4];
        let read_ok = tty::read(slave, &mut buf, true) == Ok(1) && buf[0] == b'x';

        tty::set_termios(slave, &saved).unwrap();

        if !write_ok || !read_ok {
            klog_info!(
                "TTY_TEST: BUG - rapid alloc cycle {} data flow broken (write={}, read={})",
                i,
                write_ok,
                read_ok
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// pty_open_slave on a freed slave returns NotAllocated.
pub fn test_pty_open_slave_after_free() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_backing = tty::pty_open_slave(slave).unwrap();

    // Free the pair.
    drop(slave_backing);
    drop(master_backing);

    // Attempting to open the freed slave should fail.
    match tty::pty_open_slave(slave) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        Ok(backing) => {
            klog_info!("TTY_TEST: BUG - pty_open_slave on freed slave unexpectedly succeeded");
            drop(backing);
            TestResult::Fail
        }
        Err(other) => {
            klog_info!(
                "TTY_TEST: BUG - pty_open_slave on freed slave expected NotAllocated, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}
// ===========================================================================
// PTY Lifetime Safety & Scalable Capacity
// ===========================================================================

/// MAX_TTYS is now 32.
pub fn test_max_ttys_is_32() -> TestResult {
    if crate::tty::MAX_TTYS != 32 {
        klog_info!(
            "TTY_TEST: BUG - MAX_TTYS should be 32, got {}",
            crate::tty::MAX_TTYS
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A master's peer link upgrades to the live slave backing.
pub fn test_master_peer_link_targets_slave() -> TestResult {
    tty::table::tty_table_init();
    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    let peer = peer_link_of(master);
    match peer.upgrade() {
        Some(slave_backing) if slave_backing.index() == slave => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - master peer link should upgrade to the slave backing");
            TestResult::Fail
        }
    }
}

/// A slave's peer link upgrades to the live master backing.
pub fn test_slave_peer_link_targets_master() -> TestResult {
    tty::table::tty_table_init();
    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    let peer = peer_link_of(slave);
    match peer.upgrade() {
        Some(master_backing) if master_backing.index() == master => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - slave peer link should upgrade to the master backing");
            TestResult::Fail
        }
    }
}

/// Freeing a PTY pair drops both backings: their weak links stop upgrading.
pub fn test_backing_dies_on_free() -> TestResult {
    tty::table::tty_table_init();
    let (master, master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let master_weak = KArc::downgrade(&master_backing);
    // The master's peer link points at the slave backing.
    let slave_weak = peer_link_of(master);

    // With no slave open, dropping the last master backing frees the pair.
    drop(master_backing);

    if master_weak.upgrade().is_some() {
        klog_info!("TTY_TEST: BUG - master backing outlived its last close");
        return TestResult::Fail;
    }
    if slave_weak.upgrade().is_some() {
        klog_info!("TTY_TEST: BUG - slave backing outlived pair teardown");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A peer link captured before teardown stops upgrading once the pair frees.
pub fn test_stale_peer_link_detected() -> TestResult {
    tty::table::tty_table_init();
    let (master, master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);

    // Capture the slave's peer link (→ master) while the pair is live.
    let peer = peer_link_of(slave);
    if peer.upgrade().is_none() {
        klog_info!("TTY_TEST: BUG - peer link should upgrade while the pair is live");
        return TestResult::Fail;
    }

    // Free the pair.
    drop(master_backing);

    if peer.upgrade().is_some() {
        klog_info!("TTY_TEST: BUG - peer link should be stale after teardown");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// pty_alloc wires the master's peer link to the slave that get_pty_number
/// reports.
pub fn test_pty_alloc_links_master_to_slave() -> TestResult {
    tty::table::tty_table_init();
    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let reported_slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    let peer = peer_link_of(master);
    let linked_slave = peer.upgrade().map(|b| b.index());
    if linked_slave != Some(reported_slave) {
        klog_info!("TTY_TEST: BUG - master peer link disagrees with get_pty_number");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A master write through a peer link captured before free is a safe no-op.
pub fn test_stale_write_safe_noop() -> TestResult {
    tty::table::tty_table_init();

    // Pair A.
    let (master_a, master_a_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - first pty_alloc failed");
            return TestResult::Fail;
        }
    };
    // Capture master A's peer link (→ slave A) before tearing it down.
    let stale_peer = peer_link_of(master_a);

    // Free pair A.
    drop(master_a_backing);

    // Pair B may reuse the same slots.
    let (master_b, _master_b_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - second pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_b = TtyIndex(tty::get_pty_number(master_b).unwrap() as u8);

    // The stale link no longer upgrades: the write lands nowhere.
    let written = crate::tty::pty::master_write(&stale_peer, b"stale data");
    drain_tty_nonblock(slave_b);

    if written != 0 {
        klog_info!(
            "TTY_TEST: BUG - stale master_write accepted {} bytes, want 0",
            written
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Rapid alloc/free cycles: each freed backing stops upgrading.
pub fn test_rapid_alloc_free_backing_dies() -> TestResult {
    tty::table::tty_table_init();
    for _ in 0..10 {
        let (_master, master_backing) = match tty::pty_alloc() {
            Ok(pair) => pair,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed during stress");
                return TestResult::Fail;
            }
        };
        let master_weak = KArc::downgrade(&master_backing);
        drop(master_backing);
        if master_weak.upgrade().is_some() {
            klog_info!("TTY_TEST: BUG - freed backing still upgrades in stress loop");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Data flows master→slave through the live peer link.
pub fn test_data_flow_through_peer_link() -> TestResult {
    tty::table::tty_table_init();
    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Master write -> slave read (through slave's N_TTY ldisc).
    let _ = tty::write(master, b"gen\n", false);
    let mut buf = [0u8; 16];
    match tty::read(slave, &mut buf, true) {
        Ok(n) if n == 4 && &buf[..4] == b"gen\n" => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - master->slave data flow failed: {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// A dangling (never-linked) peer accepts no writes.
pub fn test_dangling_peer_write_is_noop() -> TestResult {
    let dangling: KWeak<TtyBacking> = KWeak::new();
    let written = crate::tty::pty::master_write(&dangling, b"x");
    if written != 0 {
        klog_info!(
            "TTY_TEST: BUG - write through a dangling peer accepted {} bytes",
            written
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multiple PTY pairs can be allocated with 32 slots available.
pub fn test_multiple_pty_pairs() -> TestResult {
    tty::table::tty_table_init();
    // With 32 slots and 2 reserved (serial + vconsole), several pairs fit at
    // once. Hold every master backing so the pairs stay live simultaneously.
    let mut backings: [Option<KArc<TtyBacking>>; 10] = [const { None }; 10];
    for (i, slot) in backings.iter_mut().enumerate() {
        match tty::pty_alloc() {
            Ok((_master, backing)) => *slot = Some(backing),
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed at pair {}", i);
                return TestResult::Fail;
            }
        }
    }
    // Dropping every held backing frees all pairs.
    TestResult::Pass
}
// ===========================================================================
// PTY Namespace & Device Nodes
// ===========================================================================

/// TIOCSPTLCK and TIOCGPTLCK ioctl constants match Linux values.
pub fn test_pty_lock_ioctl_constants() -> TestResult {
    use slopos_abi::syscall::{TIOCGPTLCK, TIOCSPTLCK};
    if TIOCSPTLCK != 0x4004_5431 {
        klog_info!(
            "TTY_TEST: BUG - TIOCSPTLCK is {:#x}, expected 0x40045431",
            TIOCSPTLCK
        );
        return TestResult::Fail;
    }
    if TIOCGPTLCK != 0x8004_5439 {
        klog_info!(
            "TTY_TEST: BUG - TIOCGPTLCK is {:#x}, expected 0x80045439",
            TIOCGPTLCK
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// New PTY slaves are locked by default.
pub fn test_slave_locked_by_default() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master) {
        Ok(n) => n,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave = TtyIndex(slave_num as u8);

    // Slave should be locked by default.
    if !crate::tty::pty::is_slave_locked(slave) {
        klog_info!("TTY_TEST: BUG - new PTY slave should be locked by default");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Locked slave cannot be opened via pty_open_slave.
pub fn test_locked_slave_open_rejected() -> TestResult {
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

    // Slave is locked (default) — open should fail.
    match tty::pty_open_slave(slave) {
        Err(TtyError::PermissionDenied) => {} // expected
        Ok(backing) => {
            klog_info!("TTY_TEST: BUG - locked slave open unexpectedly succeeded");
            drop(backing);
            return TestResult::Fail;
        }
        Err(other) => {
            klog_info!(
                "TTY_TEST: BUG - locked slave open should return PermissionDenied, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// set_pty_lock unlocks the slave, enabling open.
pub fn test_unlock_enables_open() -> TestResult {
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

    // Unlock the slave.
    if let Err(e) = tty::set_pty_lock(master, false) {
        klog_info!("TTY_TEST: BUG - set_pty_lock(false) failed: {:?}", e);
        return TestResult::Fail;
    }

    // Now open should succeed.
    match tty::pty_open_slave(slave) {
        Ok(backing) => drop(backing),
        Err(e) => {
            klog_info!("TTY_TEST: BUG - unlocked slave open failed: {:?}", e);
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// get_pty_lock reads back the lock state.
pub fn test_get_lock_round_trip() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };

    // Default: locked.
    match tty::get_pty_lock(master) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - get_pty_lock should return Ok(true), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // Unlock.
    tty::set_pty_lock(master, false).unwrap();
    match tty::get_pty_lock(master) {
        Ok(false) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - after unlock, get_pty_lock should return Ok(false), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // Re-lock.
    tty::set_pty_lock(master, true).unwrap();
    match tty::get_pty_lock(master) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - after re-lock, get_pty_lock should return Ok(true), got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// set_pty_lock on non-master returns NotAllocated.
pub fn test_set_lock_non_master_rejected() -> TestResult {
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

    // Calling set_pty_lock on the slave (not master) should fail.
    match tty::set_pty_lock(slave, false) {
        Err(TtyError::NotAllocated) => {} // expected — slave is not a PtyMaster
        other => {
            klog_info!(
                "TTY_TEST: BUG - set_pty_lock on slave should return NotAllocated, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Data flow through unlocked PTY device node FDs.
pub fn test_data_flow_after_unlock() -> TestResult {
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

    // Unlock and open slave.
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Set slave to raw mode for simple data flow.
    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    // Master write -> slave read.
    let _ = tty::write(master, b"test", false);
    let mut buf = [0u8; 16];
    match tty::read(slave, &mut buf, true) {
        Ok(n) if n == 4 && &buf[..4] == b"test" => {}
        other => {
            klog_info!("TTY_TEST: BUG - data flow after unlock failed: {:?}", other);
            tty::set_termios(slave, &saved).unwrap();
            return TestResult::Fail;
        }
    }

    tty::set_termios(slave, &saved).unwrap();
    TestResult::Pass
}

/// Master close -> slave hangup still works with lock semantics.
pub fn test_master_close_slave_hangup() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = match tty::pty_alloc() {
        Ok(pair) => pair,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);

    // Unlock and open slave.
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    // Closing the master hangs up the still-open slave.
    drop(master_backing);

    // Read from slave should indicate peer closed (EOF or HungUp).
    let mut buf = [0u8; 16];
    match tty::read(slave, &mut buf, true) {
        Ok(0) | Err(TtyError::HungUp) | Err(TtyError::WouldBlock) => {} // acceptable
        other => {
            klog_info!("TTY_TEST: BUG - slave read after master close: {:?}", other);
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// Multiple simultaneous PTY pairs via /dev/ptmx.
pub fn test_multiple_pairs_with_locks() -> TestResult {
    tty::table::tty_table_init();

    let mut pairs: [(TtyIndex, TtyIndex); 5] = [(TtyIndex(0), TtyIndex(0)); 5];
    // Hold every master backing so all five pairs stay live at once.
    let mut backings: [Option<KArc<TtyBacking>>; 5] = [const { None }; 5];
    for i in 0..5 {
        let (master, backing) = match tty::pty_alloc() {
            Ok(pair) => pair,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed at pair {}", i);
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed at pair {}", i);
                return TestResult::Fail;
            }
        };
        pairs[i] = (master, TtyIndex(slave_num as u8));
        backings[i] = Some(backing);

        // Each pair's slave should be independently locked.
        if !crate::tty::pty::is_slave_locked(pairs[i].1) {
            klog_info!("TTY_TEST: BUG - pair {} slave not locked", i);
            return TestResult::Fail;
        }
    }

    // Unlock only pair 2 — others should remain locked.
    tty::set_pty_lock(pairs[2].0, false).unwrap();

    if crate::tty::pty::is_slave_locked(pairs[2].1) {
        klog_info!("TTY_TEST: BUG - pair 2 should be unlocked");
        return TestResult::Fail;
    }
    // Others still locked.
    for i in [0, 1, 3, 4] {
        if !crate::tty::pty::is_slave_locked(pairs[i].1) {
            klog_info!("TTY_TEST: BUG - pair {} should still be locked", i);
            return TestResult::Fail;
        }
    }

    // Held backings drop here, freeing every pair.
    TestResult::Pass
}

/// is_slave_locked returns false for non-PTY TTYs.
pub fn test_non_pty_not_locked() -> TestResult {
    tty::table::tty_table_init();

    // TTY 0 (serial console) should not report as locked.
    if crate::tty::pty::is_slave_locked(TtyIndex(0)) {
        klog_info!("TTY_TEST: BUG - serial console should not be slave_locked");
        return TestResult::Fail;
    }
    // TTY 1 (vconsole) should not report as locked.
    if crate::tty::pty::is_slave_locked(TtyIndex(1)) {
        klog_info!("TTY_TEST: BUG - vconsole should not be slave_locked");
        return TestResult::Fail;
    }
    // Out-of-range index.
    if crate::tty::pty::is_slave_locked(TtyIndex(255)) {
        klog_info!("TTY_TEST: BUG - out-of-range index should not be slave_locked");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// get_pty_lock on non-master returns error.
pub fn test_get_lock_non_master_error() -> TestResult {
    tty::table::tty_table_init();

    // Serial console is not a PTY master.
    match tty::get_pty_lock(TtyIndex(0)) {
        Err(TtyError::NotAllocated) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - get_pty_lock on console should return NotAllocated, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }

    // Invalid index.
    match tty::get_pty_lock(TtyIndex(255)) {
        Err(TtyError::InvalidIndex) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - get_pty_lock on invalid index should return InvalidIndex, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_session_id_zero_is_none,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_session_id_round_trip, suite = tty_test_pty_core);
slopos_testing::stest!(name = test_pgrp_id_zero_is_none, suite = tty_test_pty_core);
slopos_testing::stest!(name = test_pgrp_id_round_trip, suite = tty_test_pty_core);
slopos_testing::stest!(name = test_session_option_fields, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_session_option_attach_detach,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_raw_disc_new_empty, suite = tty_test_pty_core);
slopos_testing::stest!(name = test_raw_disc_input_read, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_raw_disc_output_passthrough,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_raw_disc_flush, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_ldisc_kind_ntty_delegation,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_ldisc_kind_raw_delegation,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_driver_id_variants,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_master_driver_kind,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_pty_slave_driver_kind, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_pty_alloc_pair_both_initialized,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_close_master_first_frees_pair,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_close_slave_first_frees_pair,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_reallocation_after_free,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_open_slave_validates_type,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_open_slave_prevents_free,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_extra_slave_open_keeps_slave_alive,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_rapid_alloc_free_realloc,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_open_slave_after_free,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_max_ttys_is_32, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_master_peer_link_targets_slave,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_slave_peer_link_targets_master,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_backing_dies_on_free, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_stale_peer_link_detected,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_alloc_links_master_to_slave,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_stale_write_safe_noop, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_rapid_alloc_free_backing_dies,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_data_flow_through_peer_link,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_dangling_peer_write_is_noop,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_multiple_pty_pairs, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_pty_lock_ioctl_constants,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_slave_locked_by_default,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_locked_slave_open_rejected,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_unlock_enables_open, suite = tty_test_pty_core);
slopos_testing::stest!(name = test_get_lock_round_trip, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_set_lock_non_master_rejected,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_data_flow_after_unlock,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_master_close_slave_hangup,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_multiple_pairs_with_locks,
    suite = tty_test_pty_core
);
slopos_testing::stest!(name = test_non_pty_not_locked, suite = tty_test_pty_core);
slopos_testing::stest!(
    name = test_pty_master_empty_read_would_block_not_eof,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_pty_winsize_shared_across_pair,
    suite = tty_test_pty_core
);
slopos_testing::stest!(
    name = test_get_lock_non_master_error,
    suite = tty_test_pty_core
);
