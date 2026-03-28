//! File Manager Application
//!
//! Nautilus-inspired file browser with sidebar, breadcrumb navigation,
//! list view with columns, keyboard navigation, and proper error handling.

use std::fs;
use std::path::PathBuf;
use std::string::String;

use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
use crate::gfx::{self, DrawBuffer};
use crate::theme::*;

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

const FM_CONTENT_WIDTH: u32 = FM_WIDTH as u32;
const FM_CONTENT_HEIGHT: u32 = (FM_HEIGHT - FM_TITLE_HEIGHT) as u32;

// Keyboard scancodes (matching shell/input.rs)
const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;

const BACKSPACE_ASCII: u8 = 0x08;
const ENTER_ASCII: u8 = 0x0D;

// ---------------------------------------------------------------------------
// Sidebar bookmarks
// ---------------------------------------------------------------------------

struct Bookmark {
    label: &'static str,
    path: &'static str,
    icon: &'static str,
}

const BOOKMARKS: &[Bookmark] = &[
    Bookmark {
        label: "Root",
        path: "/",
        icon: "/",
    },
    Bookmark {
        label: "Home",
        path: "/home",
        icon: "~",
    },
    Bookmark {
        label: "Temp",
        path: "/tmp",
        icon: "T",
    },
    Bookmark {
        label: "Devices",
        path: "/dev",
        icon: "D",
    },
    Bookmark {
        label: "Programs",
        path: "/bin",
        icon: "B",
    },
    Bookmark {
        label: "System",
        path: "/sys",
        icon: "S",
    },
];

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

struct FileEntry {
    name: String,
    is_directory: bool,
    size: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadState {
    Ok,
    Empty,
    Error,
}

// ---------------------------------------------------------------------------
// FileManager
// ---------------------------------------------------------------------------

pub struct FileManager {
    current_path: PathBuf,
    entries: std::vec::Vec<FileEntry>,
    load_state: LoadState,
    error_msg: String,

    // Navigation history
    history_back: std::vec::Vec<PathBuf>,
    history_forward: std::vec::Vec<PathBuf>,

    // View state
    scroll_offset: usize,
    selected: Option<usize>,
    hover_row: Option<usize>,

    // Sidebar hover
    sidebar_hover: Option<usize>,

    // Cached layout metrics (set in draw, used in event handling)
    last_pointer_x: i32,
    last_pointer_y: i32,
}

impl FileManager {
    fn new() -> Self {
        let mut fm = Self {
            current_path: PathBuf::from("/"),
            entries: std::vec::Vec::new(),
            load_state: LoadState::Ok,
            error_msg: String::new(),
            history_back: std::vec::Vec::new(),
            history_forward: std::vec::Vec::new(),
            scroll_offset: 0,
            selected: None,
            hover_row: None,
            sidebar_hover: None,
            last_pointer_x: 0,
            last_pointer_y: 0,
        };
        fm.refresh();
        fm
    }

    // -----------------------------------------------------------------------
    // Directory loading via std::fs
    // -----------------------------------------------------------------------

    fn refresh(&mut self) {
        self.entries.clear();
        self.error_msg.clear();

        let read_dir = match fs::read_dir(&self.current_path) {
            Ok(rd) => rd,
            Err(e) => {
                self.load_state = LoadState::Error;
                self.error_msg = format!("{}", e);
                return;
            }
        };

        for dir_entry in read_dir {
            let dir_entry = match dir_entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            let name = match dir_entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };
            if name == "." || name == ".." {
                continue;
            }

            let (is_directory, size) = match dir_entry.metadata() {
                Ok(meta) => (meta.is_dir(), meta.len() as u32),
                Err(_) => (false, 0),
            };

            self.entries.push(FileEntry {
                name,
                is_directory,
                size,
            });
        }

        // Sort: directories first, then alphabetical (case-insensitive)
        self.entries.sort_by(|a, b| {
            b.is_directory.cmp(&a.is_directory).then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
        });

        self.load_state = if self.entries.is_empty() {
            LoadState::Empty
        } else {
            LoadState::Ok
        };
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    fn navigate_to(&mut self, path: PathBuf) {
        self.history_back.push(self.current_path.clone());
        self.history_forward.clear();
        self.current_path = path;
        self.scroll_offset = 0;
        self.selected = None;
        self.hover_row = None;
        self.refresh();
    }

