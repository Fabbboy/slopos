//! Cell-granular damage for the terminal grid.
//!
//! Damage is one inclusive column span per screen row — the granularity a
//! terminal actually changes at. A cursor blink reports one cell, a keystroke
//! one span, a scroll every row; nothing reports a window.

use alloc::vec::Vec;

/// An inclusive damaged column range within one row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub first: u16,
    pub last: u16,
}

impl Span {
    #[inline]
    fn merged(self, other: Span) -> Span {
        Span {
            first: self.first.min(other.first),
            last: self.last.max(other.last),
        }
    }
}

/// Per-row damaged column spans over a `rows`-tall grid.
#[derive(Clone, Default)]
pub struct CellDamage {
    spans: Vec<Option<Span>>,
    any: bool,
}

impl CellDamage {
    pub fn new() -> Self {
        Self {
            spans: Vec::new(),
            any: false,
        }
    }

    /// Re-shape to `rows` rows, dropping all recorded damage.
    pub fn set_rows(&mut self, rows: usize) {
        self.spans.clear();
        self.spans.resize(rows, None);
        self.any = false;
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.spans.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    pub fn clear(&mut self) {
        for slot in self.spans.iter_mut() {
            *slot = None;
        }
        self.any = false;
    }

    /// Damage columns `first..=last` of `row`. Out-of-range rows are dropped,
    /// so a caller racing a resize cannot record damage the grid cannot paint.
    pub fn add(&mut self, row: usize, first: u16, last: u16) {
        if first > last {
            return;
        }
        let Some(slot) = self.spans.get_mut(row) else {
            return;
        };
        let span = Span { first, last };
        *slot = Some(match *slot {
            Some(existing) => existing.merged(span),
            None => span,
        });
        self.any = true;
    }

    pub fn add_row(&mut self, row: usize, cols: u16) {
        if cols > 0 {
            self.add(row, 0, cols - 1);
        }
    }

    pub fn add_rows(&mut self, first_row: usize, last_row: usize, cols: u16) {
        for row in first_row..=last_row.min(self.spans.len().saturating_sub(1)) {
            self.add_row(row, cols);
        }
    }

    pub fn add_all(&mut self, cols: u16) {
        for row in 0..self.spans.len() {
            self.add_row(row, cols);
        }
    }

    #[inline]
    pub fn span(&self, row: usize) -> Option<Span> {
        self.spans.get(row).copied().flatten()
    }

    /// Merge every span of `other` into this set.
    pub fn union(&mut self, other: &CellDamage) {
        for (row, span) in other.spans.iter().enumerate() {
            if let Some(s) = span {
                self.add(row, s.first, s.last);
            }
        }
    }
}

/// Damage of recently presented frames, used to resolve what a recycled buffer
/// needs repainted.
///
/// A surface with more than one buffer hands back a slot whose contents are
/// several frames old, so repainting only *this* frame's damage into it would
/// resurrect whatever that slot last held. `age` follows the
/// `EGL_EXT_buffer_age` convention the Wayland ecosystem settled on: `n` means
/// the slot holds the frame presented `n` frames ago, and `0` means its
/// contents are undefined.
pub struct DamageHistory {
    frames: Vec<CellDamage>,
    cap: usize,
}

impl DamageHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            frames: Vec::new(),
            cap: cap.max(1),
        }
    }

    /// Forget every recorded frame. Required whenever the buffers are replaced
    /// (a resize), because no recorded region describes their new contents.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Record `frame` as the damage just presented.
    pub fn push(&mut self, frame: CellDamage) {
        self.frames.insert(0, frame);
        self.frames.truncate(self.cap);
    }

    /// What must be repainted to make a buffer of `age` show `current`, or
    /// `None` when history cannot account for the slot's contents and the
    /// caller must repaint in full.
    pub fn resolve(&self, age: u32, current: &CellDamage) -> Option<CellDamage> {
        if age == 0 {
            return None;
        }
        let older = (age - 1) as usize;
        if older > self.frames.len() {
            return None;
        }
        let mut out = current.clone();
        for frame in &self.frames[..older] {
            out.union(frame);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(row: usize, col: u16, rows: usize) -> CellDamage {
        let mut d = CellDamage::new();
        d.set_rows(rows);
        d.add(row, col, col);
        d
    }

    #[test]
    fn undefined_buffer_contents_force_a_full_repaint() {
        let h = DamageHistory::new(4);
        assert!(h.resolve(0, &one(0, 0, 4)).is_none());
    }

    #[test]
    fn age_one_repaints_only_this_frame() {
        let mut h = DamageHistory::new(4);
        h.push(one(1, 5, 4));
        let out = h.resolve(1, &one(0, 0, 4)).expect("resolvable");
        assert_eq!(out.span(0), Some(Span { first: 0, last: 0 }));
        assert_eq!(out.span(1), None);
    }

    #[test]
    fn age_two_also_repaints_the_previous_frame() {
        let mut h = DamageHistory::new(4);
        h.push(one(1, 5, 4));
        let out = h.resolve(2, &one(0, 0, 4)).expect("resolvable");
        assert_eq!(out.span(0), Some(Span { first: 0, last: 0 }));
        assert_eq!(out.span(1), Some(Span { first: 5, last: 5 }));
    }

    #[test]
    fn an_age_older_than_history_forces_a_full_repaint() {
        let mut h = DamageHistory::new(4);
        h.push(one(1, 5, 4));
        assert!(h.resolve(3, &one(0, 0, 4)).is_none());
    }

    #[test]
    fn history_beyond_the_cap_is_dropped() {
        let mut h = DamageHistory::new(1);
        h.push(one(1, 5, 4));
        h.push(one(2, 6, 4));
        assert!(h.resolve(3, &one(0, 0, 4)).is_none());
        let out = h.resolve(2, &one(0, 0, 4)).expect("resolvable");
        assert_eq!(out.span(2), Some(Span { first: 6, last: 6 }));
        assert_eq!(out.span(1), None);
    }

    #[test]
    fn clear_forces_a_full_repaint_for_a_recycled_slot() {
        let mut h = DamageHistory::new(4);
        h.push(one(1, 5, 4));
        h.clear();
        assert!(h.resolve(2, &one(0, 0, 4)).is_none());
        assert!(h.resolve(1, &one(0, 0, 4)).is_some());
    }

    #[test]
    fn fresh_damage_is_empty() {
        let mut d = CellDamage::new();
        d.set_rows(4);
        assert!(d.is_empty());
        assert_eq!(d.span(0), None);
    }

    #[test]
    fn one_cell_stays_one_cell() {
        let mut d = CellDamage::new();
        d.set_rows(4);
        d.add(2, 7, 7);
        assert!(!d.is_empty());
        assert_eq!(d.span(2), Some(Span { first: 7, last: 7 }));
        assert_eq!(d.span(1), None);
    }

    #[test]
    fn spans_on_one_row_merge_to_their_hull() {
        let mut d = CellDamage::new();
        d.set_rows(2);
        d.add(0, 2, 4);
        d.add(0, 9, 11);
        assert_eq!(d.span(0), Some(Span { first: 2, last: 11 }));
    }

    #[test]
    fn out_of_range_rows_are_dropped() {
        let mut d = CellDamage::new();
        d.set_rows(2);
        d.add(9, 0, 0);
        assert!(d.is_empty());
    }

    #[test]
    fn inverted_span_is_dropped() {
        let mut d = CellDamage::new();
        d.set_rows(2);
        d.add(0, 5, 4);
        assert!(d.is_empty());
    }

    #[test]
    fn union_takes_the_other_sets_spans() {
        let mut a = CellDamage::new();
        a.set_rows(3);
        a.add(0, 1, 1);
        let mut b = CellDamage::new();
        b.set_rows(3);
        b.add(0, 5, 5);
        b.add(2, 0, 3);
        a.union(&b);
        assert_eq!(a.span(0), Some(Span { first: 1, last: 5 }));
        assert_eq!(a.span(2), Some(Span { first: 0, last: 3 }));
    }

    #[test]
    fn set_rows_drops_recorded_damage() {
        let mut d = CellDamage::new();
        d.set_rows(3);
        d.add_all(10);
        d.set_rows(5);
        assert!(d.is_empty());
        assert_eq!(d.rows(), 5);
    }
}
