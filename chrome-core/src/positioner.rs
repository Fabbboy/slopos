//! Placing a surface relative to another, and keeping it on screen.
//!
//! The parameter set is the one `xdg_positioner` uses — an anchor rectangle, an
//! edge of it, a gravity, a size, an offset, a work area and a constraint
//! policy. Those are interface facts rather than an implementation, and taking
//! the shape rather than inventing one means that if the protocol's popup role
//! is ever wired up it reuses this code instead of growing a second copy that
//! disagrees at the edges.
//!
//! **Sliding is the common case here, not the exception.** The thing that
//! opens a popover in this shell is a status item packed against the right
//! screen edge, so a naive placer clips on the first frame anyone looks at.
//! That is why the constraint policy is a parameter with a default that
//! includes [`ConstraintAdjustment::SLIDE_X`] rather than an afterthought.
//!
//! Geometry only: no framebuffer, no descriptors. The renderer draws the
//! result and the input path tests clicks against it, and neither decision
//! needs a running compositor to be checked, which is what puts the placement
//! rules under `cargo test -p slopos-chrome-core`.

/// An axis-aligned rectangle in surface-local or screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    #[inline]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    /// One past the right edge.
    #[inline]
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// One past the bottom edge.
    #[inline]
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Nothing to draw and nothing to hit.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Whether `(x, y)` falls inside.
    ///
    /// This is the light-dismiss predicate: a press that is *not* inside any
    /// open surface dismisses it. One function so the renderer's idea of the
    /// surface's extent and the input path's cannot diverge — if they did, the
    /// result is a popover that swallows clicks past its own border, or one
    /// that dismisses on a click within it.
    #[inline]
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        !self.is_empty() && x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A requested size. What the positioner returns may be smaller, if the policy
/// permits resizing and nothing else fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

/// Which point of the anchor rectangle the surface is positioned against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

/// Which way the surface extends from the anchor point.
///
/// Read it as "where the surface goes": [`Gravity::BottomLeft`] puts the
/// surface below and to the left, so its *top-right* corner lands on the
/// anchor point — which is what right-aligns a popover under a status item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gravity {
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

/// What the positioner may do when the surface does not fit.
///
/// A bitmask rather than an enum because the axes are independent: sliding
/// horizontally while resizing vertically is the normal policy for a panel
/// hanging off a screen-edge item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintAdjustment(u32);

impl ConstraintAdjustment {
    /// Let it hang off the work area. Almost never what anyone wants; present
    /// because "do nothing" has to be expressible for the shape to match.
    pub const NONE: ConstraintAdjustment = ConstraintAdjustment(0);
    pub const SLIDE_X: ConstraintAdjustment = ConstraintAdjustment(1 << 0);
    pub const SLIDE_Y: ConstraintAdjustment = ConstraintAdjustment(1 << 1);
    pub const FLIP_X: ConstraintAdjustment = ConstraintAdjustment(1 << 2);
    pub const FLIP_Y: ConstraintAdjustment = ConstraintAdjustment(1 << 3);
    pub const RESIZE_X: ConstraintAdjustment = ConstraintAdjustment(1 << 4);
    pub const RESIZE_Y: ConstraintAdjustment = ConstraintAdjustment(1 << 5);

    /// What a shell popover wants: slide along both axes, and give up height
    /// before position if the content is taller than the screen. Deliberately
    /// no flipping — a panel that jumped above the bar it hangs from would
    /// read as a different widget.
    pub const PANEL: ConstraintAdjustment = ConstraintAdjustment(
        Self::SLIDE_X.0 | Self::SLIDE_Y.0 | Self::RESIZE_X.0 | Self::RESIZE_Y.0,
    );

    #[inline]
    pub const fn contains(self, other: ConstraintAdjustment) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub const fn union(self, other: ConstraintAdjustment) -> ConstraintAdjustment {
        ConstraintAdjustment(self.0 | other.0)
    }
}

