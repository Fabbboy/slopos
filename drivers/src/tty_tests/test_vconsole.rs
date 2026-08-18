use super::*;

pub fn test_utf8_2byte_renders_codepoint() -> TestResult {
    let mut parser = VtParser::new();
    // é = U+00E9 = 0xC3 0xA9
    let a1 = parser.advance(0xC3);
    if a1 != VtAction::Nop {
        return TestResult::Fail;
    }
    let a2 = parser.advance(0xA9);
    if a2 != VtAction::Print(0x00E9) {
        klog_info!("TTY_TEST: expected Print(0xE9), got {:?}", a2);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_utf8_3byte_renders_codepoint() -> TestResult {
    let mut parser = VtParser::new();
    // 中 = U+4E2D = 0xE4 0xB8 0xAD
    let a1 = parser.advance(0xE4);
    let a2 = parser.advance(0xB8);
    let a3 = parser.advance(0xAD);
    if a1 != VtAction::Nop || a2 != VtAction::Nop {
        return TestResult::Fail;
    }
    if a3 != VtAction::Print(0x4E2D) {
        klog_info!("TTY_TEST: expected Print(0x4E2D), got {:?}", a3);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_utf8_4byte_renders_codepoint() -> TestResult {
    let mut parser = VtParser::new();
    // 😀 = U+1F600 = 0xF0 0x9F 0x98 0x80
    let _ = parser.advance(0xF0);
    let _ = parser.advance(0x9F);
    let _ = parser.advance(0x98);
    let a4 = parser.advance(0x80);
    if a4 != VtAction::Print(0x1F600) {
        klog_info!("TTY_TEST: expected Print(0x1F600), got {:?}", a4);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_utf8_invalid_byte_emits_replacement() -> TestResult {
    let mut parser = VtParser::new();
    let a = parser.advance(0xFF);
    if a != VtAction::Print(0xFFFD) {
        klog_info!("TTY_TEST: expected replacement char, got {:?}", a);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_utf8_truncated_sequence_emits_replacement() -> TestResult {
    let mut parser = VtParser::new();
    let a1 = parser.advance(0xC3);
    if a1 != VtAction::Nop {
        return TestResult::Fail;
    }
    let a2 = parser.advance(b'A');
    if a2 != VtAction::Print(0xFFFD) {
        klog_info!("TTY_TEST: expected replacement, got {:?}", a2);
        return TestResult::Fail;
    }
    let a3 = parser.advance(0); // Dummy byte to drain the pending queue.
    if a3 != VtAction::Print(b'A' as u32) {
        klog_info!("TTY_TEST: expected re-processed 'A', got {:?}", a3);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_utf8_overlong_rejected() -> TestResult {
    let mut parser = VtParser::new();
    // 0xC0 = lead byte of the overlong encoding of '/' (U+002F).
    let a = parser.advance(0xC0);
    if a != VtAction::Print(0xFFFD) {
        klog_info!("TTY_TEST: expected replacement for 0xC0, got {:?}", a);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ascii_still_works() -> TestResult {
    let mut parser = VtParser::new();
    let a = parser.advance(b'Z');
    if a != VtAction::Print(b'Z' as u32) {
        klog_info!("TTY_TEST: ASCII 'Z' broken, got {:?}", a);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sgr_256_foreground() -> TestResult {
    let mut p = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[38;5;196m" {
        let a = p.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    if last != VtAction::SetAttribute(SgrAttr::Foreground256(196)) {
        klog_info!("TTY_TEST: expected Foreground256(196), got {:?}", last);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sgr_256_background() -> TestResult {
    let mut p = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[48;5;232m" {
        let a = p.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    if last != VtAction::SetAttribute(SgrAttr::Background256(232)) {
        klog_info!("TTY_TEST: expected Background256(232), got {:?}", last);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sgr_truecolor_foreground() -> TestResult {
    let mut p = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[38;2;255;128;0m" {
        let a = p.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    if last != VtAction::SetAttribute(SgrAttr::ForegroundRgb(255, 128, 0)) {
        klog_info!(
            "TTY_TEST: expected ForegroundRgb(255,128,0), got {:?}",
            last
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sgr_truecolor_background() -> TestResult {
    let mut p = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[48;2;10;20;30m" {
        let a = p.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    if last != VtAction::SetAttribute(SgrAttr::BackgroundRgb(10, 20, 30)) {
        klog_info!("TTY_TEST: expected BackgroundRgb(10,20,30), got {:?}", last);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vconsole_256_color_sets_fg() -> TestResult {
    let mut state = boxed_vconsole_state();
    for &b in b"\x1b[38;5;1m" {
        state.process_byte(b);
    }
    state.process_byte(b'X');
    // 256-colour index 1 = ANSI red = 0x00AA0000
    if state.cells.get(0, 0).attrs.fg != 0x00AA0000 {
        klog_info!(
            "TTY_TEST: expected fg 0x00AA0000, got 0x{:08x}",
            state.cells.get(0, 0).attrs.fg
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vconsole_truecolor_sets_fg() -> TestResult {
    let mut state = boxed_vconsole_state();
    for &b in b"\x1b[38;2;255;128;0m" {
        state.process_byte(b);
    }
    state.process_byte(b'Y');
    let expected: u32 = (255 << 16) | (128 << 8) | 0;
    if state.cells.get(0, 0).attrs.fg != expected {
        klog_info!(
            "TTY_TEST: expected fg 0x{:08x}, got 0x{:08x}",
            expected,
            state.cells.get(0, 0).attrs.fg
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_bracketed_paste_enable_disable() -> TestResult {
    let mut p = VtParser::new();
    if p.bracketed_paste {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?2004h" {
        p.advance(b);
    }
    if !p.bracketed_paste {
        klog_info!("TTY_TEST: bracketed paste not enabled");
        return TestResult::Fail;
    }
    for &b in b"\x1b[?2004l" {
        p.advance(b);
    }
    if p.bracketed_paste {
        klog_info!("TTY_TEST: bracketed paste not disabled");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_decawm_default_on() -> TestResult {
    let p = VtParser::new();
    if !p.auto_wrap {
        klog_info!("TTY_TEST: auto_wrap should default to true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_decawm_toggle() -> TestResult {
    let mut p = VtParser::new();
    for &b in b"\x1b[?7l" {
        p.advance(b);
    }
    if p.auto_wrap {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?7h" {
        p.advance(b);
    }
    if !p.auto_wrap {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_decckm_toggle() -> TestResult {
    let mut p = VtParser::new();
    if p.cursor_key_mode {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?1h" {
        p.advance(b);
    }
    if !p.cursor_key_mode {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?1l" {
        p.advance(b);
    }
    if p.cursor_key_mode {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_decom_toggle() -> TestResult {
    let mut p = VtParser::new();
    if p.origin_mode {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?6h" {
        p.advance(b);
    }
    if !p.origin_mode {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?6l" {
        p.advance(b);
    }
    if p.origin_mode {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_dectcem_still_works() -> TestResult {
    let mut state = boxed_vconsole_state();
    if !state.cursor_visible {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?25l" {
        state.process_byte(b);
    }
    if state.cursor_visible {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?25h" {
        state.process_byte(b);
    }
    if !state.cursor_visible {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_alt_screen_still_works() -> TestResult {
    let mut state = boxed_vconsole_state();
    state.process_byte(b'Z');
    if state.cells.get(0, 0).codepoint != b'Z' as u32 {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?1049h" {
        state.process_byte(b);
    }
    if !state.in_alt_screen || state.cells.get(0, 0).codepoint != b' ' as u32 {
        return TestResult::Fail;
    }
    for &b in b"\x1b[?1049l" {
        state.process_byte(b);
    }
    if state.in_alt_screen || state.cells.get(0, 0).codepoint != b'Z' as u32 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_cell_model_u32() -> TestResult {
    let state = boxed_vconsole_state();
    if state.cells.get(0, 0).codepoint != 0x20 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_vconsole_utf8_hello_renders() -> TestResult {
    let mut state = boxed_vconsole_state();
    // "Héllo": é = U+00E9 = 0xC3 0xA9
    for &b in b"H\xc3\xa9llo" {
        state.process_byte(b);
    }
    if state.cells.get(0, 0).codepoint != b'H' as u32 {
        return TestResult::Fail;
    }
    if state.cells.get(0, 1).codepoint != 0x00E9 {
        klog_info!(
            "TTY_TEST: cell[0][1] = 0x{:04x}, expected 0x00E9",
            state.cells.get(0, 1).codepoint
        );
        return TestResult::Fail;
    }
    if state.cells.get(0, 2).codepoint != b'l' as u32
        || state.cells.get(0, 3).codepoint != b'l' as u32
    {
        return TestResult::Fail;
    }
    if state.cells.get(0, 4).codepoint != b'o' as u32 {
        return TestResult::Fail;
    }
    if state.cursor_col != 5 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_double_width_cjk() -> TestResult {
    let mut state = boxed_vconsole_state();
    // 中 = U+4E2D = 0xE4 0xB8 0xAD
    for &b in &[0xE4u8, 0xB8, 0xAD] {
        state.process_byte(b);
    }
    if state.cells.get(0, 0).codepoint != 0x4E2D {
        klog_info!(
            "TTY_TEST: cell[0][0] = 0x{:04x}, expected 0x4E2D",
            state.cells.get(0, 0).codepoint
        );
        return TestResult::Fail;
    }
    // 0xFFFF_FFFF = double-width continuation marker.
    if state.cells.get(0, 1).codepoint != 0xFFFF_FFFF {
        klog_info!(
            "TTY_TEST: cell[0][1] = 0x{:08x}, expected continuation",
            state.cells.get(0, 1).codepoint
        );
        return TestResult::Fail;
    }
    if state.cursor_col != 2 {
        klog_info!("TTY_TEST: cursor_col = {}, expected 2", state.cursor_col);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_invalid_utf8_in_vconsole() -> TestResult {
    let mut state = boxed_vconsole_state();
    state.process_byte(0xFF);
    if state.cells.get(0, 0).codepoint != 0xFFFD {
        klog_info!(
            "TTY_TEST: cell[0][0] = 0x{:04x}, expected 0xFFFD",
            state.cells.get(0, 0).codepoint
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_mixed_ascii_utf8_escapes() -> TestResult {
    let mut state = boxed_vconsole_state();
    // 0xC3 0xA9 = é; fg 0x00AA0000 = red, 0x00AAAAAA = default.
    state.process_byte(b'A');
    for &b in b"\x1b[31m" {
        state.process_byte(b);
    }
    for &b in &[0xC3u8, 0xA9] {
        state.process_byte(b);
    }
    for &b in b"\x1b[0m" {
        state.process_byte(b);
    }
    state.process_byte(b'B');

    if state.cells.get(0, 0).codepoint != b'A' as u32 {
        return TestResult::Fail;
    }
    if state.cells.get(0, 1).codepoint != 0xE9 {
        return TestResult::Fail;
    }
    if state.cells.get(0, 1).attrs.fg != 0x00AA0000 {
        klog_info!(
            "TTY_TEST: é fg = 0x{:08x}, expected red",
            state.cells.get(0, 1).attrs.fg
        );
        return TestResult::Fail;
    }
    if state.cells.get(0, 2).codepoint != b'B' as u32 {
        return TestResult::Fail;
    }
    if state.cells.get(0, 2).attrs.fg != 0x00AAAAAA {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_256color_cube_mapping() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Cube index 21 = 16 + 36*0 + 6*0 + 5 → b = 5*51 = 255, g = 0, r = 0.
    for &b in b"\x1b[38;5;21m" {
        state.process_byte(b);
    }
    state.process_byte(b'X');
    let expected_21: u32 = 0x000000FF; // blue
    if state.cells.get(0, 0).attrs.fg != expected_21 {
        klog_info!(
            "TTY_TEST: index 21 fg = 0x{:08x}, expected 0x{:08x}",
            state.cells.get(0, 0).attrs.fg,
            expected_21
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_256color_grayscale_mapping() -> TestResult {
    let mut state = boxed_vconsole_state();
    // Index 232 = first grayscale = 8 + 10*0 = 8 → rgb(8,8,8).
    for &b in b"\x1b[38;5;232m" {
        state.process_byte(b);
    }
    state.process_byte(b'G');
    let v: u32 = 8;
    let expected = (v << 16) | (v << 8) | v;
    if state.cells.get(0, 0).attrs.fg != expected {
        klog_info!(
            "TTY_TEST: grayscale 232 fg = 0x{:08x}, expected 0x{:08x}",
            state.cells.get(0, 0).attrs.fg,
            expected
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_is_double_width_ranges() -> TestResult {
    use slopos_abi::unicode::is_double_width;
    // CJK Unified Ideographs
    if !is_double_width(0x4E2D) {
        return TestResult::Fail;
    }
    // Hangul Syllables
    if !is_double_width(0xAC00) {
        return TestResult::Fail;
    }
    // Fullwidth Latin A
    if !is_double_width(0xFF21) {
        return TestResult::Fail;
    }
    // ASCII 'A'
    if is_double_width(0x41) {
        return TestResult::Fail;
    }
    // Latin é
    if is_double_width(0xE9) {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_sgr_standard_colors_unaffected() -> TestResult {
    let mut p = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[31m" {
        let a = p.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    if last != VtAction::SetAttribute(SgrAttr::ForegroundColor(1)) {
        klog_info!("TTY_TEST: standard red fg broken, got {:?}", last);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_parser_fuzz_utf8_no_panic() -> TestResult {
    let mut parser = VtParser::new();
    // Feed a mix of valid/invalid UTF-8, escape sequences, and random bytes.
    let chaos: [u8; 30] = [
        0xC3, 0xA9, // valid 2-byte (é)
        0xE4, 0xB8, 0xAD, // valid 3-byte (中)
        0xF0, 0x9F, 0x98, 0x80, // valid 4-byte (😀)
        0xFF, // invalid
        0x80, // orphan continuation
        0xC3, 0x1B, // truncated 2-byte then ESC
        b'[', b'3', b'1', b'm', // CSI SGR
        0xE0, 0x80, // overlong 3-byte start
        0x80, // continuation for overlong
        b'A', b'B', b'C', // ASCII
        0xF5, // invalid lead
        0xC2, 0x80, // valid 2-byte (U+0080)
        0xED, 0xA0, 0x80, // surrogate (U+D800, invalid)
        0x00, // NUL
    ];
    for &b in &chaos {
        let _ = parser.advance(b);
    }
    TestResult::Pass
}

pub fn test_vtparser_fuzz_no_panic() -> TestResult {
    let mut parser = VtParser::new();

    let assert_ground = |p: &mut VtParser| -> bool {
        // Send BEL (terminates OSC) + ESC \ (terminates OSC via ST) first,
        // then probe with a printable char to confirm Ground state.
        let _ = p.advance(0x07); // BEL — terminates OSC string
        let _ = p.advance(0x1B); // ESC
        let _ = p.advance(b'\\'); // ST — terminates any ESC sequence
        for _ in 0..64 {
            if p.advance(b'G') == VtAction::Print(b'G' as u32) {
                return true;
            }
        }
        false
    };

    for b in 0u8..=u8::MAX {
        let _ = parser.advance(b);
        if !assert_ground(&mut parser) {
            klog_info!(
                "TTY_TEST: BUG - parser did not return to Ground after byte 0x{:02x}",
                b
            );
            return TestResult::Fail;
        }
    }

    for b in 0u8..=u8::MAX {
        let _ = parser.advance(0x1B);
        let _ = parser.advance(b);
        if !assert_ground(&mut parser) {
            klog_info!(
                "TTY_TEST: BUG - parser did not recover after ESC 0x{:02x}",
                b
            );
            return TestResult::Fail;
        }
    }

    let csi_sequences: [&[u8]; 8] = [
        b"\x1b[31m",
        b"\x1b[0m",
        b"\x1b[999;1H",
        b"\x1b[38;2;255;0;127m",
        b"\x1b[48;5;200m",
        b"\x1b[?2004h",
        b"\x1b[?7l",
        b"\x1b[12;34;56;78;90m",
    ];
    for seq in csi_sequences {
        for &b in seq {
            let _ = parser.advance(b);
        }
        if !assert_ground(&mut parser) {
            klog_info!("TTY_TEST: BUG - parser did not recover after CSI fuzz sequence");
            return TestResult::Fail;
        }
    }

    for _ in 0..2048 {
        let _ = parser.advance(0x80);
    }
    if !assert_ground(&mut parser) {
        klog_info!("TTY_TEST: BUG - parser did not recover after continuation-byte flood");
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_replacement_glyph_exists() -> TestResult {
    let Some(atlas) = slopos_font::atlas::global() else {
        klog_info!("TTY_TEST: glyph atlas not initialised");
        return TestResult::Fail;
    };
    let coverage = atlas.get_coverage(0xFFFD);
    // The replacement glyph should not be all zeros.
    let has_nonzero = coverage.iter().any(|&b| b != 0);
    if !has_nonzero {
        klog_info!("TTY_TEST: replacement glyph coverage is all zeros");
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_get_glyph_for_codepoint_ascii() -> TestResult {
    let Some(atlas) = slopos_font::atlas::global() else {
        klog_info!("TTY_TEST: glyph atlas not initialised");
        return TestResult::Fail;
    };
    // For ASCII 'A', coverage should have non-zero pixels (it's a visible character).
    let coverage = atlas.get_coverage(b'A' as u32);
    let has_nonzero = coverage.iter().any(|&b| b != 0);
    if !has_nonzero {
        klog_info!("TTY_TEST: glyph for 'A' has zero coverage");
        return TestResult::Fail;
    }
    TestResult::Pass
}
