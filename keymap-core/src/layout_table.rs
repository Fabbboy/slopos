//! Layout-table logic: validation, binary (de)serialisation, and the built-in
//! US-QWERTY layout. The POD wire types ([`LayoutTable`], [`Cell`],
//! [`ComposeEntry`]) live in `slopos-abi` and are re-exported here.

use slopos_abi::input::keycode::*;
pub use slopos_abi::input::layout::{
    Cell, CellKind, ComposeEntry, LAYOUT_MAGIC, LAYOUT_NAME_LEN, LAYOUT_VERSION, LayoutTable,
    MAX_COMPOSE, MAX_DEADKEYS, NUM_KEYS, NUM_LEVELS,
};

/// Largest valid Unicode scalar value.
const UNICODE_MAX: u32 = 0x0010_FFFF;

/// Why a [`LayoutTable`] (parsed or uploaded) was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutError {
    /// Wrong size, magic, or version.
    BadHeader,
    /// `num_compose` exceeds [`MAX_COMPOSE`].
    TooManyCompose,
    /// A literal cell holds a surrogate / out-of-range / forbidden codepoint.
    BadCodepoint,
    /// A cell references a dead-key index that is out of range or undeclared.
    UndeclaredDeadKey,
    /// A compose entry references an undeclared dead key or bad codepoint.
    BadCompose,
    /// The compose table is not prefix-free / has a contradictory duplicate.
    AmbiguousCompose,
    /// The name field is not valid UTF-8 or is empty.
    BadName,
    /// A text line could not be parsed.
    Syntax,
    /// A `KEY_*` name was not recognised.
    UnknownKeycode,
    /// A keysym name / literal was not recognised.
    UnknownKeysym,
    /// More keys/compose/deadkeys than the table can hold.
    Overflow,
    /// The source / buffer exceeded the size cap.
    TooLarge,
    /// A cell or caps flag is set on a layout-independent key (Enter, nav, …).
    ForbiddenKey,
}

/// Is `cp` a codepoint a literal cell / compose result may hold? Rejects `0` (the
/// "absent" sentinel), surrogates, and controls — C0, DEL and C1, the last also
/// colliding with the legacy TTY nav pseudo-codes 0x80..=0x88.
fn is_valid_glyph(cp: u32) -> bool {
    cp != 0
        && cp <= UNICODE_MAX
        && !(0xD800..=0xDFFF).contains(&cp)
        && cp >= 0x20
        && cp != 0x7F
        && !(0x80..=0x9F).contains(&cp)
}

/// Is `usage` a key whose output legitimately varies by layout? [`validate`]
/// rejects tables that set a cell on any other key, since the resolver consults
/// the table before its layout-independent fallbacks.
pub fn is_layout_dependent(usage: u16) -> bool {
    (KEY_A..=KEY_0).contains(&usage)
        || (KEY_MINUS..=KEY_SLASH).contains(&usage)
        || usage == KEY_NONUS_BACKSLASH
}

/// Validate a (parsed or uploaded) layout: the single gate every untrusted
/// [`LayoutTable`] passes through before the kernel installs it.
pub fn validate(t: &LayoutTable) -> Result<(), LayoutError> {
    if t.magic != LAYOUT_MAGIC || t.version != LAYOUT_VERSION {
        return Err(LayoutError::BadHeader);
    }
    if t.num_compose as usize > MAX_COMPOSE {
        return Err(LayoutError::TooManyCompose);
    }
    let end = t.name.iter().position(|&b| b == 0).unwrap_or(t.name.len());
    if end == 0 || core::str::from_utf8(&t.name[..end]).is_err() {
        return Err(LayoutError::BadName);
    }

    for (usage, key) in t.levels.iter().enumerate() {
        let dependent = is_layout_dependent(usage as u16);
        if !dependent && t.caps[usage] != 0 {
            return Err(LayoutError::ForbiddenKey);
        }
        for cell in key {
            match cell.kind() {
                CellKind::None => {}
                _ if !dependent => return Err(LayoutError::ForbiddenKey),
                CellKind::Literal(cp) => {
                    if !is_valid_glyph(cp) {
                        return Err(LayoutError::BadCodepoint);
                    }
                }
                CellKind::Dead(idx) => {
                    let i = idx as usize;
                    if i >= MAX_DEADKEYS || t.dead_accent[i] == 0 {
                        return Err(LayoutError::UndeclaredDeadKey);
                    }
                }
            }
        }
    }

    for &acc in &t.dead_accent {
        if acc != 0 && !is_valid_glyph(acc) {
            return Err(LayoutError::BadCodepoint);
        }
    }

    let n = t.num_compose as usize;
    for (i, e) in t.compose[..n].iter().enumerate() {
        let d = e.dead as usize;
        if d >= MAX_DEADKEYS || t.dead_accent[d] == 0 {
            return Err(LayoutError::BadCompose);
        }
        // base may be a space (0x20); result must be a real glyph.
        if !(e.base == 0x20 || is_valid_glyph(e.base)) || !is_valid_glyph(e.result) {
            return Err(LayoutError::BadCompose);
        }
        for prev in &t.compose[..i] {
            if prev.dead == e.dead && prev.base == e.base {
                return Err(LayoutError::AmbiguousCompose);
            }
        }
    }

    Ok(())
}

