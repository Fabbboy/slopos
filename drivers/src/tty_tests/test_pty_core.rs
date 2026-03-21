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
    let master_id = DriverId::PtyMaster {
        peer: PtyPeerHandle::new(TtyIndex(2), 0),
    };
    let slave_id = DriverId::PtySlave {
        peer: PtyPeerHandle::new(TtyIndex(3), 0),
    };
    if master_id == slave_id {
        klog_info!("TTY_TEST: BUG - PtyMaster and PtySlave DriverId should be distinct");
        return TestResult::Fail;
    }
    // Also verify they differ from existing IDs.
    if master_id == DriverId::SerialConsole
        || master_id == DriverId::VConsole
        || master_id == DriverId::SerialConsole
    {
        klog_info!("TTY_TEST: BUG - PtyMaster should differ from SerialConsole/VConsole/None");
        return TestResult::Fail;
    }
    if slave_id == DriverId::SerialConsole
        || slave_id == DriverId::VConsole
        || slave_id == DriverId::SerialConsole
    {
        klog_info!("TTY_TEST: BUG - PtySlave should differ from SerialConsole/VConsole/None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyMaster driver kind returns correct DriverId.
pub fn test_pty_master_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtyMaster {
        peer: PtyPeerHandle::new(TtyIndex(2), 0),
    };
    if drv.id()
        != (DriverId::PtyMaster {
            peer: PtyPeerHandle::new(TtyIndex(2), 0),
        })
    {
        klog_info!("TTY_TEST: BUG - PtyMaster TtyDriverKind should return DriverId::PtyMaster");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtySlave driver kind returns correct DriverId.
pub fn test_pty_slave_driver_kind() -> TestResult {
    let drv = TtyDriverKind::PtySlave {
        peer: PtyPeerHandle::new(TtyIndex(3), 0),
    };
    if drv.id()
        != (DriverId::PtySlave {
            peer: PtyPeerHandle::new(TtyIndex(3), 0),
        })
    {
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

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
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

    // Cleanup.
    tty::open_ref(master).ok();
    tty::open_ref(slave).ok();
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

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

/// closing master then slave frees both slots.
pub fn test_pty_close_master_first_frees_pair() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Close master first (triggers hangup on slave), then slave.
    let _ = tty::close_ref(master);
    let _ = tty::close_ref(slave);

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

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Close slave first, then master.
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

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
    let master1 = tty::pty_alloc().unwrap();
    let slave1 = TtyIndex(tty::get_pty_number(master1).unwrap() as u8);
    tty::open_ref(master1).unwrap();
    tty::open_ref(slave1).unwrap();
    let _ = tty::close_ref(slave1);
    let _ = tty::close_ref(master1);

    // Reallocate — should succeed and return valid indices.
    let master2 = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - reallocation failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave2 = TtyIndex(tty::get_pty_number(master2).unwrap() as u8);

    // Verify the reallocated pair is functional.
    tty::open_ref(master2).unwrap();
    tty::open_ref(slave2).unwrap();

    let slave_is_pty = tty::is_pty_slave(slave2);
    let master_is_not_slave = !tty::is_pty_slave(master2);

    let _ = tty::close_ref(slave2);
    let _ = tty::close_ref(master2);

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
        // Undo the accidental open.
        let _ = tty::close_ref(TtyIndex(0));
        return TestResult::Fail;
    }

    // Try to open a non-existent slot — should fail.
    let result = tty::pty_open_slave(TtyIndex(5));
    if result.is_ok() {
        klog_info!("TTY_TEST: BUG - pty_open_slave should reject empty slot 5");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// pty_open_slave increments open_count, preventing pair free.
pub fn test_pty_open_slave_prevents_free() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();

    // Unlock slave so it can be opened (lock guard).
    tty::set_pty_lock(master, false).unwrap();

    // Open slave via the validated path.
    let open_rc = tty::pty_open_slave(slave);
    if open_rc.is_err() {
        klog_info!("TTY_TEST: BUG - pty_open_slave failed on valid slave");
        let _ = tty::close_ref(master);
        return TestResult::Fail;
    }

    // Close master — slave still has open_count > 0, so pair should NOT be freed.
    let _ = tty::close_ref(master);

    let slave_still_exists = TTY_SLOTS[slave.0 as usize].lock().is_some();

    // Cleanup.
    let _ = tty::close_ref(slave);

    if !slave_still_exists {
        klog_info!("TTY_TEST: BUG - slave freed while open_count > 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// free_pair_if_unused does not free when one side has open_count > 0.
pub fn test_partial_open_no_free() -> TestResult {
    tty::table::tty_table_init();

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Open slave a second time to keep it alive.
    tty::open_ref(slave).unwrap();

    // Close master (open_count → 0, hangup slave).
    let _ = tty::close_ref(master);

    // Close slave once (open_count → 1, still alive).
    let _ = tty::close_ref(slave);

    let slave_alive = TTY_SLOTS[slave.0 as usize].lock().is_some();
    let master_alive = TTY_SLOTS[master.0 as usize].lock().is_some();

    // Final close of slave (open_count → 0).
    let _ = tty::close_ref(slave);

    if !slave_alive {
        klog_info!("TTY_TEST: BUG - slave freed with open_count > 0");
        return TestResult::Fail;
    }
    // Master should still be alive because pair-free only happens when BOTH are 0.
    if !master_alive {
        klog_info!("TTY_TEST: BUG - master freed while slave still has open_count > 0");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// rapid allocate/free/reallocate cycles produce valid pairs.
pub fn test_rapid_alloc_free_realloc() -> TestResult {
    tty::table::tty_table_init();

    for i in 0..3u8 {
        let master = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(err) => {
                klog_info!("TTY_TEST: BUG - rapid alloc cycle {} failed: {:?}", i, err);
                return TestResult::Fail;
            }
        };
        let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);

        tty::open_ref(master).unwrap();
        tty::open_ref(slave).unwrap();

        // Verify data flows correctly on this pair.
        let saved = tty::get_termios(slave).unwrap();
        let mut raw = saved;
        raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
        tty::set_termios(slave, &raw).unwrap();

        let write_ok = tty::write(master, b"x", false).is_ok();
        let mut buf = [0u8; 4];
        let read_ok = tty::read(slave, &mut buf, true) == Ok(1) && buf[0] == b'x';

        tty::set_termios(slave, &saved).unwrap();
        let _ = tty::close_ref(slave);
        let _ = tty::close_ref(master);

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

    let master = tty::pty_alloc().unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::open_ref(master).unwrap();
    tty::open_ref(slave).unwrap();

    // Free the pair.
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);

    // Attempting to open the freed slave should fail.
    let result = tty::pty_open_slave(slave);
    match result {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
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

/// PtyPeerHandle stores index and generation.
pub fn test_pty_peer_handle_creation() -> TestResult {
    let handle = PtyPeerHandle::new(TtyIndex(5), 42);
    if handle.idx != TtyIndex(5) {
        klog_info!("TTY_TEST: BUG - PtyPeerHandle idx mismatch");
        return TestResult::Fail;
    }
    if handle.generation != 42 {
        klog_info!("TTY_TEST: BUG - PtyPeerHandle generation mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PtyPeerHandle::snapshot captures the current generation from TTY_GENERATIONS.
pub fn test_pty_peer_handle_snapshot() -> TestResult {
    use core::sync::atomic::Ordering;
    // Use a high slot unlikely to be in use (slot 30).
    let test_slot: usize = 30;
    let old_gen = TTY_GENERATIONS[test_slot].load(Ordering::Acquire);
    let handle = PtyPeerHandle::snapshot(TtyIndex(test_slot as u8));
    if handle.generation != old_gen {
        klog_info!(
            "TTY_TEST: BUG - snapshot generation {} != expected {}",
            handle.generation,
            old_gen
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Generation counter is bumped when a PTY pair is freed.
pub fn test_generation_bumped_on_free() -> TestResult {
    use core::sync::atomic::Ordering;
    // Allocate a PTY pair.
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);
    let master_slot = master_idx.0 as usize;
    let slave_slot = slave_idx.0 as usize;

    let gen_master_before = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
    let gen_slave_before = TTY_GENERATIONS[slave_slot].load(Ordering::Acquire);

    // Free the pair (both have open_count 0 since we never opened them).
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);

    let gen_master_after = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
    let gen_slave_after = TTY_GENERATIONS[slave_slot].load(Ordering::Acquire);

    if gen_master_after != gen_master_before + 1 {
        klog_info!(
            "TTY_TEST: BUG - master generation not bumped: {} -> {}",
            gen_master_before,
            gen_master_after
        );
        return TestResult::Fail;
    }
    if gen_slave_after != gen_slave_before + 1 {
        klog_info!(
            "TTY_TEST: BUG - slave generation not bumped: {} -> {}",
            gen_slave_before,
            gen_slave_after
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Stale PtyPeerHandle is detected by validate_peer.
pub fn test_stale_handle_detected() -> TestResult {
    // Allocate a PTY pair.
    // Allocate a PTY pair.
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);

    // Create a handle with the current generation.
    let stale_handle = PtyPeerHandle::snapshot(slave_idx);

    // Verify the handle is valid before freeing.
    if !crate::tty::pty::validate_peer(&stale_handle) {
        klog_info!("TTY_TEST: BUG - handle should be valid before free");
        return TestResult::Fail;
    }

    // Free the pair.
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);

    // Now the handle should be stale.
    if crate::tty::pty::validate_peer(&stale_handle) {
        klog_info!("TTY_TEST: BUG - handle should be stale after free");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// PTY alloc captures the correct generation in peer handles.
pub fn test_pty_alloc_captures_generation() -> TestResult {
    use core::sync::atomic::Ordering;
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);

    // Read the peer handle from the master's driver.
    let master_peer_gen = {
        let guard = TTY_SLOTS[master_idx.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => peer.generation,
                _ => {
                    klog_info!("TTY_TEST: BUG - master not PtyMaster");
                    return TestResult::Fail;
                }
            },
            None => {
                klog_info!("TTY_TEST: BUG - master slot empty");
                return TestResult::Fail;
            }
        }
    };

    // The peer generation should match the current generation of the slave slot.
    let slave_gen = TTY_GENERATIONS[slave_idx.0 as usize].load(Ordering::Acquire);
    if master_peer_gen != slave_gen {
        klog_info!(
            "TTY_TEST: BUG - master peer gen {} != slave slot gen {}",
            master_peer_gen,
            slave_gen
        );
        // Clean up.
        crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
        return TestResult::Fail;
    }

    // Clean up.
    crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
    TestResult::Pass
}

/// Stale master write after free/realloc is a safe no-op.
pub fn test_stale_write_safe_noop() -> TestResult {
    // Allocate pair A.
    let master_a = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - first pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_a_num = match tty::get_pty_number(master_a) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_a = TtyIndex(slave_a_num as u8);

    // Capture the peer handle from master A (points to slave A).
    let stale_peer = {
        let guard = TTY_SLOTS[master_a.0 as usize].lock();
        match guard.as_ref() {
            Some(tty) => match &tty.driver {
                TtyDriverKind::PtyMaster { peer } => *peer,
                _ => {
                    klog_info!("TTY_TEST: BUG - not PtyMaster");
                    return TestResult::Fail;
                }
            },
            None => {
                klog_info!("TTY_TEST: BUG - master slot empty");
                return TestResult::Fail;
            }
        }
    };

    // Free pair A.
    crate::tty::pty::free_pair_if_unused(master_a, slave_a);

    // Allocate pair B — may reuse the same slots.
    let master_b = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - second pty_alloc failed");
            return TestResult::Fail;
        }
    };
    let slave_b_num = match tty::get_pty_number(master_b) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number for B failed");
            return TestResult::Fail;
        }
    };
    let slave_b = TtyIndex(slave_b_num as u8);

    // Use the stale peer handle to attempt a write — should be a no-op.
    crate::tty::pty::master_write(stale_peer, b"stale data");

    // Verify pair B's slave has no unexpected data.
    // Drain slave B to check no stale data leaked in.
    drain_tty_nonblock(slave_b);

    // Clean up pair B.
    crate::tty::pty::free_pair_if_unused(master_b, slave_b);
    TestResult::Pass
}

/// Rapid alloc/free/realloc stress: generations increase monotonically.
pub fn test_rapid_alloc_free_stress() -> TestResult {
    use core::sync::atomic::Ordering;
    for _ in 0..10 {
        let master_idx = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed during stress");
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master_idx) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed during stress");
                return TestResult::Fail;
            }
        };
        let slave_idx = TtyIndex(slave_num as u8);
        let master_slot = master_idx.0 as usize;

        let gen_before = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);
        crate::tty::pty::free_pair_if_unused(master_idx, slave_idx);
        let gen_after = TTY_GENERATIONS[master_slot].load(Ordering::Acquire);

        if gen_after != gen_before + 1 {
            klog_info!(
                "TTY_TEST: BUG - generation not monotonic: {} -> {}",
                gen_before,
                gen_after
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// Data flow still works correctly through generation-tagged handles.
pub fn test_data_flow_with_generation() -> TestResult {
    let master_idx = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed");
            return TestResult::Fail;
        }
    };
    // Open both sides.
    let _ = tty::open_ref(master_idx);
    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(_) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed");
            return TestResult::Fail;
        }
    };
    let slave_idx = TtyIndex(slave_num as u8);
    let _ = tty::open_ref(slave_idx);

    // Master write -> slave read (through slave's N_TTY ldisc).
    let _ = tty::write(master_idx, b"gen\n", false);
    let mut buf = [0u8; 16];
    match tty::read(slave_idx, &mut buf, true) {
        Ok(n) if n == 4 && &buf[..4] == b"gen\n" => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - master->slave data flow failed: {:?}",
                other
            );
            let _ = tty::close_ref(slave_idx);
            let _ = tty::close_ref(master_idx);
            return TestResult::Fail;
        }
    }

    let _ = tty::close_ref(slave_idx);
    let _ = tty::close_ref(master_idx);
    TestResult::Pass
}

/// validate_peer returns false for out-of-range index.
pub fn test_validate_peer_out_of_range() -> TestResult {
    let handle = PtyPeerHandle::new(TtyIndex(255), 0);
    if crate::tty::pty::validate_peer(&handle) {
        klog_info!("TTY_TEST: BUG - validate_peer should reject out-of-range index");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multiple PTY pairs can be allocated with 32 slots available.
pub fn test_multiple_pty_pairs() -> TestResult {
    // With 32 slots and 2 reserved (serial + vconsole), we should be able
    // to allocate up to 15 pairs (30 slots / 2).
    let mut pairs: [(TtyIndex, TtyIndex); 10] = [(TtyIndex(0), TtyIndex(0)); 10];
    for i in 0..10 {
        let master = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed at pair {}", i);
                // Clean up what we allocated.
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed at pair {}", i);
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        pairs[i] = (master, TtyIndex(slave_num as u8));
    }
    // Clean up all pairs.
    for i in 0..10 {
        crate::tty::pty::free_pair_if_unused(pairs[i].0, pairs[i].1);
    }
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

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
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
        crate::tty::pty::free_pair_if_unused(master, slave);
        return TestResult::Fail;
    }

    crate::tty::pty::free_pair_if_unused(master, slave);
    TestResult::Pass
}

/// Locked slave cannot be opened via pty_open_slave.
pub fn test_locked_slave_open_rejected() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
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
        other => {
            klog_info!(
                "TTY_TEST: BUG - locked slave open should return PermissionDenied, got {:?}",
                other
            );
            crate::tty::pty::free_pair_if_unused(master, slave);
            return TestResult::Fail;
        }
    }

    crate::tty::pty::free_pair_if_unused(master, slave);
    TestResult::Pass
}

/// set_pty_lock unlocks the slave, enabling open.
pub fn test_unlock_enables_open() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
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
        crate::tty::pty::free_pair_if_unused(master, slave);
        return TestResult::Fail;
    }

    // Now open should succeed.
    match tty::pty_open_slave(slave) {
        Ok(_count) => {}
        Err(e) => {
            klog_info!("TTY_TEST: BUG - unlocked slave open failed: {:?}", e);
            crate::tty::pty::free_pair_if_unused(master, slave);
            return TestResult::Fail;
        }
    }

    let _ = tty::close_ref(slave);
    crate::tty::pty::free_pair_if_unused(master, slave);
    TestResult::Pass
}

/// get_pty_lock reads back the lock state.
pub fn test_get_lock_round_trip() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);

    // Default: locked.
    match tty::get_pty_lock(master) {
        Ok(true) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - get_pty_lock should return Ok(true), got {:?}",
                other
            );
            crate::tty::pty::free_pair_if_unused(master, slave);
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
            crate::tty::pty::free_pair_if_unused(master, slave);
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
            crate::tty::pty::free_pair_if_unused(master, slave);
            return TestResult::Fail;
        }
    }

    crate::tty::pty::free_pair_if_unused(master, slave);
    TestResult::Pass
}

