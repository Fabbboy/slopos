//! Taskbar state tracking, start menu items, and layout geometry.

use crate::syscall::UserWindowInfo;
use crate::theme::*;

use super::MAX_WINDOWS;
use super::output::WINDOW_STATE_MINIMIZED;

// ── Start menu ──────────────────────────────────────────────────────────────

pub struct StartMenuItem {
    pub label: &'static str,
    pub window_title: Option<&'static [u8]>,
    pub program_name: &'static str,
}

pub const START_MENU_ITEMS: [StartMenuItem; 3] = [
    StartMenuItem {
        label: "Files",
        window_title: Some(b"Files"),
        program_name: "file_manager",
    },
    StartMenuItem {
        label: "Monitor",
        window_title: Some(b"System Monitor"),
        program_name: "sysmon",
    },
    StartMenuItem {
        label: "Shell",
        window_title: None,
        program_name: "shell",
    },
];

// ── Taskbar state (for conditional redraw) ──────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TaskbarState {
    pub window_count: u32,
    pub focused_task: u32,
    window_states: u32,
    pub start_menu_open: bool,
}

impl TaskbarState {
    pub const fn empty() -> Self {
        Self {
            window_count: 0,
            focused_task: 0,
            window_states: 0,
            start_menu_open: false,
        }
    }

    pub fn from_windows(
        windows: &[UserWindowInfo; MAX_WINDOWS],
        count: u32,
        focused: u32,
        start_menu_open: bool,
    ) -> Self {
        let mut states = 0u32;
        for i in 0..count.min(32) as usize {
            if windows[i].state == WINDOW_STATE_MINIMIZED {
                states |= 1 << i;
            }
        }
        Self {
            window_count: count,
            focused_task: focused,
            window_states: states,
            start_menu_open,
        }
    }
}

// ── Layout geometry helpers ─────────────────────────────────────────────────

#[inline]
pub fn start_button_x() -> i32 {
    TASKBAR_BUTTON_PADDING
}

#[inline]
pub fn app_buttons_start_x() -> i32 {
    start_button_x() + START_BUTTON_WIDTH + START_APPS_GAP
}

#[inline]
pub fn start_button_y(fb_height: i32) -> i32 {
    fb_height - TASKBAR_HEIGHT + TASKBAR_BUTTON_PADDING
}

#[inline]
pub fn start_button_height() -> i32 {
    TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2)
}

#[inline]
pub fn start_menu_height() -> i32 {
    (START_MENU_ITEMS.len() as i32 * START_MENU_ITEM_HEIGHT) + (START_MENU_PADDING * 2)
}

#[inline]
pub fn start_menu_x() -> i32 {
    start_button_x()
}

#[inline]
pub fn start_menu_y(fb_height: i32) -> i32 {
    start_button_y(fb_height) - start_menu_height() - TASKBAR_BUTTON_PADDING
}
