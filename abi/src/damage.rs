//! Damage tracking for compositor and rendering
//!
//! This module provides unified damage tracking types used by both
//! the kernel compositor and userland drawing buffers.
//!
//! This replaces duplicate implementations in:
//! - video/src/compositor_context.rs (DamageRect/DamageTracker)
//! - userland/src/gfx/mod.rs (DamageRect/DamageTracker)

/// Maximum damage regions before automatic merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

/// A rectangular damage region in buffer-local coordinates
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32, // inclusive
    pub y1: i32, // inclusive
}

impl DamageRect {
    /// Create an invalid (empty) damage rect
    #[inline]
    pub const fn invalid() -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: -1,
            y1: -1,
        }
    }

    /// Check if this rect is valid (non-empty)
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    /// Calculate the area of this rect
    #[inline]
    pub fn area(&self) -> i32 {
        if !self.is_valid() {
            0
        } else {
            (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)
        }
    }

    /// Compute the union (bounding box) of two rects
    #[inline]
    pub fn union(&self, other: &Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Calculate what the area would be if merged with another rect
    #[inline]
    pub fn combined_area(&self, other: &Self) -> i32 {
        self.union(other).area()
    }

    /// Clip this rect to buffer bounds
    #[inline]
    pub fn clip(&self, width: i32, height: i32) -> Self {
        Self {
            x0: self.x0.max(0),
            y0: self.y0.max(0),
            x1: self.x1.min(width - 1),
            y1: self.y1.min(height - 1),
        }
    }

    /// Check if this rect intersects with another
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.x0 <= other.x1 && self.x1 >= other.x0 && self.y0 <= other.y1 && self.y1 >= other.y0
    }
}

/// Tracks damage regions with automatic merging when at capacity
#[derive(Clone)]
pub struct DamageTracker {
    regions: [DamageRect; MAX_DAMAGE_REGIONS],
    count: u8,
}

impl Default for DamageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DamageTracker {
    /// Create an empty damage tracker
    pub const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            count: 0,
        }
    }

    /// Add a damage region, merging if at capacity
    pub fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        if (self.count as usize) >= MAX_DAMAGE_REGIONS {
            self.merge_smallest_pair();
        }

        if (self.count as usize) < MAX_DAMAGE_REGIONS {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        }
    }

    /// Add damage by coordinates
    #[inline]
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.add(DamageRect { x0, y0, x1, y1 });
    }

    fn merge_smallest_pair(&mut self) {
        if self.count < 2 {
            return;
        }

        let count = self.count as usize;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        for i in 0..count {
            for j in (i + 1)..count {
                let combined = self.regions[i].combined_area(&self.regions[j]);
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        self.regions[best_i] = self.regions[best_i].union(&self.regions[best_j]);
        if best_j < count - 1 {
            self.regions[best_j] = self.regions[count - 1];
        }
        self.count -= 1;
    }

    /// Clear all damage
    #[inline]
    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// Get the number of damage regions
    #[inline]
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Get the damage regions slice
    #[inline]
    pub fn regions(&self) -> &[DamageRect] {
        &self.regions[..self.count as usize]
    }

    /// Get the bounding box of all damage
    pub fn bounding_box(&self) -> DamageRect {
        if self.count == 0 {
            return DamageRect::invalid();
        }
        let mut result = self.regions[0];
        for i in 1..self.count as usize {
            result = result.union(&self.regions[i]);
        }
        result
    }

    /// Check if there is any damage
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.count > 0
    }
}
