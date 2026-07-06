//! Text `*.layout` → [`LayoutTable`] parser.
//!
//! This is the **userland** half of the layout pipeline: a small, allocation-free
//! parser that turns a human-authored `*.layout` text file into the validated
//! binary [`LayoutTable`] POD that gets uploaded to the kernel. The kernel never
//! runs this — it only ingests the already-validated binary — so the text grammar
//! is not part of the kernel's attack surface.
//!
//! Being alloc-free, it is fully host-testable and reusable everywhere.
//!
//! # Grammar (line-oriented; `#` begins a comment to end of line)
//!
//! ```text
//! name = <short-name>
//! title = <free text>                       # ignored (metadata)
//! deadkeys = acute grave circumflex ...      # declares dead keys, in index order
//! key <KEY_NAME> <yes|no> <base> <shift> <altgr> <shift+altgr>
//! compose dead:<name> <base> -> <result>
//! ```
//!
//! Each level token is `-`/`none` (empty), a `dead:<name>` reference, a single
//! literal character (`z`, `@`, `ä`), or a keysym name (`euro`, `sharp_s`,
//! `aacute`). Missing trailing levels default to empty.

use slopos_abi::input::keycode::*;

use crate::layout_table::{
    Cell, ComposeEntry, LAYOUT_MAGIC, LAYOUT_NAME_LEN, LAYOUT_VERSION, LayoutError, LayoutTable,
    MAX_COMPOSE, MAX_DEADKEYS, NUM_KEYS, NUM_LEVELS, validate,
};

/// Hard cap on source size before any work (defensive; layouts are tiny).
const MAX_SRC: usize = 64 * 1024;

/// Max whitespace-separated tokens we read from one line.
const MAX_TOKENS: usize = 12;

/// Parse `src` into `out`, which is cleared first. On success `out` is a
/// validated [`LayoutTable`]; on failure `out` is left partially written and the
/// error describes the first problem.
pub fn parse(src: &[u8], out: &mut LayoutTable) -> Result<(), LayoutError> {
    if src.len() > MAX_SRC {
        return Err(LayoutError::TooLarge);
    }

    clear(out);

    // Declared dead-key names, in declaration order → index. Slices borrow `src`.
    let mut dead_names: [&[u8]; MAX_DEADKEYS] = [b""; MAX_DEADKEYS];
    let mut dead_count: usize = 0;

    for raw_line in src.split(|&b| b == b'\n') {
        // Strip an inline comment (`#` always begins a comment).
        let line = match raw_line.iter().position(|&b| b == b'#') {
            Some(i) => &raw_line[..i],
            None => raw_line,
        };

        let mut toks: [&[u8]; MAX_TOKENS] = [b""; MAX_TOKENS];
        let n = tokenize(line, &mut toks);
        if n == 0 {
            continue;
        }

        match toks[0] {
            b"name" => {
                let val = last_value(&toks[..n]).ok_or(LayoutError::Syntax)?;
                set_name(out, val)?;
            }
            b"title" | b"version" => { /* metadata: ignored */ }
            b"deadkeys" => {
                parse_deadkeys(out, &toks[..n], &mut dead_names, &mut dead_count)?;
            }
            b"key" => {
                parse_key(out, &toks[..n], &dead_names[..dead_count])?;
            }
            b"compose" => {
                parse_compose(out, &toks[..n], &dead_names[..dead_count])?;
            }
            _ => return Err(LayoutError::Syntax),
        }
    }

    out.magic = LAYOUT_MAGIC;
    out.version = LAYOUT_VERSION;
    validate(out)
}

/// Clear `out` in place (no large rvalue on the stack).
fn clear(out: &mut LayoutTable) {
    out.magic = LAYOUT_MAGIC;
    out.version = LAYOUT_VERSION;
    out.num_compose = 0;
    out.name = [0; LAYOUT_NAME_LEN];
    out.caps = [0; NUM_KEYS];
    out.dead_accent = [0; MAX_DEADKEYS];
    for key in out.levels.iter_mut() {
        *key = [Cell::NONE; NUM_LEVELS];
    }
}

