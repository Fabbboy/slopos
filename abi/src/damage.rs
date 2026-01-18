//! Damage tracking for compositor and rendering
//!
//! This module provides unified damage tracking types used by both
//! the kernel compositor and userland drawing buffers.
//!
//! The generic `DamageTracker<N>` supports different capacities:
//! - `DamageTracker` (8 regions) - lightweight, for userland clients
//! - `InternalDamageTracker` (32 regions) - higher resolution, for kernel compositor

/// Maximum damage regions for client-side tracking (ABI-stable)
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

/// Generic damage tracker with configurable capacity.
///
/// Tracks rectangular damage regions with automatic merging when at capacity.
/// Supports a `full_damage` mode for graceful degradation when tracking overflows.
#[derive(Clone)]
pub struct DamageTracker<const N: usize = MAX_DAMAGE_REGIONS> {
    regions: [DamageRect; N],
    count: u8,
    /// Set when damage exceeds capacity - means entire surface is dirty
    full_damage: bool,
}

impl<const N: usize> Default for DamageTracker<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> DamageTracker<N> {
    /// Create an empty damage tracker
    pub const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); N],
            count: 0,
            full_damage: false,
        }
    }

    /// Add a damage region.
    ///
    /// When at capacity, uses `merge_smallest_pair()` to make room.
    /// This is the default strategy suitable for most use cases.
    pub fn add(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        if self.full_damage {
            return;
        }

        if (self.count as usize) >= N {
            self.merge_smallest_pair();
        }

        if (self.count as usize) < N {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        } else {
            // Still no space after merge - mark full damage
            self.full_damage = true;
        }
    }

    /// Add a damage region with immediate overlap merging.
    ///
    /// If the new rect intersects an existing region, they are merged immediately.
    /// When at capacity with no overlaps, marks full_damage.
    /// This strategy is better for high-frequency updates (kernel compositor).
    pub fn add_merge_overlapping(&mut self, rect: DamageRect) {
        if !rect.is_valid() {
            return;
        }

        if self.full_damage {
            return;
        }

        // Try to merge with existing regions
        for i in 0..(self.count as usize) {
            if self.regions[i].intersects(&rect) {
                self.regions[i] = self.regions[i].union(&rect);
                self.merge_all_overlapping();
                return;
            }
        }

        // Add as new region if space available
        if (self.count as usize) < N {
            self.regions[self.count as usize] = rect;
            self.count += 1;
        } else {
            // No space - mark full damage
            self.full_damage = true;
        }
    }

    /// Add damage by coordinates
    #[inline]
    pub fn add_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.add(DamageRect { x0, y0, x1, y1 });
    }

    /// Merge the pair of regions with smallest combined area
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

    /// Merge all overlapping regions to reduce count
    fn merge_all_overlapping(&mut self) {
        if self.count <= 1 {
            return;
        }

        let mut i = 0;
        while i < self.count as usize {
            let mut j = i + 1;
            while j < self.count as usize {
                if self.regions[i].intersects(&self.regions[j]) {
                    self.regions[i] = self.regions[i].union(&self.regions[j]);
                    // Remove region j by swapping with last
                    self.count -= 1;
                    self.regions[j] = self.regions[self.count as usize];
                    // Don't increment j - check the swapped region
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    /// Clear all damage
    #[inline]
    pub fn clear(&mut self) {
        self.count = 0;
        self.full_damage = false;
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

    /// Check if there is any damage (regions or full_damage)
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.count > 0 || self.full_damage
    }

    /// Check if there are no damage regions and not full_damage
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 && !self.full_damage
    }

    /// Check if full surface damage is indicated
    #[inline]
    pub fn is_full_damage(&self) -> bool {
        self.full_damage
    }

    /// Mark the entire surface as damaged
    #[inline]
    pub fn set_full_damage(&mut self) {
        self.full_damage = true;
    }

    /// Export to a smaller array format (for ABI crossing).
    ///
    /// Returns (regions_array, count) where count is u8::MAX if full_damage
    /// or if regions had to be truncated.
    pub fn export_to_array<const M: usize>(&self) -> ([DamageRect; M], u8) {
        let mut out = [DamageRect::invalid(); M];

        if self.full_damage {
            return (out, u8::MAX);
        }

        let export_count = (self.count as usize).min(M);
        for i in 0..export_count {
            out[i] = self.regions[i];
        }

        // If we had to truncate, indicate full damage
        if (self.count as usize) > M {
            return (out, u8::MAX);
        }

        (out, export_count as u8)
    }
}

/// Maximum damage regions for internal/kernel tracking (higher resolution)
pub const MAX_INTERNAL_DAMAGE_REGIONS: usize = 32;

/// Type alias for internal/kernel damage tracking (32 regions)
pub type InternalDamageTracker = DamageTracker<MAX_INTERNAL_DAMAGE_REGIONS>;
