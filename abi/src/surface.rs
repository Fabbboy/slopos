//! Surface role and state definitions (Wayland-style)

pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

pub const MAX_CHILDREN: usize = 8;

/// Corresponds to Wayland's xdg_toplevel, xdg_popup, and wl_subsurface roles.
/// Once set, a surface's role cannot be changed (Wayland semantics).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceRole {
    #[default]
    None = 0,
    Toplevel = 1,
    Popup = 2,
    Subsurface = 3,
}

impl SurfaceRole {
    #[inline]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::None),
            1 => Some(Self::Toplevel),
            2 => Some(Self::Popup),
            3 => Some(Self::Subsurface),
            _ => None,
        }
    }

    #[inline]
    pub fn can_have_parent(self) -> bool {
        matches!(self, Self::Subsurface | Self::Popup)
    }

    #[inline]
    pub fn is_toplevel(self) -> bool {
        matches!(self, Self::Toplevel)
    }
}
