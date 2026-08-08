//! The network indicator's artwork, described as geometry rather than pixels.
//!
//! The icon has to be drawn in three inks and four badges.
//! `gfx::image::draw_image` takes no colour-modulation parameter, so a sprite
//! sheet would mean one bitmap per combination and a tinting blitter that does
//! not exist. A rect table costs neither: the renderer recolours the same shape
//! per state, and first-party geometry carries no third-party icon licence,
//! where an icon set such as Adwaita (CC-BY-SA-3.0) or Material (Apache-2.0)
//! would.
//!
//! The wired glyph is a bus with three drops and three nodes on a 14×9 unit
//! grid. Every edge is axis-aligned, so it needs no anti-aliasing at scale 1
//! and scales by an integer multiplier without resampling.

use crate::netstate::NetIndicatorState;

/// Width of the unit grid every glyph is drawn on.
pub const GLYPH_W: i32 = 14;
/// Height of the unit grid every glyph is drawn on.
pub const GLYPH_H: i32 = 9;

/// Which shape the indicator draws.
///
/// There is deliberately no wireless variant: the tree has no wireless driver,
/// and an unreachable arm in the renderer is a worse lie than a missing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphBase {
    Wired,
}

/// How the shape is coloured — the state's severity, not a literal colour, so
/// the palette lives in the renderer's theme rather than here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// The link works.
    Ok,
    /// Something is in progress and will resolve on its own.
    Transient,
    /// The link is not usable.
    Down,
}

/// The mark cut into the glyph's corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    None,
    /// Reachable, but not all the way — the amber of a window's minimize
    /// button.
    Warn,
    /// The link itself is broken — the red of a window's close button.
    Error,
    /// Switched off deliberately. A diagonal stroke, not a dot: "off" is not a
    /// fault and must not borrow a fault's colour.
    Slash,
}

/// One axis-aligned rectangle in the unit grid, `(x, y)` from the glyph's
/// top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The wired glyph: a horizontal bus, three drops, three nodes.
const WIRED: [GlyphRect; 7] = [
    // Bus.
    GlyphRect {
        x: 1,
        y: 0,
        w: 12,
        h: 2,
    },
    // Drops.
    GlyphRect {
        x: 1,
        y: 2,
        w: 2,
        h: 3,
    },
    GlyphRect {
        x: 6,
        y: 2,
        w: 2,
        h: 3,
    },
    GlyphRect {
        x: 11,
        y: 2,
        w: 2,
        h: 3,
    },
    // Nodes.
    GlyphRect {
        x: 0,
        y: 5,
        w: 4,
        h: 4,
    },
    GlyphRect {
        x: 5,
        y: 5,
        w: 4,
        h: 4,
    },
    GlyphRect {
        x: 10,
        y: 5,
        w: 4,
        h: 4,
    },
];

/// The wired glyph's rect table.
pub const WIRED_RECTS: &[GlyphRect] = &WIRED;

/// A shape, an ink and a badge — everything the renderer needs, and nothing
/// about pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphSpec {
    pub base: GlyphBase,
    pub ink: Ink,
    pub badge: Badge,
}

impl GlyphSpec {
    /// The rectangles making up this spec's shape.
    #[inline]
    pub const fn rects(&self) -> &'static [GlyphRect] {
        rects_for(self.base)
    }
}

/// The rectangles making up `base`.
pub const fn rects_for(base: GlyphBase) -> &'static [GlyphRect] {
    match base {
        GlyphBase::Wired => WIRED_RECTS,
    }
}

