use crate::damage::DamageRect;

pub use crate::damage::{
    MAX_DAMAGE_REGIONS as MAX_WINDOW_DAMAGE_REGIONS, MAX_INTERNAL_DAMAGE_REGIONS,
};

pub const MAX_BUFFER_AGE: u8 = 8;

pub const CURSOR_SHAPE_DEFAULT: u8 = 0;
pub const CURSOR_SHAPE_TEXT: u8 = 1;
pub const CURSOR_SHAPE_POINTER: u8 = 2;

/// Fixed-size application identifier following the Wayland `set_app_id()` convention.
/// Apps declare their identity (e.g. "org.slopos.shell") and the compositor uses
/// this for window-to-dock matching instead of the window title.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AppId(pub [u8; 32]);

impl AppId {
    pub const EMPTY: Self = Self([0u8; 32]);

    pub fn from_str(s: &str) -> Self {
        let mut buf = [0u8; 32];
        let len = s.len().min(31);
        buf[..len].copy_from_slice(&s.as_bytes()[..len]);
        Self(buf)
    }

    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        core::str::from_utf8(&self.0[..len]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.0[0] == 0
    }
}

impl Default for AppId {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WindowInfo {
    pub task_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: u8,
    pub damage_count: u8,
    pub cursor_shape: u8,
    pub _padding: u8,
    pub shm_token: u32,
    pub damage_regions: [DamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [u8; 32],
    pub app_id: AppId,
}

impl WindowInfo {
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.damage_count > 0
    }

    #[inline]
    pub fn is_full_damage(&self) -> bool {
        self.damage_count == u8::MAX
    }

    #[inline]
    pub fn title_str(&self) -> &str {
        let len = self
            .title
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.title.len());
        core::str::from_utf8(&self.title[..len]).unwrap_or("<invalid>")
    }

    #[inline]
    pub fn bounds(&self) -> DamageRect {
        DamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + self.width as i32 - 1,
            y1: self.y + self.height as i32 - 1,
        }
    }

    #[inline]
    pub fn app_id_str(&self) -> &str {
        self.app_id.as_str()
    }

    #[inline]
    pub fn damage_regions(&self) -> &[DamageRect] {
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
            cursor_shape: 0,
            _padding: 0,
            shm_token: 0,
            damage_regions: [DamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS],
            title: [0; 32],
            app_id: AppId::EMPTY,
        }
    }
}
