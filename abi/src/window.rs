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
    /// Check if this rect is valid (non-empty)
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.x1 >= self.x0 && self.y1 >= self.y0
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
