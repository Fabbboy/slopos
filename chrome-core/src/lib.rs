//! Pure geometry and state for the desktop's chrome: the system bar's status
//! items and the indicators drawn in them.
//!
//! **The compositor crate cannot be tested.** `userland/Cargo.toml` sets
//! `test = false` on its `[lib]` target and `just test-host` does not name
//! `slopos-userland`, so every `#[cfg(test)]` inside it is dead code that never
//! compiles and never runs. Chrome geometry is exactly the kind of logic where
//! a wrong answer looks like a right one until someone clicks the wrong pixel,
//! so it lives here, where `cargo test -p slopos-chrome-core` runs it on the
//! host.
//!
//! **Layout and hit-testing must be one function.** A bar that draws from a
//! cached layout and hit-tests against a different one produces a widget that
//! does not respond to its own click. [`status::layout_status_items`] is the
//! single source both paths call, and a test walks every x coordinate of a
//! 1920-wide screen asserting the two agree.
//!
//! **Indicator art is geometry, not pixels.** `gfx::image::draw_image` has no
//! colour-modulation parameter, so a sprite sheet would need one bitmap per
//! ink×badge combination. [`glyph`] describes the network icon as axis-aligned
//! rectangles in a unit grid instead: no anti-aliasing at scale 1, integer
//! scaling, one shape recoloured per state, and no third-party icon set to
//! license.
//!
//! Human-readable strings come from [`slopos_net_core::render`] rather than
//! being spelled a second time here, so the bar and `ip` never contradict each
//! other. The console font covers ASCII, Latin-1 and exactly `€ ˚ ˇ`, so an em
//! dash draws as the replacement glyph; a test holds every string this crate
//! can produce to `slopos_net_core::render::is_renderable`.
//!
//! Everything here is free of `alloc`, `unsafe` and any syscall surface.

#![no_std]
#![forbid(unsafe_code)]

pub mod glyph;
pub mod netstate;
pub mod positioner;
pub mod status;
pub mod toggle;

pub use glyph::{Badge, GLYPH_H, GLYPH_W, GlyphBase, GlyphRect, GlyphSpec, Ink, glyph_for};
pub use netstate::{
    IfaceKind, IfaceRow, NO_ADDRESS_LABEL, NetIndicatorState, NetPanelModel, PANEL_TITLE,
    indicator_label, indicator_state_for,
};
pub use positioner::{Anchor, ConstraintAdjustment, Gravity, Positioner, Rect, Size, position};
pub use status::{
    BAR_HEIGHT, BAR_ITEM_GAP, BAR_PADDING_X, MAX_STATUS_ITEMS, StatusItemSpec, StatusKind,
    StatusLayout, StatusSlot, hit_status_item, layout_status_items,
};
pub use toggle::{TOGGLE_OFF, TOGGLE_ON, ToggleGeometry, toggle_geometry};
