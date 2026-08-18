//! Keyboard-layout wire types — the binary `LayoutTable` POD.
//!
//! A [`LayoutTable`] is a fixed-capacity `#[repr(C)]` POD: both the in-memory form
//! the resolver reads and the blob userland uploads via `SYSCALL_KEYMAP_LOAD`.
//! Every field is a plain integer, so any byte pattern is structurally valid and
//! only semantic validation applies (`slopos_keymap_core::validate`).

/// HID usages the table indexes directly (`0x00..0x80`) — every layout-dependent
/// key. Modifier usages (`0xE0..`) never reach the table.
pub const NUM_KEYS: usize = 0x80;

/// Levels per key: 0 = base, 1 = shift, 2 = AltGr, 3 = shift+AltGr.
pub const NUM_LEVELS: usize = 4;

pub const MAX_DEADKEYS: usize = 8;

/// Maximum compose (dead-key + base → result) entries.
pub const MAX_COMPOSE: usize = 256;

/// Bytes reserved for the layout's short name (`"us"`, `"de_CH"`, …), NUL-padded.
pub const LAYOUT_NAME_LEN: usize = 16;

/// Blob magic: ASCII `"SLKB"` little-endian. Guards `SYSCALL_KEYMAP_LOAD`.
pub const LAYOUT_MAGIC: u32 = 0x424B_4C53;

/// Binary format version. Bump on any incompatible `LayoutTable` layout change.
pub const LAYOUT_VERSION: u16 = 1;

/// High bit of a [`Cell`] marking it as a dead key rather than a literal.
const CELL_DEAD_FLAG: u32 = 0x8000_0000;

/// One level of one key. Transparent `u32`: `0` = none, `CELL_DEAD_FLAG | idx` =
/// dead key `idx`, otherwise a literal Unicode scalar. Control characters are
/// layout-independent and never stored here.
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
    pub const NONE: Cell = Cell(0);

    /// A literal codepoint cell (`0` collapses to [`Cell::NONE`]).
    pub const fn literal(cp: u32) -> Cell {
        Cell(cp & !CELL_DEAD_FLAG)
    }

    pub const fn dead(idx: u8) -> Cell {
        Cell(CELL_DEAD_FLAG | idx as u32)
    }

    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LayoutTable {
    pub magic: u32,
    pub version: u16,
    /// Number of valid entries in `compose` (`0..=MAX_COMPOSE`).
    pub num_compose: u16,
    /// Short name (NUL-padded), e.g. `b"de_CH"`.
    pub name: [u8; LAYOUT_NAME_LEN],
    /// Per-key caps-affected flag: nonzero ⇒ CapsLock case-folds this key.
    pub caps: [u8; NUM_KEYS],
    pub levels: [[Cell; NUM_LEVELS]; NUM_KEYS],
    /// Bare spacing-accent codepoint per declared dead key (`0` = undeclared).
    pub dead_accent: [u32; MAX_DEADKEYS],
    /// Compose table (only the first `num_compose` entries are valid).
    pub compose: [ComposeEntry; MAX_COMPOSE],
}

impl LayoutTable {
    /// A zeroed table with a valid header.
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

    /// Short name up to the first NUL; `""` on invalid UTF-8.
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

    pub fn compose_lookup(&self, dead: u8, base: u32) -> Option<u32> {
        let n = (self.num_compose as usize).min(MAX_COMPOSE);
        for entry in &self.compose[..n] {
            if entry.dead == dead && entry.base == base {
                return Some(entry.result);
            }
        }
        None
    }

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