/// Split a line into whitespace-separated token slices; returns the count.
fn tokenize<'a>(line: &'a [u8], out: &mut [&'a [u8]]) -> usize {
    let mut n = 0;
    let mut i = 0;
    let len = line.len();
    while i < len && n < out.len() {
        while i < len && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        let start = i;
        while i < len && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        out[n] = &line[start..i];
        n += 1;
    }
    n
}

/// For `key = value`, return the value (last token, after an optional `=`).
fn last_value<'a>(toks: &[&'a [u8]]) -> Option<&'a [u8]> {
    let last = *toks.last()?;
    if last == b"=" { None } else { Some(last) }
}

fn set_name(out: &mut LayoutTable, val: &[u8]) -> Result<(), LayoutError> {
    if val.is_empty() || val.len() > LAYOUT_NAME_LEN {
        return Err(LayoutError::BadName);
    }
    out.name = [0; LAYOUT_NAME_LEN];
    out.name[..val.len()].copy_from_slice(val);
    Ok(())
}

fn parse_deadkeys<'a>(
    out: &mut LayoutTable,
    toks: &[&'a [u8]],
    dead_names: &mut [&'a [u8]; MAX_DEADKEYS],
    dead_count: &mut usize,
) -> Result<(), LayoutError> {
    for &tok in &toks[1..] {
        if tok == b"=" {
            continue;
        }
        if *dead_count >= MAX_DEADKEYS {
            return Err(LayoutError::Overflow);
        }
        let accent = dead_accent_by_name(tok).ok_or(LayoutError::UnknownKeysym)?;
        dead_names[*dead_count] = tok;
        out.dead_accent[*dead_count] = accent;
        *dead_count += 1;
    }
    Ok(())
}

fn parse_key(
    out: &mut LayoutTable,
    toks: &[&[u8]],
    dead_names: &[&[u8]],
) -> Result<(), LayoutError> {
    // key <NAME> <caps> [base] [shift] [altgr] [shift+altgr]
    if toks.len() < 3 {
        return Err(LayoutError::Syntax);
    }
    let usage = keycode_by_name(toks[1]).ok_or(LayoutError::UnknownKeycode)?;
    let u = usage as usize;
    if u >= NUM_KEYS {
        return Err(LayoutError::Overflow);
    }
    let caps = parse_bool(toks[2])?;
    out.caps[u] = caps as u8;

    for level in 0..NUM_LEVELS {
        let tok = toks.get(3 + level).copied().unwrap_or(b"-");
        out.levels[u][level] = resolve_cell(tok, dead_names)?;
    }
    Ok(())
}

fn parse_compose(
    out: &mut LayoutTable,
    toks: &[&[u8]],
    dead_names: &[&[u8]],
) -> Result<(), LayoutError> {
    // compose dead:<name> <base> -> <result>
    if toks.len() != 5 || toks[3] != b"->" {
        return Err(LayoutError::Syntax);
    }
    let dead = parse_dead_ref(toks[1], dead_names)?;
    let base = resolve_keysym(toks[2]).ok_or(LayoutError::UnknownKeysym)?;
    let result = resolve_keysym(toks[4]).ok_or(LayoutError::UnknownKeysym)?;

    let idx = out.num_compose as usize;
    if idx >= MAX_COMPOSE {
        return Err(LayoutError::Overflow);
    }
    out.compose[idx] = ComposeEntry {
        dead,
        _pad: [0; 3],
        base,
        result,
    };
    out.num_compose += 1;
    Ok(())
}

/// Resolve a level token into a [`Cell`].
fn resolve_cell(tok: &[u8], dead_names: &[&[u8]]) -> Result<Cell, LayoutError> {
    if tok == b"-" || tok == b"none" {
        return Ok(Cell::NONE);
    }
    if let Some(name) = tok.strip_prefix(b"dead:") {
        let idx = parse_dead_ref_name(name, dead_names)?;
        return Ok(Cell::dead(idx));
    }
    let cp = resolve_keysym(tok).ok_or(LayoutError::UnknownKeysym)?;
    Ok(Cell::literal(cp))
}

fn parse_dead_ref(tok: &[u8], dead_names: &[&[u8]]) -> Result<u8, LayoutError> {
    let name = tok.strip_prefix(b"dead:").ok_or(LayoutError::Syntax)?;
    parse_dead_ref_name(name, dead_names)
}

fn parse_dead_ref_name(name: &[u8], dead_names: &[&[u8]]) -> Result<u8, LayoutError> {
    dead_names
        .iter()
        .position(|&d| d == name)
        .map(|i| i as u8)
        .ok_or(LayoutError::UndeclaredDeadKey)
}

fn parse_bool(tok: &[u8]) -> Result<bool, LayoutError> {
    match tok {
        b"yes" | b"true" | b"1" => Ok(true),
        b"no" | b"false" | b"0" => Ok(false),
        _ => Err(LayoutError::Syntax),
    }
}

/// Resolve a glyph token to a codepoint: a single literal character, or a keysym
/// name from the table below.
fn resolve_keysym(tok: &[u8]) -> Option<u32> {
    let s = core::str::from_utf8(tok).ok()?;
    let mut chars = s.chars();
    if let Some(c) = chars.next() {
        if chars.next().is_none() {
            // Exactly one character: a literal glyph.
            return Some(c as u32);
        }
    }
    keysym_by_name(s)
}

/// Canonical `KEY_*` name → HID usage, for the layout-dependent keys only.
fn keycode_by_name(name: &[u8]) -> Option<u16> {
    let s = core::str::from_utf8(name).ok()?;
    Some(match s {
        "KEY_A" => KEY_A,
        "KEY_B" => KEY_B,
        "KEY_C" => KEY_C,
        "KEY_D" => KEY_D,
        "KEY_E" => KEY_E,
        "KEY_F" => KEY_F,
        "KEY_G" => KEY_G,
        "KEY_H" => KEY_H,
        "KEY_I" => KEY_I,
        "KEY_J" => KEY_J,
        "KEY_K" => KEY_K,
        "KEY_L" => KEY_L,
        "KEY_M" => KEY_M,
        "KEY_N" => KEY_N,
        "KEY_O" => KEY_O,
        "KEY_P" => KEY_P,
        "KEY_Q" => KEY_Q,
        "KEY_R" => KEY_R,
        "KEY_S" => KEY_S,
        "KEY_T" => KEY_T,
        "KEY_U" => KEY_U,
        "KEY_V" => KEY_V,
        "KEY_W" => KEY_W,
        "KEY_X" => KEY_X,
        "KEY_Y" => KEY_Y,
        "KEY_Z" => KEY_Z,
        "KEY_1" => KEY_1,
        "KEY_2" => KEY_2,
        "KEY_3" => KEY_3,
        "KEY_4" => KEY_4,
        "KEY_5" => KEY_5,
        "KEY_6" => KEY_6,
        "KEY_7" => KEY_7,
        "KEY_8" => KEY_8,
        "KEY_9" => KEY_9,
        "KEY_0" => KEY_0,
        "KEY_MINUS" => KEY_MINUS,
        "KEY_EQUAL" => KEY_EQUAL,
        "KEY_LEFTBRACE" => KEY_LEFTBRACE,
        "KEY_RIGHTBRACE" => KEY_RIGHTBRACE,
        "KEY_BACKSLASH" => KEY_BACKSLASH,
        "KEY_NONUS_HASH" => KEY_NONUS_HASH,
        "KEY_NONUS_BACKSLASH" => KEY_NONUS_BACKSLASH,
        "KEY_SEMICOLON" => KEY_SEMICOLON,
        "KEY_APOSTROPHE" => KEY_APOSTROPHE,
        "KEY_GRAVE" => KEY_GRAVE,
        "KEY_COMMA" => KEY_COMMA,
        "KEY_DOT" => KEY_DOT,
        "KEY_SLASH" => KEY_SLASH,
        _ => return None,
    })
}

/// Dead-key name → its bare spacing accent codepoint.
fn dead_accent_by_name(name: &[u8]) -> Option<u32> {
    let s = core::str::from_utf8(name).ok()?;
    Some(match s {
        "acute" => 0x00B4,      // ´
        "grave" => 0x0060,      // `
        "circumflex" => 0x005E, // ^
        "diaeresis" => 0x00A8,  // ¨
        "tilde" => 0x007E,      // ~
        "cedilla" => 0x00B8,    // ¸
        "ring" => 0x02DA,       // ˚
        "caron" => 0x02C7,      // ˇ
        _ => return None,
    })
}

/// Keysym name → codepoint. Covers ASCII punctuation, common symbols, and the
/// Latin-1/accented letters de/de_CH/fr_CH need (grows as layouts are added).
fn keysym_by_name(s: &str) -> Option<u32> {
    Some(match s {
        // whitespace / explicit
        "space" => 0x20,
        // ASCII punctuation (named forms; useful where the literal is awkward)
        "exclam" => 0x21,
        "dquote" | "quotedbl" => 0x22,
        "numbersign" | "hash" => 0x23,
        "dollar" => 0x24,
        "percent" => 0x25,
        "ampersand" => 0x26,
        "apostrophe" | "quote" => 0x27,
        "parenleft" => 0x28,
        "parenright" => 0x29,
        "asterisk" => 0x2A,
        "plus" => 0x2B,
        "comma" => 0x2C,
        "minus" => 0x2D,
        "period" => 0x2E,
        "slash" => 0x2F,
        "colon" => 0x3A,
        "semicolon" => 0x3B,
        "less" => 0x3C,
        "equal" => 0x3D,
        "greater" => 0x3E,
        "question" => 0x3F,
        "at" => 0x40,
        "bracketleft" => 0x5B,
        "backslash" => 0x5C,
        "bracketright" => 0x5D,
        "asciicircum" | "caret" => 0x5E,
        "underscore" => 0x5F,
        "grave" => 0x60,
        "braceleft" => 0x7B,
        "bar" | "pipe" => 0x7C,
        "braceright" => 0x7D,
        "asciitilde" => 0x7E,
        // symbols
        "euro" => 0x20AC,
        "cent" => 0x00A2,
        "sterling" | "pound" => 0x00A3,
        "currency" => 0x00A4,
        "yen" => 0x00A5,
        "brokenbar" => 0x00A6,
        "section" => 0x00A7,
        "diaeresis" => 0x00A8,
        "copyright" => 0x00A9,
        "notsign" => 0x00AC,
        "registered" => 0x00AE,
        "degree" => 0x00B0,
        "plusminus" => 0x00B1,
        "acute" => 0x00B4,
        "mu" | "micro" => 0x00B5,
        "paragraph" => 0x00B6,
        "periodcentered" => 0x00B7,
        "onequarter" => 0x00BC,
        "onehalf" => 0x00BD,
        "threequarters" => 0x00BE,
        "guillemotleft" => 0x00AB,
        "guillemotright" => 0x00BB,
        // Latin-1 letters (lower then upper), German / Swiss / French
        "agrave" => 0x00E0,
        "Agrave" => 0x00C0,
        "aacute" => 0x00E1,
        "Aacute" => 0x00C1,
        "acircumflex" => 0x00E2,
        "Acircumflex" => 0x00C2,
        "atilde" => 0x00E3,
        "adiaeresis" => 0x00E4,
        "Adiaeresis" => 0x00C4,
        "aring" => 0x00E5,
        "Aring" => 0x00C5,
        "ae" => 0x00E6,
        "ccedilla" => 0x00E7,
        "Ccedilla" => 0x00C7,
        "egrave" => 0x00E8,
        "Egrave" => 0x00C8,
        "eacute" => 0x00E9,
        "Eacute" => 0x00C9,
        "ecircumflex" => 0x00EA,
        "Ecircumflex" => 0x00CA,
        "ediaeresis" => 0x00EB,
        "igrave" => 0x00EC,
        "iacute" => 0x00ED,
        "icircumflex" => 0x00EE,
        "idiaeresis" => 0x00EF,
        "ntilde" => 0x00F1,
        "ograve" => 0x00F2,
        "oacute" => 0x00F3,
        "ocircumflex" => 0x00F4,
        "otilde" => 0x00F5,
        "odiaeresis" => 0x00F6,
        "Odiaeresis" => 0x00D6,
        "oslash" => 0x00F8,
        "ugrave" => 0x00F9,
        "uacute" => 0x00FA,
        "ucircumflex" => 0x00FB,
        "udiaeresis" => 0x00FC,
        "Udiaeresis" => 0x00DC,
        "ydiaeresis" => 0x00FF,
        "ssharp" | "sharp_s" => 0x00DF,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{DeadKeyState, KeyOutcome, Locks, Mods, resolve};
    use crate::layout_table::{CellKind, LayoutError};

    const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: false,
    };
    const ALTGR: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: true,
    };
    const NUM_ON: Locks = Locks {
        caps: false,
        num: true,
        scroll: false,
    };

    const DE_CH: &[u8] = b"\
# slopos layout v1
name = de_CH
title = Swiss German (QWERTZ)
deadkeys = acute grave circumflex diaeresis

key KEY_Y   yes  z  Z  -  -
key KEY_Z   yes  y  Y  -  -
key KEY_2   no   2  dquote  at  -      # AltGr+2 = @
key KEY_E   yes  e  E  euro  -         # AltGr+E = euro
key KEY_EQUAL  no  dead:acute  dead:grave  -  -
compose dead:acute  a -> aacute
compose dead:acute  e -> eacute
compose dead:acute  space -> acute
";

    fn parse_ok(src: &[u8]) -> LayoutTable {
        let mut t = LayoutTable::empty();
        parse(src, &mut t).expect("layout should parse");
        t
    }

    #[test]
    fn parses_name_and_validates() {
        let t = parse_ok(DE_CH);
        assert_eq!(t.name_str(), "de_CH");
        assert!(crate::layout_table::validate(&t).is_ok());
    }

    #[test]
    fn parses_qwertz_swap_and_altgr() {
        let t = parse_ok(DE_CH);
        let mut d = DeadKeyState::new();
        assert_eq!(
            resolve(&t, KEY_Y, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('z' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_Z, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('y' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_2, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('@' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_E, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x20AC)
        );
    }

    #[test]
    fn parses_dead_key_and_composes() {
        let t = parse_ok(DE_CH);
        // The '=' key base is a dead acute.
        assert_eq!(t.cell(KEY_EQUAL, 0).kind(), CellKind::Dead(0));
        let mut d = DeadKeyState::new();
        assert_eq!(
            resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::None
        );
        // 'e' is defined in this layout (`key KEY_E ... e ...`) and has an acute
        // compose rule → é.
        assert_eq!(
            resolve(&t, KEY_E, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E9)
        ); // é
    }

    #[test]
    fn literal_single_char_and_keysym_names() {
        assert_eq!(resolve_keysym(b"z"), Some('z' as u32));
        assert_eq!(resolve_keysym(b"@"), Some('@' as u32));
        assert_eq!(resolve_keysym(b"euro"), Some(0x20AC));
        assert_eq!(resolve_keysym(b"sharp_s"), Some(0x00DF));
        assert_eq!(resolve_keysym("ä".as_bytes()), Some(0x00E4)); // literal non-ASCII
        assert_eq!(resolve_keysym(b"nonsense_keysym"), None);
    }

    // --- malformed / hostile inputs reject cleanly ---

    fn parse_err(src: &[u8]) -> LayoutError {
        let mut t = LayoutTable::empty();
        parse(src, &mut t).unwrap_err()
    }

    #[test]
    fn rejects_unknown_keycode() {
        assert_eq!(
            parse_err(b"name = x\nkey KEY_BOGUS no a A - -\n"),
            LayoutError::UnknownKeycode
        );
    }

    #[test]
    fn rejects_unknown_keysym() {
        assert_eq!(
            parse_err(b"name = x\nkey KEY_A no flubber A - -\n"),
            LayoutError::UnknownKeysym
        );
    }

    #[test]
    fn rejects_undeclared_dead_key() {
        assert_eq!(
            parse_err(b"name = x\nkey KEY_EQUAL no dead:acute - - -\n"),
            LayoutError::UndeclaredDeadKey
        );
    }

    #[test]
    fn rejects_missing_name() {
        // No `name =` line ⇒ empty name ⇒ validate rejects.
        assert_eq!(parse_err(b"key KEY_A no a A - -\n"), LayoutError::BadName);
    }

    #[test]
    fn rejects_oversize_source() {
        // A source larger than the cap is rejected before any work.
        let big = [b' '; MAX_SRC + 1];
        assert_eq!(parse_err(&big), LayoutError::TooLarge);
    }

    #[test]
    fn rejects_ambiguous_compose() {
        let src = b"name = x\ndeadkeys = acute\ncompose dead:acute a -> aacute\ncompose dead:acute a -> agrave\n";
        assert_eq!(parse_err(src), LayoutError::AmbiguousCompose);
    }

    // --- the actual shipped layout files parse + resolve correctly ---

    const US_FILE: &str = include_str!("../../assets/keymaps/us.layout");
    const DE_CH_FILE: &str = include_str!("../../assets/keymaps/de_CH.layout");

    #[test]
    fn shipped_us_layout_matches_builtin() {
        let t = parse_ok(US_FILE.as_bytes());
        assert_eq!(t.name_str(), "us");
        let mut d = DeadKeyState::new();
        assert_eq!(
            resolve(&t, KEY_A, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('a' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_A, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('A' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_2, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('@' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_3, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('#' as u32)
        );
    }

    #[test]
    fn shipped_de_ch_layout_is_correct() {
        let t = parse_ok(DE_CH_FILE.as_bytes());
        assert_eq!(t.name_str(), "de_CH");
        let mut d = DeadKeyState::new();

        // QWERTZ: physical Y types z, physical Z types y.
        assert_eq!(
            resolve(&t, KEY_Y, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('z' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_Z, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('y' as u32)
        );

        // AltGr coding glyphs.
        assert_eq!(
            resolve(&t, KEY_2, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('@' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_3, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('#' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_E, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x20AC)
        ); // €
        assert_eq!(
            resolve(&t, KEY_7, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('|' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_LEFTBRACE, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('[' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_APOSTROPHE, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('{' as u32)
        );

        // Umlaut keys: base umlaut, Shift = French accent (real Swiss layout).
        assert_eq!(
            resolve(&t, KEY_LEFTBRACE, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00FC) // ü
        );
        assert_eq!(
            resolve(&t, KEY_LEFTBRACE, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E8) // è
        );
        assert_eq!(
            resolve(&t, KEY_SEMICOLON, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E9) // é
        );
        assert_eq!(
            resolve(&t, KEY_APOSTROPHE, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E4) // ä
        );
        assert_eq!(
            resolve(&t, KEY_APOSTROPHE, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E0) // à
        );

        // Capital umlaut via the diaeresis dead key: ¨ then U → Ü.
        assert_eq!(
            resolve(&t, KEY_RIGHTBRACE, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::None
        );
        assert_eq!(
            resolve(&t, KEY_U, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00DC) // Ü
        );

        // Dead circumflex (KEY_EQUAL base) then 'a' → â.
        assert_eq!(
            resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::None
        );
        assert_eq!(
            resolve(&t, KEY_A, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E2)
        ); // â

        // Dead acute (AltGr+KEY_MINUS) then 'e' → é.
        assert_eq!(
            resolve(&t, KEY_MINUS, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::None
        );
        assert_eq!(
            resolve(&t, KEY_E, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00E9)
        ); // é

        // Dead diaeresis (KEY_RIGHTBRACE base) then space → bare ¨.
        assert_eq!(
            resolve(&t, KEY_RIGHTBRACE, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::None
        );
        assert_eq!(
            resolve(&t, KEY_SPACE, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x00A8)
        );
    }
}
