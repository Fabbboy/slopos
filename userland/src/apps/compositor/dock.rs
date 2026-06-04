//! Launcher shelf (dock) — rendering, layout, magnification, and hit-testing
//! for the bottom launcher bar.

use slopos_abi::draw::Color32;
use slopos_font::FontRenderer;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_ENTRIES: usize = 16;

/// Font size for the hover label tooltip (bitmap fallback uses cell size).
const LABEL_FONT_SIZE: u16 = 11;

/// Font size for the single-character icon label.
const ICON_LABEL_FONT_SIZE: u16 = 20;

// ---------------------------------------------------------------------------
// ShelfEntry
// ---------------------------------------------------------------------------

/// A single shelf entry representing a pinned or running application.
pub struct ShelfEntry {
    pub name: [u8; 32],
    pub name_len: usize,
    pub program_path: [u8; 64],
    pub path_len: usize,
    pub pinned: bool,
    pub running: bool,
    pub task_id: u32,
    pub icon_color: u32,
    pub icon_letter: u8,
    pub app_id: [u8; 32],
    pub app_id_len: usize,
}

impl ShelfEntry {
    const fn empty() -> Self {
        Self {
            name: [0u8; 32],
            name_len: 0,
            program_path: [0u8; 64],
            path_len: 0,
            pinned: false,
            running: false,
            task_id: 0,
            icon_color: 0,
            icon_letter: 0,
            app_id: [0u8; 32],
            app_id_len: 0,
        }
    }

    /// Return the name as a `&str` (up to the stored length).
    fn name_str(&self) -> &str {
        let len = self.name_len.min(self.name.len());
        // Safety: we only store valid ASCII in name.
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Helpers to build entries from static data
// ---------------------------------------------------------------------------

fn make_pinned(
    name: &[u8],
    path: &[u8],
    app_id: &[u8],
    color: Color32,
    alpha: u8,
    letter: u8,
) -> ShelfEntry {
    let mut entry = ShelfEntry::empty();
    let n = name.len().min(entry.name.len());
    entry.name[..n].copy_from_slice(&name[..n]);
    entry.name_len = n;
    let p = path.len().min(entry.program_path.len());
    entry.program_path[..p].copy_from_slice(&path[..p]);
    entry.path_len = p;
    entry.pinned = true;
    entry.icon_color = argb(color, alpha);
    entry.icon_letter = letter;
    let a = app_id.len().min(entry.app_id.len());
    entry.app_id[..a].copy_from_slice(&app_id[..a]);
    entry.app_id_len = a;
    entry
}

// ---------------------------------------------------------------------------
// Per-icon layout data (computed each frame)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct IconLayout {
    /// Center X of this icon in screen coordinates.
    center_x: i32,
    /// Top Y of this icon (icons grow upward from a common bottom line).
    top_y: i32,
    /// Rendered size after magnification scaling.
    size: i32,
}

// ---------------------------------------------------------------------------
// LauncherShelf
// ---------------------------------------------------------------------------

/// Launcher shelf state and rendering.
pub struct LauncherShelf {
    entries: [ShelfEntry; MAX_ENTRIES],
    entry_count: usize,
    hovered_index: Option<usize>,
    last_bounds: DamageRect,
    /// Set when shelf entries change (app opens/closes); cleared by `take_content_dirty()`.
    content_dirty: bool,
}

impl LauncherShelf {
    pub fn new() -> Self {
        Self {
            entries: [const { ShelfEntry::empty() }; MAX_ENTRIES],
            entry_count: 0,
            hovered_index: None,
            last_bounds: DamageRect::invalid(),
            content_dirty: false,
        }
    }

    /// Returns `true` and clears the flag if shelf content changed since last call.
    pub fn take_content_dirty(&mut self) -> bool {
        let dirty = self.content_dirty;
        self.content_dirty = false;
        dirty
    }

