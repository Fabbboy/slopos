//! Standalone File Manager Application
//!
//! This is a standalone userland application that runs in its own window,
//! following the same architecture as the shell.

use core::ffi::c_void;
use core::str;

use crate::gfx::{self, rgb, DrawBuffer};
use crate::syscall::{
    sys_fb_info, sys_fs_list, sys_input_poll_batch, sys_surface_commit, sys_surface_set_title,
    sys_yield, InputEvent, InputEventType, ShmBuffer, UserFbInfo, UserFsEntry, UserFsList,
};
use crate::theme::*;

// Content area constants (excluding compositor-drawn title bar)
const FM_CONTENT_WIDTH: i32 = FM_WIDTH;
const FM_CONTENT_HEIGHT: i32 = FM_HEIGHT - FM_TITLE_HEIGHT;
const NAV_ROW_HEIGHT: i32 = 24;

/// Standalone File Manager with its own surface buffer
pub struct FileManager {
    // Surface management
    shm_buffer: Option<ShmBuffer>,
    width: i32,
    height: i32,
    pitch: usize,
    bytes_pp: u8,
    needs_redraw: bool,

    // Input tracking (pointer position from motion events)
    pointer_x: i32,
    pointer_y: i32,

    // File system state
    current_path: [u8; 128],
    entries: [UserFsEntry; 32],
    entry_count: u32,
    scroll_top: i32,
}

impl FileManager {
    /// Creates a new FileManager for standalone operation
    pub fn new() -> Self {
        let mut fm = Self {
            shm_buffer: None,
            width: FM_CONTENT_WIDTH,
            height: FM_CONTENT_HEIGHT,
            pitch: 0,
            bytes_pp: 4,
            needs_redraw: true,
            pointer_x: 0,
            pointer_y: 0,
            current_path: [0; 128],
            entries: [UserFsEntry::new(); 32],
            entry_count: 0,
            scroll_top: 0,
        };
        fm.current_path[0] = b'/';
        fm.refresh();
        fm
    }

    /// Initialize the surface buffer for rendering
    fn init_surface(&mut self) -> bool {
        // Get framebuffer info for bpp
        let mut fb_info = UserFbInfo::default();
        if sys_fb_info(&mut fb_info) != 0 {
            return false;
        }

        self.bytes_pp = ((fb_info.bpp as usize + 7) / 8) as u8;
        self.pitch = (self.width as usize) * (self.bytes_pp as usize);

        let buffer_size = self.pitch * (self.height as usize);
        let shm = match ShmBuffer::create(buffer_size) {
            Ok(buf) => buf,
            Err(_) => return false,
        };

        if shm
            .attach_surface(self.width as u32, self.height as u32)
            .is_err()
        {
            return false;
        }

        self.shm_buffer = Some(shm);
        true
    }

