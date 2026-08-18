//! A two-position switch, as geometry.
//!
//! The knob's position is a function of a fixed-point `t` rather than a bool,
//! so an eased transition is a change to what `t` is passed and not a second
//! placement rule. Invariant: the knob stays inside its track at every `t`.

use crate::positioner::Rect;

/// `t` at rest in the off position.
pub const TOGGLE_OFF: i32 = 0;
/// `t` at rest in the on position. Fixed point with 256 as one, so easing
/// needs no floating point.
pub const TOGGLE_ON: i32 = 256;

/// Inset of the knob from the track's edge on every side.
pub const TOGGLE_PADDING: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToggleGeometry {
    pub track: Rect,
    pub knob: Rect,
}

/// Place the knob in `track` for `t`, clamped to `TOGGLE_OFF..=TOGGLE_ON`.
///
/// A track too small to hold a padded knob yields an empty knob rather than a
/// negative one, so a caller can draw the track and skip the knob.
pub fn toggle_geometry(track: Rect, t: i32) -> ToggleGeometry {
    let t = t.clamp(TOGGLE_OFF, TOGGLE_ON);
    // Sized by the tighter dimension: height alone overhangs the right inset in
    // a track only a little wider than it is tall.
    let size = (track.h - 2 * TOGGLE_PADDING)
        .min(track.w - 2 * TOGGLE_PADDING)
        .max(0);
    if size == 0 || track.w <= 2 * TOGGLE_PADDING {
        return ToggleGeometry {
            track,
            knob: Rect::new(track.x, track.y, 0, 0),
        };
    }

    let left = track.x + TOGGLE_PADDING;
    let travel = (track.w - 2 * TOGGLE_PADDING - size).max(0);
    // Rounded, not truncated: truncation leaves an odd `travel` a pixel short
    // of the right inset while sitting exactly on the left one.
    let dx = (travel * t + TOGGLE_ON / 2) / TOGGLE_ON;

    ToggleGeometry {
        track,
        knob: Rect::new(left + dx, track.y + TOGGLE_PADDING, size, size),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Rect {
        Rect::new(100, 40, 44, 22)
    }

    #[test]
    fn the_knob_stays_inside_the_track_across_the_whole_range() {
        let track = track();
        for t in TOGGLE_OFF..=TOGGLE_ON {
            let g = toggle_geometry(track, t);
            assert!(
                g.knob.x >= track.x + TOGGLE_PADDING,
                "t={t}: knob left {} crosses the inset",
                g.knob.x
            );
            assert!(
                g.knob.right() <= track.right() - TOGGLE_PADDING,
                "t={t}: knob right {} crosses the inset",
                g.knob.right()
            );
            assert!(g.knob.y >= track.y + TOGGLE_PADDING, "t={t}: knob above");
            assert!(
                g.knob.bottom() <= track.bottom() - TOGGLE_PADDING,
                "t={t}: knob below"
            );
        }
    }

    #[test]
    fn out_of_range_t_clamps() {
        let track = track();
        assert_eq!(
            toggle_geometry(track, -5000).knob,
            toggle_geometry(track, TOGGLE_OFF).knob
        );
        assert_eq!(
            toggle_geometry(track, 5000).knob,
            toggle_geometry(track, TOGGLE_ON).knob
        );
    }

    #[test]
    fn the_endpoints_are_symmetric() {
        let track = track();
        let off = toggle_geometry(track, TOGGLE_OFF).knob;
        let on = toggle_geometry(track, TOGGLE_ON).knob;
        assert_eq!(off.x - track.x, TOGGLE_PADDING);
        assert_eq!(track.right() - on.right(), TOGGLE_PADDING);
        assert_eq!(off.w, on.w);
    }

    #[test]
    fn the_knob_advances_monotonically() {
        let track = track();
        let mut previous = toggle_geometry(track, TOGGLE_OFF).knob.x;
        for t in TOGGLE_OFF..=TOGGLE_ON {
            let x = toggle_geometry(track, t).knob.x;
            assert!(x >= previous, "t={t}: knob moved backwards");
            previous = x;
        }
    }

    #[test]
    fn odd_travel_still_reaches_both_ends() {
        for w in 20..64 {
            let track = Rect::new(0, 0, w, 22);
            let off = toggle_geometry(track, TOGGLE_OFF).knob;
            let on = toggle_geometry(track, TOGGLE_ON).knob;
            assert_eq!(off.x, TOGGLE_PADDING, "w={w}");
            assert_eq!(
                on.right(),
                track.right() - TOGGLE_PADDING,
                "w={w}: knob stops short of the right inset"
            );
        }
    }

    #[test]
    fn a_degenerate_track_yields_an_empty_knob() {
        for (w, h) in [(0, 0), (4, 4), (44, 4), (4, 22), (2, 2)] {
            let g = toggle_geometry(Rect::new(0, 0, w, h), TOGGLE_ON);
            assert!(g.knob.w >= 0 && g.knob.h >= 0, "w={w} h={h}");
            if w <= 2 * TOGGLE_PADDING || h <= 2 * TOGGLE_PADDING {
                assert!(g.knob.is_empty(), "w={w} h={h}: knob should be empty");
            }
        }
    }
}
