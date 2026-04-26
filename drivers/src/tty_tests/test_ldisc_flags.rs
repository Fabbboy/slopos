//! Split from test_ldisc.rs: test_ldisc_flags.rs

use super::fixtures::*;

// ===========================================================================
// Input flag processing tests
// ===========================================================================

/// ICRNL: CR (0x0D) is mapped to NL (0x0A) when ICRNL is set.
pub fn test_ldisc_icrnl() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable ICRNL in c_iflag.
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::ICRNL;
    ld.set_termios(&t);

    // Feed CR — should be treated as NL and flush edit buffer.
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(0x0D); // CR

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - ICRNL did not flush on CR");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Should get "ab\n" (3 bytes) — CR was converted to NL.
    if n != 3 || buf[2] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - ICRNL mismatch (n={}, b2=0x{:02x})",
            n,
            buf[2]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// IGNCR: CR is discarded entirely when IGNCR is set.
pub fn test_ldisc_igncr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::IGNCR;
    ld.set_termios(&t);

    // Feed CR — should be silently discarded.
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x0D); // CR — should be ignored

    // No newline was delivered, so canonical mode should NOT have flushed.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - IGNCR did not discard CR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// INLCR: NL (0x0A) is mapped to CR (0x0D) when INLCR is set.
pub fn test_ldisc_inlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::INLCR;
    // Disable ICANON so we can inspect raw bytes.
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    ld.input_char(b'\n'); // NL — should become CR
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'\r' {
        klog_info!(
            "TTY_TEST: BUG - INLCR did not map NL to CR (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ISTRIP: bit 7 is stripped from input bytes.
pub fn test_ldisc_istrip() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= InputFlags::ISTRIP;
    t.c_lflag &= !LocalFlags::ICANON;
    ld.set_termios(&t);

    ld.input_char(0xC1); // 0xC1 with bit 7 set -> 0x41 = 'A'
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != 0x41 {
        klog_info!(
            "TTY_TEST: BUG - ISTRIP did not strip bit 7 (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Output processing tests
// ===========================================================================

/// OPOST+ONLCR: NL is converted to CR+NL on output.
pub fn test_ldisc_opost_onlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - ONLCR expected CR+NL, got len={}", len);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress | OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - ONLCR suppressed or tabbed NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// OPOST+OCRNL: CR is converted to NL on output.
pub fn test_ldisc_opost_ocrnl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::OPOST | OutputFlags::OCRNL;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\r') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - OCRNL expected NL, got 0x{:02x}", buf[0]);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress | OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - OCRNL suppressed or tabbed CR");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// No OPOST: bytes pass through unmodified.
pub fn test_ldisc_output_raw() -> TestResult {
    let mut ld = LineDisc::new();
    // Explicitly disable OPOST (default now has OPOST|ONLCR).
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::empty();
    ld.set_termios(&t);

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - raw output modified NL");
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - raw output suppressed NL");
            return TestResult::Fail;
        }
        OutputAction::Tab(_) => {
            klog_info!("TTY_TEST: BUG - raw output produced Tab for NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}
// ===========================================================================
// ECHOCTL tests
// ===========================================================================

/// ECHOCTL: control characters are echoed as ^X.
pub fn test_ldisc_echoctl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::ECHOCTL;
    // Disable ISIG so Ctrl+C is not caught as signal.
    t.c_lflag &= !LocalFlags::ISIG;
    ld.set_termios(&t);

    // Feed Ctrl+C (0x03) — should echo ^C (2 bytes).
    let action = ld.input_char(0x03);
    match action {
        InputAction::Echo { buf, len } => {
            if len != 2 || buf[0] != b'^' || buf[1] != b'C' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOCTL expected ^C, got [{}, {}] len={}",
                    buf[0] as char,
                    buf[1] as char,
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ECHOCTL did not produce Echo for Ctrl+C");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}
// ===========================================================================
// VLNEXT (literal next) tests
// ===========================================================================

/// VLNEXT: Ctrl+V makes the next character literal.
pub fn test_ldisc_vlnext() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);

    // Press Ctrl+V (VLNEXT = 0x16).
    ld.input_char(0x16);

    // Now press Ctrl+C (0x03) — should be inserted literally, not generate signal.
    let action = ld.input_char(0x03);
    match action {
        InputAction::Signal(_) => {
            klog_info!("TTY_TEST: BUG - VLNEXT did not prevent signal");
            return TestResult::Fail;
        }
        _ => {} // Any non-signal action is correct.
    }

    // Flush and read — should contain 0x03 as a literal byte.
    ld.input_char(b'\n');
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Expect: 0x03 + '\n' = 2 bytes.
    if n < 2 || buf[0] != 0x03 {
        klog_info!(
            "TTY_TEST: BUG - VLNEXT literal byte missing (n={}, b0=0x{:02x})",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// VWERASE (word erase) tests
// ===========================================================================

/// VWERASE: Ctrl+W erases the previous word.
pub fn test_ldisc_vwerase() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= LocalFlags::IEXTEN;
    ld.set_termios(&t);

    // Type "hello world".
    for &c in b"hello world" {
        ld.input_char(c);
    }

    // Ctrl+W (VWERASE = 0x17) should erase "world".
    ld.input_char(0x17);

    // Now press Enter — should get "hello \n" (the trailing space stays
    // because word erase only removes the word, not trailing spaces before it).
    ld.input_char(b'\n');
    let mut buf = [0u8; 32];
    let n = ld.read(&mut buf);
    // "hello " + NL = 7 bytes.
    if n != 7 || &buf[..6] != b"hello " {
        klog_info!(
            "TTY_TEST: BUG - VWERASE mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// edit_content() for ReprintLine
// ===========================================================================

/// edit_content returns current edit buffer contents.
pub fn test_ldisc_edit_content() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"hello" {
        ld.input_char(c);
    }
    let content = ld.edit_content();
    if content != b"hello" {
        klog_info!("TTY_TEST: BUG - edit_content mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}
// ===========================================================================
// Legacy Termios Completion (ECHOPRT, IUCLC, OLCUC)
// ===========================================================================

/// ECHOPRT: first erase produces `\` then erased char.
pub fn test_echoprt_erase_format() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOPRT;
    // Disable ECHOE to ensure ECHOPRT path is taken.
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    // Type "abc".
    for &c in b"abc" {
        ld.input_char(c);
    }

    // Erase 'c' — expect `\c` (backslash then the erased char).
    let action = ld.input_char(0x7F); // DEL = VERASE default
    match action {
        InputAction::Echo { buf, len } => {
            if len != 2 || buf[0] != b'\\' || buf[1] != b'c' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOPRT first erase expected \\c, got {:?} len={}",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!(
                "TTY_TEST: BUG - ECHOPRT erase should return Echo, got {:?}",
                action
            );
            return TestResult::Fail;
        }
    }

    // Erase 'b' — continuing sequence, expect just `b` (no leading \\).
    let action = ld.input_char(0x7F);
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'b' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOPRT subsequent erase expected b, got {:?} len={}",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ECHOPRT subsequent erase should return Echo");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// ECHOPRT: non-erase input closes the erase sequence with `/`.
pub fn test_echoprt_close_on_input() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOPRT;
    t.c_lflag &= !LocalFlags::ECHOE;
    ld.set_termios(&t);

    // Type "ab", erase 'b', then type 'x'.
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(0x7F); // erase 'b' → starts erase sequence

    // Type 'x' — should close erase sequence with '/' prepended.
    let action = ld.input_char(b'x');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 2 || buf[0] != b'/' || buf[1] != b'x' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOPRT close expected /x, got {:?} len={}",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ECHOPRT close+insert should return Echo");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// IUCLC maps A-Z to a-z in input.
pub fn test_iuclc_maps_upper_to_lower() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.c_iflag |= InputFlags::IUCLC;
    ld.set_termios(&t);

    // Type 'H' — should be mapped to 'h'.
    let action = ld.input_char(b'H');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'h' {
                klog_info!(
                    "TTY_TEST: BUG - IUCLC should map H→h, got {:?} len={}",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - IUCLC should echo mapped char");
            return TestResult::Fail;
        }
    }

    // Flush and verify the cooked buffer contains 'h'.
    ld.input_char(b'\n');
    let mut buf = [0u8; 8];
    let n = ld.read(&mut buf);
    if n != 2 || buf[0] != b'h' || buf[1] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - IUCLC cooked should be h\\n, got {:?}",
            &buf[..n]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// IUCLC does not affect non-alpha or already-lowercase characters.
pub fn test_iuclc_no_effect_non_alpha() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO;
    t.c_iflag |= InputFlags::IUCLC;
    ld.set_termios(&t);

    // Type 'a' (already lowercase) — should remain 'a'.
    let action = ld.input_char(b'a');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'a' {
                klog_info!("TTY_TEST: BUG - IUCLC should not affect lowercase");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo for lowercase");
            return TestResult::Fail;
        }
    }

    // Type '5' (digit) — should remain '5'.
    let action = ld.input_char(b'5');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'5' {
                klog_info!("TTY_TEST: BUG - IUCLC should not affect digits");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo for digit");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// OLCUC maps a-z to A-Z in output.
pub fn test_olcuc_maps_lower_to_upper() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = OutputFlags::OPOST | OutputFlags::OLCUC;
    ld.set_termios(&t);

    // Process 'h' through output — should become 'H'.
    let action = ld.process_output_byte(b'h');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'H' {
                klog_info!(
                    "TTY_TEST: BUG - OLCUC should map h→H, got {:?} len={}",
                    &buf[..len as usize],
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - OLCUC should return Emit");
            return TestResult::Fail;
        }
    }

    // Process 'Z' (uppercase) — should remain 'Z'.
    let action = ld.process_output_byte(b'Z');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'Z' {
                klog_info!("TTY_TEST: BUG - OLCUC should not affect uppercase");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Emit for uppercase");
            return TestResult::Fail;
        }
    }

    // Process '5' (digit) — should remain '5'.
    let action = ld.process_output_byte(b'5');
    match action {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'5' {
                klog_info!("TTY_TEST: BUG - OLCUC should not affect digits");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Emit for digit");
            return TestResult::Fail;
        }
    }

    TestResult::Pass
}

/// All three flags disabled by default (no effect in default termios).
pub fn test_flags_disabled_by_default() -> TestResult {
    let ld = LineDisc::new();
    let t = ld.termios();

    // ECHOPRT should not be in default c_lflag.
    if t.local_flags().contains(LocalFlags::ECHOPRT) {
        klog_info!("TTY_TEST: BUG - ECHOPRT should not be in default c_lflag");
        return TestResult::Fail;
    }

    // IUCLC should not be in default c_iflag.
    if t.input_flags().contains(InputFlags::IUCLC) {
        klog_info!("TTY_TEST: BUG - IUCLC should not be in default c_iflag");
        return TestResult::Fail;
    }

    // OLCUC should not be in default c_oflag.
    if t.output_flags().contains(OutputFlags::OLCUC) {
        klog_info!("TTY_TEST: BUG - OLCUC should not be in default c_oflag");
        return TestResult::Fail;
    }

    TestResult::Pass
}

slopos_testing::stest!(name = test_ldisc_icrnl, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_igncr, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_inlcr, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_istrip, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_opost_onlcr, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_opost_ocrnl, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_output_raw, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_echoctl, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_vlnext, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_vwerase, suite = tty_test_ldisc_flags);
slopos_testing::stest!(name = test_ldisc_edit_content, suite = tty_test_ldisc_flags);
slopos_testing::stest!(
    name = test_echoprt_erase_format,
    suite = tty_test_ldisc_flags
);
slopos_testing::stest!(
    name = test_echoprt_close_on_input,
    suite = tty_test_ldisc_flags
);
slopos_testing::stest!(
    name = test_iuclc_maps_upper_to_lower,
    suite = tty_test_ldisc_flags
);
slopos_testing::stest!(
    name = test_iuclc_no_effect_non_alpha,
    suite = tty_test_ldisc_flags
);
slopos_testing::stest!(
    name = test_olcuc_maps_lower_to_upper,
    suite = tty_test_ldisc_flags
);
slopos_testing::stest!(
    name = test_flags_disabled_by_default,
    suite = tty_test_ldisc_flags
);
