//! Right-to-left layout of the system bar's status items.
//!
//! The bar packs its indicators against the right screen edge: the item at
//! index 0 is rightmost, each subsequent present item is placed to its left
//! with [`BAR_ITEM_GAP`] between them, and whatever horizontal space is left
//! over becomes [`StatusLayout::app_name_limit`] — the right edge the bar's
//! app-name text may not cross.
//!
//! [`layout_status_items`] is the only function that knows where an item is.
//! Drawing, hit-testing and damage all call it, which is what makes a status
//! item respond to the click that lands on it: nothing caches a rectangle from
//! a previous frame and hit-tests the cursor against stale geometry.

/// The most status items the bar lays out in one pass. Items beyond this are
/// ignored rather than wrapping onto a second row: the bar is one 24 px strip
/// and has nowhere to wrap to.
pub const MAX_STATUS_ITEMS: usize = 8;

/// Inset from a screen edge to the outermost bar content. Symmetric: the
/// app-name group is inset by this on the left, the rightmost status item by
/// this on the right.
pub const BAR_PADDING_X: i32 = 12;

/// Horizontal gap between two adjacent status items.
pub const BAR_ITEM_GAP: i32 = 16;

/// Height of the interactive bar strip. The bar paints one further border row
/// at `y == BAR_HEIGHT`, which is part of the bar's damage but not part of any
/// item's hit region — a click on the border belongs to the strip, not to an
/// indicator.
pub const BAR_HEIGHT: i32 = 24;

/// Which indicator a status item is.
///
/// The discriminants are stable because they index the compositor's per-item
/// cache array; appending a variant is safe, reordering is not.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Clock = 0,
    Network = 1,
}

/// What an item tells the layout about itself.
///
/// `width` is measured by whoever owns the item's rendering (text width for
/// the clock, glyph extent for the network indicator), because only that code
/// knows the font. `revision` is opaque to the layout: it changes whenever the
/// item's *content* changes, and the bar's damage pass repaints exactly the
/// items whose revision moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusItemSpec {
    pub kind: StatusKind,
    pub present: bool,
    pub width: i32,
    pub revision: u32,
}

impl StatusItemSpec {
    /// An item of `kind` that is not currently shown. A hidden item takes no
    /// width *and no gap*, so hiding one closes the hole it left rather than
    /// leaving a ragged edge.
    pub const fn hidden(kind: StatusKind) -> Self {
        Self {
            kind,
            present: false,
            width: 0,
            revision: 0,
        }
    }

    /// Whether this spec is eligible for a slot. A zero- or negative-width
    /// item is treated as hidden: it would consume a gap while painting
    /// nothing, which reads as a layout bug rather than as an empty widget.
    #[inline]
    pub const fn is_placeable(&self) -> bool {
        self.present && self.width > 0
    }
}

/// Where the layout put one item.
///
/// `idx` is the item's index in the slice handed to [`layout_status_items`],
/// not its slot position, so a caller can find the item's cached state again
/// without searching by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSlot {
    pub kind: StatusKind,
    pub x: i32,
    pub w: i32,
    pub idx: usize,
}

impl StatusSlot {
    const EMPTY: Self = Self {
        kind: StatusKind::Clock,
        x: 0,
        w: 0,
        idx: 0,
    };

    /// Whether `(x, y)` falls inside this slot's interactive region.
    #[inline]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        y >= 0 && y < BAR_HEIGHT && x >= self.x && x < self.x + self.w
    }

    /// The x coordinate one past the slot's right edge.
    #[inline]
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
}

/// The placed items plus everything derived from their placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusLayout {
    pub slots: [StatusSlot; MAX_STATUS_ITEMS],
    pub slot_count: usize,
    /// Right edge available to the bar's app-name text: the leftmost placed
    /// item's x, minus one gap. With no items placed this is the right screen
    /// edge less [`BAR_PADDING_X`].
    pub app_name_limit: i32,
    /// Index into [`Self::slots`] of the slot under the cursor.
    pub hovered: Option<usize>,
    /// Digest of everything the geometry depends on — each item's kind,
    /// presence and width, and how many items were offered. Two layouts with
    /// equal signatures place their items identically for a given screen
    /// width, which is what lets the bar tell "an item moved" (repaint from
    /// the leftmost item to the screen edge) from "an item's content changed"
    /// (repaint that one slot).
    ///
    /// Deliberately **not** a function of [`StatusItemSpec::revision`]: a
    /// content change must not read as a layout change.
    pub signature: u64,
}