    /// Populate default pinned applications (Shell, Files, Monitor).
    /// Each entry carries an `app_id` for reliable matching; title-based
    /// matching is used as a fallback when the app_id is unavailable.
    pub fn init_defaults(&mut self) {
        self.entries[0] = make_pinned(
            b"Terminal",
            b"/bin/terminal",
            b"org.slopos.terminal",
            ICON_SHELL,
            ICON_SHELL_ALPHA,
            b'T',
        );
        self.entries[1] = make_pinned(
            b"Files",
            b"/bin/file_manager",
            b"org.slopos.files",
            ICON_FILES,
            ICON_FILES_ALPHA,
            b'F',
        );
        self.entries[2] = make_pinned(
            b"System Monitor",
            b"/bin/sysmon",
            b"org.slopos.sysmon",
            ICON_MONITOR,
            ICON_MONITOR_ALPHA,
            b'M',
        );
        self.entry_count = 3;
    }

    /// Synchronize running state from the current window list.
    ///
    /// For pinned entries, sets `running` / `task_id` if a window title matches.
    /// For non-pinned running apps, appends them after the pinned group.
    pub fn sync_running_apps(&mut self, windows: &[UserWindowInfo], count: u32) {
        // Count pinned entries (they are always at the front).
        let pinned_count = self.pinned_count();

        // Snapshot previous running state for change detection.
        let prev_count = self.entry_count;
        let mut prev_running = [false; MAX_ENTRIES];
        let mut prev_task_ids = [0u32; MAX_ENTRIES];
        for i in 0..prev_count {
            prev_running[i] = self.entries[i].running;
            prev_task_ids[i] = self.entries[i].task_id;
        }

        // Reset running state on all pinned entries.
        for i in 0..pinned_count {
            self.entries[i].running = false;
            self.entries[i].task_id = 0;
        }

        // Trim dynamic (non-pinned) entries.
        self.entry_count = pinned_count;

        let wcount = (count as usize).min(windows.len());
        for wi in 0..wcount {
            let win = &windows[wi];
            let win_title = title_bytes(&win.title);

            // Try to match against a pinned entry — prefer app_id, fall back to title.
            let mut matched_pinned = false;
            for pi in 0..pinned_count {
                let entry = &self.entries[pi];
                // Prefer app_id matching when both sides have one
                if entry.app_id_len > 0 && !win.app_id.is_empty() {
                    let entry_id = &entry.app_id[..entry.app_id_len];
                    let win_id = win.app_id.as_str().as_bytes();
                    if entry_id == win_id {
                        self.entries[pi].running = true;
                        self.entries[pi].task_id = win.task_id;
                        matched_pinned = true;
                        break;
                    }
                }
                // Fall back to title matching
                if names_match(&entry.name, entry.name_len, win_title) {
                    self.entries[pi].running = true;
                    self.entries[pi].task_id = win.task_id;
                    matched_pinned = true;
                    break;
                }
            }

            if !matched_pinned && self.entry_count < MAX_ENTRIES {
                // Add as a dynamic running entry.
                let idx = self.entry_count;
                let mut entry = ShelfEntry::empty();
                let n = win_title.len().min(entry.name.len());
                entry.name[..n].copy_from_slice(&win_title[..n]);
                entry.name_len = n;
                entry.pinned = false;
                entry.running = true;
                entry.task_id = win.task_id;
                entry.icon_color = argb(ICON_DEFAULT, ICON_DEFAULT_ALPHA);
                entry.icon_letter = if n > 0 {
                    win_title[0].to_ascii_uppercase()
                } else {
                    b'?'
                };
                self.entries[idx] = entry;
                self.entry_count += 1;
            }
        }

        // Detect if anything actually changed.
        if self.entry_count != prev_count {
            self.content_dirty = true;
        } else {
            for i in 0..self.entry_count {
                if self.entries[i].running != prev_running[i]
                    || self.entries[i].task_id != prev_task_ids[i]
                {
                    self.content_dirty = true;
                    break;
                }
            }
        }
    }

