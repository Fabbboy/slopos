//! Pure input model for the terminal emulator: key encoding, pointer
//! selection, paste sanitizing, and the compositor-event taxonomy.
//!
//! Keys become PTY-master byte sequences (printable / control passthrough plus
//! the kernel's baked 0x80-0x88 navigation codes mapped to CSI). Pointer drags
//! select a cell rectangle; release copies to the clipboard; paste arrives as
//! bracketed-paste bytes. This module is host-testable: it touches no syscalls,
//! no protocol wire types, and no font globals — cell metrics arrive as plain
//! `i32` arguments. The userland app owns the `classify(ProtocolEvent)` bridge
//! and the actual IO.

use slopos_abi::input::{MODIFIER_CTRL, MODIFIER_SHIFT};

use super::grid::TerminalGrid;

// Kernel-baked navigation key codes (the compositor reports these as the
// "ascii" byte for non-text keys; mirror the shell exec.rs encoder table).
const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const MOUSE_LEFT: u8 = 0x01;

/// Number of scrollback lines a single Shift+PgUp / Shift+PgDn moves.
const SCROLLBACK_PAGE_LINES: usize = 10;

/// What the event loop should do after a compositor key event.
pub enum KeyAction {
    /// Write these bytes to the PTY master.
    ToMaster(KeyBytes),
    /// Scroll the local scrollback view (Shift+PgUp/PgDn).
    ScrollUp(usize),
    ScrollDown(usize),
    /// Ctrl+Shift+C: copy the pointer selection to the compositor clipboard.
    CopySelection,
    /// Ctrl+Shift+V: ask the compositor for the clipboard contents (the
    /// `PasteResult` reply feeds the paste writer).
    RequestPaste,
    /// Nothing to do.
    None,
}

/// A short, owned byte sequence for a single key (avoids heap churn per key).
pub struct KeyBytes {
    buf: [u8; 8],
    len: usize,
}

impl KeyBytes {
    fn one(b: u8) -> Self {
        let mut buf = [0u8; 8];
        buf[0] = b;
        Self { buf, len: 1 }
    }

    fn seq(s: &[u8]) -> Self {
        let mut buf = [0u8; 8];
        let len = s.len().min(8);
        buf[..len].copy_from_slice(&s[..len]);
        Self { buf, len }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// A selection endpoint anchored to a stable content coordinate: an absolute
/// line number (see [`TerminalGrid::screen_to_abs`]) plus a column. Anchoring
/// to content rather than a screen cell is what makes a copy survive scrolling
/// — the anchor keeps naming the same text after output pushes it up or the
/// user pages through history.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub line: u64,
    pub col: usize,
}

impl Anchor {
    const ZERO: Self = Self { line: 0, col: 0 };

    #[inline]
    fn key(&self) -> (u64, usize) {
        (self.line, self.col)
    }
}

/// Pointer-driven cell selection over the terminal's content.
///
/// `anchor` is where the drag began, `head` the current drag point — both in
/// absolute content coordinates. `active` is false until a drag produces a
/// non-empty range.
pub struct Selection {
    pub anchor: Anchor,
    pub head: Anchor,
    pub active: bool,
}

impl Selection {
    pub const NONE: Self = Self {
        anchor: Anchor::ZERO,
        head: Anchor::ZERO,
        active: false,
    };

    pub fn clear(&mut self) {
        *self = Self::NONE;
    }

    pub fn is_active(&self) -> bool {
        self.active && self.anchor != self.head
    }

    /// Ordered `(lo, hi)` anchors with `hi` exclusive at cell granularity, or
    /// `None` when inactive. Ordering is lexicographic on `(line, col)`.
    pub fn ordered(&self) -> Option<(Anchor, Anchor)> {
        if !self.is_active() {
            return None;
        }
        Some(if self.anchor.key() <= self.head.key() {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        })
    }
}

/// Pointer tracking state used to drive selection during the event loop.
pub struct PointerState {
    pub last_x: i32,
    pub last_y: i32,
    pub has_focus: bool,
    pub button_state: u8,
    pub prev_left: bool,
    pub dragging: bool,
}

impl PointerState {
    pub const fn new() -> Self {
        Self {
            last_x: 0,
            last_y: 0,
            has_focus: false,
            button_state: 0,
            prev_left: false,
            dragging: false,
        }
    }