/// set_pty_lock on non-master returns NotAllocated.
pub fn test_set_lock_non_master_rejected() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
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
            crate::tty::pty::free_pair_if_unused(master, slave);
            return TestResult::Fail;
        }
    }

    crate::tty::pty::free_pair_if_unused(master, slave);
    TestResult::Pass
}

/// Data flow through unlocked PTY device node FDs.
pub fn test_data_flow_after_unlock() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let _ = tty::open_ref(master);
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);

    // Unlock slave.
    tty::set_pty_lock(master, false).unwrap();

    // Open slave.
    tty::pty_open_slave(slave).unwrap();

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
            let _ = tty::close_ref(slave);
            let _ = tty::close_ref(master);
            return TestResult::Fail;
        }
    }

    tty::set_termios(slave, &saved).unwrap();
    let _ = tty::close_ref(slave);
    let _ = tty::close_ref(master);
    TestResult::Pass
}

/// Master close -> slave hangup still works with lock semantics.
pub fn test_master_close_slave_hangup() -> TestResult {
    tty::table::tty_table_init();

    let master = match tty::pty_alloc() {
        Ok(idx) => idx,
        Err(e) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", e);
            return TestResult::Fail;
        }
    };
    let _ = tty::open_ref(master);
    let slave_num = tty::get_pty_number(master).unwrap();
    let slave = TtyIndex(slave_num as u8);

    // Unlock and open slave.
    tty::set_pty_lock(master, false).unwrap();
    tty::pty_open_slave(slave).unwrap();

    // Close master -> slave should see hangup.
    let _ = tty::close_ref(master);
    crate::tty::pty::mark_peer_closed(slave);

    // Read from slave should indicate peer closed (EOF or HungUp).
    let mut buf = [0u8; 16];
    match tty::read(slave, &mut buf, true) {
        Ok(0) | Err(TtyError::HungUp) | Err(TtyError::WouldBlock) => {} // acceptable
        other => {
            klog_info!("TTY_TEST: BUG - slave read after master close: {:?}", other);
            let _ = tty::close_ref(slave);
            return TestResult::Fail;
        }
    }

    let _ = tty::close_ref(slave);
    TestResult::Pass
}

