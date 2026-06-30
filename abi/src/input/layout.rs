//! Keyboard-layout wire types — the binary `LayoutTable` POD.
//!
//! A [`LayoutTable`] is a fixed-capacity, `#[repr(C)]`, allocation-free POD: it
//! is simultaneously the in-memory form the keymap resolver reads, the host-test
//! fixture, **and** the binary blob a userland tool uploads to the kernel via
//! `SYSCALL_KEYMAP_LOAD`. Because every field is a plain integer (a [`Cell`] is
//! a transparent `u32`), *any* byte pattern is a structurally valid value, so the
//! kernel can ingest an uploaded buffer soundly and reject only semantically-bad
//! data (see `slopos_keymap_core::validate`). The keymap *logic* (validation,
//! resolution, the text parser, the built-in US layout) lives in the
//! `slopos-keymap-core` crate; only the wire type and trivial accessors live here
//! (matching how [`super::InputEvent`] keeps its type in the ABI and its routing
//! logic in the driver).
//!
//! Layout-*dependent* data only: per-key, per-level text (base / shift / AltGr /
//! shift+AltGr), a per-key caps-affected bit, and a dead-key compose table.

/// Number of HID usages the table indexes directly (`0x00..0x80`). Covers every
/// layout-dependent key (letters, number row, punctuation, the non-US keys up to
/// `KEY_NONUS_BACKSLASH = 0x64`). Modifier usages (`0xE0..`) never reach the table.
pub const NUM_KEYS: usize = 0x80;

/// Levels per key: 0 = base, 1 = shift, 2 = AltGr, 3 = shift+AltGr.
pub const NUM_LEVELS: usize = 4;

/// Maximum dead keys a layout may declare (acute, grave, circumflex, …).
pub const MAX_DEADKEYS: usize = 8;

/// Maximum compose (dead-key + base → result) entries.
pub const MAX_COMPOSE: usize = 256;

/// Bytes reserved for the layout's short name (`"us"`, `"de_CH"`, …), NUL-padded.
pub const LAYOUT_NAME_LEN: usize = 16;

/// Blob magic: ASCII `"SLKB"` little-endian. Guards `SYSCALL_KEYMAP_LOAD`.
pub const LAYOUT_MAGIC: u32 = 0x424B_4C53; // 'S' 'L' 'K' 'B'

/// Binary format version. Bump on any incompatible `LayoutTable` layout change.
pub const LAYOUT_VERSION: u16 = 1;

/// High bit of a [`Cell`] marking it as a dead key rather than a literal.
const CELL_DEAD_FLAG: u32 = 0x8000_0000;

/// One level of one key: an empty slot, a literal codepoint, or a dead key.
///
/// Encoding (transparent `u32`): `0` = none; `CELL_DEAD_FLAG | idx` = dead key
/// `idx`; otherwise a literal Unicode scalar (never `0` for a real glyph, since
/// the only `0` outcome is "absent"). Control characters are layout-independent
/// and never stored here.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell(pub u32);

/// The decoded meaning of a [`Cell`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellKind {
    None,
    Literal(u32),
    Dead(u8),
}

impl Cell {
    /// The empty cell.
    pub const NONE: Cell = Cell(0);

    /// A literal codepoint cell (`0` collapses to [`Cell::NONE`]).
    pub const fn literal(cp: u32) -> Cell {
        Cell(cp & !CELL_DEAD_FLAG)
    }

    /// A dead-key cell referencing dead-key index `idx`.
    pub const fn dead(idx: u8) -> Cell {
        Cell(CELL_DEAD_FLAG | idx as u32)
    }

    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Decode the cell into its [`CellKind`].
    #[inline]
    pub const fn kind(self) -> CellKind {
        if self.0 == 0 {
            CellKind::None
        } else if self.0 & CELL_DEAD_FLAG != 0 {
            CellKind::Dead((self.0 & !CELL_DEAD_FLAG) as u8)
        } else {
            CellKind::Literal(self.0)
        }
    }
}

/// A single dead-key + base → result composition.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ComposeEntry {
    /// Dead-key index (`0..MAX_DEADKEYS`).
    pub dead: u8,
    pub _pad: [u8; 3],
    /// The base codepoint typed after the dead key.
    pub base: u32,
    /// The composed result codepoint.
    pub result: u32,
}

impl ComposeEntry {
    pub const EMPTY: ComposeEntry = ComposeEntry {
        dead: 0,
        _pad: [0; 3],
        base: 0,
        result: 0,
    };
}

impl Default for ComposeEntry {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// A complete keyboard layout: the data-driven core of a keymap.
///
/// `#[repr(C)]`, fixed-size, no heap — see the module docs.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LayoutTable {
    /// [`LAYOUT_MAGIC`].
    pub magic: u32,
    /// [`LAYOUT_VERSION`].
    pub version: u16,
    /// Number of valid entries in `compose` (`0..=MAX_COMPOSE`).
    pub num_compose: u16,
    /// Short name (NUL-padded), e.g. `b"de_CH"`.
    pub name: [u8; LAYOUT_NAME_LEN],
    /// Per-key caps-affected flag: nonzero ⇒ CapsLock case-folds this key.
    pub caps: [u8; NUM_KEYS],
    /// Per-key, per-level cells.
    pub levels: [[Cell; NUM_LEVELS]; NUM_KEYS],
    /// Bare spacing-accent codepoint per declared dead key (`0` = undeclared).
    pub dead_accent: [u32; MAX_DEADKEYS],
    /// Compose table (only the first `num_compose` entries are valid).
    pub compose: [ComposeEntry; MAX_COMPOSE],
}

impl LayoutTable {
    /// A zeroed table (no keys, no dead keys, no name) with a valid header.
    pub const fn empty() -> Self {
        Self {
            magic: LAYOUT_MAGIC,
            version: LAYOUT_VERSION,
            num_compose: 0,
            name: [0; LAYOUT_NAME_LEN],
            caps: [0; NUM_KEYS],
            levels: [[Cell::NONE; NUM_LEVELS]; NUM_KEYS],
            dead_accent: [0; MAX_DEADKEYS],
            compose: [ComposeEntry::EMPTY; MAX_COMPOSE],
        }
    }

    /// The layout's short name as a `str` (up to the first NUL). `""` on invalid
    /// UTF-8.
    pub fn name_str(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    /// The cell at `usage`/`level`, or [`Cell::NONE`] for out-of-range usages.
    #[inline]
    pub fn cell(&self, usage: u16, level: usize) -> Cell {
        let u = usage as usize;
        if u >= NUM_KEYS || level >= NUM_LEVELS {
            return Cell::NONE;
        }
        self.levels[u][level]
    }

    /// Whether `usage` is case-folded by CapsLock under this layout.
    #[inline]
    pub fn caps_affected(&self, usage: u16) -> bool {
        let u = usage as usize;
        u < NUM_KEYS && self.caps[u] != 0
    }

    /// Look up a composed result for `dead` + `base`, if the layout defines one.
    pub fn compose_lookup(&self, dead: u8, base: u32) -> Option<u32> {
        let n = (self.num_compose as usize).min(MAX_COMPOSE);
        for entry in &self.compose[..n] {
            if entry.dead == dead && entry.base == base {
                return Some(entry.result);
            }
        }
        None
    }

    /// The bare spacing accent for a declared dead key (`0` if undeclared).
    #[inline]
    pub fn dead_accent_of(&self, dead: u8) -> u32 {
        let d = dead as usize;
        if d < MAX_DEADKEYS {
            self.dead_accent[d]
        } else {
            0
        }
    }
}
