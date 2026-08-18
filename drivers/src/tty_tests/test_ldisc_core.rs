//! Tests for the line disciplines, the TTY table, and PTYs.

use super::fixtures::*;

pub fn test_ldisc_new_has_no_data() -> TestResult {
    let ld = LineDisc::new();
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - new LineDisc reports has_data()=true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_read_empty() -> TestResult {
    let mut ld = LineDisc::new();
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - read from empty LineDisc returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_canonical_newline() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has data before newline");
        return TestResult::Fail;
    }

    ld.input_char(b'\n');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has no data after newline");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - cooked data mismatch");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ldisc_canonical_backspace() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abcd" {
        ld.input_char(c);
    }
    ld.input_char(0x08);
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes (abc\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - expected \"abc\\n\", got {:?}", &buf[..n]);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_canonical_kill() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"hello" {
        ld.input_char(c);
    }
    ld.input_char(0x15);
    for &c in b"world" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 6 {
        klog_info!("TTY_TEST: BUG - expected 6 bytes (world\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..6] != b"world\n" {
        klog_info!("TTY_TEST: BUG - data mismatch after kill");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_canonical_eof() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x04);

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 {
        klog_info!("TTY_TEST: BUG - expected 3 bytes after EOF, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - data mismatch after EOF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_signal_ctrl_c() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x03);
    match action {
        InputAction::Signal(SIGINT) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!("TTY_TEST: BUG - expected SIGINT({}), got {}", SIGINT, s);
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+C did not produce Signal action");
            TestResult::Fail
        }
    }
}

pub fn test_ldisc_raw_mode() -> TestResult {
    let mut ld = LineDisc::new();

    let mut termios = *ld.termios();
    termios.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&termios);

    ld.input_char(b'a');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - raw mode char not immediately available");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 1];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'a' {
        klog_info!("TTY_TEST: BUG - raw mode read mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_set_termios_flush() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"partial" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical should not have data before newline");
        return TestResult::Fail;
    }

    let mut termios = *ld.termios();
    termios.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&termios);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - set_termios to raw did not flush edit buffer");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 7 || &buf[..7] != b"partial" {
        klog_info!("TTY_TEST: BUG - flushed data mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_flush_all() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"abc\n" {
        ld.input_char(c);
    }
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - expected data before flush_all");
        return TestResult::Fail;
    }
    ld.flush_all();
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - flush_all left cooked data");
        return TestResult::Fail;
    }
    let mut out = [0u8; 8];
    if ld.read(&mut out) != 0 {
        klog_info!("TTY_TEST: BUG - flush_all should empty read path");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_echo_printable() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'x');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'x' {
                klog_info!("TTY_TEST: BUG - echo mismatch for 'x'");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for printable char");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_ldisc_echo_newline() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'\n');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - echo mismatch for newline");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for newline");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_tty_index_eq() -> TestResult {
    let a = TtyIndex(0);
    let b = TtyIndex(0);
    let c = TtyIndex(1);
    if a != b {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) != TtyIndex(0)");
        return TestResult::Fail;
    }
    if a == c {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) == TtyIndex(1)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_multiple_reads() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abcdef" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf1 = [0u8; 3];
    let n1 = ld.read(&mut buf1);
    if n1 != 3 || &buf1 != b"abc" {
        klog_info!("TTY_TEST: BUG - first read mismatch");
        return TestResult::Fail;
    }

    let mut buf2 = [0u8; 10];
    let n2 = ld.read(&mut buf2);
    if n2 != 4 || &buf2[..4] != b"def\n" {
        klog_info!("TTY_TEST: BUG - second read mismatch (got {} bytes)", n2);
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - buffer not empty after full drain");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_backspace_empty() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x08);
    match action {
        InputAction::None => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - backspace on empty produced non-None action");
            TestResult::Fail
        }
    }
}

