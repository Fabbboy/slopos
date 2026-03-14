//! Standalone File Manager Application

use std::fs;
use std::path::PathBuf;
use std::string::String;

use slopos_abi::draw::Color32;

use crate::appkit::{self, ControlFlow, Event, Window, WindowedApp};
use crate::gfx::{self, DrawBuffer};
use crate::theme::*;

const FM_CONTENT_WIDTH: u32 = FM_WIDTH as u32;
const FM_CONTENT_HEIGHT: u32 = (FM_HEIGHT - FM_TITLE_HEIGHT) as u32;
const NAV_ROW_HEIGHT: i32 = 24;

pub struct FileManager {
    current_path: PathBuf,
    entries: std::vec::Vec<FileEntry>,
    scroll_top: u32,
}

struct FileEntry {
    name: String,
    is_directory: bool,
}

impl FileManager {
    fn new() -> Self {
        let mut fm = Self {
            current_path: PathBuf::from("/"),
            entries: std::vec::Vec::new(),
            scroll_top: 0,
        };
        fm.refresh();
        fm
    }

    fn refresh(&mut self) {
        self.entries.clear();

        let read_dir = match fs::read_dir(&self.current_path) {
            Ok(read_dir) => read_dir,
            Err(_) => return,
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

            let file_type = match dir_entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };

            self.entries.push(FileEntry {
                name,
                is_directory: file_type.is_dir(),
            });
        }

        self.entries.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
    }

    fn navigate(&mut self, name: &str) {
        if name == ".." {
            if !self.current_path.pop() {
                self.current_path = PathBuf::from("/");
            }
            if self.current_path.as_os_str().is_empty() {
                self.current_path = PathBuf::from("/");
            }
        } else {
            self.current_path.push(name);
        }
        self.refresh();
        self.scroll_top = 0;
    }

    fn handle_click(&mut self, x: i32, y: i32) -> bool {
        if y >= 0 && y < NAV_ROW_HEIGHT {
            if x >= 4 && x < 4 + BUTTON_SIZE {
                self.navigate("..");
                return true;
            }
            return false;
        }

        let list_y = y - NAV_ROW_HEIGHT;
        if list_y >= 0 && x >= 0 {
            let idx = (list_y / FM_ITEM_HEIGHT) as u32;
            let entry_idx = self.scroll_top + idx;
            if (entry_idx as usize) < self.entries.len() {
                let entry = &self.entries[entry_idx as usize];
                if entry.is_directory {
                    let next = entry.name.clone();
                    self.navigate(next.as_str());
                }
                return true;
            }
        }
        false
    }
}

impl WindowedApp for FileManager {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Files");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        match event {
            Event::CloseRequest => return ControlFlow::Exit,
            Event::PointerPress { .. } => {
                let (px, py) = win.pointer();
                if self.handle_click(px, py) {
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

        gfx::fill_rect(fb, 0, 0, width, height, FM_COLOR_BG);

        gfx::fill_rect(fb, 0, 0, width, NAV_ROW_HEIGHT, COLOR_TITLE_BAR);
        gfx::fill_rect(fb, 4, 4, BUTTON_SIZE, BUTTON_SIZE - 8, COLOR_BUTTON);
        gfx::font::draw_string(fb, 8, 4, "^", COLOR_TEXT, COLOR_BUTTON);

        let path_str = self.current_path.to_str().unwrap_or("/");
        gfx::font::draw_string(
            fb,
            4 + BUTTON_SIZE + 8,
            4,
            path_str,
            COLOR_TEXT,
            COLOR_TITLE_BAR,
        );

        let list_start_y = NAV_ROW_HEIGHT;
        let available_height = height - NAV_ROW_HEIGHT;
        let max_visible = available_height / FM_ITEM_HEIGHT;

        for i in 0..self.entries.len() {
            if i < self.scroll_top as usize {
                continue;
            }
            let row = (i as i32) - self.scroll_top as i32;
            if row >= max_visible {
                break;
            }
            let item_y = list_start_y + (row * FM_ITEM_HEIGHT);
            let entry = &self.entries[i];

            let color = if entry.is_directory {
                Color32::rgb(0x40, 0x80, 0xFF)
            } else {
                COLOR_TEXT
            };
            gfx::font::draw_string(fb, 8, item_y + 2, entry.name.as_str(), color, FM_COLOR_BG);
        }
    }
}

pub fn file_manager_main() -> ! {
    let fm = FileManager::new();
    appkit::run(fm, FM_CONTENT_WIDTH, FM_CONTENT_HEIGHT)
}
