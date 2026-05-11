use slopos_abi::draw::Color32;

// ---------------------------------------------------------------------------
// Helper: pack a Color32 (RGB) + alpha into a u32 ARGB value
// ---------------------------------------------------------------------------

/// Pack an opaque `Color32` with a separate alpha byte into a single `u32`
/// in 0xAARRGGBB layout.
pub const fn argb(c: Color32, a: u8) -> u32 {
    (a as u32) << 24 | (c.red() as u32) << 16 | (c.green() as u32) << 8 | c.blue() as u32
}

// ===========================================================================
// Dimensions -- System Bar
// ===========================================================================

pub const SYSTEM_BAR_HEIGHT: i32 = 24;
pub const SYSTEM_BAR_PADDING_X: i32 = 12;
pub const SYSTEM_BAR_ICON_SIZE: i32 = 8;
pub const SYSTEM_BAR_ICON_GAP: i32 = 8;
pub const SYSTEM_BAR_MAX_APP_NAME_WIDTH: i32 = 200;
pub const SYSTEM_BAR_CLOCK_GAP: i32 = 16;

// ===========================================================================
// Dimensions -- Window Decorations / Title Bar
// ===========================================================================

pub const TITLE_BAR_HEIGHT: i32 = 28;
pub const WINDOW_CORNER_RADIUS: i32 = 8;
pub const TITLE_MAX_TEXT_WIDTH_MARGIN: i32 = 140;
pub const MIN_WINDOW_WIDTH: i32 = 200;
pub const MIN_WINDOW_HEIGHT: i32 = 150;

// ===========================================================================
// Dimensions -- Signal Buttons (window close / minimize / expand)
// ===========================================================================

pub const SIGNAL_BUTTON_DIAMETER: i32 = 12;
pub const SIGNAL_BUTTON_RADIUS: i32 = 6;
pub const SIGNAL_BUTTON_SPACING: i32 = 20;
pub const SIGNAL_BUTTON_1_CX: i32 = 18;
pub const SIGNAL_BUTTON_2_CX: i32 = 38;
pub const SIGNAL_BUTTON_3_CX: i32 = 58;
pub const SIGNAL_BUTTON_CY: i32 = 14;
pub const SIGNAL_GROUP_X: i32 = 6;
pub const SIGNAL_GROUP_Y: i32 = 2;
pub const SIGNAL_GROUP_W: i32 = 64;
pub const SIGNAL_GROUP_H: i32 = 24;
pub const SIGNAL_GLYPH_SIZE: i32 = 6;

// ===========================================================================
// Dimensions -- Shelf (dock)
// ===========================================================================

pub const SHELF_BOTTOM_MARGIN: i32 = 4;
pub const SHELF_PILL_RADIUS: i32 = 12;
pub const SHELF_PILL_PADDING_X: i32 = 12;
pub const SHELF_PILL_PADDING_Y: i32 = 6;
pub const SHELF_ICON_SIZE: i32 = 48;
pub const SHELF_ICON_SIZE_MAX: i32 = 64;
pub const SHELF_ICON_SPACING: i32 = 8;
pub const SHELF_ICON_CORNER_RADIUS: i32 = 10;
pub const SHELF_SEPARATOR_WIDTH: i32 = 2;
pub const SHELF_SEPARATOR_HEIGHT: i32 = 32;
pub const SHELF_SEPARATOR_MARGIN_X: i32 = 8;
pub const SHELF_DOT_DIAMETER: i32 = 4;
pub const SHELF_DOT_MARGIN_Y: i32 = 3;
pub const SHELF_LABEL_PADDING_X: i32 = 6;
pub const SHELF_LABEL_PADDING_Y: i32 = 3;
pub const SHELF_LABEL_RADIUS: i32 = 4;
pub const SHELF_LABEL_GAP_Y: i32 = 6;

// ===========================================================================
// Dimensions -- Magnification
// ===========================================================================

pub const MAGNIFICATION_PROXIMITY_X: i32 = 120;
pub const MAGNIFICATION_PROXIMITY_Y: i32 = 80;
/// 0.33 in 8.8 fixed-point (0.33 * 256 = ~84).
pub const MAGNIFICATION_AMOUNT_256: i32 = 84;

// ===========================================================================
// Dimensions -- Shadow
// ===========================================================================

pub const SHADOW_SPREAD: i32 = 12;
pub const SHADOW_OFFSET_Y: i32 = 4;
pub const SHADOW_MAX_ALPHA: u8 = 50;

// ===========================================================================
// Dimensions -- Timing
// ===========================================================================

/// Grace period (ms) before force-terminating an app on close.
pub const CLOSE_GRACE_MS: u32 = 1500;

// ===========================================================================
// Colors -- Panel / System Bar
// ===========================================================================

/// System bar / shelf panel background (semi-transparent).
pub const PANEL_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);
pub const PANEL_BG_ALPHA: u8 = 0xCC;

/// System bar bottom border line (opaque).
pub const PANEL_BORDER: Color32 = Color32::rgb(0x2A, 0x2A, 0x2C);
pub const PANEL_BORDER_ALPHA: u8 = 0xFF;

