//! Split from test_ldisc.rs: test_ldisc_utf8.rs

use super::fixtures::*;

// ===========================================================================
// UTF-8 Aware Editing (IUTF8)
// ===========================================================================

/// utf8_char_width: ASCII = 1, CJK = 2, emoji = 2.
pub fn test_utf8_char_width() -> TestResult {
    use crate::tty::ldisc::utf8_char_width;
    if utf8_char_width(b'A' as u32) != 1 {
        klog_info!("TTY_TEST: BUG - ASCII 'A' should be width 1");
        return TestResult::Fail;
    }
    // U+4E2D (中) — CJK Unified Ideograph
    if utf8_char_width(0x4E2D) != 2 {
        klog_info!("TTY_TEST: BUG - CJK U+4E2D should be width 2");
        return TestResult::Fail;
    }
    // U+1F600 (😀) — Emoji
    if utf8_char_width(0x1F600) != 2 {
        klog_info!("TTY_TEST: BUG - Emoji U+1F600 should be width 2");
        return TestResult::Fail;
    }
    // U+00E9 (é) — Latin Extended
    if utf8_char_width(0x00E9) != 1 {
        klog_info!("TTY_TEST: BUG - U+00E9 (é) should be width 1");
        return TestResult::Fail;
    }
    // U+AC00 (가) — Hangul Syllable
    if utf8_char_width(0xAC00) != 2 {
        klog_info!("TTY_TEST: BUG - Hangul U+AC00 should be width 2");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// IUTF8 backspace on ASCII erases 1 byte, clears 1 column.
pub fn test_iutf8_backspace_ascii() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    ld.input_char(b'a');
    ld.input_char(b'b');
    let action = ld.input_char(0x7F); // VERASE = DEL

    // Should erase 1 byte, echo BS-SP-BS.
    let ok = matches!(action, InputAction::Echo { buf, len } if buf[0] == 0x08 && buf[1] == 0x20 && buf[2] == 0x08 && len == 3);
    if !ok {
        klog_info!("TTY_TEST: BUG - IUTF8 backspace on ASCII should produce BS-SP-BS");
        return TestResult::Fail;
    }

    // Edit buffer should contain only 'a'.
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

/// IUTF8 backspace on 2-byte UTF-8 (é = U+00E9 = 0xC3 0xA9) erases 2 bytes,
/// clears 1 column.
pub fn test_iutf8_backspace_2byte() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type 'a' then 'é' (0xC3 0xA9).
    ld.input_char(b'a');
    ld.input_char(0xC3);
    ld.input_char(0xA9);

    // Backspace should erase both bytes of 'é'.
    let action = ld.input_char(0x7F); // VERASE = DEL

    // Width 1 char → single BS-SP-BS.
    let ok = matches!(action, InputAction::Echo { buf, len } if buf[0] == 0x08 && len == 3);
    if !ok {
        klog_info!("TTY_TEST: BUG - IUTF8 backspace on 2-byte char should produce BS-SP-BS");
        return TestResult::Fail;
    }

    // Edit buffer should contain only 'a'.
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

/// IUTF8 backspace on 3-byte CJK (中 = U+4E2D = 0xE4 0xB8 0xAD) erases 3 bytes,
/// clears 2 columns.
pub fn test_iutf8_backspace_3byte_cjk() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type 'a' then '中' (0xE4 0xB8 0xAD).
    ld.input_char(b'a');
    ld.input_char(0xE4);
    ld.input_char(0xB8);
    ld.input_char(0xAD);

    // Backspace should erase all 3 bytes of '中'.
    let action = ld.input_char(0x7F);

    // Width 2 → KillLineEcho { columns: 2 }.
    let ok = matches!(action, InputAction::KillLineEcho { columns: 2 });
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - IUTF8 backspace on CJK should produce KillLineEcho{{columns:2}}"
        );
        return TestResult::Fail;
    }

    // Edit buffer should contain only 'a'.
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