/// Everything needed to place one surface against another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Positioner {
    /// The rectangle to position against, in the same coordinates as the work
    /// area.
    pub anchor_rect: Rect,
    pub anchor: Anchor,
    pub gravity: Gravity,
    pub size: Size,
    /// Displacement applied after anchoring, before constraining.
    pub offset: (i32, i32),
    pub constraint_adjustment: ConstraintAdjustment,
}

impl Positioner {
    /// A panel hanging below a bar item, right-aligned to it.
    ///
    /// Right-aligned because the item is packed against the right screen edge:
    /// aligning the panel's right edge to the item's keeps the two visually
    /// attached as other items come and go to the item's left.
    pub const fn below_bar_item(item: Rect, size: Size, gap_y: i32) -> Positioner {
        Positioner {
            anchor_rect: item,
            anchor: Anchor::BottomRight,
            gravity: Gravity::BottomLeft,
            size,
            offset: (0, gap_y),
            constraint_adjustment: ConstraintAdjustment::PANEL,
        }
    }
}

/// The point on `rect` that `anchor` names.
const fn anchor_point(rect: Rect, anchor: Anchor) -> (i32, i32) {
    let (left, right) = (rect.x, rect.right());
    let (top, bottom) = (rect.y, rect.bottom());
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    match anchor {
        Anchor::Center => (cx, cy),
        Anchor::Top => (cx, top),
        Anchor::Bottom => (cx, bottom),
        Anchor::Left => (left, cy),
        Anchor::Right => (right, cy),
        Anchor::TopLeft => (left, top),
        Anchor::BottomLeft => (left, bottom),
        Anchor::TopRight => (right, top),
        Anchor::BottomRight => (right, bottom),
    }
}

/// Where a surface of `size` sits when it extends from `(ax, ay)` per
/// `gravity`.
const fn gravity_origin(ax: i32, ay: i32, size: Size, gravity: Gravity) -> (i32, i32) {
    let (w, h) = (size.w, size.h);
    match gravity {
        Gravity::Center => (ax - w / 2, ay - h / 2),
        Gravity::Top => (ax - w / 2, ay - h),
        Gravity::Bottom => (ax - w / 2, ay),
        Gravity::Left => (ax - w, ay - h / 2),
        Gravity::Right => (ax, ay - h / 2),
        Gravity::TopLeft => (ax - w, ay - h),
        Gravity::BottomLeft => (ax - w, ay),
        Gravity::TopRight => (ax, ay - h),
        Gravity::BottomRight => (ax, ay),
    }
}

/// The gravity that results from flipping `gravity` on the given axis.
const fn flip_gravity(gravity: Gravity, horizontal: bool) -> Gravity {
    if horizontal {
        match gravity {
            Gravity::Left => Gravity::Right,
            Gravity::Right => Gravity::Left,
            Gravity::TopLeft => Gravity::TopRight,
            Gravity::TopRight => Gravity::TopLeft,
            Gravity::BottomLeft => Gravity::BottomRight,
            Gravity::BottomRight => Gravity::BottomLeft,
            other => other,
        }
    } else {
        match gravity {
            Gravity::Top => Gravity::Bottom,
            Gravity::Bottom => Gravity::Top,
            Gravity::TopLeft => Gravity::BottomLeft,
            Gravity::BottomLeft => Gravity::TopLeft,
            Gravity::TopRight => Gravity::BottomRight,
            Gravity::BottomRight => Gravity::TopRight,
            other => other,
        }
    }
}

/// The anchor that results from flipping `anchor` on the given axis.
const fn flip_anchor(anchor: Anchor, horizontal: bool) -> Anchor {
    if horizontal {
        match anchor {
            Anchor::Left => Anchor::Right,
            Anchor::Right => Anchor::Left,
            Anchor::TopLeft => Anchor::TopRight,
            Anchor::TopRight => Anchor::TopLeft,
            Anchor::BottomLeft => Anchor::BottomRight,
            Anchor::BottomRight => Anchor::BottomLeft,
            other => other,
        }
    } else {
        match anchor {
            Anchor::Top => Anchor::Bottom,
            Anchor::Bottom => Anchor::Top,
            Anchor::TopLeft => Anchor::BottomLeft,
            Anchor::BottomLeft => Anchor::TopLeft,
            Anchor::TopRight => Anchor::BottomRight,
            Anchor::BottomRight => Anchor::TopRight,
            other => other,
        }
    }
}

