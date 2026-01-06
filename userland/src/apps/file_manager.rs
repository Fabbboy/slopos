use crate::gfx::{self, rgb, DrawBuffer};
use crate::syscall::{sys_fs_list, UserFsEntry, UserFsList};
use core::str;

// UI Constants - Duplicated/Shared from compositor style
// In a larger system these would be in a theme module

const BUTTON_SIZE: i32 = 20;
const BUTTON_PADDING: i32 = 2;

const COLOR_TITLE_BAR: u32 = rgb(0x1E, 0x1E, 0x1E);
const COLOR_BUTTON: u32 = rgb(0x3E, 0x3E, 0x42);
const COLOR_BUTTON_HOVER: u32 = rgb(0x50, 0x50, 0x52);
const COLOR_BUTTON_CLOSE_HOVER: u32 = rgb(0xE8, 0x11, 0x23);
const COLOR_TEXT: u32 = rgb(0xE0, 0xE0, 0xE0);

// File Manager Constants
pub const FM_WIDTH: i32 = 400;
pub const FM_HEIGHT: i32 = 300;
const FM_TITLE_HEIGHT: i32 = 24;
const FM_ITEM_HEIGHT: i32 = 20;
const FM_COLOR_BG: u32 = rgb(0x25, 0x25, 0x26);
#[allow(dead_code)]
const FM_COLOR_FG: u32 = rgb(0xE0, 0xE0, 0xE0);
#[allow(dead_code)]
const FM_COLOR_HL: u32 = rgb(0x3E, 0x3E, 0x42);
pub const FM_BUTTON_WIDTH: i32 = 40; // Width of "Files" button on taskbar

pub struct FileManager {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    current_path: [u8; 128],
    entries: [UserFsEntry; 32],
    entry_count: u32,
    scroll_top: i32,
    selected_index: i32,
    pub dragging: bool,
    pub drag_offset_x: i32,
    pub drag_offset_y: i32,
}

impl FileManager {
    /// Creates a new FileManager positioned at (100, 100) with the path initialized to "/" and its entry list populated.
    ///
    /// The window is initially hidden and the in-memory entry buffer is refreshed from the filesystem so the manager starts with a current directory listing.
    ///
    /// # Examples
    ///
    /// ```
    /// let fm = FileManager::new();
    /// assert_eq!(fm.x, 100);
    /// assert_eq!(fm.y, 100);
    /// assert_eq!(fm.visible, false);
    /// assert_eq!(fm.current_path[0], b'/');
    /// // entry_count reflects the number of entries populated by refresh()
    /// assert!(fm.entry_count as usize <= fm.entries.len());
    /// ```
    pub fn new() -> Self {
        let mut fm = Self {
            visible: false,
            x: 100,
            y: 100,
            current_path: [0; 128],
            entries: [UserFsEntry::new(); 32],
            entry_count: 0,
            scroll_top: 0,
            selected_index: -1,
            dragging: false,
            drag_offset_x: 0,
            drag_offset_y: 0,
        };
        fm.current_path[0] = b'/';
        fm.refresh();
        fm
    }

    /// Reloads the directory listing for the current path and updates the in-memory entries.
    ///
    /// This clears any previously stored entries, requests the filesystem to populate the
    /// internal entries buffer for `current_path`, and updates `entry_count` to reflect
    /// the number of entries returned by the filesystem.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut fm = FileManager::new();
    /// // Should complete without panic and refresh internal entries for the current path.
    /// fm.refresh();
    /// ```
    pub fn refresh(&mut self) {
        // Clear entries first to avoid stale data issues
        self.entries = [UserFsEntry::new(); 32];
        
        let mut list = UserFsList {
            // SAFE: entries points to the valid array within self
            entries: unsafe { self.entries.as_mut_ptr() },
            max_entries: 32,
            count: 0,
        };
        
        // SAFE: current_path is a valid buffer and list.entries is initialized as valid raw pointer
        unsafe {
            sys_fs_list(self.current_path.as_ptr() as *const i8, &mut list);
        }
        
        self.entry_count = list.count;
    }

