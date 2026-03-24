use slopos_abi::draw::Color32;

// Window / UI Sizes
pub const TITLE_BAR_HEIGHT: i32 = 24;
pub const BUTTON_SIZE: i32 = 20;
pub const BUTTON_PADDING: i32 = 2;

// Taskbar Sizes
pub const TASKBAR_HEIGHT: i32 = 32;
pub const TASKBAR_BUTTON_PADDING: i32 = 4;
pub const START_BUTTON_WIDTH: i32 = 56;
pub const START_APPS_GAP: i32 = 14;
pub const START_MENU_WIDTH: i32 = 180;
pub const START_MENU_ITEM_HEIGHT: i32 = 24;
pub const START_MENU_PADDING: i32 = 6;

// Colors - Dark Roulette Theme
pub const COLOR_TITLE_BAR: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const COLOR_TITLE_BAR_FOCUSED: Color32 = Color32::rgb(0x2D, 0x2D, 0x30);
pub const COLOR_BUTTON: Color32 = Color32::rgb(0x3E, 0x3E, 0x42);
pub const COLOR_BUTTON_HOVER: Color32 = Color32::rgb(0x50, 0x50, 0x52);
pub const COLOR_BUTTON_CLOSE_HOVER: Color32 = Color32::rgb(0xE8, 0x11, 0x23);
pub const COLOR_TEXT: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const COLOR_TASKBAR: Color32 = Color32::rgb(0x25, 0x25, 0x26);
pub const COLOR_CURSOR: Color32 = Color32::rgb(0xFF, 0xFF, 0xFF);
pub const COLOR_BACKGROUND: Color32 = Color32::rgb(0x00, 0x11, 0x22);
pub const COLOR_START_MENU_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);

// Shadow
pub const SHADOW_SPREAD: i32 = 12;
pub const SHADOW_OFFSET_Y: i32 = 4;
pub const SHADOW_MAX_ALPHA: u8 = 50;

// Semi-transparent title bar overlays (alpha-blended over underlying content)
pub const COLOR_TITLE_BAR_TINT: Color32 = Color32::new(0x1E, 0x1E, 0x1E, 0xD0);
pub const COLOR_TITLE_BAR_FOCUSED_TINT: Color32 = Color32::new(0x2D, 0x2D, 0x30, 0xD0);

// File Manager Specific
pub const FM_WIDTH: i32 = 400;
pub const FM_HEIGHT: i32 = 300;
pub const FM_TITLE_HEIGHT: i32 = TITLE_BAR_HEIGHT;
pub const FM_ITEM_HEIGHT: i32 = 20;
pub const FM_COLOR_BG: Color32 = Color32::rgb(0x25, 0x25, 0x26);
pub const FM_COLOR_FG: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const FM_COLOR_HL: Color32 = Color32::rgb(0x3E, 0x3E, 0x42);
pub const FM_BUTTON_WIDTH: i32 = 40;
pub const SYSINFO_BUTTON_WIDTH: i32 = 48;
