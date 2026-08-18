//! Hover tracking registry for interactive compositor elements.
//!
//! Regions are registered per frame; the registry diffs against the previous
//! frame and reports damage rects for regions whose hover state changed.

use crate::gfx::DamageRect;

const MAX_HOVER_REGIONS: usize = 64;

pub const HOVER_SIGNAL_GROUP_BASE: u32 = 0x0006_0000; // + task_id
pub const HOVER_STATUS_ITEM_BASE: u32 = 0x0007_0000; // + StatusKind discriminant

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

    pub fn begin_frame(&mut self) {
        self.previous = self.current;
        self.previous_count = self.current_count;
        self.current_count = 0;
    }

    pub fn register(&mut self, id: u32, rect: DamageRect, hovered: bool) {
        if self.current_count >= MAX_HOVER_REGIONS {
            return;
        }
        self.current[self.current_count] = HoverRegion { id, rect, hovered };
        self.current_count += 1;
    }

    #[allow(dead_code)]
    pub fn is_hovered(&self, id: u32) -> bool {
        for i in 0..self.current_count {
            if self.current[i].id == id {
                return self.current[i].hovered;
            }
        }
        false
    }

    /// Writes damage rects for regions whose hover state changed, or that
    /// appeared or disappeared while hovered. Returns the number written.
    pub fn changed_regions(&self, out: &mut [DamageRect]) -> usize {
        let mut count = 0usize;

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