// ===========================================================================
// Colors -- Shelf
// ===========================================================================

/// Shelf pill background (semi-transparent).
pub const SHELF_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);
pub const SHELF_BG_ALPHA: u8 = 0xB0;

/// Shelf divider line.
pub const SHELF_SEPARATOR: Color32 = Color32::rgb(0x3A, 0x3A, 0x3C);
pub const SHELF_SEPARATOR_ALPHA: u8 = 0xFF;

/// Running indicator dot.
pub const SHELF_DOT_ACTIVE: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const SHELF_DOT_ACTIVE_ALPHA: u8 = 0xFF;

/// Label tooltip backdrop (semi-transparent).
pub const SHELF_LABEL_BG: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const SHELF_LABEL_BG_ALPHA: u8 = 0xCC;

// ===========================================================================
// Colors -- Window Decorations / Title Bar
// ===========================================================================

/// Focused window title bar.
pub const TITLE_BAR_FOCUSED: Color32 = Color32::rgb(0x2D, 0x2D, 0x30);
pub const TITLE_BAR_FOCUSED_ALPHA: u8 = 0xFF;

/// Unfocused window title bar.
pub const TITLE_BAR_UNFOCUSED: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const TITLE_BAR_UNFOCUSED_ALPHA: u8 = 0xFF;

// ===========================================================================
// Colors -- Signal Buttons
// ===========================================================================

/// Close button (red).
pub const SIGNAL_CLOSE: Color32 = Color32::rgb(0xFF, 0x5F, 0x57);
pub const SIGNAL_CLOSE_ALPHA: u8 = 0xFF;

/// Minimize button (yellow).
pub const SIGNAL_MINIMIZE: Color32 = Color32::rgb(0xFF, 0xBD, 0x2E);
pub const SIGNAL_MINIMIZE_ALPHA: u8 = 0xFF;

/// Expand button (green).
pub const SIGNAL_EXPAND: Color32 = Color32::rgb(0x28, 0xC8, 0x40);
pub const SIGNAL_EXPAND_ALPHA: u8 = 0xFF;

/// Unfocused signal buttons.
pub const SIGNAL_INACTIVE: Color32 = Color32::rgb(0x3E, 0x3E, 0x42);
pub const SIGNAL_INACTIVE_ALPHA: u8 = 0xFF;

/// Button glyph ink.
pub const SIGNAL_GLYPH: Color32 = Color32::rgb(0x1A, 0x0A, 0x0A);
pub const SIGNAL_GLYPH_ALPHA: u8 = 0xFF;

// ===========================================================================
// Colors -- Desktop / Text
// ===========================================================================

/// Desktop background.
pub const DESKTOP_BG: Color32 = Color32::rgb(0x00, 0x11, 0x22);
pub const DESKTOP_BG_ALPHA: u8 = 0xFF;

/// Active/primary text.
pub const TEXT_PRIMARY: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const TEXT_PRIMARY_ALPHA: u8 = 0xFF;

/// Inactive/secondary text.
pub const TEXT_SECONDARY: Color32 = Color32::rgb(0x90, 0x90, 0x90);
pub const TEXT_SECONDARY_ALPHA: u8 = 0xFF;

// ===========================================================================
// Colors -- Shadow
// ===========================================================================

/// Shadow base color (alpha varies per ring).
pub const SHADOW_COLOR: Color32 = Color32::rgb(0x00, 0x00, 0x00);

// ===========================================================================
// Colors -- Icon Placeholders
// ===========================================================================

pub const ICON_SHELL: Color32 = Color32::rgb(0x4A, 0x6F, 0xA5);
pub const ICON_SHELL_ALPHA: u8 = 0xFF;

pub const ICON_FILES: Color32 = Color32::rgb(0x6B, 0x8E, 0x5A);
pub const ICON_FILES_ALPHA: u8 = 0xFF;

pub const ICON_MONITOR: Color32 = Color32::rgb(0x8E, 0x6B, 0x5A);
pub const ICON_MONITOR_ALPHA: u8 = 0xFF;

pub const ICON_DEFAULT: Color32 = Color32::rgb(0x6B, 0x5A, 0x8E);
pub const ICON_DEFAULT_ALPHA: u8 = 0xFF;

// ===========================================================================
// Legacy aliases -- kept for existing consumers during chrome migration
// ===========================================================================

pub const COLOR_BACKGROUND: Color32 = DESKTOP_BG;
pub const COLOR_CURSOR: Color32 = Color32::rgb(0xFF, 0xFF, 0xFF);
pub const COLOR_TEXT: Color32 = TEXT_PRIMARY;