    fn navigate_up(&mut self) {
        let mut parent = self.current_path.clone();
        if parent.pop() && !parent.as_os_str().is_empty() {
            self.navigate_to(parent);
        } else if self.current_path.as_os_str() != "/" {
            self.navigate_to(PathBuf::from("/"));
        }
    }

    fn navigate_back(&mut self) {
        if let Some(prev) = self.history_back.pop() {
            self.history_forward.push(self.current_path.clone());
            self.current_path = prev;
            self.scroll_offset = 0;
            self.selected = None;
            self.hover_row = None;
            self.refresh();
        }
    }

    fn navigate_forward(&mut self) {
        if let Some(next) = self.history_forward.pop() {
            self.history_back.push(self.current_path.clone());
            self.current_path = next;
            self.scroll_offset = 0;
            self.selected = None;
            self.hover_row = None;
            self.refresh();
        }
    }

    fn open_selected(&mut self) {
        if let Some(idx) = self.selected {
            if idx < self.entries.len() && self.entries[idx].is_directory {
                let name = self.entries[idx].name.clone();
                let mut next = self.current_path.clone();
                next.push(&name);
                self.navigate_to(next);
            }
        }
    }

    // -----------------------------------------------------------------------
    // View helpers
    // -----------------------------------------------------------------------

    fn visible_rows(&self, height: i32) -> usize {
        let list_area_h = height - FM_NAV_HEIGHT - FM_LIST_HEADER_HEIGHT - FM_STATUS_HEIGHT;
        if list_area_h <= 0 {
            return 0;
        }
        (list_area_h / FM_ITEM_HEIGHT) as usize
    }

    fn clamp_scroll(&mut self, height: i32) {
        let max_visible = self.visible_rows(height);
        if self.entries.len() <= max_visible {
            self.scroll_offset = 0;
        } else if self.scroll_offset > self.entries.len() - max_visible {
            self.scroll_offset = self.entries.len() - max_visible;
        }
    }

