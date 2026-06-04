//! Compositor input handling: key encoding, resize, close, and pointer
//! selection / clipboard for the terminal emulator.
//!
//! Keys become PTY-master byte sequences (printable / control passthrough plus
//! the kernel's baked 0x80-0x88 navigation codes mapped to CSI). Configure
//! events resize the grid and push the new winsize to the kernel. Pointer
//! drags select a cell rectangle; release copies to the clipboard; paste
//! arrives as bracketed-paste bytes.

use slopos_abi::input::{MODIFIER_CTRL, MODIFIER_SHIFT};
use slopos_protocol::types::Event as ProtocolEvent;

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

/// Maximum bytes copied to / pasted from the clipboard.
pub const CLIPBOARD_CAP: usize = 4096;

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

/// Pointer-driven cell selection over the rendered grid.
///
/// `start`/`end` are linearized cell offsets (`row * cols + col`) captured at
/// the grid width in effect when the drag began. `active` is false until a
/// drag produces a non-empty range.
pub struct Selection {
    pub start: usize,
    pub end: usize,
    pub active: bool,
}

impl Selection {
    pub const NONE: Self = Self {
        start: 0,
        end: 0,
        active: false,
    };

    pub fn clear(&mut self) {
        *self = Self::NONE;
    }

    pub fn is_active(&self) -> bool {
        self.active && self.start != self.end
    }

    /// Ordered selection as `((r0, c0), (r1, c1))` cell coordinates, or `None`
    /// when inactive. `c1`/`r1` is the exclusive end (one past the last cell).
    pub fn ordered_cells(&self, cols: usize) -> Option<((usize, usize), (usize, usize))> {
        if !self.is_active() || cols == 0 {
            return None;
        }
        let (lo, hi) = if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };
        Some(((lo / cols, lo % cols), (hi / cols, hi % cols)))
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

/// Convert a pixel coordinate to a clamped grid cell offset.
fn pixel_to_cell(px: i32, py: i32, grid: &TerminalGrid) -> usize {
    let cw = crate::gfx::font::cell_width().max(1);
    let ch = crate::gfx::font::cell_height().max(1);
    let col = (px / cw).clamp(0, grid.cols as i32 - 1) as usize;
    let row = (py / ch).clamp(0, grid.rows as i32 - 1) as usize;
    row * grid.cols as usize + col
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
    /// A pasted clipboard payload arrived (length-prefixed bytes).
    Paste(KeyBytes2),
    Ignored,
}

/// A larger owned buffer for pasted clipboard content.
pub struct KeyBytes2 {
    pub buf: [u8; CLIPBOARD_CAP],
    pub len: usize,
}

/// Translate a raw protocol event into a terminal-facing `CompositorEvent`.
pub fn classify(evt: &ProtocolEvent) -> CompositorEvent {
    match evt {
        ProtocolEvent::Key {
            scancode,
            ascii,
            pressed,
            ..
        } => {
            if *pressed {
                CompositorEvent::Key(*ascii as u8, *scancode as u8)
            } else {
                CompositorEvent::Ignored
            }
        }
        ProtocolEvent::Modifiers { mods } => CompositorEvent::Modifiers(*mods as u8),
        ProtocolEvent::PointerMotion { x, y, .. } => CompositorEvent::PointerMotion(*x, *y),
        ProtocolEvent::PointerEnter { x, y, .. } => CompositorEvent::PointerEnter(*x, *y),
        ProtocolEvent::PointerLeave { .. } => CompositorEvent::PointerLeave,
        ProtocolEvent::PointerButton {
            button, pressed, ..
        } => CompositorEvent::PointerButton {
            pressed: *pressed,
            code: *button as u8,
        },
        ProtocolEvent::Configure { width, height, .. } => {
            CompositorEvent::Resize(*width as i32, *height as i32)
        }
        ProtocolEvent::Close { .. } => CompositorEvent::Close,
        ProtocolEvent::PasteResult(cb) => {
            let mut out = KeyBytes2 {
                buf: [0u8; CLIPBOARD_CAP],
                len: 0,
            };
            let n = (cb.len as usize).min(CLIPBOARD_CAP);
            out.buf[..n].copy_from_slice(&cb.data[..n]);
            out.len = n;
            CompositorEvent::Paste(out)
        }
        _ => CompositorEvent::Ignored,
    }
}

/// Update pointer-driven selection from a button/motion change. Returns true
/// when the selection changed (so the caller re-renders).
pub fn update_selection(
    ptr: &mut PointerState,
    selection: &mut Selection,
    grid: &TerminalGrid,
) -> bool {
    let left = ptr.left_pressed();
    let newly_pressed = left && !ptr.prev_left;
    let newly_released = !left && ptr.prev_left;
    let mut changed = false;

    if newly_pressed {
        let off = pixel_to_cell(ptr.last_x, ptr.last_y, grid);
        selection.start = off;
        selection.end = off;
        selection.active = true;
        ptr.dragging = true;
        changed = true;
    } else if ptr.dragging && left {
        let off = pixel_to_cell(ptr.last_x, ptr.last_y, grid);
        if off != selection.end {
            selection.end = off;
            changed = true;
        }
    }

    if newly_released && ptr.dragging {
        ptr.dragging = false;
        if selection.start == selection.end {
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

/// Extract the selected text from the live grid into `out`, returning the
/// number of bytes captured (capped at [`CLIPBOARD_CAP`]). Trailing blanks on
/// each selected row are trimmed; multi-row selections join with `\n`.
pub fn collect_selection(grid: &TerminalGrid, selection: &Selection, out: &mut [u8]) -> usize {
    let cols = grid.cols as usize;
    let Some(((r0, c0), (r1, c1))) = selection.ordered_cells(cols) else {
        return 0;
    };
    let cap = out.len().min(CLIPBOARD_CAP);
    let mut n = 0usize;

    let lo = r0 * cols + c0;
    let hi = r1 * cols + c1;
    let mut pos = lo;
    let mut row_start_n = 0usize;
    let mut last_nonblank = 0usize;
    let mut cur_row = r0;

    while pos < hi && n < cap {
        let row = pos / cols;
        let col = pos % cols;
        if row != cur_row {
            // Row boundary: trim trailing blanks, then emit newline.
            n = last_nonblank;
            if n < cap {
                out[n] = b'\n';
                n += 1;
            }
            row_start_n = n;
            last_nonblank = n;
            cur_row = row;
        }
        if n >= cap {
            break;
        }
        let cp = grid.visible_cell(row, col).glyph();
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
        pos += 1;
    }
    // Trim trailing blanks on the final row.
    if last_nonblank >= row_start_n {
        n = last_nonblank;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

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