    /// Get a DrawBuffer for rendering
    fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        let shm = self.shm_buffer.as_mut()?;
        DrawBuffer::new(
            shm.as_mut_slice(),
            self.width as u32,
            self.height as u32,
            self.pitch,
            self.bytes_pp,
        )
    }

    /// Reload directory listing
    pub fn refresh(&mut self) {
        self.entries = [UserFsEntry::new(); 32];

        let mut list = UserFsList {
            entries: self.entries.as_mut_ptr(),
            max_entries: 32,
            count: 0,
        };

        unsafe {
            sys_fs_list(self.current_path.as_ptr() as *const i8, &mut list);
        }

        self.entry_count = list.count;
        self.needs_redraw = true;
    }

    /// Navigate to parent or child directory
    fn navigate(&mut self, name: &[u8]) {
        if name == b".." {
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 {
                len += 1;
            }

            if len > 1 {
                let mut i = len - 1;
                while i > 0 && self.current_path[i] != b'/' {
                    self.current_path[i] = 0;
                    i -= 1;
                }
                if i > 0 {
                    self.current_path[i] = 0;
                }
            }
        } else {
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 {
                len += 1;
            }

            if len + 1 + name.len() + 1 <= 128 {
                if len > 1 || (len == 1 && self.current_path[0] != b'/') {
                    self.current_path[len] = b'/';
                    len += 1;
                } else if len == 0 {
                    self.current_path[0] = b'/';
                    len = 1;
                }

                for (i, &b) in name.iter().enumerate() {
                    self.current_path[len + i] = b;
                }
                self.current_path[len + name.len()] = 0;
            }
        }
        self.refresh();
        self.scroll_top = 0;
    }

    /// Handle a click at window-local coordinates (0,0 = top-left of content area)
    fn handle_click(&mut self, x: i32, y: i32) -> bool {
        // Navigation row (Up button)
        if y >= 0 && y < NAV_ROW_HEIGHT {
            // Up button on the left side
            if x >= 4 && x < 4 + BUTTON_SIZE {
                self.navigate(b"..");
                return true;
            }
            return false;
        }

        // File list area (below navigation row)
        let list_y = y - NAV_ROW_HEIGHT;
        if list_y >= 0 && x >= 0 && x < self.width {
            let idx = list_y / FM_ITEM_HEIGHT;
            let entry_idx = self.scroll_top + idx;

            if entry_idx >= 0 && entry_idx < self.entry_count as i32 {
                let entry = self.entries[entry_idx as usize];
                if entry.is_directory() {
                    // Directory - navigate into it
                    let mut name_len = 0;
                    while name_len < 64 && entry.name[name_len] != 0 {
                        name_len += 1;
                    }
                    let name = &entry.name[..name_len];
                    self.navigate(name);
                }
                return true;
            }
        }

        false
    }

    /// Poll for input events and handle them
    fn poll_input(&mut self) {
        let mut events = [InputEvent::default(); 16];
        let count = sys_input_poll_batch(&mut events) as usize;

        for i in 0..count {
            let ev = &events[i];
            match ev.event_type {
                InputEventType::PointerMotion | InputEventType::PointerEnter => {
                    // Track pointer position from motion/enter events
                    // Coordinates are window-local (translated by kernel)
                    self.pointer_x = ev.pointer_x();
                    self.pointer_y = ev.pointer_y();
                }
                InputEventType::PointerButtonPress => {
                    // Use tracked position for click handling
                    // (button events don't include coordinates)
                    if self.handle_click(self.pointer_x, self.pointer_y) {
                        self.needs_redraw = true;
                    }
                }
                _ => {}
            }
        }
    }

    /// Draw the file manager content (no title bar - compositor handles that)
    fn draw(&mut self) {
        // Copy values before borrowing self for draw_buffer
        let width = self.width;
        let height = self.height;
        let current_path = self.current_path;
        let entry_count = self.entry_count;
        let scroll_top = self.scroll_top;
        let entries = self.entries;

        let Some(mut buf) = self.draw_buffer() else {
            return;
        };

        // Clear background
        gfx::fill_rect(&mut buf, 0, 0, width, height, FM_COLOR_BG);

        // Navigation row background
        gfx::fill_rect(&mut buf, 0, 0, width, NAV_ROW_HEIGHT, COLOR_TITLE_BAR);

        // Up button
        gfx::fill_rect(&mut buf, 4, 4, BUTTON_SIZE, BUTTON_SIZE - 8, COLOR_BUTTON);
        gfx::font::draw_string(&mut buf, 8, 4, "^", COLOR_TEXT, COLOR_BUTTON);

        // Current path display
        let mut len = 0;
        while len < current_path.len() && current_path[len] != 0 {
            len += 1;
        }
        let path_str = str::from_utf8(&current_path[..len]).unwrap_or("/");
        gfx::font::draw_string(
            &mut buf,
            4 + BUTTON_SIZE + 8,
            4,
            path_str,
            COLOR_TEXT,
            COLOR_TITLE_BAR,
        );

        // File list (below navigation row)
        let list_start_y = NAV_ROW_HEIGHT;
        let available_height = height - NAV_ROW_HEIGHT;
        let max_visible = available_height / FM_ITEM_HEIGHT;

        for i in 0..entry_count as usize {
            if i < scroll_top as usize {
                continue;
            }
            let row = (i as i32) - scroll_top;
            if row >= max_visible {
                break;
            }

            let item_y = list_start_y + (row * FM_ITEM_HEIGHT);
            let entry = &entries[i];

            // Extract name
            let mut name_len = 0;
            while name_len < 64 && entry.name[name_len] != 0 {
                name_len += 1;
            }
            let name = str::from_utf8(&entry.name[..name_len]).unwrap_or("?");

            // Color: blue for directories, white for files
            let color = if entry.is_directory() {
                rgb(0x40, 0x80, 0xFF)
            } else {
                COLOR_TEXT
            };

            gfx::font::draw_string(&mut buf, 8, item_y + 2, name, color, FM_COLOR_BG);
        }
    }
}

/// Main entry point for standalone file manager binary
#[unsafe(link_section = ".user_text")]
pub fn file_manager_main(_arg: *mut c_void) {
    let mut fm = FileManager::new();

    // Initialize surface
    if !fm.init_surface() {
        // Surface init failed - just yield forever
        loop {
            sys_yield();
        }
    }

    // Set window title
    sys_surface_set_title("Files");

    // Initial draw
    fm.draw();
    let _ = sys_surface_commit();

    // Main event loop
    loop {
        fm.poll_input();

        if fm.needs_redraw {
            fm.draw();
            let _ = sys_surface_commit();
            fm.needs_redraw = false;
        }

        sys_yield();
    }
}