/// IUTF8 backspace on 4-byte emoji (😀 = U+1F600 = 0xF0 0x9F 0x98 0x80)
/// erases 4 bytes, clears 2 columns.
pub fn test_iutf8_backspace_4byte_emoji() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type emoji 😀 (0xF0 0x9F 0x98 0x80).
    ld.input_char(0xF0);
    ld.input_char(0x9F);
    ld.input_char(0x98);
    ld.input_char(0x80);

    // Backspace.
    let action = ld.input_char(0x7F);

    // Width 2 → KillLineEcho { columns: 2 }.
    let ok = matches!(action, InputAction::KillLineEcho { columns: 2 });
    if !ok {
        klog_info!(
            "TTY_TEST: BUG - IUTF8 backspace on 4-byte emoji should produce KillLineEcho{{columns:2}}"
        );
        return TestResult::Fail;
    }

    // Edit buffer should be empty.
    if !ld.edit_content().is_empty() {
        klog_info!("TTY_TEST: BUG - edit buffer should be empty after erasing emoji");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Without IUTF8, backspace on multi-byte erases only 1 byte (legacy).
pub fn test_no_iutf8_backspace_multibyte() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE;
    // Explicitly do NOT set IUTF8.
    t.c_iflag &= !InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type é (0xC3 0xA9).
    ld.input_char(0xC3);
    ld.input_char(0xA9);

    // Backspace — should erase only 1 byte (legacy behavior).
    ld.input_char(0x7F);

    // Edit buffer should have 1 byte remaining (0xC3).
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

/// IUTF8 column tracking: inserting a 2-byte char adds 1 column,
/// inserting a 3-byte CJK adds 2 columns.
pub fn test_iutf8_insert_column_tracking() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHOE | LocalFlags::ECHOKE;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Insert 'a' (col=1), 'é' (col=2), '中' (col=4).
    ld.input_char(b'a'); // column: 1
    ld.input_char(0xC3); // leading byte of é — no column yet
    ld.input_char(0xA9); // completes é — column: 2
    ld.input_char(0xE4); // leading byte of 中 — no column yet
    ld.input_char(0xB8); // continuation — no column yet
    ld.input_char(0xAD); // completes 中 — column: 4

    // Kill line should report 4 columns.
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

/// IUTF8 word erase on mixed ASCII + UTF-8 content.
pub fn test_iutf8_word_erase_mixed() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::IEXTEN;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type "hello中" — 'hello' is 5 ASCII bytes, '中' is 3 UTF-8 bytes (non-word).
    for &b in b"hello" {
        ld.input_char(b);
    }
    ld.input_char(0xE4); // 中
    ld.input_char(0xB8);
    ld.input_char(0xAD);

    // Ctrl+W (word erase): should erase '中' (non-word) then 'hello' (word).
    let action = ld.input_char(0x17); // VWERASE = Ctrl+W

    // Edit buffer should be empty.
    if !ld.edit_content().is_empty() {
        klog_info!(
            "TTY_TEST: BUG - word erase should clear all, got {} bytes left",
            ld.edit_content().len()
        );
        return TestResult::Fail;
    }

    // Should produce a ReprintLine (multi-char erase with ECHO).
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

/// IUTF8 word erase preserves preceding content.
pub fn test_iutf8_word_erase_preserves_prefix() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag = LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::IEXTEN;
    t.c_iflag |= InputFlags::IUTF8;
    ld.set_termios(&t);

    // Type "ab cd".
    for &b in b"ab cd" {
        ld.input_char(b);
    }

    // Ctrl+W — should erase 'cd' (word) and ' ' (trailing non-word before it).
    // Wait — POSIX word erase: skip trailing non-word, then erase word.
    // Buffer: 'a','b',' ','c','d'
    // Skip trailing non-word: 'd' is word, so nothing skipped.
    // Erase word chars: 'd','c' erased. Stop at ' '.
    ld.input_char(0x17);

    // Edit buffer should be "ab ".
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

/// IUTF8 flag constant is 0x4000.
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
