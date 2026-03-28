//! Hover tracking registry for interactive compositor elements.
//!
//! Each frame the compositor registers interactive regions with their hit-test
//! results. The registry auto-diffs against the previous frame and reports
//! damage rects for regions whose hover state changed.

use crate::gfx::DamageRect;

/// Maximum number of interactive regions tracked per frame.
const MAX_HOVER_REGIONS: usize = 64;

// ── Hover ID namespace constants ────────────────────────────────────────────

pub const HOVER_SIGNAL_GROUP_BASE: u32 = 0x0006_0000; // + task_id

// ── Region ──────────────────────────────────────────────────────────────────

#[derive(Copy, Clone)]
struct HoverRegion {
    id: u32,
    rect: DamageRect,
    hovered: bool,
}

impl HoverRegion {
    const fn empty() -> Self {
        Self {
            id: 0,
            rect: DamageRect::invalid(),
            hovered: false,
        }
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

pub struct HoverRegistry {
    current: [HoverRegion; MAX_HOVER_REGIONS],
    current_count: usize,
    previous: [HoverRegion; MAX_HOVER_REGIONS],
    previous_count: usize,
}

impl HoverRegistry {
    pub fn new() -> Self {
        Self {
            current: [HoverRegion::empty(); MAX_HOVER_REGIONS],
            current_count: 0,
            previous: [HoverRegion::empty(); MAX_HOVER_REGIONS],
            previous_count: 0,
        }
    }

    /// Swap current → previous and reset current for the new frame.
    pub fn begin_frame(&mut self) {
        self.previous = self.current;
        self.previous_count = self.current_count;
        self.current_count = 0;
    }

    /// Register an interactive region for the current frame.
    pub fn register(&mut self, id: u32, rect: DamageRect, hovered: bool) {
        if self.current_count >= MAX_HOVER_REGIONS {
            return;
        }
        self.current[self.current_count] = HoverRegion { id, rect, hovered };
        self.current_count += 1;
    }

    /// Check whether a region is hovered in the current frame.
    #[allow(dead_code)]
    pub fn is_hovered(&self, id: u32) -> bool {
        for i in 0..self.current_count {
            if self.current[i].id == id {
                return self.current[i].hovered;
            }
        }
        false
    }

    /// Diff current vs previous frame and write damage rects for regions whose
    /// hover state changed, appeared while hovered, or disappeared while hovered.
    /// Returns the number of rects written.
    pub fn changed_regions(&self, out: &mut [DamageRect]) -> usize {
        let mut count = 0usize;

        // Check current regions against previous
        for i in 0..self.current_count {
            let cur = &self.current[i];
            match self.find_previous(cur.id) {
                Some(prev) => {
                    if cur.hovered != prev.hovered {
                        if count < out.len() && prev.rect.is_valid() {
                            out[count] = prev.rect;
                            count += 1;
                        }
                        if count < out.len() && cur.rect.is_valid() {
                            out[count] = cur.rect;
                            count += 1;
                        }
                    }
                }
                None => {
                    if cur.hovered && count < out.len() && cur.rect.is_valid() {
                        out[count] = cur.rect;
                        count += 1;
                    }
                }
            }
        }

        // Regions that disappeared while hovered
        for i in 0..self.previous_count {
            let prev = &self.previous[i];
            if prev.hovered && self.find_current(prev.id).is_none() {
                if count < out.len() && prev.rect.is_valid() {
                    out[count] = prev.rect;
                    count += 1;
                }
            }
        }

        count
    }

    fn find_previous(&self, id: u32) -> Option<&HoverRegion> {
        for i in 0..self.previous_count {
            if self.previous[i].id == id {
                return Some(&self.previous[i]);
            }
        }
        None
    }

    fn find_current(&self, id: u32) -> Option<&HoverRegion> {
        for i in 0..self.current_count {
            if self.current[i].id == id {
                return Some(&self.current[i]);
            }
        }
        None
    }
}
