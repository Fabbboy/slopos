//! Line-discipline tests for IUTF8 editing: character width, multi-byte erase,
//! word erase and column tracking.

use super::fixtures::*;

pub fn test_utf8_char_width() -> TestResult {
    use crate::tty::ldisc::utf8_char_width;
    if utf8_char_width(b'A' as u32) != 1 {
        klog_info!("TTY_TEST: BUG - ASCII 'A' should be width 1");
        return TestResult::Fail;
    }
    if utf8_char_width(0x4E2D) != 2 {
        klog_info!("TTY_TEST: BUG - CJK U+4E2D should be width 2");
        return TestResult::Fail;
    }
    if utf8_char_width(0x1F600) != 2 {
        klog_info!("TTY_TEST: BUG - Emoji U+1F600 should be width 2");
        return TestResult::Fail;
    }
    if utf8_char_width(0x00E9) != 1 {
        klog_info!("TTY_TEST: BUG - U+00E9 (é) should be width 1");
        return TestResult::Fail;
    }
    if utf8_char_width(0xAC00) != 2 {
        klog_info!("TTY_TEST: BUG - Hangul U+AC00 should be width 2");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_iutf8_backspace_ascii() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b'b');
    let action = ld.input_char(0x7F);

    let ok = matches!(action, InputAction::Echo { buf, len } if buf[0] == 0x08 && buf[1] == 0x20 && buf[2] == 0x08 && len == 3);
    if !ok {
        klog_info!("TTY_TEST: BUG - IUTF8 backspace on ASCII should produce BS-SP-BS");
        return TestResult::Fail;
    }

    let content = ld.edit_content();
    if content != b"a" {
        klog_info!(
            "TTY_TEST: BUG - edit buffer should be [a], got {:?}",
            content
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// é = U+00E9 = 0xC3 0xA9: erasing it removes both bytes and one column.
pub fn test_iutf8_backspace_2byte() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(0xC3);
    ld.input_char(0xA9);

    let action = ld.input_char(0x7F);

    let ok = matches!(action, InputAction::Echo { buf, len } if buf[0] == 0x08 && len == 3);
    if !ok {
        klog_info!("TTY_TEST: BUG - IUTF8 backspace on 2-byte char should produce BS-SP-BS");
        return TestResult::Fail;
    }

    let content = ld.edit_content();
    if content != b"a" {
        klog_info!(
            "TTY_TEST: BUG - expected [a] after erasing 2-byte char, got {} bytes",
            content.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// 中 = U+4E2D = 0xE4 0xB8 0xAD: erasing it removes three bytes and two columns.
pub fn test_iutf8_backspace_3byte_cjk() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(0xE4);
    ld.input_char(0xB8);
    ld.input_char(0xAD);

    let action = ld.input_char(0x7F);

    let ok = matches!(action, InputAction::KillLineEcho { columns: 2 });
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - IUTF8 backspace on CJK should produce KillLineEcho{{columns:2}}"
        );
        return TestResult::Fail;
    }

    let content = ld.edit_content();
    if content != b"a" {
        klog_info!(
            "TTY_TEST: BUG - expected [a] after erasing CJK char, got {} bytes",
            content.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// 😀 = U+1F600 = 0xF0 0x9F 0x98 0x80: erasing it removes four bytes and two columns.
pub fn test_iutf8_backspace_4byte_emoji() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    ld.input_char(0xF0);
    ld.input_char(0x9F);
    ld.input_char(0x98);
    ld.input_char(0x80);

    let action = ld.input_char(0x7F);

    let ok = matches!(action, InputAction::KillLineEcho { columns: 2 });
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - IUTF8 backspace on 4-byte emoji should produce KillLineEcho{{columns:2}}"
        );
        return TestResult::Fail;
    }

    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after erasing emoji");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_no_iutf8_backspace_multibyte() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag &= !InputFlags::IUTF8;
    ld.set_termios(&t);

    // é = 0xC3 0xA9.
    ld.input_char(0xC3);
    ld.input_char(0xA9);

    ld.input_char(0x7F);

    let content = ld.edit_content();
    if content.len() != 1 || content[0] != 0xC3 {
        klog_info!(
            "TTY_TEST: BUG - without IUTF8, backspace should erase 1 byte, got {} bytes",
            content.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// A 2-byte char adds one column, a 3-byte CJK adds two.
pub fn test_iutf8_insert_column_tracking() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE | LocalFlags::ECHOKE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Insert 'a' (col=1), 'é' (col=2), '中' (col=4).
    ld.input_char(b'a');
    ld.input_char(0xC3);
    ld.input_char(0xA9);
    ld.input_char(0xE4);
    ld.input_char(0xB8);
    ld.input_char(0xAD);

    let action = ld.input_char(0x15); // VKILL = Ctrl+U
    let ok = matches!(action, InputAction::KillLineEcho { columns: 4 });
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - expected KillLineEcho{{columns:4}}, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_iutf8_word_erase_mixed() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::IEXTEN;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // "hello" then 中 (0xE4 0xB8 0xAD), which counts as a non-word char.
    for &b in b"hello" {
        ld.input_char(b);
    }
    ld.input_char(0xE4);
    ld.input_char(0xB8);
    ld.input_char(0xAD);

    let action = ld.input_char(0x17);

    if !ld.edit_content().is_empty() {
        klog_info!(
            "TTY_TEST: BUG - word erase should clear all, got {} bytes left",
            ld.edit_content().len()
        );
        return TestResult::Fail;
    }

    // A multi-char erase under ECHO reprints instead of echoing BS-SP-BS.
    let ok = matches!(action, InputAction::ReprintLine);
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - word erase should produce ReprintLine, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_iutf8_word_erase_preserves_prefix() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::IEXTEN;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    for &b in b"ab cd" {
        ld.input_char(b);
    }

    // POSIX word erase skips trailing non-word chars, then erases one word,
    // so "cd" goes and the separating space stays.
    ld.input_char(0x17);

    let content = ld.edit_content();
    if content != b"ab " {
        klog_info!(
            "TTY_TEST: BUG - expected 'ab ' after word erase, got {} bytes",
            content.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_iutf8_flag_value() -> TestResult {
    if InputFlags::IUTF8.bits() != 0x4000 {
        klog_info!("TTY_TEST: BUG - IUTF8 should be 0x4000");
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_utf8_char_width, suite = tty_test_ldisc_utf8);
slopos_testing::stest!(
    name = test_iutf8_backspace_ascii,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_backspace_2byte,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_backspace_3byte_cjk,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_backspace_4byte_emoji,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_no_iutf8_backspace_multibyte,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_insert_column_tracking,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_word_erase_mixed,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(
    name = test_iutf8_word_erase_preserves_prefix,
    suite = tty_test_ldisc_utf8
);
slopos_testing::stest!(name = test_iutf8_flag_value, suite = tty_test_ldisc_utf8);
