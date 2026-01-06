use crate::gfx::rgb;

// Window / UI Sizes
pub const TITLE_BAR_HEIGHT: i32 = 24;
pub const BUTTON_SIZE: i32 = 20;
pub const BUTTON_PADDING: i32 = 2;

// Taskbar Sizes
pub const TASKBAR_HEIGHT: i32 = 32;
pub const TASKBAR_BUTTON_WIDTH: i32 = 120;
pub const TASKBAR_BUTTON_PADDING: i32 = 4;

// Colors - Dark Roulette Theme
pub const COLOR_TITLE_BAR: u32 = rgb(0x1E, 0x1E, 0x1E);
pub const COLOR_TITLE_BAR_FOCUSED: u32 = rgb(0x2D, 0x2D, 0x30);
pub const COLOR_BUTTON: u32 = rgb(0x3E, 0x3E, 0x42);
pub const COLOR_BUTTON_HOVER: u32 = rgb(0x50, 0x50, 0x52);
pub const COLOR_BUTTON_CLOSE_HOVER: u32 = rgb(0xE8, 0x11, 0x23);
pub const COLOR_TEXT: u32 = rgb(0xE0, 0xE0, 0xE0);
pub const COLOR_TASKBAR: u32 = rgb(0x25, 0x25, 0x26);
pub const COLOR_CURSOR: u32 = rgb(0xFF, 0xFF, 0xFF);
pub const COLOR_BACKGROUND: u32 = rgb(0x00, 0x11, 0x22);

// File Manager Specific
pub const FM_WIDTH: i32 = 400;
pub const FM_HEIGHT: i32 = 300;
pub const FM_TITLE_HEIGHT: i32 = TITLE_BAR_HEIGHT;
pub const FM_ITEM_HEIGHT: i32 = 20;
pub const FM_COLOR_BG: u32 = rgb(0x25, 0x25, 0x26);
pub const FM_COLOR_FG: u32 = rgb(0xE0, 0xE0, 0xE0);
pub const FM_COLOR_HL: u32 = rgb(0x3E, 0x3E, 0x42);
pub const FM_BUTTON_WIDTH: i32 = 40;
