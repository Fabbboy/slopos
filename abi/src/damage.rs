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

    /// Check whether the point `(x, y)` lies inside this rect (inclusive).
    #[inline]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// Maximum damage regions for internal/kernel tracking (higher resolution)
pub const MAX_INTERNAL_DAMAGE_REGIONS: usize = 32;
