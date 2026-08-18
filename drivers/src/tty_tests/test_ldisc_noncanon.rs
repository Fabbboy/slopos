//! Line-discipline tests for non-canonical VMIN/VTIME reads, the VEOL/VEOL2
//! line terminators, and the expanded edit buffer.

use super::fixtures::*;

/// VMIN>0/VTIME>0 — returns immediately when VMIN bytes are
/// already available (no timeout needed).
pub fn test_vmin_vtime_enough_data_returns_immediately() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');
    tty::push_input(TtyIndex(0), b'b');
    tty::push_input(TtyIndex(0), b'c');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(n) if n >= 3 => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 with 3 bytes expected Ok(>=3), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// VMIN>0/VTIME>0 — with partial data available (less than VMIN),
/// a nonblocking read returns what is available (WouldBlock if nothing).
pub fn test_vmin_vtime_partial_nonblock() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 5; // VMIN = 5
    raw.c_cc[5] = 2; // VTIME = 2 (200ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'y');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(2) => {
            if buf[0] == b'x' && buf[1] == b'y' {
                TestResult::Pass
            } else {
                klog_info!(
                    "TTY_TEST: BUG - VMIN=5/VTIME=2 nonblock data mismatch ({}, {})",
                    buf[0],
                    buf[1]
                );
                TestResult::Fail
            }
        }
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=5/VTIME=2 nonblock with 2 bytes expected Ok(2), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// VMIN>0/VTIME>0 — with no data, nonblocking read returns
/// WouldBlock (timer does NOT start without first byte).
pub fn test_vmin_vtime_no_data_nonblock() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Err(TtyError::WouldBlock) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 no data nonblock expected WouldBlock, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// VMIN>0/VTIME>0 — the inter-byte timeout returns partial data rather than
/// blocking indefinitely for VMIN.
pub fn test_vmin_vtime_interbyte_timeout_returns_partial() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 3; // VMIN = 3
    raw.c_cc[5] = 1; // VTIME = 1 (100ms inter-byte timeout)
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // One byte: below VMIN, but enough to start the inter-byte timer.
    tty::push_input(TtyIndex(0), b'z');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(n) if n >= 1 => {
            if buf[0] != b'z' {
                klog_info!(
                    "TTY_TEST: BUG - inter-byte timeout data mismatch (got 0x{:02x})",
                    buf[0]
                );
                TestResult::Fail
            } else {
                TestResult::Pass
            }
        }
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=3/VTIME=1 with 1 byte expected Ok(>=1) after timeout, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_ldisc_vmin_vtime_helper() -> TestResult {
    let mut ld = LineDisc::new();
    let (vmin, vtime) = ld.vmin_vtime();
    if vmin != 1 || vtime != 0 {
        klog_info!(
            "TTY_TEST: BUG - default vmin_vtime expected (1,0), got ({},{})",
            vmin,
            vtime
        );
        return TestResult::Fail;
    }

    let mut t = *ld.termios();
    t.c_cc[6] = 5; // VMIN
    t.c_cc[5] = 3; // VTIME
    ld.set_termios(&t);
    let (vmin2, vtime2) = ld.vmin_vtime();
    if vmin2 != 5 || vtime2 != 3 {
        klog_info!(
            "TTY_TEST: BUG - custom vmin_vtime expected (5,3), got ({},{})",
            vmin2,
            vtime2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL character completes a canonical line.
pub fn test_veol_completes_line() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, b';');
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(b'c');
    let action = ld.input_char(b';');

    let echoed = matches!(action, InputAction::Echo { buf, len } if buf[0] == b';' && len == 1);
    if !echoed {
        klog_info!("TTY_TEST: BUG - VEOL did not produce echo of ';'");
        return TestResult::Fail;
    }

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - VEOL did not complete canonical line");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 || &buf[..4] != b"abc;" {
        klog_info!("TTY_TEST: BUG - expected 'abc;' (4 bytes), got {} bytes", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL2 character completes a canonical line.
pub fn test_veol2_completes_line() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol2, b'|');
    ld.set_termios(&t);

    ld.input_char(b'x');
    ld.input_char(b'y');
    ld.input_char(b'|');

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - VEOL2 did not complete canonical line");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 || &buf[..3] != b"xy|" {
        klog_info!("TTY_TEST: BUG - expected 'xy|' (3 bytes), got {} bytes", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL disabled (value 0 / POSIX_VDISABLE) has no effect.
pub fn test_veol_disabled_no_effect() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, POSIX_VDISABLE);
    t.set_cc(CcIndex::Veol2, POSIX_VDISABLE);
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b'b');

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - disabled VEOL produced a complete line");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL and newline both work simultaneously as independent terminators.
pub fn test_veol_and_newline_coexist() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, b';');
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b';');

    ld.input_char(b'b');
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 2 || &buf[..2] != b"a;" {
        klog_info!(
            "TTY_TEST: BUG - first line expected 'a;' (2 bytes), got {} bytes",
            n1
        );
        return TestResult::Fail;
    }

    let n2 = ld.read(&mut buf);
    if n2 != 2 || &buf[..2] != b"b\n" {
        klog_info!(
            "TTY_TEST: BUG - second line expected 'b\\n' (2 bytes), got {} bytes",
            n2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL echo behavior: character is echoed normally when ECHO is set.
pub fn test_veol_echo_behavior() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, b'#');
    ld.set_termios(&t);

    let action = ld.input_char(b'#');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'#' {
                klog_info!(
                    "TTY_TEST: BUG - VEOL echo expected '#' (1 byte), got {:?} ({} bytes)",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - VEOL did not produce Echo action");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// VEOL with no ECHO set: no echo produced.
pub fn test_veol_no_echo() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON; // ECHO off
    t.set_cc(CcIndex::Veol, b'#');
    ld.set_termios(&t);

    ld.input_char(b'a');
    let action = ld.input_char(b'#');
    match action {
        InputAction::None => {}
        _ => {
            klog_info!("TTY_TEST: BUG - VEOL produced echo with ECHO disabled");
            return TestResult::Fail;
        }
    }

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - VEOL without ECHO did not complete line");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL2 CcIndex exists and maps to index 16.
pub fn test_veol2_cc_index() -> TestResult {
    if CcIndex::Veol2.as_usize() != 16 {
        klog_info!(
            "TTY_TEST: BUG - CcIndex::Veol2 expected 16, got {}",
            CcIndex::Veol2.as_usize()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Both VEOL and VEOL2 can be set simultaneously to different characters.
pub fn test_veol_veol2_both_active() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, b';');
    t.set_cc(CcIndex::Veol2, b'|');
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b';');

    ld.input_char(b'b');
    ld.input_char(b'|');

    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 2 || &buf[..2] != b"a;" {
        klog_info!("TTY_TEST: BUG - VEOL line expected 'a;', got {} bytes", n1);
        return TestResult::Fail;
    }

    let n2 = ld.read(&mut buf);
    if n2 != 2 || &buf[..2] != b"b|" {
        klog_info!("TTY_TEST: BUG - VEOL2 line expected 'b|', got {} bytes", n2);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VEOL does not interfere with VEOF behavior.
pub fn test_veol_and_eof_coexist() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.set_cc(CcIndex::Veol, b';');
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b';');

    ld.input_char(b'b');
    let veof = ld.termios().cc(CcIndex::Veof);
    ld.input_char(veof);

    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 2 || &buf[..2] != b"a;" {
        klog_info!("TTY_TEST: BUG - VEOL line expected 'a;', got {} bytes", n1);
        return TestResult::Fail;
    }

    // The EOF-flushed line carries no delimiter.
    let n2 = ld.read(&mut buf);
    if n2 != 1 || buf[0] != b'b' {
        klog_info!(
            "TTY_TEST: BUG - EOF line expected 'b' (1 byte), got {} bytes",
            n2
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_canonical_input_over_1024() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    for i in 0..2000u16 {
        tty::push_input(idx, b'a' + (i % 26) as u8);
    }
    tty::push_input(idx, b'\n');

    let mut buf: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let mut total = 0usize;
    loop {
        match tty::read(idx, &mut buf[total..], true) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }

    if total != 2001 {
        klog_info!("TTY_TEST: BUG - read {} bytes, expected 2001", total);
        drain_tty_nonblock(idx);
        return TestResult::Fail;
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

pub fn test_large_paste_canonical() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    let paste_len = 3000;
    for i in 0..paste_len {
        tty::push_input(idx, b'A' + (i % 26) as u8);
    }
    tty::push_input(idx, b'\n');

    let mut buf: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let mut total = 0usize;
    loop {
        match tty::read(idx, &mut buf[total..], true) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }

    if total != paste_len + 1 {
        klog_info!(
            "TTY_TEST: BUG - read {} bytes, expected {}",
            total,
            paste_len + 1
        );
        drain_tty_nonblock(idx);
        return TestResult::Fail;
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

pub fn test_backspace_in_expanded_buffer() -> TestResult {
    tty::table::tty_table_init();
    let idx = TtyIndex(0);
    drain_tty_nonblock(idx);

    tty::push_input(idx, b'a');
    tty::push_input(idx, b'b');
    tty::push_input(idx, b'c');
    tty::push_input(idx, 0x7f); // DEL/backspace
    tty::push_input(idx, b'd');
    tty::push_input(idx, b'\n');

    let mut buf = [0u8; 64];
    match tty::read(idx, &mut buf, true) {
        Ok(n) if n >= 3 && &buf[..3] == b"abd" => {}
        other => {
            klog_info!("TTY_TEST: BUG - backspace in expanded buffer: {:?}", other);
            drain_tty_nonblock(idx);
            return TestResult::Fail;
        }
    }

    drain_tty_nonblock(idx);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_vmin_vtime_enough_data_returns_immediately,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_vmin_vtime_partial_nonblock,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_vmin_vtime_no_data_nonblock,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_vmin_vtime_interbyte_timeout_returns_partial,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_ldisc_vmin_vtime_helper,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol_completes_line,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol2_completes_line,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol_disabled_no_effect,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol_and_newline_coexist,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol_echo_behavior,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(name = test_veol_no_echo, suite = tty_test_ldisc_noncanon);
slopos_testing::stest!(name = test_veol2_cc_index, suite = tty_test_ldisc_noncanon);
slopos_testing::stest!(
    name = test_veol_veol2_both_active,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_veol_and_eof_coexist,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_canonical_input_over_1024,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_large_paste_canonical,
    suite = tty_test_ldisc_noncanon
);
slopos_testing::stest!(
    name = test_backspace_in_expanded_buffer,
    suite = tty_test_ldisc_noncanon
);