/// How an indicator state looks.
///
/// The ink says whether the link works, the badge says what is wrong with it.
/// Exactly one state carries each of the two fault badges, so a person learns
/// the mapping from one look at the bar rather than from a legend.
pub const fn glyph_for(state: NetIndicatorState) -> GlyphSpec {
    let (ink, badge) = match state {
        NetIndicatorState::Connected => (Ink::Ok, Badge::None),
        NetIndicatorState::Limited => (Ink::Ok, Badge::Warn),
        NetIndicatorState::Configuring => (Ink::Transient, Badge::None),
        NetIndicatorState::NoCarrier => (Ink::Down, Badge::Error),
        NetIndicatorState::Disconnected => (Ink::Down, Badge::None),
        NetIndicatorState::Disabled => (Ink::Down, Badge::Slash),
    };
    GlyphSpec {
        base: GlyphBase::Wired,
        ink,
        badge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netstate::ALL_INDICATOR_STATES;

    fn bounding_box(rects: &[GlyphRect]) -> (i32, i32, i32, i32) {
        let mut x0 = i32::MAX;
        let mut y0 = i32::MAX;
        let mut x1 = i32::MIN;
        let mut y1 = i32::MIN;
        for r in rects {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.x + r.w);
            y1 = y1.max(r.y + r.h);
        }
        (x0, y0, x1, y1)
    }

    #[test]
    fn every_state_yields_a_shape_inside_the_unit_grid() {
        for &state in ALL_INDICATOR_STATES {
            let spec = glyph_for(state);
            let rects = spec.rects();
            assert!(!rects.is_empty(), "{state:?} has no rects");

            for r in rects {
                assert!(r.w > 0 && r.h > 0, "{state:?}: degenerate rect {r:?}");
                assert!(
                    r.x >= 0 && r.y >= 0 && r.x + r.w <= GLYPH_W && r.y + r.h <= GLYPH_H,
                    "{state:?}: rect {r:?} leaves the {GLYPH_W}x{GLYPH_H} grid"
                );
            }

            let (x0, y0, x1, y1) = bounding_box(rects);
            assert_eq!(
                (x0, y0),
                (0, 0),
                "{state:?}: shape does not start at origin"
            );
            assert_eq!(
                (x1, y1),
                (GLYPH_W, GLYPH_H),
                "{state:?}: shape does not fill the grid"
            );
        }
    }

    #[test]
    fn badges_are_state_exclusive() {
        for &state in ALL_INDICATOR_STATES {
            let badge = glyph_for(state).badge;
            let expected = match state {
                NetIndicatorState::Limited => Badge::Warn,
                NetIndicatorState::NoCarrier => Badge::Error,
                NetIndicatorState::Disabled => Badge::Slash,
                _ => Badge::None,
            };
            assert_eq!(badge, expected, "{state:?}");
        }
    }

    #[test]
    fn ink_tracks_usability() {
        assert_eq!(glyph_for(NetIndicatorState::Connected).ink, Ink::Ok);
        assert_eq!(glyph_for(NetIndicatorState::Limited).ink, Ink::Ok);
        assert_eq!(
            glyph_for(NetIndicatorState::Configuring).ink,
            Ink::Transient
        );
        assert_eq!(glyph_for(NetIndicatorState::NoCarrier).ink, Ink::Down);
        assert_eq!(glyph_for(NetIndicatorState::Disconnected).ink, Ink::Down);
        assert_eq!(glyph_for(NetIndicatorState::Disabled).ink, Ink::Down);
    }

    /// Axis-aligned edges are what let the renderer scale by an integer
    /// multiplier without resampling; a rect table that only happens to be
    /// integral today would break that quietly.
    #[test]
    fn the_shape_scales_by_an_integer_multiplier() {
        for scale in 1..=4 {
            let (x0, y0, x1, y1) = bounding_box(WIRED_RECTS);
            assert_eq!(
                ((x1 - x0) * scale, (y1 - y0) * scale),
                (GLYPH_W * scale, GLYPH_H * scale)
            );
        }
    }

    /// The drops must actually connect the bus to the nodes, and the nodes
    /// must not touch each other — otherwise the icon is a solid blob at
    /// 14 px rather than a recognisable topology.
    #[test]
    fn the_wired_shape_is_a_connected_bus_with_separated_nodes() {
        let bus = WIRED_RECTS[0];
        let drops = &WIRED_RECTS[1..4];
        let nodes = &WIRED_RECTS[4..7];

        for drop in drops {
            assert_eq!(drop.y, bus.y + bus.h, "drop {drop:?} does not meet the bus");
            assert!(drop.x >= bus.x && drop.x + drop.w <= bus.x + bus.w);
        }
        for (drop, node) in drops.iter().zip(nodes) {
            assert_eq!(
                node.y,
                drop.y + drop.h,
                "node {node:?} does not meet a drop"
            );
            assert!(drop.x >= node.x && drop.x + drop.w <= node.x + node.w);
        }
        for pair in nodes.windows(2) {
            assert!(
                pair[0].x + pair[0].w < pair[1].x,
                "nodes {:?} and {:?} touch",
                pair[0],
                pair[1]
            );
        }
    }
}
