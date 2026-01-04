//! Surface role and state definitions (Wayland-style)

/// Window state constants
pub const WINDOW_STATE_NORMAL: u8 = 0;
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

/// Maximum number of child subsurfaces per surface
pub const MAX_CHILDREN: usize = 8;

/// Role of a surface in the compositor hierarchy.
///
/// Corresponds to Wayland's xdg_toplevel, xdg_popup, and wl_subsurface roles.
/// Once set, a surface's role cannot be changed (Wayland semantics).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceRole {
    /// No role assigned yet (surface exists but has no role)
    #[default]
    None = 0,
    /// Top-level window (regular application window)
    Toplevel = 1,
    /// Popup surface (menus, tooltips, dropdowns)
    Popup = 2,
    /// Subsurface (child surface positioned relative to parent)
    Subsurface = 3,
}

impl SurfaceRole {
    /// Convert from raw u8 value
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

    /// Check if this role allows having a parent
    #[inline]
    pub fn can_have_parent(self) -> bool {
        matches!(self, Self::Subsurface | Self::Popup)
    }

    /// Check if this is a top-level role
    #[inline]
    pub fn is_toplevel(self) -> bool {
        matches!(self, Self::Toplevel)
    }
}
