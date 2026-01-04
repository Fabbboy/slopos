//! Window and damage region types

/// Maximum damage regions per window (ABI-stable)
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = 8;

/// Maximum damage regions tracked internally (higher resolution)
pub const MAX_INTERNAL_DAMAGE_REGIONS: usize = 32;

/// Maximum buffer age before it's considered invalid (for damage accumulation)
pub const MAX_BUFFER_AGE: u8 = 8;

/// Per-window damage region in surface-local coordinates
#[repr(C)]
#[derive(Copy, Clone, Default, Debug)]
pub struct WindowDamageRect {
    /// Left edge (inclusive)
    pub x0: i32,
    /// Top edge (inclusive)
    pub y0: i32,
    /// Right edge (inclusive)
    pub x1: i32,
    /// Bottom edge (inclusive)
    pub y1: i32,
}

impl WindowDamageRect {
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

    /// Create a damage rect from position and size
    #[inline]
    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            x0: x,
            y0: y,
            x1: x + w - 1,
            y1: y + h - 1,
        }
    }

    /// Check if this rect is valid (non-empty)
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.x1 >= self.x0 && self.y1 >= self.y0
    }

    /// Calculate the width of this rect
    #[inline]
    pub fn width(&self) -> i32 {
        if self.is_valid() {
            self.x1 - self.x0 + 1
        } else {
            0
        }
    }

    /// Calculate the height of this rect
    #[inline]
    pub fn height(&self) -> i32 {
        if self.is_valid() {
            self.y1 - self.y0 + 1
        } else {
            0
        }
    }

    /// Calculate the area of this rect
    #[inline]
    pub fn area(&self) -> i32 {
        self.width() * self.height()
    }

    /// Check if this rect intersects another
    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.x0 <= other.x1
            && self.x1 >= other.x0
            && self.y0 <= other.y1
            && self.y1 >= other.y0
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

    /// Merge this rect with another, returning the union
    #[inline]
    pub fn merge(&self, other: &Self) -> Self {
        self.union(other)
    }
}

/// Window information structure passed between kernel and userland
///
/// This is the ABI-stable structure returned by enumerate_windows syscall.
/// Note: title is `[u8; 32]` (UTF-8) not `[c_char; 32]` to avoid unsafe FFI.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct WindowInfo {
    /// Task ID that owns this window
    pub task_id: u32,
    /// X position on screen
    pub x: i32,
    /// Y position on screen
    pub y: i32,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Window state (NORMAL, MINIMIZED, MAXIMIZED)
    pub state: u8,
    /// Number of damage regions (u8::MAX means full damage)
    pub damage_count: u8,
    /// Padding for alignment
    pub _padding: [u8; 2],
    /// Shared memory token for this surface (0 if not using shared memory)
    pub shm_token: u32,
    /// Individual damage regions
    pub damage_regions: [WindowDamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    /// Window title as UTF-8 bytes (null-terminated)
    pub title: [u8; 32],
}

impl WindowInfo {
    /// Returns true if the window has any pending damage
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.damage_count > 0
    }

    /// Returns true if full surface is damaged (damage_count == u8::MAX)
    #[inline]
    pub fn is_full_damage(&self) -> bool {
        self.damage_count == u8::MAX
    }

    /// Get the title as a string slice
    #[inline]
    pub fn title_str(&self) -> &str {
        let len = self
            .title
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.title.len());
        core::str::from_utf8(&self.title[..len]).unwrap_or("<invalid>")
    }

    /// Get the window bounds as a damage rect
    #[inline]
    pub fn bounds(&self) -> WindowDamageRect {
        WindowDamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + self.width as i32 - 1,
            y1: self.y + self.height as i32 - 1,
        }
    }

    /// Get valid damage regions slice
    #[inline]
    pub fn damage_regions(&self) -> &[WindowDamageRect] {
        if self.is_full_damage() {
            &[]
        } else {
            let count = (self.damage_count as usize).min(MAX_WINDOW_DAMAGE_REGIONS);
            &self.damage_regions[..count]
        }
    }
}

impl Default for WindowInfo {
    fn default() -> Self {
        Self {
            task_id: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            state: 0,
            damage_count: 0,
            _padding: [0; 2],
            shm_token: 0,
            damage_regions: [WindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS],
            title: [0; 32],
        }
    }
}

/// Framebuffer information structure
#[repr(C)]
#[derive(Default, Copy, Clone, Debug)]
pub struct FbInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
    pub _padding: [u8; 2],
}