    /// Change the FileManager's current path by entering a child directory or moving to the parent.
    ///
    /// If `name` is `b".."` the path is shortened to its parent component (stopping at the root).
    /// Otherwise `name` is appended as a new path component if the resulting path fits within the
    /// internal 128-byte buffer; if it does not fit, the path is left unchanged.
    /// After changing the path this method refreshes the directory listing and resets scrolling and selection.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut fm = FileManager::new();
    /// fm.navigate(b"subdir");
    /// // path contains "/subdir"
    /// assert!(fm.current_path.windows(7).any(|w| w == b"/subdir"));
    /// fm.navigate(b"..");
    /// // returned to root "/"
    /// assert_eq!(fm.current_path[1], 0);
    /// ```
    fn navigate(&mut self, name: &[u8]) {
        if name == b".." {
             // Handle parent directory
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 { len += 1; }
            
            if len > 1 {
                // Find last separator
                let mut i = len - 1;
                while i > 0 && self.current_path[i] != b'/' {
                    self.current_path[i] = 0;
                    i -= 1;
                }
                if i > 0 { self.current_path[i] = 0; } // Remove trailing slash if not root
            }
        } else {
             // Append directory
            let mut len = 0;
            while len < 128 && self.current_path[len] != 0 { len += 1; }
            
            if len + 1 + name.len() < 128 {
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
            }
        }
        self.refresh();
        self.scroll_top = 0;
        self.selected_index = -1;
    }
    
    /// Determines whether a coordinate lies within the visible file manager window.
    ///
    /// # Returns
    ///
    /// `true` if the manager is visible and the point (mx, my) is inside the window bounds, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let fm = FileManager { visible: true, x: 10, y: 20, ..FileManager::new() };
    /// assert!(fm.hit_test(10, 20));
    /// assert!(!fm.hit_test(0, 0));
    /// ```
    #[allow(dead_code)]
    pub fn hit_test(&self, mx: i32, my: i32) -> bool {
        self.visible && mx >= self.x && mx < self.x + FM_WIDTH && my >= self.y && my < self.y + FM_HEIGHT
    }
    
    /// Handle a mouse click inside the file manager window.
    ///
    /// Processes clicks on the title bar (close button, up navigation, or begin dragging)
    /// and on list items (enter directory on click). If the window is not visible, the
    /// click is ignored.
    ///
    /// # Returns
    ///
    /// `true` if the click was handled (visibility changed, navigation performed, dragging started,
    /// or a list item was activated), `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut fm = FileManager::new();
    /// fm.visible = false;
    /// // When not visible, clicks are ignored.
    /// assert!(!fm.handle_click(0, 0));
    /// ```
    pub fn handle_click(&mut self, mx: i32, my: i32) -> bool {
        if !self.visible { return false; }
        
        // Check title bar buttons
        if my >= self.y && my < self.y + FM_TITLE_HEIGHT {
             // Close button
             if mx >= self.x + FM_WIDTH - BUTTON_SIZE - BUTTON_PADDING && mx < self.x + FM_WIDTH - BUTTON_PADDING {
                 self.visible = false;
                 return true;
             }
             
             // Up button (next to close, or left side?)
             let up_x = self.x + FM_WIDTH - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
             if mx >= up_x && mx < up_x + BUTTON_SIZE {
                 self.navigate(b"..");
                 return true;
             }
             
             // Start Dragging if not on a button
             if mx >= self.x && mx < self.x + FM_WIDTH {
                 self.dragging = true;
                 self.drag_offset_x = mx - self.x;
                 self.drag_offset_y = my - self.y;
                 return true;
             }
        }
        
        // List items
        let list_y = self.y + FM_TITLE_HEIGHT;
        if my >= list_y {
            let idx = (my - list_y) / FM_ITEM_HEIGHT;
            let entry_idx = self.scroll_top + idx;
            
            if entry_idx >= 0 && entry_idx < self.entry_count as i32 {
                // Determine if it was a double click or just selection (simplified: click navigates dirs)
                let entry = self.entries[entry_idx as usize];
                 if entry.r#type == 1 { // Directory
                    // Extract name
                    let mut name_len = 0;
                    while name_len < 64 && entry.name[name_len] != 0 { name_len += 1; }
                    let name = &entry.name[..name_len];
                    self.navigate(name);
                 }
                return true;
            }
        }
        
