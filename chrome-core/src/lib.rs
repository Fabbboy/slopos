//! Pure geometry and state for the desktop's chrome: the system bar's status
//! items and the indicators drawn in them.
//!
//! Split out of the compositor crate because `userland/Cargo.toml` sets
//! `test = false` on its `[lib]` target, so a `#[cfg(test)]` there never
//! compiles. Free of `alloc`, `unsafe` and any syscall surface.

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