/// Place the surface `p` describes inside `work_area`.
///
/// Constraints are applied per axis, in the order flip, slide, resize — the
/// order that preserves intent longest. Flipping keeps the surface attached to
/// the same edge of the anchor; sliding keeps its size; resizing is what is
/// left when neither helps. Each step is skipped unless
/// [`Positioner::constraint_adjustment`] permits it, and a step that would not
/// actually unconstrain the surface is not taken.
///
/// The result is finally clamped into `work_area` regardless of policy, so a
/// caller never receives a rectangle it would have to re-check before drawing.
/// A work area smaller than the requested size yields a smaller rect, possibly
/// empty; [`Rect::is_empty`] is the check before drawing.
pub fn position(p: &Positioner, work_area: Rect) -> Rect {
    let mut size = Size {
        w: p.size.w.max(0),
        h: p.size.h.max(0),
    };
    let mut anchor = p.anchor;
    let mut gravity = p.gravity;

    let place = |anchor: Anchor, gravity: Gravity, size: Size| -> (i32, i32) {
        let (ax, ay) = anchor_point(p.anchor_rect, anchor);
        let (ox, oy) = gravity_origin(ax, ay, size, gravity);
        (ox + p.offset.0, oy + p.offset.1)
    };

    let (mut x, mut y) = place(anchor, gravity, size);

    // Sliding is only attempted on an axis where the surface could actually
    // fit. Sliding one that cannot fit pins it to an edge and destroys the
    // anchor for no gain — a panel taller than the screen would jump to the
    // top of the work area and stop touching the bar it hangs from, when what
    // is wanted is for it to stay put and lose height. Resize handles that
    // case, and this guard is what lets it.
    let fits_x = size.w <= work_area.w;
    let fits_y = size.h <= work_area.h;

    // ---- horizontal ------------------------------------------------------
    if x < work_area.x || x + size.w > work_area.right() {
        if p.constraint_adjustment
            .contains(ConstraintAdjustment::FLIP_X)
        {
            let (fa, fg) = (flip_anchor(anchor, true), flip_gravity(gravity, true));
            let (fx, _) = place(fa, fg, size);
            if fx >= work_area.x && fx + size.w <= work_area.right() {
                anchor = fa;
                gravity = fg;
                x = fx;
            }
        }
    }
    if fits_x
        && p.constraint_adjustment
            .contains(ConstraintAdjustment::SLIDE_X)
    {
        if x + size.w > work_area.right() {
            x = work_area.right() - size.w;
        }
        if x < work_area.x {
            x = work_area.x;
        }
    }
    if x + size.w > work_area.right()
        && p.constraint_adjustment
            .contains(ConstraintAdjustment::RESIZE_X)
    {
        size.w = (work_area.right() - x).max(0);
    }

    // ---- vertical --------------------------------------------------------
    if y < work_area.y || y + size.h > work_area.bottom() {
        if p.constraint_adjustment
            .contains(ConstraintAdjustment::FLIP_Y)
        {
            let (fa, fg) = (flip_anchor(anchor, false), flip_gravity(gravity, false));
            let (_, fy) = place(fa, fg, size);
            if fy >= work_area.y && fy + size.h <= work_area.bottom() {
                y = fy;
            }
        }
    }
    if fits_y
        && p.constraint_adjustment
            .contains(ConstraintAdjustment::SLIDE_Y)
    {
        if y + size.h > work_area.bottom() {
            y = work_area.bottom() - size.h;
        }
        if y < work_area.y {
            y = work_area.y;
        }
    }
    if y + size.h > work_area.bottom()
        && p.constraint_adjustment
            .contains(ConstraintAdjustment::RESIZE_Y)
    {
        size.h = (work_area.bottom() - y).max(0);
    }

    // Final clamp: whatever the policy, hand back something drawable.
    let x = x.clamp(work_area.x, work_area.right().max(work_area.x));
    let y = y.clamp(work_area.y, work_area.bottom().max(work_area.y));
    Rect {
        x,
        y,
        w: size.w.min(work_area.right() - x).max(0),
        h: size.h.min(work_area.bottom() - y).max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen with the bar's strip excluded, which is what a shell hands in
    /// as the work area.
    ///
    /// Inset on the left, right and bottom but **not** the top: the gap below
    /// the bar is the positioner's `offset`, and insetting here as well would
    /// count it twice.
    fn work_area(w: i32, h: i32) -> Rect {
        let top = crate::status::BAR_HEIGHT + 1;
        Rect::new(8, top, w - 16, h - top - 8)
    }

    /// A status item's slot, as `layout_status_items` would place it.
    fn item(right_edge: i32) -> Rect {
        Rect::new(right_edge - 14, 0, 14, crate::status::BAR_HEIGHT)
    }

    fn panel(right_edge: i32, size: Size, screen_w: i32, screen_h: i32) -> Rect {
        position(
            &Positioner::below_bar_item(item(right_edge), size, 6),
            work_area(screen_w, screen_h),
        )
    }

    #[test]
    fn right_aligns_to_the_anchor_when_there_is_room() {
        let r = panel(1200, Size { w: 280, h: 200 }, 1920, 1080);
        assert_eq!(r.right(), 1200);
        assert_eq!(r.w, 280);
        assert_eq!(r.h, 200);
    }

    #[test]
    fn hangs_below_the_item_by_the_offset() {
        let r = panel(1200, Size { w: 280, h: 200 }, 1920, 1080);
        assert_eq!(r.y, crate::status::BAR_HEIGHT + 6);
        assert!(
            r.y >= crate::status::BAR_HEIGHT,
            "a panel must not overlap the bar it hangs from"
        );
    }

    /// The case this shell hits first, not an edge case: the indicator lives
    /// at the right screen edge, so the panel must slide left rather than
    /// clip.
    #[test]
    fn slides_left_rather_than_clipping_at_the_right_edge() {
        for screen_w in [640, 800, 1024, 1280, 1920] {
            let area = work_area(screen_w, 1080);
            let r = panel(screen_w - 12, Size { w: 280, h: 200 }, screen_w, 1080);
            assert!(
                r.right() <= area.right(),
                "sw={screen_w}: right edge {} past the work area {}",
                r.right(),
                area.right()
            );
            // Sliding preserves the size; only resizing may shrink it, and
            // 280 fits on every width tested.
            assert_eq!(r.w, 280, "sw={screen_w}: slid but also shrank");
        }
    }

    #[test]
    fn slides_right_rather_than_hanging_off_the_left() {
        let r = panel(120, Size { w: 400, h: 200 }, 640, 480);
        let area = work_area(640, 480);
        assert_eq!(r.x, area.x);
        assert!(r.right() <= area.right());
    }

    /// Wider than the work area: sliding cannot help, so the width gives.
    #[test]
    fn resizes_when_sliding_cannot_help() {
        let area = work_area(640, 480);
        let r = panel(600, Size { w: 5000, h: 200 }, 640, 480);
        assert_eq!(r.x, area.x);
        assert_eq!(r.w, area.w);
    }

    /// Height gives before position: a panel that floated off the bar to fit
    /// its content would stop reading as attached to what opened it.
    #[test]
    fn a_tall_panel_is_shortened_not_moved() {
        let area = work_area(1024, 300);
        let r = panel(600, Size { w: 280, h: 5000 }, 1024, 300);
        assert_eq!(r.y, crate::status::BAR_HEIGHT + 6);
        assert!(r.bottom() <= area.bottom());
    }

    /// The panel policy deliberately does not flip: a flip would put the panel
    /// above the bar it hangs from.
    #[test]
    fn the_panel_policy_never_flips() {
        assert!(
            !ConstraintAdjustment::PANEL.contains(ConstraintAdjustment::FLIP_Y),
            "a panel that jumped above its bar reads as a different widget"
        );
        assert!(!ConstraintAdjustment::PANEL.contains(ConstraintAdjustment::FLIP_X));
        assert!(ConstraintAdjustment::PANEL.contains(ConstraintAdjustment::SLIDE_X));
    }

    /// Flipping, where a caller does ask for it, keeps the surface attached to
    /// the anchor's other side rather than sliding across it.
    #[test]
    fn flip_y_moves_a_menu_above_its_anchor() {
        let area = Rect::new(0, 0, 800, 600);
        let anchor = Rect::new(100, 560, 40, 20);
        let p = Positioner {
            anchor_rect: anchor,
            anchor: Anchor::BottomLeft,
            gravity: Gravity::BottomRight,
            size: Size { w: 100, h: 200 },
            offset: (0, 0),
            constraint_adjustment: ConstraintAdjustment::FLIP_Y,
        };
        let r = position(&p, area);
        // Flipped to sit above the anchor rather than below its bottom edge.
        assert_eq!(r.bottom(), anchor.y);
        assert_eq!(r.h, 200);
    }

    #[test]
    fn no_adjustment_leaves_the_surface_where_it_was_asked_for() {
        let area = Rect::new(0, 0, 800, 600);
        let p = Positioner {
            anchor_rect: Rect::new(700, 100, 20, 20),
            anchor: Anchor::BottomRight,
            gravity: Gravity::BottomRight,
            size: Size { w: 300, h: 100 },
            offset: (0, 0),
            constraint_adjustment: ConstraintAdjustment::NONE,
        };
        let r = position(&p, area);
        assert_eq!(r.x, 720);
        // The final clamp still refuses to hand back something undrawable.
        assert!(r.right() <= area.right());
    }

    #[test]
    fn anchor_and_gravity_place_every_corner() {
        let area = Rect::new(0, 0, 1000, 1000);
        let anchor = Rect::new(400, 400, 100, 50);
        let size = Size { w: 60, h: 40 };
        let cases = [
            (Anchor::TopLeft, Gravity::TopLeft, (400 - 60, 400 - 40)),
            (Anchor::TopRight, Gravity::TopRight, (500, 400 - 40)),
            (Anchor::BottomLeft, Gravity::BottomLeft, (400 - 60, 450)),
            (Anchor::BottomRight, Gravity::BottomRight, (500, 450)),
            (Anchor::Center, Gravity::Center, (450 - 30, 425 - 20)),
        ];
        for (a, g, want) in cases {
            let p = Positioner {
                anchor_rect: anchor,
                anchor: a,
                gravity: g,
                size,
                offset: (0, 0),
                constraint_adjustment: ConstraintAdjustment::NONE,
            };
            let r = position(&p, area);
            assert_eq!((r.x, r.y), want, "anchor={a:?} gravity={g:?}");
        }
    }

    #[test]
    fn degenerate_work_areas_yield_no_negative_extent() {
        for (w, h) in [(0, 0), (1, 1), (16, 16), (40, 30), (640, 24)] {
            let r = panel(w, Size { w: 280, h: 200 }, w, h);
            assert!(r.w >= 0, "w={w} h={h}: negative width {}", r.w);
            assert!(r.h >= 0, "w={w} h={h}: negative height {}", r.h);
        }
    }

    #[test]
    fn contains_agrees_at_every_pixel() {
        let r = panel(1200, Size { w: 280, h: 200 }, 1920, 1080);
        for x in 0..1920 {
            assert_eq!(r.contains(x, r.y), x >= r.x && x < r.right(), "x={x}");
        }
        for y in 0..1080 {
            assert_eq!(r.contains(r.x, y), y >= r.y && y < r.bottom(), "y={y}");
        }
        // The bar strip is never inside, so a click on the item that opened
        // the panel reaches the bar's own hit test rather than reading as
        // "inside the panel".
        assert!(!r.contains(r.right() - 1, crate::status::BAR_HEIGHT / 2));
    }

    #[test]
    fn an_empty_rect_contains_nothing() {
        let r = Rect::new(10, 10, 0, 50);
        assert!(!r.contains(10, 20));
        assert!(Rect::EMPTY.is_empty());
        assert!(!Rect::EMPTY.contains(0, 0));
    }
}