// Old window-decoration constants -- aliased to new names so that files
// not yet ported to the new chrome still compile.
pub const COLOR_TITLE_BAR: Color32 = TITLE_BAR_UNFOCUSED;
pub const COLOR_TITLE_BAR_FOCUSED: Color32 = TITLE_BAR_FOCUSED;
pub const COLOR_TITLE_BAR_TINT: Color32 = Color32::new(0x1E, 0x1E, 0x1E, 0xD0);
pub const COLOR_TITLE_BAR_FOCUSED_TINT: Color32 = Color32::new(0x2D, 0x2D, 0x30, 0xD0);
pub const COLOR_BUTTON: Color32 = SIGNAL_INACTIVE;
pub const COLOR_BUTTON_HOVER: Color32 = Color32::rgb(0x50, 0x50, 0x52);
pub const COLOR_BUTTON_CLOSE_HOVER: Color32 = SIGNAL_CLOSE;
pub const BUTTON_SIZE: i32 = 20;
pub const BUTTON_PADDING: i32 = 2;

// Old taskbar/start-menu constants -- aliased so the compositor compiles
// until the taskbar is replaced by the shelf + system bar.
pub const TASKBAR_HEIGHT: i32 = 32;
pub const TASKBAR_BUTTON_PADDING: i32 = 4;
pub const START_BUTTON_WIDTH: i32 = 56;
pub const START_APPS_GAP: i32 = 14;
pub const START_MENU_WIDTH: i32 = 180;
pub const START_MENU_ITEM_HEIGHT: i32 = 24;
pub const START_MENU_PADDING: i32 = 6;
pub const COLOR_TASKBAR: Color32 = PANEL_BG;
pub const COLOR_START_MENU_BG: Color32 = PANEL_BG;

// File Manager Specific
pub const FM_WIDTH: i32 = 640;
pub const FM_HEIGHT: i32 = 420;
pub const FM_TITLE_HEIGHT: i32 = TITLE_BAR_HEIGHT;
pub const FM_ITEM_HEIGHT: i32 = 22;
pub const FM_COLOR_BG: Color32 = Color32::rgb(0x1E, 0x1E, 0x20);
pub const FM_COLOR_FG: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const FM_COLOR_HL: Color32 = Color32::rgb(0x3E, 0x3E, 0x42);
pub const FM_BUTTON_WIDTH: i32 = 40;

// File Manager -- Sidebar
pub const FM_SIDEBAR_WIDTH: i32 = 140;
pub const FM_SIDEBAR_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);
pub const FM_SIDEBAR_ITEM_HEIGHT: i32 = 22;
pub const FM_SIDEBAR_HOVER: Color32 = Color32::rgb(0x2A, 0x2A, 0x2E);
pub const FM_SIDEBAR_ACTIVE: Color32 = Color32::rgb(0x30, 0x50, 0x80);
pub const FM_SIDEBAR_TEXT: Color32 = Color32::rgb(0xB0, 0xB0, 0xB0);
pub const FM_SIDEBAR_HEADING: Color32 = Color32::rgb(0x70, 0x70, 0x74);

// File Manager -- Navigation bar
pub const FM_NAV_HEIGHT: i32 = 28;
pub const FM_NAV_BG: Color32 = Color32::rgb(0x25, 0x25, 0x28);
pub const FM_NAV_BUTTON: Color32 = Color32::rgb(0x38, 0x38, 0x3C);
pub const FM_NAV_BUTTON_HOVER: Color32 = Color32::rgb(0x48, 0x48, 0x4E);
pub const FM_NAV_BUTTON_DISABLED: Color32 = Color32::rgb(0x2A, 0x2A, 0x2E);

// File Manager -- List view
pub const FM_LIST_HEADER_HEIGHT: i32 = 20;
pub const FM_LIST_HEADER_BG: Color32 = Color32::rgb(0x22, 0x22, 0x24);
pub const FM_LIST_HEADER_BORDER: Color32 = Color32::rgb(0x35, 0x35, 0x38);
pub const FM_LIST_HOVER: Color32 = Color32::rgb(0x2A, 0x2A, 0x2E);
pub const FM_LIST_SELECTED: Color32 = Color32::rgb(0x28, 0x48, 0x78);
pub const FM_LIST_ALT_BG: Color32 = Color32::rgb(0x20, 0x20, 0x22);
pub const FM_DIR_COLOR: Color32 = Color32::rgb(0x60, 0x9C, 0xF0);
pub const FM_FILE_COLOR: Color32 = Color32::rgb(0xD0, 0xD0, 0xD0);
pub const FM_SIZE_COLOR: Color32 = Color32::rgb(0x80, 0x80, 0x84);
pub const FM_ERROR_COLOR: Color32 = Color32::rgb(0xE0, 0x50, 0x50);

// File Manager -- Status bar
pub const FM_STATUS_HEIGHT: i32 = 20;
pub const FM_STATUS_BG: Color32 = Color32::rgb(0x22, 0x22, 0x24);
pub const FM_STATUS_TEXT: Color32 = Color32::rgb(0x90, 0x90, 0x94);

// File Manager -- Scrollbar
pub const FM_SCROLLBAR_WIDTH: i32 = 6;
pub const FM_SCROLLBAR_BG: Color32 = Color32::rgb(0x1E, 0x1E, 0x20);
pub const FM_SCROLLBAR_THUMB: Color32 = Color32::rgb(0x50, 0x50, 0x54);

pub const SYSINFO_BUTTON_WIDTH: i32 = 48;
