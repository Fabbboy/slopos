//! Split from test_ldisc.rs: test_vtparser.rs

use super::fixtures::*;

// ===========================================================================
// VT100/ANSI Terminal Emulation
// ===========================================================================

/// Parser starts in ground state, printable ASCII → Print.
pub fn test_parser_print_ascii() -> TestResult {
    let mut parser = VtParser::new();
    let action = parser.advance(b'A');
    if action != VtAction::Print(b'A' as u32) {
        klog_info!("TTY_TEST: BUG - expected Print('A'), got {:?}", action);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Control characters produce Execute.
pub fn test_parser_execute_control() -> TestResult {
    let mut parser = VtParser::new();
    for &ctrl in &[b'\n', b'\r', 0x08u8, b'\t', 0x07] {
        let action = parser.advance(ctrl);
        if action != VtAction::Execute(ctrl) {
            klog_info!(
                "TTY_TEST: BUG - expected Execute(0x{:02x}), got {:?}",
                ctrl,
                action
            );
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// ESC [ 2 J → EraseDisplay(All).
pub fn test_clear_screen() -> TestResult {
    let mut parser = VtParser::new();
    // Feed ESC [ 2 J
    let _ = parser.advance(0x1B);
    let _ = parser.advance(b'[');
    let _ = parser.advance(b'2');
    let action = parser.advance(b'J');
    if action != VtAction::EraseDisplay(EraseMode::All) {
        klog_info!(
            "TTY_TEST: BUG - expected EraseDisplay(All), got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ESC [ 10 ; 20 H → SetCursorPos { row: 9, col: 19 } (0-based).
pub fn test_cursor_position() -> TestResult {
    let mut parser = VtParser::new();
    for &b in b"\x1b[10;20H" {
        let action = parser.advance(b);
        if b == b'H' {
            if action != (VtAction::SetCursorPos { row: 9, col: 19 }) {
                klog_info!(
                    "TTY_TEST: BUG - expected SetCursorPos(9,19), got {:?}",
                    action
                );
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

/// ESC [ 31 m → SetAttribute(ForegroundColor(1)) (red).
pub fn test_sgr_red_foreground() -> TestResult {
    let mut parser = VtParser::new();
    for &b in b"\x1b[31m" {
        let action = parser.advance(b);
        if b == b'm' {
            if action != VtAction::SetAttribute(SgrAttr::ForegroundColor(1)) {
                klog_info!(
                    "TTY_TEST: BUG - expected ForegroundColor(1), got {:?}",
                    action
                );
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

/// ESC [ 0 m → SetAttribute(Reset).
pub fn test_sgr_reset() -> TestResult {
    let mut parser = VtParser::new();
    for &b in b"\x1b[0m" {
        let action = parser.advance(b);
        if b == b'm' {
            if action != VtAction::SetAttribute(SgrAttr::Reset) {
                klog_info!("TTY_TEST: BUG - expected Reset, got {:?}", action);
                return TestResult::Fail;
            }
        }
    }
    TestResult::Pass
}

/// ESC [ A → MoveCursor { Up, 1 }.
pub fn test_cursor_up() -> TestResult {
    let mut parser = VtParser::new();
    let _ = parser.advance(0x1B);
    let _ = parser.advance(b'[');
    let action = parser.advance(b'A');
    if action
        != (VtAction::MoveCursor {
            direction: Direction::Up,
            count: 1,
        })
    {
        klog_info!("TTY_TEST: BUG - expected MoveCursor Up 1, got {:?}", action);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Malformed sequences return to ground without crash.
pub fn test_malformed_sequence_resilience() -> TestResult {
    let mut parser = VtParser::new();
    // Incomplete ESC [ then immediately a printable char
    let _ = parser.advance(0x1B);
    let _ = parser.advance(b'[');
    // Feed a high byte (invalid in CSI param) → should abort to ground
    let action = parser.advance(0xFF);
    if action != VtAction::Nop {
        klog_info!(
            "TTY_TEST: BUG - expected Nop on malformed, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    // Parser should be back in Ground — next printable should work
    let action = parser.advance(b'X');
    if action != VtAction::Print(b'X' as u32) {
        klog_info!(
            "TTY_TEST: BUG - expected Print('X') after malformed, got {:?}",
            action
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Multi-param SGR (e.g. ESC[1;31m) queues both actions.
pub fn test_sgr_multi_param() -> TestResult {
    let mut parser = VtParser::new();
    // Feed ESC [ 1 ; 31 m → Bold + ForegroundColor(1)
    for &b in b"\x1b[1;31" {
        let _ = parser.advance(b);
    }
    let first = parser.advance(b'm');
    if first != VtAction::SetAttribute(SgrAttr::Bold) {
        klog_info!("TTY_TEST: BUG - expected Bold, got {:?}", first);
        return TestResult::Fail;
    }
    // The second SGR action is pending — drain it by advancing any byte
    // (parser returns pending before processing new byte).
    let second = parser.advance(b'A');
    if second != VtAction::SetAttribute(SgrAttr::ForegroundColor(1)) {
        klog_info!(
            "TTY_TEST: BUG - expected ForegroundColor(1), got {:?}",
            second
        );
        return TestResult::Fail;
    }
    // After pending is drained, the 'A' should produce Print('A').
    let third = parser.advance(b'B');
    if third != VtAction::Print(b'B' as u32) {
        klog_info!("TTY_TEST: BUG - expected Print('B'), got {:?}", third);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VConsoleState processes ESC[2J to clear screen.
pub fn test_vconsole_clear_screen() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Write some chars first.
    state.process_byte(b'H');
    state.process_byte(b'i');
    if state.cells.get(0, 0).codepoint != b'H' as u32
        || state.cells.get(0, 1).codepoint != b'i' as u32
    {
        klog_info!("TTY_TEST: BUG - chars not written");
        return TestResult::Fail;
    }
    // Send ESC[2J to clear.
    for &b in b"\x1b[2J" {
        state.process_byte(b);
    }
    if state.cells.get(0, 0).codepoint != b' ' as u32
        || state.cells.get(0, 1).codepoint != b' ' as u32
    {
        klog_info!("TTY_TEST: BUG - screen not cleared");
        return TestResult::Fail;
    }
    if state.cursor_row != 0 || state.cursor_col != 0 {
        klog_info!("TTY_TEST: BUG - cursor not at origin after clear");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VConsoleState processes ESC[10;20H to move cursor.
pub fn test_vconsole_cursor_pos() -> TestResult {
    let mut state = boxed_vconsole_state();
    for &b in b"\x1b[10;20H" {
        state.process_byte(b);
    }
    if state.cursor_row != 9 || state.cursor_col != 19 {
        klog_info!(
            "TTY_TEST: BUG - cursor at ({},{}) expected (9,19)",
            state.cursor_row,
            state.cursor_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SGR red foreground changes cursor_attrs.fg.
pub fn test_vconsole_sgr_color() -> TestResult {
    let mut state = boxed_vconsole_state();
    // ESC[31m = red foreground
    for &b in b"\x1b[31m" {
        state.process_byte(b);
    }
    // Red = ANSI_COLORS[1] = 0x00AA0000
    if state.cursor_attrs.fg != 0x00AA0000 {
        klog_info!(
            "TTY_TEST: BUG - fg is 0x{:08x}, expected 0x00AA0000",
            state.cursor_attrs.fg
        );
        return TestResult::Fail;
    }
    // Write a char and verify cell attrs
    state.process_byte(b'X');
    if state.cells.get(0, 0).attrs.fg != 0x00AA0000 {
        klog_info!(
            "TTY_TEST: BUG - cell fg is 0x{:08x}, expected 0x00AA0000",
            state.cells.get(0, 0).attrs.fg
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// SGR reset restores default colors.
pub fn test_vconsole_sgr_reset() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Set red fg, then reset
    for &b in b"\x1b[31m" {
        state.process_byte(b);
    }
    for &b in b"\x1b[0m" {
        state.process_byte(b);
    }
    if state.cursor_attrs.fg != 0x00AAAAAA {
        klog_info!(
            "TTY_TEST: BUG - fg not reset: 0x{:08x}",
            state.cursor_attrs.fg
        );
        return TestResult::Fail;
    }
    if state.cursor_attrs.bold || state.cursor_attrs.inverse || state.cursor_attrs.underline {
        klog_info!("TTY_TEST: BUG - attrs not reset");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Save/restore cursor (ESC 7 / ESC 8).
pub fn test_vconsole_save_restore_cursor() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Move to (5,10)
    for &b in b"\x1b[6;11H" {
        state.process_byte(b);
    }
    // Save cursor
    state.process_byte(0x1B);
    state.process_byte(b'7');
    // Move elsewhere
    for &b in b"\x1b[1;1H" {
        state.process_byte(b);
    }
    if state.cursor_row != 0 || state.cursor_col != 0 {
        klog_info!("TTY_TEST: BUG - cursor not at (0,0)");
        return TestResult::Fail;
    }
    // Restore cursor
    state.process_byte(0x1B);
    state.process_byte(b'8');
    if state.cursor_row != 5 || state.cursor_col != 10 {
        klog_info!(
            "TTY_TEST: BUG - cursor at ({},{}) expected (5,10)",
            state.cursor_row,
            state.cursor_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Fuzz the parser with random-ish byte sequences — no panic.
pub fn test_parser_fuzz_no_panic() -> TestResult {
    let mut parser = VtParser::new();
    // Feed every possible byte value through the parser.
    for b in 0u8..=255 {
        let _ = parser.advance(b);
    }
    // Feed typical escape sequences interleaved with garbage.
    let fuzz: &[u8] = b"\x1b[999;999H\x1b[38;5;200m\xff\x00\x1b]garbage\x07\x1b[?25l";
    for &b in fuzz {
        let _ = parser.advance(b);
    }
    // If we get here without panic, pass.
    TestResult::Pass
}

/// Erase line (EL) modes work correctly.
pub fn test_vconsole_erase_line() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Write "ABCDE" at row 0
    for &b in b"ABCDE" {
        state.process_byte(b);
    }
    // Move cursor to col 2 (ESC[1;3H = row 0, col 2)
    for &b in b"\x1b[1;3H" {
        state.process_byte(b);
    }
    // Erase to end of line (ESC[K or ESC[0K)
    for &b in b"\x1b[K" {
        state.process_byte(b);
    }
    // Cols 0,1 should still have A,B; cols 2+ should be spaces
    if state.cells.get(0, 0).codepoint != b'A' as u32
        || state.cells.get(0, 1).codepoint != b'B' as u32
    {
        klog_info!("TTY_TEST: BUG - A/B were erased");
        return TestResult::Fail;
    }
    if state.cells.get(0, 2).codepoint != b' ' as u32
        || state.cells.get(0, 3).codepoint != b' ' as u32
        || state.cells.get(0, 4).codepoint != b' ' as u32
    {
        klog_info!("TTY_TEST: BUG - cols 2-4 not cleared");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Cursor movement clamping (ESC[A at row 0 stays at row 0).
pub fn test_cursor_movement_clamping() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Cursor starts at (0,0), move up — should stay at 0
    for &b in b"\x1b[5A" {
        state.process_byte(b);
    }
    if state.cursor_row != 0 {
        klog_info!(
            "TTY_TEST: BUG - cursor_row is {}, expected 0",
            state.cursor_row
        );
        return TestResult::Fail;
    }
    // Move left at col 0 — should stay at 0
    for &b in b"\x1b[5D" {
        state.process_byte(b);
    }
    if state.cursor_col != 0 {
        klog_info!(
            "TTY_TEST: BUG - cursor_col is {}, expected 0",
            state.cursor_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Scroll up via ESC[S.
pub fn test_vconsole_scroll_up() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Write 'A' at row 0 col 0, 'B' at row 1 col 0
    state.process_byte(b'A');
    for &b in b"\x1b[2;1H" {
        state.process_byte(b);
    }
    state.process_byte(b'B');
    // Scroll up 1
    for &b in b"\x1b[1S" {
        state.process_byte(b);
    }
    // Row 0 should now have 'B' (shifted from row 1)
    if state.cells.get(0, 0).codepoint != b'B' as u32 {
        klog_info!(
            "TTY_TEST: BUG - row 0 col 0 is '{}', expected 'B'",
            char::from_u32(state.cells.get(0, 0).codepoint).unwrap_or('?')
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_parser_print_ascii, suite = tty_test_vtparser);
slopos_testing::stest!(
    name = test_parser_execute_control,
    suite = tty_test_vtparser
);
slopos_testing::stest!(name = test_clear_screen, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_cursor_position, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_sgr_red_foreground, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_sgr_reset, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_cursor_up, suite = tty_test_vtparser);
slopos_testing::stest!(
    name = test_malformed_sequence_resilience,
    suite = tty_test_vtparser
);
slopos_testing::stest!(name = test_sgr_multi_param, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_vconsole_clear_screen, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_vconsole_cursor_pos, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_vconsole_sgr_color, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_vconsole_sgr_reset, suite = tty_test_vtparser);
slopos_testing::stest!(
    name = test_vconsole_save_restore_cursor,
    suite = tty_test_vtparser
);
slopos_testing::stest!(name = test_parser_fuzz_no_panic, suite = tty_test_vtparser);
slopos_testing::stest!(name = test_vconsole_erase_line, suite = tty_test_vtparser);
slopos_testing::stest!(
    name = test_cursor_movement_clamping,
    suite = tty_test_vtparser
);
slopos_testing::stest!(name = test_vconsole_scroll_up, suite = tty_test_vtparser);