impl StatusLayout {
    /// The placed slots, in right-to-left order.
    #[inline]
    pub fn slots(&self) -> &[StatusSlot] {
        &self.slots[..self.slot_count]
    }

    /// The slot holding `kind`, if that item was placed.
    pub fn slot_for(&self, kind: StatusKind) -> Option<&StatusSlot> {
        self.slots().iter().find(|slot| slot.kind == kind)
    }
}

/// Place `items` right-to-left from the right screen edge.
///
/// Index 0 of `items` is the rightmost item. Hidden (or zero-width) items are
/// skipped entirely — they consume neither width nor a gap — so the remaining
/// items close up against the edge instead of leaving a hole.
///
/// `cursor_x` / `cursor_y` only select [`StatusLayout::hovered`]; the geometry
/// itself does not depend on them, so a caller that wants placement without
/// hover can pass a coordinate no cursor can occupy.
pub fn layout_status_items(
    items: &[StatusItemSpec],
    screen_width: i32,
    cursor_x: i32,
    cursor_y: i32,
) -> StatusLayout {
    let mut slots = [StatusSlot::EMPTY; MAX_STATUS_ITEMS];
    let mut slot_count = 0usize;
    let mut cur = screen_width - BAR_PADDING_X;

    let offered = items.len().min(MAX_STATUS_ITEMS);
    let mut sig = Fnv::new();
    sig.write_u64(offered as u64);

    for (idx, item) in items.iter().take(MAX_STATUS_ITEMS).enumerate() {
        sig.write_u64(item.kind as u64);
        sig.write_u64(item.present as u64);
        sig.write_u64(item.width as u32 as u64);

        if !item.is_placeable() {
            continue;
        }

        cur -= item.width;
        slots[slot_count] = StatusSlot {
            kind: item.kind,
            x: cur,
            w: item.width,
            idx,
        };
        slot_count += 1;
        cur -= BAR_ITEM_GAP;
    }

    let hovered = hit_slot(&slots[..slot_count], cursor_x, cursor_y);

    StatusLayout {
        slots,
        slot_count,
        app_name_limit: cur,
        hovered,
        signature: sig.finish(),
    }
}

/// Which indicator `(x, y)` lands on, if any.
///
/// The counterpart of [`layout_status_items`] and the only supported way to
/// route a click: pass a layout produced for the same screen width and the
/// answer is the item actually drawn under that pixel.
pub fn hit_status_item(layout: &StatusLayout, x: i32, y: i32) -> Option<StatusKind> {
    hit_slot(layout.slots(), x, y).map(|i| layout.slots[i].kind)
}

fn hit_slot(slots: &[StatusSlot], x: i32, y: i32) -> Option<usize> {
    slots.iter().position(|slot| slot.contains(x, y))
}

// ---------------------------------------------------------------------------
// Signature digest
// ---------------------------------------------------------------------------