// Explicit little-endian wire format in struct field order: driving it field by
// field rather than reinterpreting struct bytes keeps padding out of the format.

/// Exact byte length of a serialised [`LayoutTable`].
pub const SERIALIZED_LEN: usize = 4
    + 2
    + 2
    + LAYOUT_NAME_LEN
    + NUM_KEYS
    + (NUM_KEYS * NUM_LEVELS * 4)
    + (MAX_DEADKEYS * 4)
    + (MAX_COMPOSE * 9);

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.buf[self.pos] = v;
        self.pos += 1;
    }
    fn u16(&mut self, v: u16) {
        self.buf[self.pos..self.pos + 2].copy_from_slice(&v.to_le_bytes());
        self.pos += 2;
    }
    fn u32(&mut self, v: u32) {
        self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_le_bytes());
        self.pos += 4;
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf[self.pos..self.pos + b.len()].copy_from_slice(b);
        self.pos += b.len();
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.buf[self.pos];
        self.pos += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        let mut a = [0u8; 2];
        a.copy_from_slice(&self.buf[self.pos..self.pos + 2]);
        self.pos += 2;
        u16::from_le_bytes(a)
    }
    fn u32(&mut self) -> u32 {
        let mut a = [0u8; 4];
        a.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
        self.pos += 4;
        u32::from_le_bytes(a)
    }
    fn bytes(&mut self, out: &mut [u8]) {
        out.copy_from_slice(&self.buf[self.pos..self.pos + out.len()]);
        self.pos += out.len();
    }
}

/// Serialise `t` into `buf`, which must be at least [`SERIALIZED_LEN`] bytes.
pub fn serialize(t: &LayoutTable, buf: &mut [u8]) -> Result<usize, LayoutError> {
    if buf.len() < SERIALIZED_LEN {
        return Err(LayoutError::TooLarge);
    }
    let mut w = Writer { buf, pos: 0 };
    w.u32(t.magic);
    w.u16(t.version);
    w.u16(t.num_compose);
    w.bytes(&t.name);
    w.bytes(&t.caps);
    for key in &t.levels {
        for cell in key {
            w.u32(cell.0);
        }
    }
    for &a in &t.dead_accent {
        w.u32(a);
    }
    for e in &t.compose {
        w.u8(e.dead);
        w.u32(e.base);
        w.u32(e.result);
    }
    Ok(w.pos)
}

/// Deserialise a [`SERIALIZED_LEN`]-byte blob into `out` in place (no large stack
/// rvalue) and [`validate`] it; every field is overwritten.
pub fn deserialize(buf: &[u8], out: &mut LayoutTable) -> Result<(), LayoutError> {
    if buf.len() != SERIALIZED_LEN {
        return Err(LayoutError::BadHeader);
    }
    let mut r = Reader { buf, pos: 0 };
    out.magic = r.u32();
    out.version = r.u16();
    out.num_compose = r.u16();
    r.bytes(&mut out.name);
    r.bytes(&mut out.caps);
    for key in out.levels.iter_mut() {
        for cell in key.iter_mut() {
            *cell = Cell(r.u32());
        }
    }
    for a in out.dead_accent.iter_mut() {
        *a = r.u32();
    }
    for e in out.compose.iter_mut() {
        e.dead = r.u8();
        e._pad = [0; 3];
        e.base = r.u32();
        e.result = r.u32();
    }
    validate(out)
}