    /// Draw the shelf onto the compositing buffer.
    pub fn draw(
        &mut self,
        buf: &mut DrawBuffer,
        screen_width: u32,
        screen_height: u32,
        cursor_x: i32,
        cursor_y: i32,
        mut font: Option<&mut FontRenderer>,
        clip: Option<DamageRect>,
    ) {
        if self.entry_count == 0 {
            self.last_bounds = DamageRect::invalid();
            return;
        }

        let clip_rect = clip.unwrap_or_else(|| full_screen_clip(screen_width, screen_height));

        let pinned_count = self.pinned_count();
        let running_non_pinned = self.entry_count - pinned_count;
        let has_separator = running_non_pinned > 0;

        // -----------------------------------------------------------------
        // 1. Compute per-icon magnification scale (in 8.8 fixed-point).
        // -----------------------------------------------------------------
        let mut icon_scales = [256i32; MAX_ENTRIES]; // 256 = 1.0
        let mut icon_sizes = [SHELF_ICON_SIZE; MAX_ENTRIES];

        // We need a first pass to find un-magnified icon centers so we can
        // evaluate proximity. Base layout (no magnification) is used for
        // proximity testing.
        let base_total_w = self.base_content_width(pinned_count, has_separator);
        let base_pill_w = base_total_w + 2 * SHELF_PILL_PADDING_X;
        let base_pill_x = (screen_width as i32 - base_pill_w) / 2;
        let base_pill_y =
            screen_height as i32 - SHELF_BOTTOM_MARGIN - SHELF_ICON_SIZE - 2 * SHELF_PILL_PADDING_Y;
        let shelf_top = base_pill_y;

        // Is the cursor within the vertical proximity zone?
        let in_y_proximity =
            cursor_y >= shelf_top - MAGNIFICATION_PROXIMITY_Y && cursor_y <= screen_height as i32;

        if in_y_proximity {
            // Compute base icon center-X positions.
            let mut cx = base_pill_x + SHELF_PILL_PADDING_X;
            for i in 0..self.entry_count {
                if i == pinned_count && has_separator {
                    cx +=
                        SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
                }
                let icon_cx = cx + SHELF_ICON_SIZE / 2;
                let dist = abs_i32(cursor_x - icon_cx);

                if dist < MAGNIFICATION_PROXIMITY_X {
                    let prox = MAGNIFICATION_PROXIMITY_X;
                    let d = prox - dist;
                    // scale_256 = 256 + 84 * (PROX - dist)^2 / PROX^2
                    let scale_256 = 256 + MAGNIFICATION_AMOUNT_256 * d * d / (prox * prox);
                    icon_scales[i] = scale_256;
                    icon_sizes[i] = SHELF_ICON_SIZE * scale_256 / 256;
                }
                cx += SHELF_ICON_SIZE + SHELF_ICON_SPACING;
            }
        }

        // -----------------------------------------------------------------
        // 2. Position icons left-to-right with their actual (scaled) sizes.
        // -----------------------------------------------------------------
        let total_w = self.scaled_content_width(&icon_sizes, pinned_count, has_separator);
        let pill_w = total_w + 2 * SHELF_PILL_PADDING_X;
        let max_icon_size = max_in_slice(&icon_sizes, self.entry_count);
        let pill_h = max_icon_size + 2 * SHELF_PILL_PADDING_Y;
        let pill_x = (screen_width as i32 - pill_w) / 2;
        let pill_y = screen_height as i32 - SHELF_BOTTOM_MARGIN - pill_h;

        // Icons are bottom-aligned to: pill_y + pill_h - SHELF_PILL_PADDING_Y
        let icons_bottom = pill_y + pill_h - SHELF_PILL_PADDING_Y;

        let mut layouts = [IconLayout {
            center_x: 0,
            top_y: 0,
            size: 0,
        }; MAX_ENTRIES];
        let mut cx = pill_x + SHELF_PILL_PADDING_X;
        let mut separator_x = 0i32;

        for i in 0..self.entry_count {
            if i == pinned_count && has_separator {
                separator_x = cx + SHELF_SEPARATOR_MARGIN_X;
                cx += SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
            }
            let sz = icon_sizes[i];
            layouts[i] = IconLayout {
                center_x: cx + sz / 2,
                top_y: icons_bottom - sz,
                size: sz,
            };
            cx += sz + SHELF_ICON_SPACING;
        }

        // -----------------------------------------------------------------
        // 3. Hit-test to determine hovered icon.
        // -----------------------------------------------------------------
        self.hovered_index = None;
        for i in 0..self.entry_count {
            let l = &layouts[i];
            let left = l.center_x - l.size / 2;
            if cursor_x >= left
                && cursor_x < left + l.size
                && cursor_y >= l.top_y
                && cursor_y < icons_bottom
            {
                self.hovered_index = Some(i);
                break;
            }
        }

        // -----------------------------------------------------------------
        // 4. Draw the pill background.
        // -----------------------------------------------------------------
        let shelf_bg = Color32::new(
            SHELF_BG.red(),
            SHELF_BG.green(),
            SHELF_BG.blue(),
            SHELF_BG_ALPHA,
        );
        slopos_gfx::canvas_ops::rounded_rect_filled(
            buf,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            SHELF_PILL_RADIUS,
            shelf_bg,
        );

        // -----------------------------------------------------------------
        // 5. Draw the separator (if present).
        // -----------------------------------------------------------------
        if has_separator {
            let sep_centered_y = icons_bottom - (SHELF_ICON_SIZE + SHELF_SEPARATOR_HEIGHT) / 2;
            let sep_color = Color32::new(
                SHELF_SEPARATOR.red(),
                SHELF_SEPARATOR.green(),
                SHELF_SEPARATOR.blue(),
                SHELF_SEPARATOR_ALPHA,
            );
            gfx::fill_rect_clipped(
                buf,
                separator_x,
                sep_centered_y,
                SHELF_SEPARATOR_WIDTH,
                SHELF_SEPARATOR_HEIGHT,
                sep_color,
                &clip_rect,
            );
        }

        // -----------------------------------------------------------------
        // 6. Draw each icon.
        // -----------------------------------------------------------------
        for i in 0..self.entry_count {
            let l = &layouts[i];
            let entry = &self.entries[i];
            let icon_x = l.center_x - l.size / 2;
            let icon_y = l.top_y;

            // Icon background (rounded rect with per-app color).
            let c = Color32(entry.icon_color);
            slopos_gfx::canvas_ops::rounded_rect_filled(
                buf,
                icon_x,
                icon_y,
                l.size,
                l.size,
                SHELF_ICON_CORNER_RADIUS,
                c,
            );

            // Centered single-character label.
            let letter_str = [entry.icon_letter];
            if let Ok(s) = core::str::from_utf8(&letter_str) {
                let text_color = Color32::new(
                    TEXT_PRIMARY.red(),
                    TEXT_PRIMARY.green(),
                    TEXT_PRIMARY.blue(),
                    TEXT_PRIMARY_ALPHA,
                );

                if let Some(ref mut f) = font {
                    let (tw, th) = f.measure_text(s, ICON_LABEL_FONT_SIZE);
                    let tx = icon_x + (l.size - tw) / 2;
                    let ty = icon_y + (l.size - th) / 2;
                    f.draw_text(buf, tx, ty, s, ICON_LABEL_FONT_SIZE, text_color, c);
                } else {
                    // Bitmap fallback: 8x16 cell, center it.
                    let tw = 8;
                    let th = 16;
                    let tx = icon_x + (l.size - tw) / 2;
                    let ty = icon_y + (l.size - th) / 2;
                    gfx::draw_str_clipped(buf, tx, ty, s, text_color, c, &clip_rect);
                }
            }

            // Running indicator dot (below pill, centered under icon).
            if entry.running {
                let dot_cx = l.center_x;
                let dot_cy = pill_y + pill_h + SHELF_DOT_MARGIN_Y + SHELF_DOT_DIAMETER / 2;
                let dot_color = Color32::new(
                    SHELF_DOT_ACTIVE.red(),
                    SHELF_DOT_ACTIVE.green(),
                    SHELF_DOT_ACTIVE.blue(),
                    SHELF_DOT_ACTIVE_ALPHA,
                );
                gfx::draw_circle_filled(buf, dot_cx, dot_cy, SHELF_DOT_DIAMETER / 2, dot_color);
            }
        }

        // -----------------------------------------------------------------
        // 7. Hover label tooltip (drawn above pill).
        // -----------------------------------------------------------------
        if let Some(hi) = self.hovered_index {
            let entry = &self.entries[hi];
            let l = &layouts[hi];
            let name = entry.name_str();
            if !name.is_empty() {
                let label_bg = Color32::new(
                    SHELF_LABEL_BG.red(),
                    SHELF_LABEL_BG.green(),
                    SHELF_LABEL_BG.blue(),
                    SHELF_LABEL_BG_ALPHA,
                );
                let text_color = Color32::new(
                    TEXT_PRIMARY.red(),
                    TEXT_PRIMARY.green(),
                    TEXT_PRIMARY.blue(),
                    TEXT_PRIMARY_ALPHA,
                );

                if let Some(ref mut f) = font {
                    let (tw, th) = f.measure_text(name, LABEL_FONT_SIZE);
                    let lbl_w = tw + 2 * SHELF_LABEL_PADDING_X;
                    let lbl_h = th + 2 * SHELF_LABEL_PADDING_Y;
                    let lbl_x = l.center_x - lbl_w / 2;
                    let lbl_y = pill_y - SHELF_LABEL_GAP_Y - lbl_h;

                    slopos_gfx::canvas_ops::rounded_rect_filled(
                        buf,
                        lbl_x,
                        lbl_y,
                        lbl_w,
                        lbl_h,
                        SHELF_LABEL_RADIUS,
                        label_bg,
                    );
                    f.draw_text(
                        buf,
                        lbl_x + SHELF_LABEL_PADDING_X,
                        lbl_y + SHELF_LABEL_PADDING_Y,
                        name,
                        LABEL_FONT_SIZE,
                        text_color,
                        label_bg,
                    );
                } else {
                    // Bitmap fallback: estimate width as 8 * len.
                    let tw = 8 * name.len() as i32;
                    let th = 16i32;
                    let lbl_w = tw + 2 * SHELF_LABEL_PADDING_X;
                    let lbl_h = th + 2 * SHELF_LABEL_PADDING_Y;
                    let lbl_x = l.center_x - lbl_w / 2;
                    let lbl_y = pill_y - SHELF_LABEL_GAP_Y - lbl_h;

                    gfx::fill_rect_blended(buf, lbl_x, lbl_y, lbl_w, lbl_h, label_bg);
                    gfx::draw_str_clipped(
                        buf,
                        lbl_x + SHELF_LABEL_PADDING_X,
                        lbl_y + SHELF_LABEL_PADDING_Y,
                        name,
                        text_color,
                        label_bg,
                        &clip_rect,
                    );
                }
            }
        }

        // -----------------------------------------------------------------
        // 8. Record bounds for damage tracking.
        // -----------------------------------------------------------------
        // The total visual footprint includes the label above and dots below.
        let label_overhead = if self.hovered_index.is_some() {
            SHELF_LABEL_GAP_Y + 16 + 2 * SHELF_LABEL_PADDING_Y
        } else {
            0
        };
        let dot_overhead = SHELF_DOT_MARGIN_Y + SHELF_DOT_DIAMETER;

        self.last_bounds = DamageRect {
            x0: pill_x,
            y0: pill_y - label_overhead,
            x1: pill_x + pill_w - 1,
            y1: pill_y + pill_h + dot_overhead,
        };
    }