    fn ensure_selected_visible(&mut self, height: i32) {
        if let Some(sel) = self.selected {
            let max_visible = self.visible_rows(height);
            if max_visible == 0 {
                return;
            }
            if sel < self.scroll_offset {
                self.scroll_offset = sel;
            } else if sel >= self.scroll_offset + max_visible {
                self.scroll_offset = sel - max_visible + 1;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Hit testing
    // -----------------------------------------------------------------------

    fn handle_pointer_motion(&mut self, x: i32, y: i32, _width: i32, height: i32) -> bool {
        self.last_pointer_x = x;
        self.last_pointer_y = y;
        let mut changed = false;

        // Sidebar hover
        let old_sb = self.sidebar_hover;
        self.sidebar_hover = None;
        if x < FM_SIDEBAR_WIDTH && y >= FM_NAV_HEIGHT {
            let heading_offset = gfx::font::cell_height() + 6;
            let rel_y = y - FM_NAV_HEIGHT - heading_offset;
            if rel_y >= 0 {
                let idx = (rel_y / FM_SIDEBAR_ITEM_HEIGHT) as usize;
                if idx < BOOKMARKS.len() {
                    self.sidebar_hover = Some(idx);
                }
            }
        }
        if self.sidebar_hover != old_sb {
            changed = true;
        }

        // List hover
        let old_hover = self.hover_row;
        self.hover_row = None;
        if x >= FM_SIDEBAR_WIDTH {
            let list_top = FM_NAV_HEIGHT + FM_LIST_HEADER_HEIGHT;
            let list_bottom = height - FM_STATUS_HEIGHT;
            if y >= list_top && y < list_bottom {
                let rel_y = y - list_top;
                let row = (rel_y / FM_ITEM_HEIGHT) as usize + self.scroll_offset;
                if row < self.entries.len() {
                    self.hover_row = Some(row);
                }
            }
        }
        if self.hover_row != old_hover {
            changed = true;
        }

        changed
    }

    fn handle_click(&mut self, x: i32, y: i32, _width: i32, height: i32) -> bool {
        let cell_w = gfx::font::cell_width();
        let btn_w = cell_w * 3 + 4;

        // Navigation bar clicks
        if y < FM_NAV_HEIGHT {
            let nav_x = FM_SIDEBAR_WIDTH + 4;
            // Back button
            if x >= nav_x && x < nav_x + btn_w {
                self.navigate_back();
                return true;
            }
            // Forward button
            if x >= nav_x + btn_w + 2 && x < nav_x + btn_w * 2 + 2 {
                self.navigate_forward();
                return true;
            }
            // Up button
            if x >= nav_x + (btn_w + 2) * 2 && x < nav_x + (btn_w + 2) * 2 + btn_w {
                self.navigate_up();
                return true;
            }
            return false;
        }

        // Sidebar clicks
        if x < FM_SIDEBAR_WIDTH && y >= FM_NAV_HEIGHT {
            let heading_offset = gfx::font::cell_height() + 6;
            let rel_y = y - FM_NAV_HEIGHT - heading_offset;
            if rel_y >= 0 {
                let idx = (rel_y / FM_SIDEBAR_ITEM_HEIGHT) as usize;
                if idx < BOOKMARKS.len() {
                    self.navigate_to(PathBuf::from(BOOKMARKS[idx].path));
                    return true;
                }
            }
            return false;
        }

        // List area clicks
        let list_top = FM_NAV_HEIGHT + FM_LIST_HEADER_HEIGHT;
        let list_bottom = height - FM_STATUS_HEIGHT;
        if y >= list_top && y < list_bottom && x >= FM_SIDEBAR_WIDTH {
            let rel_y = y - list_top;
            let row = (rel_y / FM_ITEM_HEIGHT) as usize + self.scroll_offset;
            if row < self.entries.len() {
                if self.selected == Some(row) {
                    // Double-click equivalent: clicking already-selected item opens it
                    if self.entries[row].is_directory {
                        let name = self.entries[row].name.clone();
                        let mut next = self.current_path.clone();
                        next.push(&name);
                        self.navigate_to(next);
                        return true;
                    }
                } else {
                    self.selected = Some(row);
                    return true;
                }
            } else {
                self.selected = None;
                return true;
            }
        }

        false
    }

    fn handle_key(&mut self, scancode: u8, ascii: u8, height: i32) -> bool {
        match scancode {
            KEY_UP => {
                match self.selected {
                    Some(0) | None => self.selected = Some(0),
                    Some(s) => self.selected = Some(s - 1),
                }
                self.ensure_selected_visible(height);
                true
            }
            KEY_DOWN => {
                let max = self.entries.len().saturating_sub(1);
                match self.selected {
                    None => self.selected = Some(0),
                    Some(s) if s < max => self.selected = Some(s + 1),
                    _ => {}
                }
                self.ensure_selected_visible(height);
                true
            }
            KEY_PAGE_UP => {
                let page = self.visible_rows(height).max(1);
                match self.selected {
                    None => self.selected = Some(0),
                    Some(s) => self.selected = Some(s.saturating_sub(page)),
                }
                self.ensure_selected_visible(height);
                true
            }
            KEY_PAGE_DOWN => {
                let page = self.visible_rows(height).max(1);
                let max = self.entries.len().saturating_sub(1);
                match self.selected {
                    None => self.selected = Some(max.min(page)),
                    Some(s) => self.selected = Some((s + page).min(max)),
                }
                self.ensure_selected_visible(height);
                true
            }
            KEY_HOME => {
                self.selected = Some(0);
                self.ensure_selected_visible(height);
                true
            }
            KEY_END => {
                if !self.entries.is_empty() {
                    self.selected = Some(self.entries.len() - 1);
                    self.ensure_selected_visible(height);
                }
                true
            }
            _ => {
                // ASCII-based keys
                match ascii {
                    ENTER_ASCII => {
                        self.open_selected();
                        true
                    }
                    BACKSPACE_ASCII => {
                        self.navigate_up();
                        true
                    }
                    _ => false,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Size formatting
// ---------------------------------------------------------------------------

fn format_size(size: u32) -> String {
    if size < 1024 {
        format!("{} B", size)
    } else if size < 1024 * 1024 {
        format!("{} K", size / 1024)
    } else {
        format!("{:.1} M", size as f64 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// WindowedApp implementation
// ---------------------------------------------------------------------------

impl WindowedApp for FileManager {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Files");
        win.set_app_id("org.slopos.files");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => return ControlFlow::Exit,

            Event::PointerMotion { x, y } => {
                let w = win.width() as i32;
                let h = win.height() as i32;
                if self.handle_pointer_motion(x, y, w, h) {
                    win.request_redraw();
                }
            }

            Event::PointerPress { .. } => {
                let (px, py) = win.pointer();
                let w = win.width() as i32;
                let h = win.height() as i32;
                if self.handle_click(px, py, w, h) {
                    win.request_redraw();
                }
            }

            Event::KeyPress { scancode, ascii } => {
                let h = win.height() as i32;
                if self.handle_key(scancode, ascii, h) {
                    win.request_redraw();
                }
            }

            _ => {}
        }
        ControlFlow::Continue
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let width = fb.width() as i32;
        let height = fb.height() as i32;

        self.clamp_scroll(height);

        // ===================================================================
        // Sidebar
        // ===================================================================
        gfx::fill_rect(fb, 0, 0, FM_SIDEBAR_WIDTH, height, FM_SIDEBAR_BG);

        let cell_h = gfx::font::cell_height();
        let cell_w = gfx::font::cell_width();

        // "Places" heading
        let heading_y = FM_NAV_HEIGHT + 4;
        gfx::font::draw_string(
            fb,
            8,
            heading_y,
            "Places",
            FM_SIDEBAR_HEADING,
            FM_SIDEBAR_BG,
        );

        let items_start_y = FM_NAV_HEIGHT + cell_h + 6;
        for (i, bm) in BOOKMARKS.iter().enumerate() {
            let item_y = items_start_y + (i as i32) * FM_SIDEBAR_ITEM_HEIGHT;

            // Highlight: active path or hover
            let is_active = self.current_path.to_str() == Some(bm.path);
            let bg = if is_active {
                FM_SIDEBAR_ACTIVE
            } else if self.sidebar_hover == Some(i) {
                FM_SIDEBAR_HOVER
            } else {
                FM_SIDEBAR_BG
            };
            gfx::fill_rect(fb, 0, item_y, FM_SIDEBAR_WIDTH, FM_SIDEBAR_ITEM_HEIGHT, bg);

            // Icon placeholder
            let text_color = if is_active {
                COLOR_TEXT
            } else {
                FM_SIDEBAR_TEXT
            };
            gfx::font::draw_string(fb, 10, item_y + 3, bm.icon, FM_DIR_COLOR, bg);
            gfx::font::draw_string(fb, 10 + cell_w + 6, item_y + 3, bm.label, text_color, bg);
        }

        // Separator line between sidebar and content
        gfx::fill_rect(
            fb,
            FM_SIDEBAR_WIDTH - 1,
            0,
            1,
            height,
            FM_LIST_HEADER_BORDER,
        );

        // ===================================================================
        // Navigation bar
        // ===================================================================
        let nav_left = FM_SIDEBAR_WIDTH;
        let nav_w = width - FM_SIDEBAR_WIDTH;
        gfx::fill_rect(fb, nav_left, 0, nav_w, FM_NAV_HEIGHT, FM_NAV_BG);

        // Bottom border
        gfx::fill_rect(
            fb,
            nav_left,
            FM_NAV_HEIGHT - 1,
            nav_w,
            1,
            FM_LIST_HEADER_BORDER,
        );

        let btn_w = cell_w * 3 + 4;
        let btn_h = FM_NAV_HEIGHT - 8;
        let btn_y = 4;
        let mut bx = nav_left + 4;

        // Back button
        let back_color = if self.history_back.is_empty() {
            FM_NAV_BUTTON_DISABLED
        } else {
            FM_NAV_BUTTON
        };
        gfx::fill_rect(fb, bx, btn_y, btn_w, btn_h, back_color);
        let back_text_color = if self.history_back.is_empty() {
            FM_SIDEBAR_HEADING
        } else {
            COLOR_TEXT
        };
        gfx::font::draw_string(fb, bx + 4, btn_y + 2, " < ", back_text_color, back_color);

        // Forward button
        bx += btn_w + 2;
        let fwd_color = if self.history_forward.is_empty() {
            FM_NAV_BUTTON_DISABLED
        } else {
            FM_NAV_BUTTON
        };
        gfx::fill_rect(fb, bx, btn_y, btn_w, btn_h, fwd_color);
        let fwd_text_color = if self.history_forward.is_empty() {
            FM_SIDEBAR_HEADING
        } else {
            COLOR_TEXT
        };
        gfx::font::draw_string(fb, bx + 4, btn_y + 2, " > ", fwd_text_color, fwd_color);

        // Up button
        bx += btn_w + 2;
        gfx::fill_rect(fb, bx, btn_y, btn_w, btn_h, FM_NAV_BUTTON);
        gfx::font::draw_string(fb, bx + 4, btn_y + 2, " ^ ", COLOR_TEXT, FM_NAV_BUTTON);

        // Path breadcrumb
        let path_x = bx + btn_w + 8;
        let path_str = self.current_path.to_str().unwrap_or("/");
        let max_path_chars = ((nav_w - (path_x - nav_left) - 8) / cell_w) as usize;
        let display_path = if path_str.len() > max_path_chars && max_path_chars > 3 {
            let start = path_str.len() - (max_path_chars - 3);
            format!("...{}", &path_str[start..])
        } else {
            String::from(path_str)
        };
        gfx::font::draw_string(fb, path_x, btn_y + 2, &display_path, COLOR_TEXT, FM_NAV_BG);

        // ===================================================================
        // Column header
        // ===================================================================
        let header_y = FM_NAV_HEIGHT;
        let content_left = FM_SIDEBAR_WIDTH;
        let content_w = width - FM_SIDEBAR_WIDTH;
        gfx::fill_rect(
            fb,
            content_left,
            header_y,
            content_w,
            FM_LIST_HEADER_HEIGHT,
            FM_LIST_HEADER_BG,
        );
        gfx::fill_rect(
            fb,
            content_left,
            header_y + FM_LIST_HEADER_HEIGHT - 1,
            content_w,
            1,
            FM_LIST_HEADER_BORDER,
        );

        // Column labels
        gfx::font::draw_string(
            fb,
            content_left + 8,
            header_y + 2,
            "Name",
            FM_SIZE_COLOR,
            FM_LIST_HEADER_BG,
        );
        let size_col_x = width - FM_SCROLLBAR_WIDTH - cell_w * 8;
        gfx::font::draw_string(
            fb,
            size_col_x,
            header_y + 2,
            "Size",
            FM_SIZE_COLOR,
            FM_LIST_HEADER_BG,
        );

        // ===================================================================
        // File list area
        // ===================================================================
        let list_top = FM_NAV_HEIGHT + FM_LIST_HEADER_HEIGHT;
        let list_bottom = height - FM_STATUS_HEIGHT;
        let list_h = list_bottom - list_top;

        // Background
        gfx::fill_rect(fb, content_left, list_top, content_w, list_h, FM_COLOR_BG);

        match self.load_state {
            LoadState::Error => {
                // Show error message centered in the list area
                let msg_y = list_top + list_h / 2 - cell_h;
                let err_icon_w = gfx::font::string_width("!");
                let err_x = content_left + (content_w - err_icon_w) / 2;
                gfx::font::draw_string(fb, err_x, msg_y, "!", FM_ERROR_COLOR, FM_COLOR_BG);

                let msg_w = gfx::font::string_width(&self.error_msg);
                let msg_x = content_left + (content_w - msg_w) / 2;
                gfx::font::draw_string(
                    fb,
                    msg_x,
                    msg_y + cell_h + 4,
                    &self.error_msg,
                    FM_ERROR_COLOR,
                    FM_COLOR_BG,
                );
            }
            LoadState::Empty => {
                let msg = "Empty directory";
                let msg_w = gfx::font::string_width(msg);
                let msg_x = content_left + (content_w - msg_w) / 2;
                let msg_y = list_top + list_h / 2 - cell_h / 2;
                gfx::font::draw_string(fb, msg_x, msg_y, msg, FM_SIZE_COLOR, FM_COLOR_BG);
            }
            LoadState::Ok => {
                let max_visible = self.visible_rows(height);
                for vi in 0..max_visible {
                    let idx = self.scroll_offset + vi;
                    if idx >= self.entries.len() {
                        break;
                    }
                    let entry = &self.entries[idx];
                    let item_y = list_top + (vi as i32) * FM_ITEM_HEIGHT;

                    // Row background: selection > hover > alternating
                    let is_selected = self.selected == Some(idx);
                    let is_hover = self.hover_row == Some(idx);
                    let row_bg = if is_selected {
                        FM_LIST_SELECTED
                    } else if is_hover {
                        FM_LIST_HOVER
                    } else if idx % 2 == 1 {
                        FM_LIST_ALT_BG
                    } else {
                        FM_COLOR_BG
                    };
                    gfx::fill_rect(
                        fb,
                        content_left,
                        item_y,
                        content_w - FM_SCROLLBAR_WIDTH,
                        FM_ITEM_HEIGHT,
                        row_bg,
                    );

                    // Type indicator + name
                    let (indicator, name_color) = if entry.is_directory {
                        ("[D]", FM_DIR_COLOR)
                    } else {
                        ("   ", FM_FILE_COLOR)
                    };
                    gfx::font::draw_string(
                        fb,
                        content_left + 4,
                        item_y + 3,
                        indicator,
                        FM_DIR_COLOR,
                        row_bg,
                    );

                    let name_x = content_left + 4 + cell_w * 4;
                    let max_name_chars = ((size_col_x - name_x - 4) / cell_w).max(1) as usize;
                    let display_name = if entry.name.len() > max_name_chars && max_name_chars > 3 {
                        format!("{}...", &entry.name[..max_name_chars - 3])
                    } else {
                        entry.name.clone()
                    };
                    gfx::font::draw_string(
                        fb,
                        name_x,
                        item_y + 3,
                        &display_name,
                        name_color,
                        row_bg,
                    );

                    // Size column (files only)
                    if !entry.is_directory {
                        let size_str = format_size(entry.size);
                        let size_w = gfx::font::string_width(&size_str);
                        let size_x = width - FM_SCROLLBAR_WIDTH - 4 - size_w;
                        gfx::font::draw_string(
                            fb,
                            size_x,
                            item_y + 3,
                            &size_str,
                            FM_SIZE_COLOR,
                            row_bg,
                        );
                    } else {
                        gfx::font::draw_string(
                            fb,
                            size_col_x,
                            item_y + 3,
                            "--",
                            FM_SIZE_COLOR,
                            row_bg,
                        );
                    }
                }
            }
        }

        // ===================================================================
        // Scrollbar
        // ===================================================================
        let sb_x = width - FM_SCROLLBAR_WIDTH;
        gfx::fill_rect(
            fb,
            sb_x,
            list_top,
            FM_SCROLLBAR_WIDTH,
            list_h,
            FM_SCROLLBAR_BG,
        );

        if !self.entries.is_empty() && self.load_state == LoadState::Ok {
            let max_visible = self.visible_rows(height);
            if self.entries.len() > max_visible && max_visible > 0 {
                let thumb_h = ((max_visible as i32) * list_h / (self.entries.len() as i32))
                    .max(12)
                    .min(list_h);
                let scrollable = self.entries.len() - max_visible;
                let track_space = list_h - thumb_h;
                let thumb_y = if scrollable > 0 {
                    list_top + (self.scroll_offset as i32) * track_space / (scrollable as i32)
                } else {
                    list_top
                };
                gfx::fill_rect(
                    fb,
                    sb_x + 1,
                    thumb_y,
                    FM_SCROLLBAR_WIDTH - 2,
                    thumb_h,
                    FM_SCROLLBAR_THUMB,
                );
            }
        }

        // ===================================================================
        // Status bar
        // ===================================================================
        let status_y = height - FM_STATUS_HEIGHT;
        gfx::fill_rect(
            fb,
            content_left,
            status_y,
            content_w,
            FM_STATUS_HEIGHT,
            FM_STATUS_BG,
        );
        gfx::fill_rect(
            fb,
            content_left,
            status_y,
            content_w,
            1,
            FM_LIST_HEADER_BORDER,
        );

        // Also fill sidebar portion of status bar
        gfx::fill_rect(
            fb,
            0,
            status_y,
            FM_SIDEBAR_WIDTH,
            FM_STATUS_HEIGHT,
            FM_STATUS_BG,
        );
        gfx::fill_rect(fb, 0, status_y, FM_SIDEBAR_WIDTH, 1, FM_LIST_HEADER_BORDER);

        let status_text = match self.load_state {
            LoadState::Error => String::from("Error"),
            LoadState::Empty => String::from("0 items"),
            LoadState::Ok => {
                let dir_count = self.entries.iter().filter(|e| e.is_directory).count();
                let file_count = self.entries.len() - dir_count;
                let base = if dir_count > 0 && file_count > 0 {
                    format!("{} folders, {} files", dir_count, file_count)
                } else if dir_count > 0 {
                    format!("{} folders", dir_count)
                } else {
                    format!("{} files", file_count)
                };
                if let Some(sel) = self.selected {
                    if sel < self.entries.len() {
                        let e = &self.entries[sel];
                        if e.is_directory {
                            format!("{} | \"{}\"", base, e.name)
                        } else {
                            format!("{} | \"{}\" ({})", base, e.name, format_size(e.size))
                        }
                    } else {
                        base
                    }
                } else {
                    base
                }
            }
        };
        gfx::font::draw_string(
            fb,
            content_left + 6,
            status_y + 3,
            &status_text,
            FM_STATUS_TEXT,
            FM_STATUS_BG,
        );
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn file_manager_main() -> ! {
    let fm = FileManager::new();
    appkit::run(fm, FM_CONTENT_WIDTH, FM_CONTENT_HEIGHT)
}