/// The built-in US-QWERTY layout.
pub static US_QWERTY: LayoutTable = us_qwerty();

const fn set_name(t: &mut LayoutTable, name: &[u8]) {
    let mut i = 0;
    while i < name.len() && i < LAYOUT_NAME_LEN {
        t.name[i] = name[i];
        i += 1;
    }
}

/// Set a key's base/shift literals (`0` ⇒ that level is empty); AltGr stays empty.
const fn set2(t: &mut LayoutTable, usage: u16, caps: bool, base: u32, shift: u32) {
    let u = usage as usize;
    t.caps[u] = caps as u8;
    t.levels[u][0] = if base != 0 {
        Cell::literal(base)
    } else {
        Cell::NONE
    };
    t.levels[u][1] = if shift != 0 {
        Cell::literal(shift)
    } else {
        Cell::NONE
    };
}

/// Compile-time builder for US-QWERTY: letters, number row and punctuation; no
/// AltGr, no dead keys.
pub const fn us_qwerty() -> LayoutTable {
    let mut t = LayoutTable::empty();
    set_name(&mut t, b"us");

    let mut u = KEY_A;
    while u <= KEY_Z {
        let lower = b'a' as u32 + (u - KEY_A) as u32;
        set2(&mut t, u, true, lower, lower - 0x20);
        u += 1;
    }

    set2(&mut t, KEY_1, false, b'1' as u32, b'!' as u32);
    set2(&mut t, KEY_2, false, b'2' as u32, b'@' as u32);
    set2(&mut t, KEY_3, false, b'3' as u32, b'#' as u32);
    set2(&mut t, KEY_4, false, b'4' as u32, b'$' as u32);
    set2(&mut t, KEY_5, false, b'5' as u32, b'%' as u32);
    set2(&mut t, KEY_6, false, b'6' as u32, b'^' as u32);
    set2(&mut t, KEY_7, false, b'7' as u32, b'&' as u32);
    set2(&mut t, KEY_8, false, b'8' as u32, b'*' as u32);
    set2(&mut t, KEY_9, false, b'9' as u32, b'(' as u32);
    set2(&mut t, KEY_0, false, b'0' as u32, b')' as u32);

    set2(&mut t, KEY_MINUS, false, b'-' as u32, b'_' as u32);
    set2(&mut t, KEY_EQUAL, false, b'=' as u32, b'+' as u32);
    set2(&mut t, KEY_LEFTBRACE, false, b'[' as u32, b'{' as u32);
    set2(&mut t, KEY_RIGHTBRACE, false, b']' as u32, b'}' as u32);
    set2(&mut t, KEY_BACKSLASH, false, b'\\' as u32, b'|' as u32);
    set2(&mut t, KEY_NONUS_HASH, false, b'\\' as u32, b'|' as u32);
    set2(
        &mut t,
        KEY_NONUS_BACKSLASH,
        false,
        b'\\' as u32,
        b'|' as u32,
    );
    set2(&mut t, KEY_SEMICOLON, false, b';' as u32, b':' as u32);
    set2(&mut t, KEY_APOSTROPHE, false, b'\'' as u32, b'"' as u32);
    set2(&mut t, KEY_GRAVE, false, b'`' as u32, b'~' as u32);
    set2(&mut t, KEY_COMMA, false, b',' as u32, b'<' as u32);
    set2(&mut t, KEY_DOT, false, b'.' as u32, b'>' as u32);
    set2(&mut t, KEY_SLASH, false, b'/' as u32, b'?' as u32);

    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_default_validates() {
        assert!(validate(&US_QWERTY).is_ok());
        assert_eq!(US_QWERTY.name_str(), "us");
    }

    #[test]
    fn cell_encoding_round_trips() {
        assert_eq!(Cell::NONE.kind(), CellKind::None);
        assert_eq!(
            Cell::literal(b'@' as u32).kind(),
            CellKind::Literal(b'@' as u32)
        );
        assert_eq!(Cell::literal(0x20AC).kind(), CellKind::Literal(0x20AC)); // €
        assert_eq!(Cell::dead(3).kind(), CellKind::Dead(3));
    }

    #[test]
    fn serialize_round_trips() {
        let mut buf = [0u8; SERIALIZED_LEN];
        let n = serialize(&US_QWERTY, &mut buf).unwrap();
        assert_eq!(n, SERIALIZED_LEN);
        let mut out = LayoutTable::empty();
        deserialize(&buf, &mut out).unwrap();
        assert_eq!(out.name_str(), "us");
        assert_eq!(out.cell(KEY_2, 1).kind(), CellKind::Literal(b'@' as u32));
        assert_eq!(out.caps[KEY_A as usize], 1);
    }

    #[test]
    fn deserialize_rejects_wrong_length() {
        let buf = [0u8; SERIALIZED_LEN - 1];
        assert_eq!(
            deserialize(&buf, &mut LayoutTable::empty()),
            Err(LayoutError::BadHeader)
        );
    }

    #[test]
    fn deserialize_validates_payload() {
        let mut buf = [0u8; SERIALIZED_LEN];
        serialize(&US_QWERTY, &mut buf).unwrap();
        buf[0] ^= 0xFF; // corrupt magic
        assert_eq!(
            deserialize(&buf, &mut LayoutTable::empty()),
            Err(LayoutError::BadHeader)
        );
    }

    #[test]
    fn validate_rejects_bad_header() {
        let mut t = US_QWERTY;
        t.magic = 0xDEAD_BEEF;
        assert_eq!(validate(&t), Err(LayoutError::BadHeader));
    }

    #[test]
    fn validate_rejects_surrogate_and_control() {
        let mut t = US_QWERTY;
        t.levels[KEY_A as usize][0] = Cell(0xD800); // surrogate
        assert_eq!(validate(&t), Err(LayoutError::BadCodepoint));
        let mut t2 = US_QWERTY;
        t2.levels[KEY_A as usize][0] = Cell(0x07); // C0 control
        assert_eq!(validate(&t2), Err(LayoutError::BadCodepoint));
        let mut t3 = US_QWERTY;
        t3.levels[KEY_A as usize][0] = Cell(0x85); // C1 control (NEL)
        assert_eq!(validate(&t3), Err(LayoutError::BadCodepoint));
    }

    #[test]
    fn validate_rejects_cells_on_layout_independent_keys() {
        let mut t = US_QWERTY;
        t.levels[KEY_ENTER as usize][0] = Cell::literal(b'x' as u32);
        assert_eq!(validate(&t), Err(LayoutError::ForbiddenKey));

        let mut t2 = US_QWERTY;
        t2.levels[KEY_LEFT as usize][0] = Cell::literal(b'x' as u32);
        assert_eq!(validate(&t2), Err(LayoutError::ForbiddenKey));

        let mut t3 = US_QWERTY;
        t3.caps[KEY_ENTER as usize] = 1;
        assert_eq!(validate(&t3), Err(LayoutError::ForbiddenKey));
    }

    #[test]
    fn validate_rejects_undeclared_dead_and_compose() {
        let mut t = US_QWERTY;
        t.levels[KEY_EQUAL as usize][0] = Cell::dead(0); // dead 0 undeclared
        assert_eq!(validate(&t), Err(LayoutError::UndeclaredDeadKey));

        let mut t2 = US_QWERTY;
        t2.num_compose = 1;
        t2.compose[0] = ComposeEntry {
            dead: 0,
            _pad: [0; 3],
            base: b'a' as u32,
            result: 0x00E1,
        };
        assert_eq!(validate(&t2), Err(LayoutError::BadCompose));
    }

    #[test]
    fn validate_rejects_ambiguous_compose() {
        let mut t = US_QWERTY;
        t.dead_accent[0] = 0x00B4; // declare acute
        t.num_compose = 2;
        t.compose[0] = ComposeEntry {
            dead: 0,
            _pad: [0; 3],
            base: b'a' as u32,
            result: 0x00E1,
        };
        t.compose[1] = ComposeEntry {
            dead: 0,
            _pad: [0; 3],
            base: b'a' as u32,
            result: 0x00C1,
        };
        assert_eq!(validate(&t), Err(LayoutError::AmbiguousCompose));
    }
}