    /// Hit-test a screen coordinate against shelf icons.
    ///
    /// Returns the index of the entry under the cursor, or `None`.
    pub fn hit_test(&self, px: i32, py: i32) -> Option<usize> {
        if !self.last_bounds.is_valid() || self.entry_count == 0 {
            return None;
        }
        // Quick reject: outside the pill bounds (with some margin for dots).
        if px < self.last_bounds.x0
            || px > self.last_bounds.x1
            || py < self.last_bounds.y0
            || py > self.last_bounds.y1
        {
            return None;
        }
        // Use the cached hovered_index from the most recent draw pass.
        self.hovered_index
    }

    /// Return the bounding rectangle of the most recently drawn shelf.
    pub fn bounds(&self) -> DamageRect {
        self.last_bounds
    }

    /// Return a reference to a shelf entry by index, if valid.
    pub fn entry(&self, index: usize) -> Option<&ShelfEntry> {
        if index < self.entry_count {
            Some(&self.entries[index])
        } else {
            None
        }
    }

    // =====================================================================
    // Internal helpers
    // =====================================================================

    /// Count how many entries are pinned (always at the front).
    fn pinned_count(&self) -> usize {
        let mut n = 0;
        for i in 0..self.entry_count {
            if self.entries[i].pinned {
                n += 1;
            } else {
                break;
            }
        }
        n
    }