pub fn test_tty_write_returns_input_len() -> TestResult {
    tty::table::tty_table_init();
    let mut t = tty::get_termios(TtyIndex(0)).unwrap();
    let saved = t;
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello\n";
    let n = tty::write(TtyIndex(0), data, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();
    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_input_event_delivery() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let dummy_task: u32 = 9999;
    crate::input_event::input_set_keyboard_focus(dummy_task);

    crate::ps2::keyboard::handle_scancode(0x1E);

    let has_events = crate::input_event::input_has_events(dummy_task);

    crate::input_event::input_set_keyboard_focus(0);
    crate::input_event::input_cleanup_task(dummy_task);
    drain_tty_nonblock(TtyIndex(0));

    if !has_events {
        klog_info!("TTY_TEST: keyboard event NOT delivered to input_event queue");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_break_code_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    // Break code for 'a': make code 0x1E | 0x80.
    crate::ps2::keyboard::handle_scancode(0x9E);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if matches!(n, Ok(v) if v > 0) {
        klog_info!(
            "TTY_TEST: BUG - break code produced input (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_modifier_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    crate::ps2::keyboard::handle_scancode(0x2A); // shift press
    crate::ps2::keyboard::handle_scancode(0x1D); // ctrl press
    crate::ps2::keyboard::handle_scancode(0x38); // alt press

    crate::ps2::keyboard::handle_scancode(0xAA); // shift release
    crate::ps2::keyboard::handle_scancode(0x9D); // ctrl release
    crate::ps2::keyboard::handle_scancode(0xB8); // alt release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if matches!(n, Ok(v) if v > 0) {
        klog_info!(
            "TTY_TEST: BUG - modifier key produced input (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_press_release_single_char() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    crate::ps2::keyboard::handle_scancode(0x1E); // press
    crate::ps2::keyboard::handle_scancode(0x9E); // release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(1) || out[0] != b'a' {
        klog_info!(
            "TTY_TEST: BUG - press+release should yield 1 char 'a' (n={:?}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vconsole_drain_via_drain_hw_input() -> TestResult {
    tty::table::tty_table_init();

    drain_tty_nonblock(TtyIndex(1));

    let saved = tty::get_termios(TtyIndex(1)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    tty::set_termios(TtyIndex(1), &raw).unwrap();

    let has = tty::has_data(TtyIndex(1));
    tty::set_termios(TtyIndex(1), &saved).unwrap();

    if has {
        klog_info!("TTY_TEST: BUG - VConsole drain_hw_input_locked produced phantom data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_multi_key_sequence() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    crate::ps2::keyboard::handle_scancode(0x23); // 'h'
    crate::ps2::keyboard::handle_scancode(0x17); // 'i'

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(2) || out[0] != b'h' || out[1] != b'i' {
        klog_info!(
            "TTY_TEST: BUG - multi-key sequence mismatch (n={:?}, b0=0x{:02x}, b1=0x{:02x})",
            n,
            out[0],
            out[1]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_write_output_processing() -> TestResult {
    tty::table::tty_table_init();
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"hello\nworld\n";
    let n = tty::write(TtyIndex(0), data, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - write with OPOST+ONLCR returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_write_raw_passthrough() -> TestResult {
    tty::table::tty_table_init();
    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_oflag = OutputFlags::empty();
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"raw\ndata";
    let n = tty::write(TtyIndex(0), data, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - raw write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_write_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    let data = b"nothing";
    let n = tty::write(TtyIndex(7), data, false);
    if n != Err(TtyError::NotAllocated) {
        klog_info!(
            "TTY_TEST: BUG - write to invalid TTY returned {:?} instead of NotAllocated",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_per_tty_termios_isolation() -> TestResult {
    tty::table::tty_table_init();

    let t0_saved = tty::get_termios(TtyIndex(0)).unwrap();
    let t1_saved = tty::get_termios(TtyIndex(1)).unwrap();

    let mut t0_new = t0_saved;
    t0_new.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    tty::set_termios(TtyIndex(0), &t0_new).unwrap();

    let t1_check = tty::get_termios(TtyIndex(1)).unwrap();

    tty::set_termios(TtyIndex(0), &t0_saved).unwrap();

    if t1_check.c_oflag != t1_saved.c_oflag {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 c_oflag changed when TTY 0 was modified ({:?} vs {:?})",
            t1_check.c_oflag,
            t1_saved.c_oflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_per_tty_winsize_isolation() -> TestResult {
    tty::table::tty_table_init();

    let ws0_saved = tty::get_winsize(TtyIndex(0)).unwrap();
    let ws1_saved = tty::get_winsize(TtyIndex(1)).unwrap();

    let custom = slopos_abi::syscall::UserWinsize {
        ws_row: 42,
        ws_col: 120,
        ws_xpixel: 1920,
        ws_ypixel: 1080,
    };
    tty::set_winsize(TtyIndex(0), &custom).unwrap();

    let ws1_check = tty::get_winsize(TtyIndex(1)).unwrap();

    tty::set_winsize(TtyIndex(0), &ws0_saved).unwrap();

    if ws1_check.ws_row != ws1_saved.ws_row || ws1_check.ws_col != ws1_saved.ws_col {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 winsize changed when TTY 0 was modified ({}x{} vs {}x{})",
            ws1_check.ws_row,
            ws1_check.ws_col,
            ws1_saved.ws_row,
            ws1_saved.ws_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_per_tty_fg_pgrp_isolation() -> TestResult {
    tty::table::tty_table_init();

    let scope0 = SessionScope::new(100, 100);
    let scope1 = SessionScope::new(200, 200);
    tty::session::test_install_session(TtyIndex(0), scope0.session_weak(), scope0.pgrp_weak());
    tty::session::test_install_session(TtyIndex(1), scope1.session_weak(), scope1.pgrp_weak());

    let pgid0 = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let pgid1 = tty::get_foreground_pgrp(TtyIndex(1)).unwrap_or(0);

    tty::detach_session(TtyIndex(0));
    tty::detach_session(TtyIndex(1));

    if pgid0 != 100 {
        klog_info!("TTY_TEST: BUG - TTY 0 fg_pgrp should be 100, got {}", pgid0);
        return TestResult::Fail;
    }
    if pgid1 != 200 {
        klog_info!("TTY_TEST: BUG - TTY 1 fg_pgrp should be 200, got {}", pgid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_per_tty_has_data_isolation() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'\n');

    let has0 = tty::has_data(TtyIndex(0));
    let has1 = tty::has_data(TtyIndex(1));

    drain_tty_nonblock(TtyIndex(0));

    if !has0 {
        klog_info!("TTY_TEST: BUG - TTY 0 should have data after push_input");
        return TestResult::Fail;
    }
    if has1 {
        klog_info!("TTY_TEST: BUG - TTY 1 should NOT have data (isolation failure)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_per_tty_session_isolation() -> TestResult {
    tty::table::tty_table_init();

    let scope = SessionScope::new(500, 500);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    let sid0 = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let sid1 = tty::get_session_id(TtyIndex(1)).unwrap_or(0);

    tty::detach_session(TtyIndex(0));

    if sid0 != 500 {
        klog_info!(
            "TTY_TEST: BUG - TTY 0 session_id should be 500, got {}",
            sid0
        );
        return TestResult::Fail;
    }
    if sid1 != 0 {
        klog_info!("TTY_TEST: BUG - TTY 1 session_id should be 0, got {}", sid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_read_invalid_tty_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let mut buf = [0u8; 8];
    let n = tty::read(TtyIndex(7), &mut buf, true);
    if n != Err(TtyError::NotAllocated) {
        klog_info!(
            "TTY_TEST: BUG - read from invalid TTY returned {:?} instead of NotAllocated",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_index_abi_type() -> TestResult {
    let idx: slopos_abi::syscall::TtyIndex = slopos_abi::syscall::TtyIndex(3);
    let idx2: TtyIndex = TtyIndex(3);
    if idx != idx2 {
        klog_info!("TTY_TEST: BUG - ABI TtyIndex != drivers TtyIndex");
        return TestResult::Fail;
    }
    if idx.0 != 3 {
        klog_info!("TTY_TEST: BUG - TtyIndex inner value mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_signal_constants() -> TestResult {
    if SIGINT != 2 {
        klog_info!("TTY_TEST: BUG - SIGINT should be 2, got {}", SIGINT);
        return TestResult::Fail;
    }
    if SIGQUIT != 3 {
        klog_info!("TTY_TEST: BUG - SIGQUIT should be 3, got {}", SIGQUIT);
        return TestResult::Fail;
    }
    if SIGTSTP != 20 {
        klog_info!("TTY_TEST: BUG - SIGTSTP should be 20, got {}", SIGTSTP);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_set_compositor_focus_does_not_set_fg_pgrp() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(42, 42);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    let fg_before = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);

    let _ = tty::set_compositor_focus(99);
    let fg_after = tty::get_foreground_pgrp(TtyIndex(0)).unwrap_or(0);
    let _ = tty::set_compositor_focus(0);
    tty::detach_session(TtyIndex(0));

    if fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - set_compositor_focus changed fg_pgrp: {} -> {}",
            fg_before,
            fg_after
        );
        return TestResult::Fail;
    }
    if fg_before != 42 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should be 42, got {}", fg_before);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_check_read_sole_gate_background() -> TestResult {
    let scope = SessionScope::new(10, 10);
    let mut s = TtySession::new();
    scope.attach_to(&mut s);
    s.focused_task_id = 42;

    // Compositor focus is not POSIX foreground.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - compositor-focused but bg pgrp should be BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_backing_strong_count_is_open_count() -> TestResult {
    tty::table::tty_table_init();

    let open1 = tty::open_tty(TtyIndex(0)).expect("open console");
    let open2 = open1.clone();
    let count_two_opens = KArc::strong_count(&open1);
    drop(open2);
    let count_one_open = KArc::strong_count(&open1);
    drop(open1);
    let closed = crate::tty::table::TTY_BACKINGS[0]
        .lock()
        .upgrade()
        .is_none();

    if count_two_opens != 2 || count_one_open != 1 || !closed {
        klog_info!(
            "TTY_TEST: BUG - open count lifecycle mismatch: two={} one={} closed={}",
            count_two_opens,
            count_one_open,
            closed
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_sets_flag_and_detaches_session() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(500, 500);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'\n');

    let _hangup = HangupScope::hang_up(TtyIndex(0));
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let hung = tty::is_hung_up(TtyIndex(0));
    let has_data = tty::has_data(TtyIndex(0));

    let _ = tty::open_tty(TtyIndex(0));

    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - hangup should detach session, got sid={}",
            sid
        );
        return TestResult::Fail;
    }
    if !hung {
        klog_info!("TTY_TEST: BUG - hangup did not set hung_up flag");
        return TestResult::Fail;
    }
    if has_data {
        klog_info!("TTY_TEST: BUG - hangup should flush cooked data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_nonblock_read_eio() -> TestResult {
    tty::table::tty_table_init();
    let _con = tty::open_tty(TtyIndex(0));
    let _hangup = HangupScope::hang_up(TtyIndex(0));

    let mut out = [0u8; 8];
    let rc = tty::read(TtyIndex(0), &mut out, true);

    if rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - nonblock read on hung tty expected Ok(0), got {:?}",
            rc
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_hangup_blocking_read_eof() -> TestResult {
    tty::table::tty_table_init();
    let _con = tty::open_tty(TtyIndex(0));
    let _hangup = HangupScope::hang_up(TtyIndex(0));

    let mut out = [0u8; 8];
    let rc = tty::read(TtyIndex(0), &mut out, false);

    if rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - blocking read on hung tty expected EOF 0, got {:?}",
            rc
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_error_variants() -> TestResult {
    let e1 = TtyError::InvalidIndex;
    let e2 = TtyError::NotAllocated;
    let e3 = TtyError::WouldBlock;
    let e4 = TtyError::HungUp;
    let e5 = TtyError::PermissionDenied;
    let e6 = TtyError::BackgroundRead;
    let e7 = TtyError::UnsupportedLineDiscipline;
    if e1 == e2 || e2 == e3 || e3 == e4 || e4 == e5 || e5 == e6 || e6 == e7 {
        klog_info!("TTY_TEST: BUG - TtyError variants not distinct");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_read_returns_result() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(0), &mut buf, true) {
        Err(TtyError::WouldBlock) => {}
        other => {
            klog_info!(
                "TTY_TEST: BUG - empty nonblock read expected WouldBlock, got {:?}",
                other
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_read_invalid_index_error() -> TestResult {
    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(99), &mut buf, true) {
        Err(TtyError::InvalidIndex) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected InvalidIndex, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_read_not_allocated_error() -> TestResult {
    tty::table::tty_table_init();
    let mut buf = [0u8; 8];
    match tty::read(TtyIndex(5), &mut buf, true) {
        Err(TtyError::NotAllocated) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - expected NotAllocated, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_write_returns_result() -> TestResult {
    tty::table::tty_table_init();
    match tty::write(TtyIndex(0), b"hello", false) {
        Ok(5) => TestResult::Pass,
        other => {
            klog_info!("TTY_TEST: BUG - write expected Ok(5), got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_get_termios_returns_result() -> TestResult {
    tty::table::tty_table_init();
    match tty::get_termios(TtyIndex(0)) {
        Ok(t) => {
            if !t.c_lflag.contains(LocalFlags::ICANON) {
                klog_info!("TTY_TEST: BUG - default termios should have ICANON");
                return TestResult::Fail;
            }
            TestResult::Pass
        }
        Err(e) => {
            klog_info!("TTY_TEST: BUG - get_termios failed: {:?}", e);
            TestResult::Fail
        }
    }
}

pub fn test_vmin0_vtime0_immediate_return() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 0;
    raw.c_cc[5] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(0) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=0/VTIME=0 expected Ok(0), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_vmin_enforcement() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[6] = 3;
    raw.c_cc[5] = 0;
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
                "TTY_TEST: BUG - VMIN=3 read expected Ok(>=3), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_vmin0_vtime0_with_data_immediate_return() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 0;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'y');

    let mut buf = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut buf, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(2) if &buf[..2] == b"xy" => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN=0/VTIME=0 with data expected Ok(2), got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_vmin_limited_by_buffer_size() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 8;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');
    tty::push_input(TtyIndex(0), b'b');
    tty::push_input(TtyIndex(0), b'c');

    let mut buf = [0u8; 3];
    let result = tty::read(TtyIndex(0), &mut buf, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(3) if &buf == b"abc" => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - VMIN larger than buffer should cap at buffer size, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_canonical_to_noncanonical_preserves_buffered_data() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();

    tty::push_input(TtyIndex(0), b'a');
    tty::push_input(TtyIndex(0), b'b');
    tty::push_input(TtyIndex(0), b'c');

    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(3) if &out[..3] == b"abc" => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - canonical->noncanonical should preserve buffered data, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_set_fg_pgrp_checked_permission_denied() -> TestResult {
    tty::table::tty_table_init();
    let scope = SessionScope::new(10, 10);
    tty::session::test_install_session(TtyIndex(0), scope.session_weak(), scope.pgrp_weak());
    match tty::set_foreground_pgrp_checked(TtyIndex(0), 20, 99) {
        Err(TtyError::PermissionDenied) => {
            tty::detach_session(TtyIndex(0));
            TestResult::Pass
        }
        other => {
            tty::detach_session(TtyIndex(0));
            klog_info!("TTY_TEST: BUG - expected PermissionDenied, got {:?}", other);
            TestResult::Fail
        }
    }
}

pub fn test_hangup_read_returns_hung_up() -> TestResult {
    tty::table::tty_table_init();
    let _con = tty::open_tty(TtyIndex(0));
    let _hangup = HangupScope::hang_up(TtyIndex(0));

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);

    // POSIX requires EOF, not an error, for reads after hangup.
    match result {
        Ok(0) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - hangup nonblock read expected Ok(0) EOF, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_per_tty_lock_independence() -> TestResult {
    tty::table::tty_table_init();

    // Two acquires of one lock class: the inner takes a subclass so lockdep
    // still checks the ascending-slot order instead of an unordered nest.
    let guard0 = TTY_SLOTS[0].lock();
    let guard1 = TTY_SLOTS[1].lock_nested(1);

    let ok0 = guard0.is_some();
    let ok1 = guard1.is_some();
    drop(guard1);
    drop(guard0);

    if !ok0 || !ok1 {
        klog_info!("TTY_TEST: BUG - per-TTY slots not independently lockable");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_driver_id_round_trip() -> TestResult {
    let serial = TtyDriverKind::SerialConsole(crate::tty::driver::SerialConsoleDriver);
    let vconsole = TtyDriverKind::VConsole(VConsoleDriver);
    let also_serial = TtyDriverKind::SerialConsole(SerialConsoleDriver);

    if !matches!(serial.id(), DriverId::SerialConsole) {
        klog_info!("TTY_TEST: BUG - SerialConsole id mismatch");
        return TestResult::Fail;
    }
    if !matches!(vconsole.id(), DriverId::VConsole) {
        klog_info!("TTY_TEST: BUG - VConsole id mismatch");
        return TestResult::Fail;
    }
    if !matches!(also_serial.id(), DriverId::SerialConsole) {
        klog_info!("TTY_TEST: BUG - second SerialConsole id mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_split_write_returns_input_len() -> TestResult {
    tty::table::tty_table_init();

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut t = saved;
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    tty::set_termios(TtyIndex(0), &t).unwrap();

    let data = b"abc\ndef\n";
    let n = tty::write(TtyIndex(0), data, false);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if n != Ok(data.len()) {
        klog_info!(
            "TTY_TEST: BUG - split-write returned {:?} instead of Ok({})",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_idle_cb_iterates_all_ttys() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    tty::push_input(TtyIndex(1), b'z');
    tty::push_input(TtyIndex(1), b'\n');

    // has_data drains hw input under the slot lock — the idle callback's path.
    let has1 = tty::has_data(TtyIndex(1));
    drain_tty_nonblock(TtyIndex(1));

    if !has1 {
        klog_info!("TTY_TEST: BUG - idle callback path did not find data on TTY 1");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_merged_drain_read() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    tty::push_input(TtyIndex(0), b'o');
    tty::push_input(TtyIndex(0), b'k');
    tty::push_input(TtyIndex(0), b'\n');

    let mut out = [0u8; 16];
    let n = tty::read(TtyIndex(0), &mut out, true);
    if n != Ok(3) || &out[..3] != b"ok\n" {
        klog_info!(
            "TTY_TEST: BUG - merged drain+read mismatch (n={:?}, data={:?})",
            n,
            &out[..3]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_with_tty_per_slot() -> TestResult {
    tty::table::tty_table_init();

    let idx0 = tty::table::with_tty(TtyIndex(0), |tty| tty.index);
    let idx1 = tty::table::with_tty(TtyIndex(1), |tty| tty.index);
    let idx_empty = tty::table::with_tty(TtyIndex(5), |tty| tty.index);

    if idx0 != Some(TtyIndex(0)) {
        klog_info!("TTY_TEST: BUG - with_tty slot 0 returned wrong index");
        return TestResult::Fail;
    }
    if idx1 != Some(TtyIndex(1)) {
        klog_info!("TTY_TEST: BUG - with_tty slot 1 returned wrong index");
        return TestResult::Fail;
    }
    if idx_empty.is_some() {
        klog_info!("TTY_TEST: BUG - with_tty empty slot 5 returned Some");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_driver_id_clonable() -> TestResult {
    let id = DriverId::SerialConsole;
    let id_clone = id.clone();

    if !matches!(id_clone, DriverId::SerialConsole) {
        klog_info!("TTY_TEST: BUG - DriverId clone lost its variant");
        return TestResult::Fail;
    }
    if matches!(DriverId::VConsole, DriverId::SerialConsole) {
        klog_info!("TTY_TEST: BUG - DriverId variants must be distinguishable");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_default_termios_has_icrnl() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    if !t.c_iflag.contains(InputFlags::ICRNL) {
        klog_info!(
            "TTY_TEST: BUG - default c_iflag missing ICRNL (got 0x{:x})",
            t.c_iflag.bits()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_default_termios_has_opost_onlcr() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    let expected = OutputFlags::OPOST | OutputFlags::ONLCR;
    if (t.c_oflag & expected) != expected {
        klog_info!(
            "TTY_TEST: BUG - default c_oflag missing OPOST|ONLCR (got 0x{:x})",
            t.c_oflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_default_termios_has_full_lflag() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();
    let expected = LocalFlags::ISIG
        | LocalFlags::ICANON
        | LocalFlags::ECHO
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHOCTL
        | LocalFlags::ECHOKE;
    if (t.c_lflag & expected) != expected {
        klog_info!(
            "TTY_TEST: BUG - default c_lflag missing flags (got 0x{:x}, want 0x{:x})",
            t.c_lflag,
            expected
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_output_column_tracking_printable() -> TestResult {
    let mut ld = LineDisc::new();
    for ch in b"Hello" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 3 {
                klog_info!(
                    "TTY_TEST: BUG - after 5 chars expected tab=3 spaces, got {}",
                    n
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant for tab byte");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_output_column_tracking_newline() -> TestResult {
    let mut ld = LineDisc::new();
    for ch in b"Hello" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(b'\n');
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - after NL expected tab=8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_output_column_tracking_cr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::OPOST | OutputFlags::XTABS;
    ld.set_termios(&t);

    for ch in b"ABCDE" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(b'\r');
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - after CR expected tab=8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_output_column_tracking_tab() -> TestResult {
    let mut ld = LineDisc::new();
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - tab at col 0 expected 8 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant at col 0");
            return TestResult::Fail;
        }
    }
    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 5 {
                klog_info!("TTY_TEST: BUG - tab at col 11 expected 5 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant at col 11");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_output_column_tracking_backspace() -> TestResult {
    let mut ld = LineDisc::new();
    for ch in b"AB" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(0x08);
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 7 {
                klog_info!("TTY_TEST: BUG - after BS expected tab=7 spaces, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    let mut ld2 = LineDisc::new();
    ld2.process_output_byte(0x08);
    match ld2.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!(
                    "TTY_TEST: BUG - BS at col 0 should stay 0, tab gave {} spaces",
                    n
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_onocr_at_column_zero() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONOCR;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\r') {
        OutputAction::Suppress => {}
        _other => {
            klog_info!("TTY_TEST: BUG - ONOCR at col 0 should suppress CR");
            return TestResult::Fail;
        }
    }
    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\r') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\r' {
                klog_info!("TTY_TEST: BUG - ONOCR at col 3 should emit CR");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ONOCR at col 3 should emit, not suppress");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_default_onlcr_newline_expands() -> TestResult {
    let mut ld = LineDisc::new();
    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!(
                    "TTY_TEST: BUG - default ONLCR should produce CR+NL, got len={}",
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - default ONLCR should emit, not suppress/tab");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_signal_values_from_signal_module() -> TestResult {
    if SIGINT != 2 {
        klog_info!("TTY_TEST: BUG - SIGINT should be 2, got {}", SIGINT);
        return TestResult::Fail;
    }
    if SIGQUIT != 3 {
        klog_info!("TTY_TEST: BUG - SIGQUIT should be 3, got {}", SIGQUIT);
        return TestResult::Fail;
    }
    if SIGTSTP != 20 {
        klog_info!("TTY_TEST: BUG - SIGTSTP should be 20, got {}", SIGTSTP);
        return TestResult::Fail;
    }
    if SIGHUP != 1 {
        klog_info!("TTY_TEST: BUG - SIGHUP should be 1, got {}", SIGHUP);
        return TestResult::Fail;
    }
    if SIGCONT != 18 {
        klog_info!("TTY_TEST: BUG - SIGCONT should be 18, got {}", SIGCONT);
        return TestResult::Fail;
    }
    if SIGTTIN != 21 {
        klog_info!("TTY_TEST: BUG - SIGTTIN should be 21, got {}", SIGTTIN);
        return TestResult::Fail;
    }
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_signal_uses_signal_module() -> TestResult {
    let mut ld = LineDisc::new();
    match ld.input_char(3) {
        InputAction::Signal(sig) if sig == SIGINT => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+C should produce Signal(SIGINT=2)");
            return TestResult::Fail;
        }
    }
    match ld.input_char(28) {
        InputAction::Signal(sig) if sig == SIGQUIT => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+\\ should produce Signal(SIGQUIT=3)");
            return TestResult::Fail;
        }
    }
    match ld.input_char(26) {
        InputAction::Signal(sig) if sig == SIGTSTP => {}
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+Z should produce Signal(SIGTSTP=20)");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_hangup_signals_from_signal_module() -> TestResult {
    if SIGHUP != 1 {
        klog_info!("TTY_TEST: BUG - SIGHUP should be 1, got {}", SIGHUP);
        return TestResult::Fail;
    }
    if SIGCONT != 18 {
        klog_info!("TTY_TEST: BUG - SIGCONT should be 18, got {}", SIGCONT);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_job_control_signals_from_signal_module() -> TestResult {
    if SIGTTIN != 21 {
        klog_info!("TTY_TEST: BUG - SIGTTIN should be 21, got {}", SIGTTIN);
        return TestResult::Fail;
    }
    if SIGTTOU != 22 {
        klog_info!("TTY_TEST: BUG - SIGTTOU should be 22, got {}", SIGTTOU);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_canonical_one_line_per_read() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');
    for &c in b"def" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n1 = ld.read(&mut buf);
    if n1 != 4 || &buf[..4] != b"abc\n" {
        klog_info!(
            "TTY_TEST: BUG - canonical read should return one line (got {} bytes)",
            n1
        );
        return TestResult::Fail;
    }

    let n2 = ld.read(&mut buf);
    if n2 != 4 || &buf[..4] != b"def\n" {
        klog_info!(
            "TTY_TEST: BUG - canonical second read mismatch (got {} bytes)",
            n2
        );
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data should be false after reading both lines");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_canonical_has_data_line_count() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"hello" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data true before newline");
        return TestResult::Fail;
    }

    ld.input_char(b'\n');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data false after newline");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let _ = ld.read(&mut buf);
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data true after reading line");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_canonical_eof_line_boundary() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x04);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical has_data false after EOF flush");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - EOF flush read mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after reading EOF-flushed chunk");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sigwinch_constant() -> TestResult {
    if SIGWINCH != 28 {
        klog_info!("TTY_TEST: BUG - SIGWINCH should be 28, got {}", SIGWINCH);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_word_erase_path_boundary() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);

    for &c in b"/usr/local/bin" {
        ld.input_char(c);
    }

    ld.input_char(0x17);

    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 12 || &buf[..11] != b"/usr/local/" {
        klog_info!(
            "TTY_TEST: BUG - word erase path boundary mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_word_erase_mixed_boundary() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);

    for &c in b"hello---world" {
        ld.input_char(c);
    }

    ld.input_char(0x17);

    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 9 || &buf[..8] != b"hello---" {
        klog_info!(
            "TTY_TEST: BUG - word erase mixed boundary mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_word_erase_trailing_spaces() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);

    for &c in b"hello   " {
        ld.input_char(c);
    }

    ld.input_char(0x17);

    ld.input_char(b'\n');
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - word erase trailing spaces mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_canonical_small_buffer_read() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abcdefgh" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf = [0u8; 3];
    let n1 = ld.read(&mut buf);
    if n1 != 3 || &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - small buffer first read mismatch");
        return TestResult::Fail;
    }

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data false mid-line");
        return TestResult::Fail;
    }

    let mut buf2 = [0u8; 64];
    let n2 = ld.read(&mut buf2);
    if n2 != 6 || &buf2[..6] != b"defgh\n" {
        klog_info!(
            "TTY_TEST: BUG - small buffer second read mismatch (got {} bytes)",
            n2
        );
        return TestResult::Fail;
    }

    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data true after full line consumed");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tcsetsw_preserves_pending_input() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');

    let mut changed = raw;
    changed.c_lflag &= !LocalFlags::ECHO;
    tty::set_termios_wait(TtyIndex(0), &changed).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Ok(1) if out[0] == b'a' => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSW should preserve pending input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_tcsetsf_flushes_pending_input() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'a');

    let mut changed = raw;
    changed.c_lflag &= !LocalFlags::ECHO;
    tty::set_termios_flush(TtyIndex(0), &changed).unwrap();

    let mut out = [0u8; 8];
    let result = tty::read(TtyIndex(0), &mut out, true);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    match result {
        Err(TtyError::WouldBlock) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TCSETSF should flush pending input, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_read_with_attach_false_skips_auto_attach() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'z');

    let mut out = [0u8; 8];
    let result = tty::read_with_attach(TtyIndex(0), &mut out, true, false);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if result != Ok(1) || out[0] != b'z' || sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - read_with_attach(false) should not auto-attach (result={:?}, sid={}, b0=0x{:02x})",
            result,
            sid,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_read_with_attach_true_skips_durable_attach() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !LocalFlags::ICANON;
    raw.c_cc[slopos_abi::syscall::VMIN] = 1;
    raw.c_cc[slopos_abi::syscall::VTIME] = 0;
    tty::set_termios(TtyIndex(0), &raw).unwrap();

    tty::push_input(TtyIndex(0), b'y');

    let mut out = [0u8; 8];
    let result = tty::read_with_attach(TtyIndex(0), &mut out, true, true);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if result != Ok(1) || out[0] != b'y' || sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - read_with_attach(true) should no longer claim ownership (result={:?}, sid={}, b0=0x{:02x})",
            result,
            sid,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_acquire_and_release_controlling_terminal() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));

    let scope = SessionScope::new(42, 77);
    let acquire = tty::acquire_controlling_terminal(TtyIndex(0), scope.pgrp_weak());
    let sid_after_acquire = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    let release = tty::release_controlling_terminal(TtyIndex(0), 42);
    let sid_after_release = tty::get_session_id(TtyIndex(0)).unwrap_or(0);

    if acquire != Ok(()) || sid_after_acquire != 42 || release != Ok(true) || sid_after_release != 0
    {
        klog_info!(
            "TTY_TEST: BUG - explicit controlling-terminal transition mismatch (acquire={:?}, sid_acquire={}, release={:?}, sid_release={})",
            acquire,
            sid_after_acquire,
            release,
            sid_after_release
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_release_wrong_session_is_noop() -> TestResult {
    tty::table::tty_table_init();
    tty::detach_session(TtyIndex(0));
    let scope = SessionScope::new(88, 88);
    tty::acquire_controlling_terminal(TtyIndex(0), scope.pgrp_weak()).unwrap();

    let release = tty::release_controlling_terminal(TtyIndex(0), 99);
    let sid = tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    tty::release_controlling_terminal(TtyIndex(0), 88).unwrap();

    if release != Ok(false) || sid != 88 {
        klog_info!(
            "TTY_TEST: BUG - wrong-session release should be ignored (release={:?}, sid={})",
            release,
            sid
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_get_ldisc_default_is_ntty() -> TestResult {
    tty::table::tty_table_init();

    match tty::get_ldisc(TtyIndex(0)) {
        Ok(slopos_abi::syscall::N_TTY) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - default line discipline should be N_TTY, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_set_ldisc_round_trip_preserves_termios() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));

    let saved = tty::get_termios(TtyIndex(0)).unwrap();
    let mut configured = saved;
    configured.c_lflag &= !LocalFlags::ICANON;
    configured.c_cc[slopos_abi::syscall::VMIN] = 7;
    configured.c_cc[slopos_abi::syscall::VTIME] = 3;
    tty::set_termios(TtyIndex(0), &configured).unwrap();

    tty::set_ldisc(TtyIndex(0), slopos_abi::syscall::N_RAW).unwrap();
    let raw_kind = tty::get_ldisc(TtyIndex(0));
    let raw_termios = tty::get_termios(TtyIndex(0)).unwrap();

    tty::push_input(TtyIndex(0), b'q');
    let mut out = [0u8; 8];
    let raw_read = tty::read(TtyIndex(0), &mut out, true);

    tty::set_ldisc(TtyIndex(0), slopos_abi::syscall::N_TTY).unwrap();
    let ntty_kind = tty::get_ldisc(TtyIndex(0));
    let ntty_termios = tty::get_termios(TtyIndex(0)).unwrap();
    tty::set_termios(TtyIndex(0), &saved).unwrap();

    if raw_kind != Ok(slopos_abi::syscall::N_RAW)
        || ntty_kind != Ok(slopos_abi::syscall::N_TTY)
        || raw_termios.c_cc[slopos_abi::syscall::VMIN] != 7
        || raw_termios.c_cc[slopos_abi::syscall::VTIME] != 3
        || ntty_termios.c_cc[slopos_abi::syscall::VMIN] != 7
        || ntty_termios.c_cc[slopos_abi::syscall::VTIME] != 3
        || raw_termios.c_line != slopos_abi::syscall::N_RAW as u8
        || ntty_termios.c_line != slopos_abi::syscall::N_TTY as u8
        || raw_read != Ok(1)
        || out[0] != b'q'
    {
        klog_info!(
            "TTY_TEST: BUG - ldisc round-trip mismatch (raw_kind={:?}, ntty_kind={:?}, raw_read={:?}, raw_line={}, ntty_line={})",
            raw_kind,
            ntty_kind,
            raw_read,
            raw_termios.c_line,
            ntty_termios.c_line
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_set_ldisc_invalid_id_rejected() -> TestResult {
    tty::table::tty_table_init();

    match tty::set_ldisc(TtyIndex(0), 99) {
        Err(TtyError::UnsupportedLineDiscipline) => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - invalid ldisc id should be rejected, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

pub fn test_pty_alloc_returns_master_and_slave() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = match tty::pty_alloc(slopos_ostd::process::quota::root()) {
        Ok(pair) => pair,
        Err(err) => {
            klog_info!("TTY_TEST: BUG - pty_alloc failed: {:?}", err);
            return TestResult::Fail;
        }
    };
    let slave = match tty::get_pty_number(master) {
        Ok(idx) => TtyIndex(idx as u8),
        Err(err) => {
            klog_info!("TTY_TEST: BUG - get_pty_number failed: {:?}", err);
            return TestResult::Fail;
        }
    };

    tty::set_pty_lock(master, false).ok();
    let slave_open = tty::pty_open_slave(slave);
    let master_ok = master_backing.is_pty_master();
    let slave_ok = slave_open.is_ok();
    let slave_is_pty = tty::is_pty_slave(slave);
    let master_is_not_slave = !tty::is_pty_slave(master);

    drop(slave_open);
    drop(master_backing);

    if !master_ok || !slave_ok || master == slave || !slave_is_pty || !master_is_not_slave {
        klog_info!(
            "TTY_TEST: BUG - PTY allocation mismatch (master_ok={}, slave_ok={}, master={}, slave={}, slave_is_pty={}, master_is_not_slave={})",
            master_ok,
            slave_ok,
            master.0,
            slave.0,
            slave_is_pty,
            master_is_not_slave
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_pty_master_to_slave_flow() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    let saved = tty::get_termios(slave).unwrap();
    let mut raw = saved;
    raw.c_lflag &= !(LocalFlags::ICANON | LocalFlags::ECHO);
    tty::set_termios(slave, &raw).unwrap();

    let write_rc = tty::write(master, b"hello", false);
    let mut buf = [0u8; 16];
    let read_rc = tty::read(slave, &mut buf, true);

    tty::set_termios(slave, &saved).unwrap();

    if write_rc != Ok(5) || read_rc != Ok(5) || &buf[..5] != b"hello" {
        klog_info!(
            "TTY_TEST: BUG - PTY master->slave flow mismatch (write={:?}, read={:?}, data={:?})",
            write_rc,
            read_rc,
            &buf[..5]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_pty_slave_to_master_flow() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    let write_rc = tty::write(slave, b"world\n", false);
    let mut buf = [0u8; 16];
    let read_rc = tty::read(master, &mut buf, true);

    if write_rc != Ok(6) || read_rc != Ok(7) || &buf[..7] != b"world\r\n" {
        klog_info!(
            "TTY_TEST: BUG - PTY slave->master flow mismatch (write={:?}, read={:?}, data={:?})",
            write_rc,
            read_rc,
            &buf[..7]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_master_close_hangs_up_slave() -> TestResult {
    tty::table::tty_table_init();

    let (master, master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    drop(master_backing);
    let is_hung = tty::is_hung_up(slave);
    let mut buf = [0u8; 8];
    let read_rc = tty::read(slave, &mut buf, true);

    if !is_hung || read_rc != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - master close should hang up slave (is_hung={}, read={:?})",
            is_hung,
            read_rc
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_slave_close_eofs_master_and_stays_reopenable() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let slave_open = tty::pty_open_slave(slave).unwrap();

    let mut buf = [0u8; 8];
    let with_slave_open = tty::read(master, &mut buf, true);

    drop(slave_open);
    let after_close = tty::read(master, &mut buf, true);

    let reopened = tty::pty_open_slave(slave);
    let reopen_ok = reopened.is_ok();
    let after_reopen = tty::read(master, &mut buf, true);
    drop(reopened);

    if with_slave_open != Err(TtyError::WouldBlock) {
        klog_info!(
            "TTY_TEST: BUG - master read with slave open should block, got {:?}",
            with_slave_open
        );
        return TestResult::Fail;
    }
    if after_close != Ok(0) {
        klog_info!(
            "TTY_TEST: BUG - master read after last slave close should be EOF Ok(0), got {:?}",
            after_close
        );
        return TestResult::Fail;
    }
    if !reopen_ok {
        klog_info!("TTY_TEST: BUG - slave should be reopenable while the master lives");
        return TestResult::Fail;
    }
    if after_reopen != Err(TtyError::WouldBlock) {
        klog_info!(
            "TTY_TEST: BUG - reopen should clear the EOF latch and block again, got {:?}",
            after_reopen
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_pty_canonical_editing_on_slave() -> TestResult {
    tty::table::tty_table_init();

    let (master, _master_backing) = tty::pty_alloc(slopos_ostd::process::quota::root()).unwrap();
    let slave = TtyIndex(tty::get_pty_number(master).unwrap() as u8);
    tty::set_pty_lock(master, false).unwrap();
    let _slave_backing = tty::pty_open_slave(slave).unwrap();

    let saved = tty::get_termios(slave).unwrap();
    let mut no_echo = saved;
    no_echo.c_lflag &= !LocalFlags::ECHO;
    tty::set_termios(slave, &no_echo).unwrap();

    let write_rc = tty::write(master, b"foo\nbar\n", false);
    let mut buf = [0u8; 16];
    let first_read = tty::read(slave, &mut buf, true);
    let second_read = tty::read(slave, &mut buf, true);

    tty::set_termios(slave, &saved).unwrap();

    if write_rc != Ok(8) || first_read != Ok(4) || second_read != Ok(4) {
        klog_info!(
            "TTY_TEST: BUG - PTY canonical reads mismatch (write={:?}, first={:?}, second={:?})",
            write_rc,
            first_read,
            second_read
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_ignbrk_discards_break() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::IGNBRK;
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    ld.set_termios(&t);
    let action = ld.input_char(0x00);
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - IGNBRK should discard break (NUL)");
        return TestResult::Fail;
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - IGNBRK should not buffer any data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_brkint_generates_sigint() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::BRKINT;
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG;
    ld.set_termios(&t);
    ld.input_char(b'a');
    let action = ld.input_char(0x00);
    match action {
        InputAction::Signal(sig) if sig == SIGINT => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - BRKINT should generate SIGINT, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - BRKINT should flush input queues");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_parmrk_inserts_marker() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::PARMRK;
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);
    ld.input_char(0x00);
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 3 || buf[0] != 0xFF || buf[1] != 0x00 || buf[2] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - PARMRK should insert 0xFF 0x00 0x00, got {} bytes: {:?}",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_nul_without_break_flags_passes_through() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::empty();
    t.c_lflag = LocalFlags::empty();
    ld.set_termios(&t);
    ld.input_char(0x00);
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != 0x00 {
        klog_info!(
            "TTY_TEST: BUG - NUL without break flags should pass through, got {} bytes",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_echoke_visual_erase() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::empty();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOKE;
    ld.set_termios(&t);
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(b'c');
    let action = ld.input_char(0x15);
    match action {
        InputAction::KillLineEcho { columns } if columns == 3 => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOKE should return KillLineEcho{{columns:3}}, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_echok_newline_on_kill() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::empty();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOK;
    ld.set_termios(&t);
    ld.input_char(b'a');
    ld.input_char(b'b');
    let action = ld.input_char(0x15);
    match action {
        InputAction::Echo { buf, len } if len == 1 && buf[0] == b'\n' => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOK should echo newline on kill, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_echoctl_erase_two_columns() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::empty();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE | LocalFlags::ECHOCTL;
    ld.set_termios(&t);
    // Ctrl+V (VLNEXT, needs IEXTEN) inserts the next byte literally.
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);
    ld.input_char(0x16);
    ld.input_char(0x01);
    let action = ld.input_char(0x7F);
    match action {
        InputAction::KillLineEcho { columns } if columns == 2 => {}
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOCTL erase should return KillLineEcho{{columns:2}}, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_bytes_available() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::empty();
    t.c_iflag = InputFlags::empty();
    ld.set_termios(&t);
    if ld.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh LineDisc should have 0 bytes available");
        return TestResult::Fail;
    }
    ld.input_char(b'x');
    ld.input_char(b'y');
    ld.input_char(b'z');
    if ld.bytes_available() != 3 {
        klog_info!(
            "TTY_TEST: BUG - expected 3 bytes available, got {}",
            ld.bytes_available()
        );
        return TestResult::Fail;
    }
    let mut buf = [0u8; 2];
    ld.read(&mut buf);
    if ld.bytes_available() != 1 {
        klog_info!(
            "TTY_TEST: BUG - expected 1 byte available after reading 2, got {}",
            ld.bytes_available()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_raw_disc_bytes_available() -> TestResult {
    let mut rd = RawDisc::new();
    if rd.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh RawDisc should have 0 bytes available");
        return TestResult::Fail;
    }
    rd.input_char(b'a');
    rd.input_char(b'b');
    if rd.bytes_available() != 2 {
        klog_info!(
            "TTY_TEST: BUG - expected 2 bytes available, got {}",
            rd.bytes_available()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_kind_bytes_available() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    {
        let mut t = *lk.termios();
        t.c_lflag = LocalFlags::empty();
        t.c_iflag = InputFlags::empty();
        lk.set_termios(&t);
    }
    if lk.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - fresh LdiscKind::NTty should have 0 bytes");
        return TestResult::Fail;
    }
    lk.input_char(b'q');
    if lk.bytes_available() != 1 {
        klog_info!("TTY_TEST: BUG - expected 1 byte available via LdiscKind");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_fionread_constant() -> TestResult {
    if slopos_abi::syscall::FIONREAD != 0x541B {
        klog_info!("TTY_TEST: BUG - FIONREAD should be 0x541B");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_kill_empty_line_no_echo() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::empty();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOKE;
    ld.set_termios(&t);
    let action = ld.input_char(0x15);
    if !matches!(action, InputAction::None) {
        klog_info!(
            "TTY_TEST: BUG - kill on empty line should return None, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ignbrk_takes_priority_over_brkint() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag = InputFlags::IGNBRK | InputFlags::BRKINT;
    t.c_lflag = LocalFlags::ISIG;
    ld.set_termios(&t);
    let action = ld.input_char(0x00);
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - IGNBRK should take priority over BRKINT");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_input_flags_from_bits() -> TestResult {
    let flags = InputFlags::from_bits_truncate(0x100);
    if !flags.contains(InputFlags::ICRNL) {
        klog_info!("TTY_TEST: BUG - InputFlags::from_bits_truncate(0x100) missing ICRNL");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_output_flags_from_bits() -> TestResult {
    let flags = OutputFlags::from_bits_truncate(0x05);
    if !flags.contains(OutputFlags::OPOST | OutputFlags::ONLCR) {
        klog_info!("TTY_TEST: BUG - OutputFlags::from_bits_truncate(0x05) missing OPOST|ONLCR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_local_flags_from_bits() -> TestResult {
    let raw = (LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG).bits();
    let flags = LocalFlags::from_bits_truncate(raw);
    if flags != (LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::ISIG) {
        klog_info!("TTY_TEST: BUG - LocalFlags round-trip mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_cc_index_values() -> TestResult {
    if CcIndex::Vintr.as_usize() != 0
        || CcIndex::Veof.as_usize() != 4
        || CcIndex::Vtime.as_usize() != 5
        || CcIndex::Vmin.as_usize() != 6
        || CcIndex::Vwerase.as_usize() != 14
    {
        klog_info!("TTY_TEST: BUG - CcIndex values do not match expected ABI indices");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_posix_vdisable() -> TestResult {
    if POSIX_VDISABLE != 0 {
        klog_info!(
            "TTY_TEST: BUG - POSIX_VDISABLE should be 0, got {}",
            POSIX_VDISABLE
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_tty_error_to_errno() -> TestResult {
    let pairs = [
        (TtyError::InvalidIndex, -22),
        (TtyError::NotAllocated, -6),
        (TtyError::BackgroundRead, -5),
        (TtyError::BackgroundWrite, -5),
        (TtyError::HungUp, -5),
        (TtyError::WouldBlock, -11),
        (TtyError::PermissionDenied, -1),
        (TtyError::UnsupportedLineDiscipline, -22),
        (TtyError::CrossSessionDenied, -5),
        (TtyError::SignalInterrupt, -4),
        (TtyError::Restart, -512),
    ];
    for (err, expected) in pairs {
        if err.to_errno() != expected {
            klog_info!(
                "TTY_TEST: BUG - TtyError::{:?}.to_errno()={} expected {}",
                err,
                err.to_errno(),
                expected
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_tty_error_signal_interrupt() -> TestResult {
    if TtyError::SignalInterrupt.to_errno() != -4 {
        klog_info!("TTY_TEST: BUG - SignalInterrupt should map to -4 (EINTR)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_user_termios_typed_accessors() -> TestResult {
    let mut t = slopos_abi::syscall::UserTermios::default();
    t.c_iflag = InputFlags::ICRNL;
    t.c_lflag = LocalFlags::ECHO;
    t.set_cc(CcIndex::Vintr, 0x03);

    if !t.input_flags().contains(InputFlags::ICRNL)
        || !t.local_flags().contains(LocalFlags::ECHO)
        || t.cc(CcIndex::Vintr) != 0x03
    {
        klog_info!("TTY_TEST: BUG - UserTermios typed accessors mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_typed_flags_behavioral_equivalence() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"abc\n" {
        ld.input_char(c);
    }
    let mut out = [0u8; 8];
    let n = ld.read(&mut out);
    if n != 4 || &out[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - typed flag migration changed canonical behavior");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_control_flags_empty() -> TestResult {
    if !ControlFlags::empty().is_empty() || ControlFlags::empty().bits() != 0 {
        klog_info!("TTY_TEST: BUG - ControlFlags::empty is not zero/empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ldisc_ops_linedisc_trait_delegation() -> TestResult {
    let mut ld = LineDisc::new();
    let t = ld.termios();
    if t.c_line != slopos_abi::syscall::N_TTY as u8 {
        klog_info!("TTY_TEST: BUG - termios for LineDisc returned wrong c_line");
        return TestResult::Fail;
    }
    let (vmin, _vtime) = ld.vmin_vtime();
    if vmin != 1 {
        klog_info!("TTY_TEST: BUG - vmin_vtime for LineDisc wrong vmin");
        return TestResult::Fail;
    }
    if !ld.is_canonical() {
        klog_info!("TTY_TEST: BUG - is_canonical for LineDisc should be true");
        return TestResult::Fail;
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - has_data for LineDisc should be false initially");
        return TestResult::Fail;
    }
    if ld.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - bytes_available for LineDisc should be 0");
        return TestResult::Fail;
    }
    if ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - is_stopped for LineDisc should be false");
        return TestResult::Fail;
    }
    let _action = ld.input_char(InputEvent::normal(b'x'));
    ld.flush_all();
    TestResult::Pass
}

pub fn test_ldisc_ops_rawdisc_trait_delegation() -> TestResult {
    let mut rd = RawDisc::new();
    let t = rd.termios();
    if t.c_line != slopos_abi::syscall::N_RAW as u8 {
        klog_info!("TTY_TEST: BUG - termios for RawDisc returned wrong c_line");
        return TestResult::Fail;
    }
    if rd.is_canonical() {
        klog_info!("TTY_TEST: BUG - is_canonical for RawDisc should be false");
        return TestResult::Fail;
    }
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - has_data for RawDisc should be false initially");
        return TestResult::Fail;
    }
    let action = rd.input_char(InputEvent::normal(b'z'));
    if !matches!(action, InputAction::None) {
        klog_info!("TTY_TEST: BUG - RawDisc input_char via trait should return None");
        return TestResult::Fail;
    }
    if !rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should have data after input_char via trait");
        return TestResult::Fail;
    }
    if rd.bytes_available() != 1 {
        klog_info!("TTY_TEST: BUG - RawDisc bytes_available should be 1 after input");
        return TestResult::Fail;
    }
    rd.flush_all();
    if rd.has_data() {
        klog_info!("TTY_TEST: BUG - RawDisc should have no data after flush via trait");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_dispatch_macro_ntty_routing() -> TestResult {
    let mut lk = LdiscKind::NTty(LineDisc::new());
    if lk.id() != slopos_abi::syscall::N_TTY {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty id() wrong");
        return TestResult::Fail;
    }
    if !lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty is_canonical should be true");
        return TestResult::Fail;
    }
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty has_data should be false initially");
        return TestResult::Fail;
    }
    if lk.bytes_available() != 0 {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty bytes_available should be 0");
        return TestResult::Fail;
    }
    let _ = lk.input_char(b'A');
    let _ = lk.input_char(b'\n');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty should have data after newline");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 2 || buf[0] != b'A' || buf[1] != b'\n' {
        klog_info!("TTY_TEST: BUG - LdiscKind::NTty read mismatch n={}", n);
        return TestResult::Fail;
    }
    lk.flush_all();
    TestResult::Pass
}

pub fn test_dispatch_macro_raw_routing() -> TestResult {
    let mut lk = LdiscKind::Raw(RawDisc::new());
    if lk.id() != slopos_abi::syscall::N_RAW {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw id() wrong");
        return TestResult::Fail;
    }
    if lk.is_canonical() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw is_canonical should be false");
        return TestResult::Fail;
    }
    let _ = lk.input_char(b'R');
    if !lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have data after input");
        return TestResult::Fail;
    }
    if lk.bytes_available() != 1 {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw bytes_available should be 1");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 4];
    let n = lk.read(&mut buf);
    if n != 1 || buf[0] != b'R' {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw read mismatch");
        return TestResult::Fail;
    }
    lk.flush_all();
    if lk.has_data() {
        klog_info!("TTY_TEST: BUG - LdiscKind::Raw should have no data after flush");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_from_id_still_works() -> TestResult {
    let default_termios = LineDisc::new().termios().clone();
    let ntty = LdiscKind::from_id(slopos_abi::syscall::N_TTY, default_termios).expect("alloc");
    if ntty.is_none() {
        klog_info!("TTY_TEST: BUG - from_id(N_TTY) returned None");
        return TestResult::Fail;
    }
    if ntty.unwrap().id() != slopos_abi::syscall::N_TTY {
        klog_info!("TTY_TEST: BUG - from_id(N_TTY) id mismatch");
        return TestResult::Fail;
    }
    let nraw = LdiscKind::from_id(slopos_abi::syscall::N_RAW, default_termios).expect("alloc");
    if nraw.is_none() {
        klog_info!("TTY_TEST: BUG - from_id(N_RAW) returned None");
        return TestResult::Fail;
    }
    if nraw.unwrap().id() != slopos_abi::syscall::N_RAW {
        klog_info!("TTY_TEST: BUG - from_id(N_RAW) id mismatch");
        return TestResult::Fail;
    }
    if LdiscKind::from_id(999, default_termios)
        .expect("alloc")
        .is_some()
    {
        klog_info!("TTY_TEST: BUG - from_id(999) should return None");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_process_output_byte_dispatch() -> TestResult {
    let mut ntty = LdiscKind::NTty(LineDisc::new());
    let action = ntty.process_output_byte(b'\n');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - NTty output '\\n' should be CR+LF");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - NTty output '\\n' should be Emit");
            return TestResult::Fail;
        }
    }
    let mut raw = LdiscKind::Raw(RawDisc::new());
    let action = raw.process_output_byte(b'\n');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - Raw output '\\n' should be passthrough");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Raw output '\\n' should be Emit");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_edit_content_dispatch() -> TestResult {
    let mut ntty = LdiscKind::NTty(LineDisc::new());
    let _ = ntty.input_char(b'h');
    let _ = ntty.input_char(b'i');
    let content = ntty.edit_content();
    if content.len() != 2 || content[0] != b'h' || content[1] != b'i' {
        klog_info!("TTY_TEST: BUG - NTty edit_content should show typed chars");
        return TestResult::Fail;
    }
    let raw = LdiscKind::Raw(RawDisc::new());
    if !raw.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - Raw edit_content should be empty");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TABDLY/TAB0/TAB3/XTABS bit values follow the Linux termios ABI.
pub fn test_tabdly_abi_constants() -> TestResult {
    if OutputFlags::TABDLY.bits() != 0x1800 {
        klog_info!(
            "TTY_TEST: BUG - TABDLY != 0x1800, got 0x{:x}",
            OutputFlags::TABDLY.bits()
        );
        return TestResult::Fail;
    }
    if OutputFlags::TAB0.bits() != 0x0000 {
        klog_info!(
            "TTY_TEST: BUG - TAB0 != 0x0000, got 0x{:x}",
            OutputFlags::TAB0.bits()
        );
        return TestResult::Fail;
    }
    if OutputFlags::TAB3.bits() != 0x1800 {
        klog_info!(
            "TTY_TEST: BUG - TAB3 != 0x1800, got 0x{:x}",
            OutputFlags::TAB3.bits()
        );
        return TestResult::Fail;
    }
    if OutputFlags::XTABS.bits() != OutputFlags::TAB3.bits() {
        klog_info!("TTY_TEST: BUG - XTABS != TAB3");
        return TestResult::Fail;
    }

    if OutputFlags::TABDLY.bits() != 0x1800 {
        klog_info!("TTY_TEST: BUG - OutputFlags::TABDLY mismatch");
        return TestResult::Fail;
    }
    if OutputFlags::TAB3.bits() != 0x1800 {
        klog_info!("TTY_TEST: BUG - OutputFlags::TAB3 mismatch");
        return TestResult::Fail;
    }
    if OutputFlags::XTABS.bits() != OutputFlags::TAB3.bits() {
        klog_info!("TTY_TEST: BUG - OutputFlags::XTABS mismatch");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_default_oflag_includes_xtabs() -> TestResult {
    let ld = LineDisc::new();
    let oflag = ld.termios().output_flags();

    if !oflag.contains(OutputFlags::OPOST) {
        klog_info!("TTY_TEST: BUG - default oflag missing OPOST");
        return TestResult::Fail;
    }
    if !oflag.contains(OutputFlags::ONLCR) {
        klog_info!("TTY_TEST: BUG - default oflag missing ONLCR");
        return TestResult::Fail;
    }
    if !oflag.contains(OutputFlags::XTABS) {
        klog_info!("TTY_TEST: BUG - default oflag missing XTABS");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_xtabs_expands_tab_to_spaces() -> TestResult {
    let mut ld = LineDisc::new();
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - XTABS tab at col 0 expected 8, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - XTABS expected Tab variant at col 0");
            return TestResult::Fail;
        }
    }

    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 5 {
                klog_info!("TTY_TEST: BUG - XTABS tab at col 11 expected 5, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - XTABS expected Tab variant at col 11");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_tab0_passes_literal_tab() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = (t.c_oflag & !OutputFlags::TABDLY) | OutputFlags::TAB0;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\t') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\t' {
                klog_info!(
                    "TTY_TEST: BUG - TAB0 expected literal tab, got [{:#04x}, {:#04x}] len={}",
                    buf[0],
                    buf[1],
                    len
                );
                return TestResult::Fail;
            }
        }
        OutputAction::Tab(n) => {
            klog_info!("TTY_TEST: BUG - TAB0 should not produce Tab({})", n);
            return TestResult::Fail;
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - TAB0 should not suppress");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_tab0_column_tracking() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = (t.c_oflag & !OutputFlags::TABDLY) | OutputFlags::TAB0;
    ld.set_termios(&t);

    for ch in b"abc" {
        ld.process_output_byte(*ch);
    }
    ld.process_output_byte(b'\t');

    ld.process_output_byte(b'x');
    match ld.process_output_byte(b'\t') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\t' {
                klog_info!("TTY_TEST: BUG - TAB0 column tracking second tab wrong");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - TAB0 should emit literal tab");
            return TestResult::Fail;
        }
    }

    ld.process_output_byte(b'y');
    // Column is not directly observable; switch to XTABS and count the spaces.
    let mut t2 = *ld.termios();
    t2.c_oflag |= OutputFlags::XTABS;
    ld.set_termios(&t2);

    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 7 {
                klog_info!(
                    "TTY_TEST: BUG - TAB0 column drift: expected 7 spaces, got {}",
                    n
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant after switching to XTABS");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_xtabs_column_tracking_mixed() -> TestResult {
    let mut ld = LineDisc::new();

    ld.process_output_byte(b'a');
    ld.process_output_byte(b'b');
    ld.process_output_byte(b'\r');

    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - tab after CR expected 8, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant after CR");
            return TestResult::Fail;
        }
    }

    ld.process_output_byte(b'\n');

    match ld.process_output_byte(b'\t') {
        OutputAction::Tab(n) => {
            if n != 8 {
                klog_info!("TTY_TEST: BUG - tab after NL expected 8, got {}", n);
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Tab variant after NL");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_tabdly_termios_roundtrip() -> TestResult {
    let mut ld = LineDisc::new();

    let mut t = *ld.termios();
    t.c_oflag &= !OutputFlags::TABDLY;
    ld.set_termios(&t);

    let readback = ld.termios().output_flags();
    if readback.contains(OutputFlags::TAB3) {
        klog_info!("TTY_TEST: BUG - TAB0 readback still has TAB3 set");
        return TestResult::Fail;
    }

    let mut t2 = *ld.termios();
    t2.c_oflag |= OutputFlags::XTABS;
    ld.set_termios(&t2);

    let readback2 = ld.termios().output_flags();
    if !readback2.contains(OutputFlags::XTABS) {
        klog_info!("TTY_TEST: BUG - XTABS readback missing after set");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_no_opost_tab_passthrough() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::empty();
    ld.set_termios(&t);

    match ld.process_output_byte(b'\t') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\t' {
                klog_info!("TTY_TEST: BUG - no OPOST should pass tab through");
                return TestResult::Fail;
            }
        }
        OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - no OPOST should not expand tab");
            return TestResult::Fail;
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - no OPOST should not suppress tab");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

pub fn test_existing_output_unaffected() -> TestResult {
    let mut ld = LineDisc::new();

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - ONLCR regression with XTABS default");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Emit for NL with ONLCR");
            return TestResult::Fail;
        }
    }

    match ld.process_output_byte(b'A') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'A' {
                klog_info!("TTY_TEST: BUG - printable char regression with XTABS default");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Emit for printable");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

slopos_testing::stest!(
    name = test_ldisc_new_has_no_data,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_ldisc_read_empty, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_ldisc_canonical_newline,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_canonical_backspace,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_canonical_kill,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_ldisc_canonical_eof, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_ldisc_signal_ctrl_c, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_ldisc_raw_mode, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_ldisc_set_termios_flush,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_ldisc_flush_all, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_ldisc_echo_printable,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_ldisc_echo_newline, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_tty_index_eq, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_ldisc_multiple_reads,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_backspace_empty,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_write_returns_input_len,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_keyboard_input_event_delivery,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_keyboard_break_code_no_input,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_keyboard_modifier_no_input,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_keyboard_press_release_single_char,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_vconsole_drain_via_drain_hw_input,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_keyboard_multi_key_sequence,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_write_output_processing,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_write_raw_passthrough,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_write_invalid_index,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_per_tty_termios_isolation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_per_tty_winsize_isolation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_per_tty_fg_pgrp_isolation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_per_tty_has_data_isolation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_per_tty_session_isolation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_read_invalid_tty_returns_error,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_tty_index_abi_type, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_signal_constants, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_set_compositor_focus_does_not_set_fg_pgrp,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_check_read_sole_gate_background,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_backing_strong_count_is_open_count,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_hangup_sets_flag_and_detaches_session,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_hangup_nonblock_read_eio,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tty_hangup_blocking_read_eof,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_tty_error_variants, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_read_returns_result, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_read_invalid_index_error,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_read_not_allocated_error,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_write_returns_result,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_get_termios_returns_result,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_vmin0_vtime0_immediate_return,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_vmin_enforcement, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_vmin0_vtime0_with_data_immediate_return,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_vmin_limited_by_buffer_size,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_canonical_to_noncanonical_preserves_buffered_data,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_set_fg_pgrp_checked_permission_denied,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_hangup_read_returns_hung_up,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_per_tty_lock_independence,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_driver_id_round_trip,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_split_write_returns_input_len,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_idle_cb_iterates_all_ttys,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_merged_drain_read, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_with_tty_per_slot, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_driver_id_clonable, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_default_termios_has_icrnl,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_default_termios_has_opost_onlcr,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_default_termios_has_full_lflag,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_column_tracking_printable,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_column_tracking_newline,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_column_tracking_cr,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_column_tracking_tab,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_column_tracking_backspace,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_onocr_at_column_zero,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_default_onlcr_newline_expands,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_signal_values_from_signal_module,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_signal_uses_signal_module,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_hangup_signals_from_signal_module,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_job_control_signals_from_signal_module,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_canonical_one_line_per_read,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_canonical_has_data_line_count,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_canonical_eof_line_boundary,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_sigwinch_constant, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_word_erase_path_boundary,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_word_erase_mixed_boundary,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_word_erase_trailing_spaces,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_canonical_small_buffer_read,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tcsetsw_preserves_pending_input,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tcsetsf_flushes_pending_input,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_read_with_attach_false_skips_auto_attach,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_read_with_attach_true_skips_durable_attach,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_acquire_and_release_controlling_terminal,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_release_wrong_session_is_noop,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_get_ldisc_default_is_ntty,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_set_ldisc_round_trip_preserves_termios,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_set_ldisc_invalid_id_rejected,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_pty_alloc_returns_master_and_slave,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_pty_master_to_slave_flow,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_pty_slave_to_master_flow,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_master_close_hangs_up_slave,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_slave_close_eofs_master_and_stays_reopenable,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_pty_canonical_editing_on_slave,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ignbrk_discards_break,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_brkint_generates_sigint,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_parmrk_inserts_marker,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_nul_without_break_flags_passes_through,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_echoke_visual_erase, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_echok_newline_on_kill,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_echoctl_erase_two_columns,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_bytes_available, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_raw_disc_bytes_available,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_kind_bytes_available,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_fionread_constant, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_kill_empty_line_no_echo,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ignbrk_takes_priority_over_brkint,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_input_flags_from_bits,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_output_flags_from_bits,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_local_flags_from_bits,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_cc_index_values, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_posix_vdisable, suite = tty_test_ldisc_core);
slopos_testing::stest!(name = test_tty_error_to_errno, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_tty_error_signal_interrupt,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_user_termios_typed_accessors,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_typed_flags_behavioral_equivalence,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_control_flags_empty, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_ldisc_ops_linedisc_trait_delegation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_ldisc_ops_rawdisc_trait_delegation,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_dispatch_macro_ntty_routing,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_dispatch_macro_raw_routing,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(name = test_from_id_still_works, suite = tty_test_ldisc_core);
slopos_testing::stest!(
    name = test_process_output_byte_dispatch,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_edit_content_dispatch,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tabdly_abi_constants,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_default_oflag_includes_xtabs,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_xtabs_expands_tab_to_spaces,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tab0_passes_literal_tab,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tab0_column_tracking,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_xtabs_column_tracking_mixed,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_tabdly_termios_roundtrip,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_no_opost_tab_passthrough,
    suite = tty_test_ldisc_core
);
slopos_testing::stest!(
    name = test_existing_output_unaffected,
    suite = tty_test_ldisc_core
);