/// FNV-1a, 64-bit. Small enough to read in one screen and stable across
/// builds, which a `core::hash` `DefaultHasher` is not (and `core` ships no
/// hasher at all).
struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    const fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.0 = (self.0 ^ byte as u64).wrapping_mul(Self::PRIME);
        }
    }

    const fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ceiling on the clock's damage: a fixed 80 px rect against the right edge
    /// of the strip, independent of the clock's measured width.
    const LEGACY_CLOCK_DAMAGE_WIDTH: i32 = 80;

    /// What the console font measures `HH:MM:SS` at: eight cells of the
    /// 10 px advance the shipped mono font rasterises to at 16 px. Equal to
    /// [`LEGACY_CLOCK_DAMAGE_WIDTH`], so a fixed rect against the right edge
    /// misses the clock's leading [`BAR_PADDING_X`] pixels.
    const CONSOLE_CLOCK_WIDTH: i32 = 8 * 10;

    fn item(kind: StatusKind, width: i32) -> StatusItemSpec {
        StatusItemSpec {
            kind,
            present: true,
            width,
            revision: 0,
        }
    }

    /// A cursor position no bar pixel can hold, for layouts taken purely for
    /// their geometry.
    const NO_CURSOR: (i32, i32) = (i32::MIN, i32::MIN);

    fn layout(items: &[StatusItemSpec], sw: i32) -> StatusLayout {
        layout_status_items(items, sw, NO_CURSOR.0, NO_CURSOR.1)
    }

    #[test]
    fn places_index_zero_rightmost() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let l = layout(&items, 1920);

        assert_eq!(l.slot_count, 2);
        assert_eq!(l.slots[0].kind, StatusKind::Clock);
        assert_eq!(l.slots[0].x, 1920 - BAR_PADDING_X - 64);
        assert_eq!(l.slots[0].right(), 1920 - BAR_PADDING_X);

        assert_eq!(l.slots[1].kind, StatusKind::Network);
        assert_eq!(l.slots[1].right(), l.slots[0].x - BAR_ITEM_GAP);
        assert_eq!(l.slots[1].x, l.slots[0].x - BAR_ITEM_GAP - 14);

        assert_eq!(l.app_name_limit, l.slots[1].x - BAR_ITEM_GAP);
    }

    #[test]
    fn no_items_leaves_the_whole_bar_to_the_app_name() {
        let l = layout(&[], 1024);
        assert_eq!(l.slot_count, 0);
        assert_eq!(l.app_name_limit, 1024 - BAR_PADDING_X);
        assert_eq!(l.hovered, None);
    }

    #[test]
    fn a_hidden_item_removes_its_gap() {
        let visible = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let hidden = [
            item(StatusKind::Clock, 64),
            StatusItemSpec::hidden(StatusKind::Network),
        ];

        let with = layout(&visible, 800);
        let without = layout(&hidden, 800);

        assert_eq!(without.slot_count, 1);
        // The clock does not move, and the space the network item vacated —
        // its width AND its gap — goes back to the app name.
        assert_eq!(without.slots[0].x, with.slots[0].x);
        assert_eq!(
            without.app_name_limit,
            with.app_name_limit + 14 + BAR_ITEM_GAP
        );
    }

    #[test]
    fn a_zero_width_item_is_not_placed() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 0)];
        let l = layout(&items, 800);
        assert_eq!(l.slot_count, 1);
        assert_eq!(l.slot_for(StatusKind::Network), None);
    }

    #[test]
    fn slots_are_disjoint_and_clear_the_app_name() {
        let items = [
            item(StatusKind::Clock, 64),
            item(StatusKind::Network, 14),
            item(StatusKind::Clock, 30),
        ];
        let l = layout(&items, 1280);

        for pair in l.slots().windows(2) {
            // Right-to-left: each slot sits strictly left of the previous one.
            assert!(pair[1].right() <= pair[0].x, "slots overlap: {pair:?}");
        }
        for slot in l.slots() {
            assert!(
                slot.x > l.app_name_limit,
                "slot {slot:?} crosses app_name_limit {}",
                l.app_name_limit
            );
        }
    }

    #[test]
    fn changing_screen_width_translates_every_slot() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let narrow = layout(&items, 800);
        let wide = layout(&items, 1920);
        let delta = 1920 - 800;

        assert_eq!(narrow.slot_count, wide.slot_count);
        assert_eq!(narrow.signature, wide.signature);
        for (n, w) in narrow.slots().iter().zip(wide.slots()) {
            assert_eq!(n.x + delta, w.x);
            assert_eq!(n.w, w.w);
            assert_eq!(n.kind, w.kind);
            assert_eq!(n.idx, w.idx);
        }
        assert_eq!(narrow.app_name_limit + delta, wide.app_name_limit);
    }

    /// What the layout draws and what the hit test answers must be the same
    /// function of x, at every pixel — an indicator that ignores the click
    /// landing on it is what any disagreement looks like.
    #[test]
    fn layout_and_hit_test_agree_at_every_pixel() {
        let items = [
            item(StatusKind::Clock, 64),
            item(StatusKind::Network, 14),
            item(StatusKind::Clock, 7),
        ];
        let l = layout(&items, 1920);

        for x in 0..1920 {
            let expected = l.slots().iter().find(|s| x >= s.x && x < s.right());
            assert_eq!(
                hit_status_item(&l, x, 12),
                expected.map(|s| s.kind),
                "disagreement at x={x}"
            );
        }

        // Every gap between placed items answers None, and so does the strip
        // left of the leftmost item.
        for slot_pair in l.slots().windows(2) {
            for x in slot_pair[1].right()..slot_pair[0].x {
                assert_eq!(hit_status_item(&l, x, 12), None, "gap pixel {x} hit");
            }
        }
        assert_eq!(hit_status_item(&l, l.app_name_limit, 12), None);
    }

    #[test]
    fn hit_test_rejects_rows_outside_the_strip() {
        let items = [item(StatusKind::Clock, 64)];
        let l = layout(&items, 1920);
        let inside = l.slots[0].x;

        assert_eq!(hit_status_item(&l, inside, 0), Some(StatusKind::Clock));
        assert_eq!(
            hit_status_item(&l, inside, BAR_HEIGHT - 1),
            Some(StatusKind::Clock)
        );
        // The border row below the strip belongs to the bar, not the item.
        assert_eq!(hit_status_item(&l, inside, BAR_HEIGHT), None);
        assert_eq!(hit_status_item(&l, inside, -1), None);
    }

    #[test]
    fn hovered_matches_the_hit_test() {
        let items = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];
        let probe = layout(&items, 1920);

        for slot_index in 0..probe.slot_count {
            let slot = probe.slots[slot_index];
            let hovered = layout_status_items(&items, 1920, slot.x + slot.w / 2, 12);
            assert_eq!(hovered.hovered, Some(slot_index));
        }

        let elsewhere = layout_status_items(&items, 1920, 4, 12);
        assert_eq!(elsewhere.hovered, None);
    }

    /// The clock's measured slot beats a fixed [`LEGACY_CLOCK_DAMAGE_WIDTH`]
    /// rect in both directions: never more pixels repainted, and never fewer
    /// than the clock actually occupies.
    ///
    /// The second half is the tight one. A [`CONSOLE_CLOCK_WIDTH`] clock is
    /// exactly as wide as that rect, so the text starts [`BAR_PADDING_X`]
    /// pixels left of it and its leading digits go unrepainted on a tick —
    /// which only shows when the hours change. The measured slot is the same
    /// width and sits where the text does.
    #[test]
    fn clock_slot_is_no_worse_than_the_legacy_damage_rect() {
        for sw in [640, 800, 1024, 1280, 1920] {
            for width in 1..=LEGACY_CLOCK_DAMAGE_WIDTH {
                let l = layout(&[item(StatusKind::Clock, width)], sw);
                let slot = l.slots[0];

                // Never more pixels than the fixed rect.
                assert!(
                    slot.w <= LEGACY_CLOCK_DAMAGE_WIDTH,
                    "sw={sw} width={width}: repaints {} px, more than the {LEGACY_CLOCK_DAMAGE_WIDTH} px it replaces",
                    slot.w
                );
                // And exactly the pixels the text occupies, right-aligned
                // against the padding.
                assert_eq!(slot.w, width);
                assert_eq!(slot.right(), sw - BAR_PADDING_X);
                assert!(slot.right() - 1 <= sw - 1);
            }
        }

        // The shipped font's clock: the width a fixed rect under-covers.
        let l = layout(&[item(StatusKind::Clock, CONSOLE_CLOCK_WIDTH)], 1280);
        assert_eq!(l.slots[0].x, 1280 - BAR_PADDING_X - CONSOLE_CLOCK_WIDTH);
        assert!(
            l.slots[0].x < 1280 - LEGACY_CLOCK_DAMAGE_WIDTH,
            "the pinned console clock width must still be the under-covered case"
        );
    }

    #[test]
    fn signature_tracks_presence_width_and_count_but_not_revision() {
        let base = [item(StatusKind::Clock, 64), item(StatusKind::Network, 14)];

        let mut revised = base;
        revised[0].revision = 7;
        revised[1].revision = 99;
        assert_eq!(
            layout(&base, 1920).signature,
            layout(&revised, 1920).signature,
            "a content change must not read as a layout change"
        );

        let mut hidden = base;
        hidden[1].present = false;
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&hidden, 1920).signature
        );

        let mut wider = base;
        wider[0].width = 65;
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&wider, 1920).signature
        );

        assert_ne!(
            layout(&base, 1920).signature,
            layout(&base[..1], 1920).signature
        );

        let mut swapped = base;
        swapped.swap(0, 1);
        assert_ne!(
            layout(&base, 1920).signature,
            layout(&swapped, 1920).signature,
            "reordering items changes what is drawn where"
        );
    }

    #[test]
    fn items_beyond_the_cap_are_ignored() {
        let items = [item(StatusKind::Network, 10); MAX_STATUS_ITEMS + 3];
        let l = layout(&items, 1920);
        assert_eq!(l.slot_count, MAX_STATUS_ITEMS);
        assert_eq!(
            l.app_name_limit,
            1920 - BAR_PADDING_X - MAX_STATUS_ITEMS as i32 * (10 + BAR_ITEM_GAP)
        );
    }
}