    /// Compute total content width at base (un-magnified) icon size.
    fn base_content_width(&self, _pinned_count: usize, has_separator: bool) -> i32 {
        if self.entry_count == 0 {
            return 0;
        }
        let icon_total = SHELF_ICON_SIZE * self.entry_count as i32
            + SHELF_ICON_SPACING * (self.entry_count as i32 - 1);
        let sep_total = if has_separator {
            SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X
                - SHELF_ICON_SPACING // separator replaces one inter-icon gap
        } else {
            0
        };
        icon_total + sep_total
    }

    /// Compute total content width with per-icon scaled sizes.
    fn scaled_content_width(
        &self,
        sizes: &[i32; MAX_ENTRIES],
        pinned_count: usize,
        has_separator: bool,
    ) -> i32 {
        if self.entry_count == 0 {
            return 0;
        }
        let mut w = 0i32;
        for i in 0..self.entry_count {
            if i == pinned_count && has_separator {
                w += SHELF_SEPARATOR_MARGIN_X + SHELF_SEPARATOR_WIDTH + SHELF_SEPARATOR_MARGIN_X;
            }
            w += sizes[i];
            if i + 1 < self.entry_count {
                w += SHELF_ICON_SPACING;
            }
        }
        w
    }
}

// ---------------------------------------------------------------------------
// Free-standing helpers
// ---------------------------------------------------------------------------

/// Absolute value for `i32` without overflow on `i32::MIN`.
#[inline]
fn abs_i32(x: i32) -> i32 {
    if x < 0 { -x } else { x }
}

/// Maximum value in the first `n` elements of a fixed-size array.
fn max_in_slice(arr: &[i32; MAX_ENTRIES], n: usize) -> i32 {
    let mut m = 0;
    for i in 0..n {
        if arr[i] > m {
            m = arr[i];
        }
    }
    m
}

/// Full-screen clip rectangle.
fn full_screen_clip(w: u32, h: u32) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: w as i32 - 1,
        y1: h as i32 - 1,
    }
}

/// Extract the title bytes (up to the first NUL) from a window title array.
fn title_bytes(title: &[u8; 32]) -> &[u8] {
    let len = title.iter().position(|&b| b == 0).unwrap_or(title.len());
    &title[..len]
}

/// Case-insensitive prefix match of a shelf entry name against a window title.
fn names_match(name: &[u8; 32], name_len: usize, win_title: &[u8]) -> bool {
    if name_len == 0 || win_title.is_empty() {
        return false;
    }
    let n = name_len.min(win_title.len());
    for i in 0..n {
        if name[i].to_ascii_lowercase() != win_title[i].to_ascii_lowercase() {
            return false;
        }
    }
    // Either both are the same length, or the entry name is a prefix.
    name_len <= win_title.len()
}