        false
    }

    /// Draws a square button with a centered label, using hover and close styles when applicable.
    ///
    /// The button is filled and the label is drawn with the window text color on the button background.
    /// The `hover` and `is_close` flags select the button fill color.
    ///
    /// # Examples
    ///
    /// ```
    /// // Create a buffer and draw a hovered close button at (8,8) with size 16.
    /// let mut buf = DrawBuffer::new();
    /// let fm = FileManager::new();
    /// fm.draw_button(&mut buf, 8, 8, 16, "X", true, true);
    /// ```
    fn draw_button(
        &self,
        buf: &mut DrawBuffer,
        x: i32,
        y: i32,
        size: i32,
        label: &str,
        hover: bool,
        is_close: bool,
    ) {
        let color = if hover && is_close {
            COLOR_BUTTON_CLOSE_HOVER
        } else if hover {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        gfx::fill_rect(buf, x, y, size, size, color);
        gfx::font::draw_string(buf, x + size / 4, y + size / 4, label, COLOR_TEXT, color);
    }
    
    /// Renders the file manager window, title bar, buttons, and visible directory entries into `buf`.
    ///
    /// The method is a no-op when the file manager is not visible. It draws the window background,
    /// title bar with the current path, close and up buttons, and each visible entry (directories
    /// are colored differently from files).
    ///
    /// # Examples
    ///
    /// ```
    /// let mut buf = DrawBuffer::new();
    /// let fm = FileManager::new();
    /// fm.draw(&mut buf);
    /// ```
    pub fn draw(&self, buf: &mut DrawBuffer) {
        if !self.visible { return; }
        
        // Window Background
        gfx::fill_rect(buf, self.x, self.y, FM_WIDTH, FM_HEIGHT, FM_COLOR_BG);
        
        // Title Bar
        gfx::fill_rect(buf, self.x, self.y, FM_WIDTH, FM_TITLE_HEIGHT, COLOR_TITLE_BAR);
        
        // Manual conversion of path to string (safe subset)
        let mut len = 0;
        while len < self.current_path.len() && self.current_path[len] != 0 { len += 1; }
        let path_str = str::from_utf8(&self.current_path[..len]).unwrap_or("/");
        
        gfx::font::draw_string(buf, self.x + 8, self.y + 4, path_str, COLOR_TEXT, COLOR_TITLE_BAR);
        
        // Close Button
        self.draw_button(buf, self.x + FM_WIDTH - BUTTON_SIZE - BUTTON_PADDING, self.y + BUTTON_PADDING, BUTTON_SIZE, "X", false, true);

        // Up Button
        let up_x = self.x + FM_WIDTH - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
        self.draw_button(buf, up_x, self.y + BUTTON_PADDING, BUTTON_SIZE, "^", false, false);

        // Content Area
        let list_y = self.y + FM_TITLE_HEIGHT;
        let _content_height = FM_HEIGHT - FM_TITLE_HEIGHT;
        
        // Draw entries
        for i in 0..self.entry_count as usize {
            if i < self.scroll_top as usize { continue; }
            let row = (i as i32) - self.scroll_top;
            let item_y = list_y + (row * FM_ITEM_HEIGHT);
            
            if item_y + FM_ITEM_HEIGHT > self.y + FM_HEIGHT { break; }
            
            let entry = &self.entries[i];
             // Extract name
            let mut name_len = 0;
            while name_len < 64 && entry.name[name_len] != 0 { name_len += 1; }
            let name = str::from_utf8(&entry.name[..name_len]).unwrap_or("?");
            
            // Color based on type: 1=DIR, 0=FILE
            let color = if entry.r#type == 1 { rgb(0x40, 0x80, 0xFF) } else { COLOR_TEXT };
            
            gfx::font::draw_string(buf, self.x + 8, item_y + 2, name, color, FM_COLOR_BG);
        }
    }
}