    pub fn left_pressed(&self) -> bool {
        self.has_focus && (self.button_state & MOUSE_LEFT) != 0
    }
}

/// Convert a pixel coordinate to a clamped `(screen_row, col)` cell. `cell_w`/
/// `cell_h` are the font metrics supplied by the caller (the app reads them
/// from its glyph atlas; the core stays font-agnostic).
fn pixel_to_cell(
    px: i32,
    py: i32,
    cell_w: i32,
    cell_h: i32,
    grid: &TerminalGrid,
) -> (usize, usize) {
    let cw = cell_w.max(1);
    let ch = cell_h.max(1);
    let col = (px / cw).clamp(0, grid.cols as i32 - 1) as usize;
    let row = (py / ch).clamp(0, grid.rows as i32 - 1) as usize;
    (row, col)
}

/// Capture the content anchor under a pixel coordinate (screen row resolved to
/// an absolute line via the grid's current view).
fn pixel_to_anchor(px: i32, py: i32, cell_w: i32, cell_h: i32, grid: &TerminalGrid) -> Anchor {
    let (row, col) = pixel_to_cell(px, py, cell_w, cell_h, grid);
    Anchor {
        line: grid.screen_to_abs(row),
        col,
    }
}

/// Encode a compositor key event into a master action.
///
/// PageUp/PageDown drive the terminal's own scrollback view (the kernel
/// keyboard driver already intercepts Shift+PageUp/Down for the in-kernel
/// vconsole, so only the plain codes ever reach a compositor client).
///
/// `mods` is the latest compositor-reported modifier state. Ctrl+Shift chords
/// are terminal commands (the xterm/GNOME clipboard convention), never PTY
/// input: the kernel bakes the same control byte for Ctrl+C and Ctrl+Shift+C,
/// so the modifier state is the only way to tell them apart.
pub fn encode_key(ascii: u8, scancode: u8, mods: u8) -> KeyAction {
    const CHORD: u8 = MODIFIER_CTRL | MODIFIER_SHIFT;
    if mods & CHORD == CHORD {
        match ascii {
            0x03 => return KeyAction::CopySelection, // Ctrl+Shift+C
            0x16 => return KeyAction::RequestPaste,  // Ctrl+Shift+V
            _ => {}
        }
    }

    if ascii != 0 {
        match ascii {
            KEY_PAGE_UP => return KeyAction::ScrollUp(SCROLLBACK_PAGE_LINES),
            KEY_PAGE_DOWN => return KeyAction::ScrollDown(SCROLLBACK_PAGE_LINES),
            // TEMPORARY bare-metal scrollback keys: many legacy/emulated PS/2
            // keyboards don't deliver Page Up/Down (E0-prefixed) reliably, so
            // bind `]`/`\` as plain-ASCII scroll up/down. These shadow the
            // literal characters in the terminal — remove once PgUp/PgDn work.
            b']' => return KeyAction::ScrollUp(SCROLLBACK_PAGE_LINES),
            b'\\' => return KeyAction::ScrollDown(SCROLLBACK_PAGE_LINES),
            KEY_UP => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[A")),
            KEY_DOWN => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[B")),
            KEY_LEFT => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[D")),
            KEY_RIGHT => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[C")),
            KEY_HOME => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[H")),
            KEY_END => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[F")),
            KEY_DELETE => return KeyAction::ToMaster(KeyBytes::seq(b"\x1b[3~")),
            // Everything else (printable text, control bytes like 0x03 Ctrl+C,
            // 0x04 Ctrl+D, CR/LF, BS/DEL, tab) passes straight through to the
            // PTY master where the kernel line discipline handles it.
            _ => return KeyAction::ToMaster(KeyBytes::one(ascii)),
        }
    }

    // Non-ASCII keys arriving without a baked code: translate scancodes the
    // same way the shell's legacy encoder did.
    let seq: &[u8] = match scancode {
        0x82 => b"\x1b[A",
        0x83 => b"\x1b[B",
        0x84 => b"\x1b[D",
        0x85 => b"\x1b[C",
        0x86 => b"\x1b[H",
        0x87 => b"\x1b[F",
        0x88 => b"\x1b[3~",
        0x80 => b"\x1b[5~",
        0x81 => b"\x1b[6~",
        _ => &[],
    };
    if seq.is_empty() {
        KeyAction::None
    } else {
        KeyAction::ToMaster(KeyBytes::seq(seq))
    }
}

/// What a (non-key) compositor event resolved to.
pub enum CompositorEvent {
    Key(u8, u8),
    /// Keyboard modifier state changed (bitfield of `MODIFIER_*`).
    Modifiers(u8),
    /// Configure: new pixel dimensions.
    Resize(i32, i32),
    Close,
    PointerMotion(i32, i32),
    PointerEnter(i32, i32),
    PointerLeave,
    PointerButton {
        pressed: bool,
        code: u8,
    },
    /// The compositor reports the clipboard holds this many bytes; the app
    /// should provide a destination memfd of that size (0 = empty, no-op).
    PasteReady(u32),
    /// The destination memfd handed to the compositor now holds this many
    /// valid clipboard bytes, ready to write to the PTY master.
    PasteResult(u32),
    Ignored,
}

/// Update pointer-driven selection from a button/motion change. Returns true
/// when the selection changed (so the caller re-renders). `cell_w`/`cell_h`
/// are the font metrics the caller reads from its glyph atlas.
pub fn update_selection(
    ptr: &mut PointerState,
    selection: &mut Selection,
    grid: &TerminalGrid,
    cell_w: i32,
    cell_h: i32,
) -> bool {
    let left = ptr.left_pressed();
    let newly_pressed = left && !ptr.prev_left;
    let newly_released = !left && ptr.prev_left;
    let mut changed = false;

    if newly_pressed {
        let a = pixel_to_anchor(ptr.last_x, ptr.last_y, cell_w, cell_h, grid);
        selection.anchor = a;
        selection.head = a;
        selection.active = true;
        ptr.dragging = true;
        changed = true;
    } else if ptr.dragging && left {
        let h = pixel_to_anchor(ptr.last_x, ptr.last_y, cell_w, cell_h, grid);
        if h != selection.head {
            selection.head = h;
            changed = true;
        }
    }

    if newly_released && ptr.dragging {
        ptr.dragging = false;
        if selection.anchor == selection.head {
            selection.clear();
            changed = true;
        }
    }

    ptr.prev_left = left;
    changed
}

/// Sanitize a clipboard payload so it can only ever act as typed text
/// (the VTE/kitty paste rule, applied to bracketed and plain pastes alike):
/// `\r\n` and `\n` normalize to `\r` (a typed Enter), `\t` passes, every
/// other C0 control and DEL is dropped, bytes above 0x7F pass through for
/// future UTF-8 text. Returns the sanitized length in `out`.
///
/// Dropping ESC outright is what makes bracketed paste injection-proof
/// (the xterm CVE-2022-45063 class): with no ESC byte in the payload, the
/// `\x1b[201~` end marker cannot appear — not literally, and not spliced
/// together from fragments around a stripped inner marker, the bypass that
/// defeats remove-the-marker filtering.
pub fn sanitize_paste(data: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < data.len() && n < out.len() {
        let b = data[i];
        match b {
            b'\r' => {
                out[n] = b'\r';
                n += 1;
                // Swallow the \n of a \r\n pair.
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => {
                out[n] = b'\r';
                n += 1;
            }
            b'\t' | 0x20..=0x7E | 0x80.. => {
                out[n] = b;
                n += 1;
            }
            // Remaining C0 controls (incl. ESC) and DEL: dropped.
            _ => {}
        }
        i += 1;
    }
    n
}

/// Upper bound on the byte length [`collect_selection`] can produce for this
/// selection: one byte per cell across every selected line plus a newline per
/// line. The caller sizes its clipboard memfd to this before collecting.
pub fn selection_byte_bound(grid: &TerminalGrid, selection: &Selection) -> usize {
    let cols = grid.cols as usize;
    match selection.ordered() {
        Some((lo, hi)) => {
            let lines = (hi.line - lo.line + 1) as usize;
            lines.saturating_mul(cols + 1)
        }
        None => 0,
    }
}

/// Extract the selected text into `out`, returning the number of bytes
/// captured (bounded only by `out.len()` — the caller sizes the buffer to the
/// selection). Reads by absolute content line via [`TerminalGrid::abs_cell`],
/// so the text is the originally-selected content regardless of the current
/// scroll position. Trailing blanks on each row are trimmed; multi-row
/// selections join with `\n`; lines evicted from scrollback yield blanks
/// rather than panicking.
pub fn collect_selection(grid: &TerminalGrid, selection: &Selection, out: &mut [u8]) -> usize {
    let cols = grid.cols as usize;
    let Some((lo, hi)) = selection.ordered() else {
        return 0;
    };
    let cap = out.len();

    // `hi` is exclusive: when it sits at column 0 the final line contributes
    // nothing, so the last line with content is `hi.line - 1`. `is_active`
    // guarantees `hi.col > 0` whenever `hi.line == lo.line`.
    let last_line = if hi.col == 0 {
        hi.line.wrapping_sub(1)
    } else {
        hi.line
    };

    let mut n = 0usize;
    let mut line = lo.line;
    let mut first = true;
    while line <= last_line && n < cap {
        if !first {
            out[n] = b'\n';
            n += 1;
            if n >= cap {
                break;
            }
        }
        first = false;

        let start_col = if line == lo.line { lo.col } else { 0 };
        let end_col = if line == hi.line { hi.col } else { cols };
        let mut last_nonblank = n;
        let mut col = start_col;
        while col < end_col && n < cap {
            let cp = grid.abs_cell(line, col).glyph();
            let byte = if (0x20..=0x7E).contains(&cp) {
                cp as u8
            } else if cp == b'\t' as u32 {
                b'\t'
            } else {
                b' '
            };
            out[n] = byte;
            n += 1;
            if byte != b' ' {
                last_nonblank = n;
            }
            col += 1;
        }
        // Trim trailing blanks on this row.
        n = last_nonblank;
        line = line.wrapping_add(1);
    }
    n
}

/// Whether absolute cell `(abs_line, col)` lies within the ordered selection
/// range `[lo, hi)` (lexicographic on `(line, col)`, `hi` exclusive). The
/// renderer converts each visible row to its absolute line and calls this to
/// shade selected cells.
pub fn cell_in_selection(abs_line: u64, col: usize, lo: Anchor, hi: Anchor) -> bool {
    let pos = (abs_line, col);
    pos >= lo.key() && pos < hi.key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const CTRL_SHIFT: u8 = MODIFIER_CTRL | MODIFIER_SHIFT;

    fn master_bytes(action: KeyAction) -> Vec<u8> {
        match action {
            KeyAction::ToMaster(b) => b.as_bytes().to_vec(),
            _ => panic!("expected ToMaster"),
        }
    }

    #[test]
    fn ctrl_shift_c_copies_instead_of_sigint() {
        assert!(matches!(
            encode_key(0x03, 0x2E, CTRL_SHIFT),
            KeyAction::CopySelection
        ));
    }

    /// Plain Ctrl+C (no Shift) must reach the PTY master so the ldisc can
    /// raise SIGINT. Regression lock for the stuck-SHIFT modifier bug: the
    /// kernel routed modifier *releases* with the raw break code (make |
    /// 0x80), no tracker cleared the bit, and after the first shifted
    /// keystroke (e.g. the ':' in a curl URL) every Ctrl+C was
    /// misclassified as the Ctrl+Shift+C copy chord and silently swallowed.
    #[test]
    fn ctrl_only_c_reaches_master_as_sigint_byte() {
        assert_eq!(
            master_bytes(encode_key(0x03, 0x2E, MODIFIER_CTRL)),
            alloc::vec![0x03]
        );
    }

    #[test]
    fn ctrl_shift_v_requests_paste() {
        assert!(matches!(
            encode_key(0x16, 0x2F, CTRL_SHIFT),
            KeyAction::RequestPaste
        ));
    }

    #[test]
    fn plain_ctrl_c_passes_through_to_ldisc() {
        assert_eq!(master_bytes(encode_key(0x03, 0x2E, MODIFIER_CTRL)), [0x03]);
    }

    #[test]
    fn shift_only_c_is_plain_text() {
        assert_eq!(master_bytes(encode_key(b'C', 0x2E, MODIFIER_SHIFT)), [b'C']);
    }

    #[test]
    fn ctrl_shift_other_keys_still_reach_master() {
        // Ctrl+Shift+A (0x01) is not a clipboard chord; the ldisc gets it.
        assert_eq!(master_bytes(encode_key(0x01, 0x1E, CTRL_SHIFT)), [0x01]);
    }

    #[test]
    fn baked_navigation_codes_map_to_csi() {
        assert_eq!(master_bytes(encode_key(KEY_UP, 0, 0)), b"\x1b[A");
        assert_eq!(master_bytes(encode_key(KEY_DOWN, 0, 0)), b"\x1b[B");
        assert_eq!(master_bytes(encode_key(KEY_LEFT, 0, 0)), b"\x1b[D");
        assert_eq!(master_bytes(encode_key(KEY_RIGHT, 0, 0)), b"\x1b[C");
        assert_eq!(master_bytes(encode_key(KEY_HOME, 0, 0)), b"\x1b[H");
        assert_eq!(master_bytes(encode_key(KEY_END, 0, 0)), b"\x1b[F");
        assert_eq!(master_bytes(encode_key(KEY_DELETE, 0, 0)), b"\x1b[3~");
    }

    #[test]
    fn page_keys_drive_local_scrollback() {
        assert!(matches!(
            encode_key(KEY_PAGE_UP, 0, 0),
            KeyAction::ScrollUp(SCROLLBACK_PAGE_LINES)
        ));
        assert!(matches!(
            encode_key(KEY_PAGE_DOWN, 0, 0),
            KeyAction::ScrollDown(SCROLLBACK_PAGE_LINES)
        ));
    }

    #[test]
    fn zero_ascii_falls_back_to_scancode_table() {
        assert_eq!(master_bytes(encode_key(0, 0x82, 0)), b"\x1b[A");
        assert_eq!(master_bytes(encode_key(0, 0x80, 0)), b"\x1b[5~");
        assert!(matches!(encode_key(0, 0x42, 0), KeyAction::None));
    }

    #[test]
    fn paste_cannot_inject_bracket_end_marker() {
        let mut out = [0u8; 64];
        // Literal end marker: the ESC is dropped, leaving inert text.
        let n = sanitize_paste(b"safe\x1b[201~rm -rf /\r", &mut out);
        assert_eq!(&out[..n], b"safe[201~rm -rf /\r");
        // Splice attack: ESC + (marker) + "[201~" must NOT reassemble a
        // marker after filtering — no ESC survives, so it cannot.
        let n = sanitize_paste(b"\x1b\x1b[201~[201~x", &mut out);
        assert_eq!(&out[..n], b"[201~[201~x");
        assert!(!out[..n].windows(6).any(|w| w == b"\x1b[201~"));
    }

    #[test]
    fn paste_normalizes_newlines_and_drops_controls() {
        let mut out = [0u8; 64];
        let n = sanitize_paste(b"one\r\ntwo\nthree", &mut out);
        assert_eq!(&out[..n], b"one\rtwo\rthree");
        // Ctrl bytes, ESC, and DEL must not be typeable from a clipboard.
        let n = sanitize_paste(b"a\x03b\x1b[Ac\x7fd\te", &mut out);
        assert_eq!(&out[..n], b"ab[Acd\te");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::grid::TerminalGrid;
    use alloc::vec::Vec;

    const CW: i32 = 8;
    const CH: i32 = 16;

    fn feed(g: &mut TerminalGrid, s: &[u8]) {
        for &b in s {
            g.process_byte(b);
        }
    }

    /// Simulate a press at screen `(r0,c0)`, drag to `(r1,c1)`, release.
    fn drag_select(
        g: &TerminalGrid,
        sel: &mut Selection,
        r0: usize,
        c0: usize,
        r1: usize,
        c1: usize,
    ) {
        let mut ptr = PointerState::new();
        ptr.has_focus = true;
        ptr.button_state = MOUSE_LEFT;
        ptr.last_x = c0 as i32 * CW;
        ptr.last_y = r0 as i32 * CH;
        update_selection(&mut ptr, sel, g, CW, CH);
        ptr.last_x = c1 as i32 * CW;
        ptr.last_y = r1 as i32 * CH;
        update_selection(&mut ptr, sel, g, CW, CH);
        ptr.button_state = 0;
        update_selection(&mut ptr, sel, g, CW, CH);
    }

    fn copied(g: &TerminalGrid, sel: &Selection) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let n = collect_selection(g, sel, &mut buf);
        buf[..n].to_vec()
    }

    #[test]
    fn screen_abs_round_trip_at_view_zero() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"a\r\nb\r\nc\r\nd");
        for r in 0..5 {
            assert_eq!(g.abs_to_screen(g.screen_to_abs(r)), Some(r));
        }
        // Row 0 is the absolute origin when not scrolled.
        assert_eq!(g.screen_to_abs(0), 0);
    }

    #[test]
    fn screen_abs_round_trip_while_scrolled() {
        let mut g = TerminalGrid::new(3, 8);
        // Force several evictions so there is history to page into.
        for i in 0..10 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        g.scroll_view_up(2);
        for r in 0..3 {
            let abs = g.screen_to_abs(r);
            assert_eq!(g.abs_to_screen(abs), Some(r), "row {r} must round-trip");
        }
    }

    #[test]
    fn total_scrolled_tracks_evictions() {
        let mut g = TerminalGrid::new(3, 8);
        // 3 rows; each newline past the bottom evicts one line. Seven
        // CRLF-terminated lines push the cursor past the bottom five times
        // (the trailing CRLF of the last line scrolls once more), so five
        // lines reach history.
        for i in 0..7 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        // Origin advanced by exactly the number of lines pushed to history,
        // and the absolute numbering stays consistent (abs of row 0 == L5).
        assert_eq!(g.screen_to_abs(0), 5);
        let mut buf = [0u8; 16];
        let n = {
            let cell_line = g.screen_to_abs(0);
            let mut k = 0;
            for col in 0..8 {
                let cp = g.abs_cell(cell_line, col).glyph();
                if (0x21..=0x7e).contains(&cp) {
                    buf[k] = cp as u8;
                    k += 1;
                }
            }
            k
        };
        assert_eq!(&buf[..n], b"L5");
    }

    #[test]
    fn copy_survives_live_output_scroll() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"AAAA\r\nBBBB\r\nCCCC");
        let mut sel = Selection::NONE;
        // Select the "AAAA" on screen row 0.
        drag_select(&g, &mut sel, 0, 0, 0, 4);
        assert_eq!(copied(&g, &sel), b"AAAA");

        // Flood output so AAAA scrolls off the live region into history.
        for i in 0..8 {
            feed(&mut g, alloc::format!("X{i}\r\n").as_bytes());
        }
        // The anchored selection still yields the originally-selected text —
        // NOT whatever now occupies screen row 0.
        assert_eq!(copied(&g, &sel), b"AAAA");
    }

    #[test]
    fn copy_survives_view_scroll() {
        let mut g = TerminalGrid::new(4, 10);
        feed(&mut g, b"FIRST\r\nSECOND\r\nTHIRD");
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 0, 0, 0, 5);
        assert_eq!(copied(&g, &sel), b"FIRST");

        // Push FIRST into history, then page the view up to look at it.
        for i in 0..6 {
            feed(&mut g, alloc::format!("Y{i}\r\n").as_bytes());
        }
        g.scroll_view_up(3);
        assert_eq!(copied(&g, &sel), b"FIRST");
    }

    #[test]
    fn multi_row_selection_joins_and_trims() {
        let mut g = TerminalGrid::new(5, 10);
        feed(&mut g, b"hello\r\nworld");
        let mut sel = Selection::NONE;
        // Select from row 0 col 0 through row 1 col 5 (end of "world").
        drag_select(&g, &mut sel, 0, 0, 1, 5);
        assert_eq!(copied(&g, &sel), b"hello\nworld");
    }

    #[test]
    fn capture_stores_absolute_line() {
        let mut g = TerminalGrid::new(3, 8);
        for i in 0..6 {
            feed(&mut g, alloc::format!("L{i}\r\n").as_bytes());
        }
        let origin = g.screen_to_abs(0);
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 1, 0, 1, 2);
        // The anchor names the absolute line of screen row 1, not row 1 itself.
        let (lo, _hi) = sel.ordered().unwrap();
        assert_eq!(lo.line, origin + 1);
    }

    #[test]
    fn evicted_selection_degrades_without_panic() {
        let mut g = TerminalGrid::new(2, 6);
        feed(&mut g, b"keep\r\n");
        let mut sel = Selection::NONE;
        drag_select(&g, &mut sel, 0, 0, 0, 4);
        assert_eq!(copied(&g, &sel), b"keep");
        // Evict far beyond the 1000-line ring so abs 0 is gone.
        for _ in 0..1100 {
            feed(&mut g, b"\r\n");
        }
        // No panic; the lost line yields blanks (trimmed to nothing).
        assert!(copied(&g, &sel).is_empty());
    }

    #[test]
    fn cell_in_selection_is_half_open() {
        let lo = Anchor { line: 2, col: 3 };
        let hi = Anchor { line: 4, col: 1 };
        assert!(!cell_in_selection(2, 2, lo, hi)); // before lo
        assert!(cell_in_selection(2, 3, lo, hi)); // at lo (inclusive)
        assert!(cell_in_selection(3, 9, lo, hi)); // interior line
        assert!(cell_in_selection(4, 0, lo, hi)); // up to hi.col
        assert!(!cell_in_selection(4, 1, lo, hi)); // at hi (exclusive)
    }
}