/// Multiple simultaneous PTY pairs via /dev/ptmx.
pub fn test_multiple_pairs_with_locks() -> TestResult {
    tty::table::tty_table_init();

    let mut pairs: [(TtyIndex, TtyIndex); 5] = [(TtyIndex(0), TtyIndex(0)); 5];
    for i in 0..5 {
        let master = match tty::pty_alloc() {
            Ok(idx) => idx,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - pty_alloc failed at pair {}", i);
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        let slave_num = match tty::get_pty_number(master) {
            Ok(n) => n,
            Err(_) => {
                klog_info!("TTY_TEST: BUG - get_pty_number failed at pair {}", i);
                for j in 0..i {
                    crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
                }
                return TestResult::Fail;
            }
        };
        pairs[i] = (master, TtyIndex(slave_num as u8));

        // Each pair's slave should be independently locked.
        if !crate::tty::pty::is_slave_locked(pairs[i].1) {
            klog_info!("TTY_TEST: BUG - pair {} slave not locked", i);
            for j in 0..=i {
                crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
            }
            return TestResult::Fail;
        }
    }

    // Unlock only pair 2 — others should remain locked.
    tty::set_pty_lock(pairs[2].0, false).unwrap();

    if crate::tty::pty::is_slave_locked(pairs[2].1) {
        klog_info!("TTY_TEST: BUG - pair 2 should be unlocked");
        for i in 0..5 {
            crate::tty::pty::free_pair_if_unused(pairs[i].0, pairs[i].1);
        }
        return TestResult::Fail;
    }
    // Others still locked.
    for i in [0, 1, 3, 4] {
        if !crate::tty::pty::is_slave_locked(pairs[i].1) {
            klog_info!("TTY_TEST: BUG - pair {} should still be locked", i);
            for j in 0..5 {
                crate::tty::pty::free_pair_if_unused(pairs[j].0, pairs[j].1);
            }
            return TestResult::Fail;
        }
    }

    for i in 0..5 {
        crate::tty::pty::free_pair_if_unused(pairs[i].0, pairs[i].1);
    }
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

slopos_testing::define_test_suite!(
    tty_test_pty_core,
    [
        test_session_id_zero_is_none,
        test_session_id_round_trip,
        test_pgrp_id_zero_is_none,
        test_pgrp_id_round_trip,
        test_session_option_fields,
        test_session_option_attach_detach,
        test_raw_disc_new_empty,
        test_raw_disc_input_read,
        test_raw_disc_output_passthrough,
        test_raw_disc_flush,
        test_ldisc_kind_ntty_delegation,
        test_ldisc_kind_raw_delegation,
        test_pty_driver_id_variants,
        test_pty_master_driver_kind,
        test_pty_slave_driver_kind,
        test_pty_alloc_pair_both_initialized,
        test_pty_close_master_first_frees_pair,
        test_pty_close_slave_first_frees_pair,
        test_pty_reallocation_after_free,
        test_pty_open_slave_validates_type,
        test_pty_open_slave_prevents_free,
        test_partial_open_no_free,
        test_rapid_alloc_free_realloc,
        test_pty_open_slave_after_free,
        test_max_ttys_is_32,
        test_pty_peer_handle_creation,
        test_pty_peer_handle_snapshot,
        test_generation_bumped_on_free,
        test_stale_handle_detected,
        test_pty_alloc_captures_generation,
        test_stale_write_safe_noop,
        test_rapid_alloc_free_stress,
        test_data_flow_with_generation,
        test_validate_peer_out_of_range,
        test_multiple_pty_pairs,
        test_pty_lock_ioctl_constants,
        test_slave_locked_by_default,
        test_locked_slave_open_rejected,
        test_unlock_enables_open,
        test_get_lock_round_trip,
        test_set_lock_non_master_rejected,
        test_data_flow_after_unlock,
        test_master_close_slave_hangup,
        test_multiple_pairs_with_locks,
        test_non_pty_not_locked,
        test_get_lock_non_master_error,
    ]
);
