use core::ffi::{c_char, c_void};

use crate::syscall::{
    sys_compositor_present, sys_enumerate_windows, sys_fb_fill_rect, sys_fb_font_draw,
    sys_fb_info, sys_mouse_read, sys_raise_window, sys_set_window_position,
    sys_set_window_state, sys_sleep_ms, sys_tty_set_focus, sys_yield, UserFbInfo,
    UserMouseEvent, UserRect, UserText, UserWindowInfo,
};

// UI Constants - Dark Roulette Theme
const TITLE_BAR_HEIGHT: i32 = 24;
const BUTTON_SIZE: i32 = 20;
const BUTTON_PADDING: i32 = 2;
const TASKBAR_HEIGHT: i32 = 32;
const TASKBAR_BUTTON_WIDTH: i32 = 120;
const TASKBAR_BUTTON_PADDING: i32 = 4;

// Colors matching the dark roulette aesthetic
const COLOR_TITLE_BAR: u32 = 0x1E1E1EFF;
const COLOR_TITLE_BAR_FOCUSED: u32 = 0x2D2D30FF;
const COLOR_BUTTON: u32 = 0x3E3E42FF;
const COLOR_BUTTON_HOVER: u32 = 0x505052FF;
const COLOR_BUTTON_CLOSE_HOVER: u32 = 0xE81123FF;
const COLOR_TEXT: u32 = 0xE0E0E0FF;
const COLOR_TASKBAR: u32 = 0x252526FF;
const COLOR_CURSOR: u32 = 0xFFFFFFFF;

const MAX_WINDOWS: usize = 32;

const WINDOW_STATE_NORMAL: u8 = 0;
const WINDOW_STATE_MINIMIZED: u8 = 1;

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
    focused_task: u32,
    dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,
    mouse_x: i32,
    mouse_y: i32,
    mouse_buttons: u8,
    mouse_buttons_prev: u8,
}

impl WindowManager {
    fn new() -> Self {
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            focused_task: 0,
            dragging: false,
            drag_task: 0,
            drag_offset_x: 0,
            drag_offset_y: 0,
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            mouse_buttons_prev: 0,
        }
    }

    /// Update mouse state from kernel
    fn update_mouse(&mut self) {
        let mut event = UserMouseEvent::default();
        if sys_mouse_read(&mut event) > 0 {
            self.mouse_buttons_prev = self.mouse_buttons;
            self.mouse_x = event.x;
            self.mouse_y = event.y;
            self.mouse_buttons = event.buttons;
        }
    }

    /// Check if mouse was just clicked (press event)
    fn mouse_clicked(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0 && (self.mouse_buttons_prev & 0x01) == 0
    }

    /// Check if mouse is currently pressed
    fn mouse_pressed(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0
    }

    /// Refresh window list from kernel
    fn refresh_windows(&mut self) {
        self.window_count = sys_enumerate_windows(&mut self.windows) as u32;
    }

    /// Handle all mouse events
    fn handle_mouse_events(&mut self, fb: &UserFbInfo) {
        let clicked = self.mouse_clicked();

        // Handle ongoing drag
        if self.dragging {
            if !self.mouse_pressed() {
                self.stop_drag();
            } else {
                self.update_drag();
            }
            return;
        }

        // Handle new clicks
        if clicked {
            // Check taskbar clicks
            if self.mouse_y >= (fb.height as i32 - TASKBAR_HEIGHT) {
                self.handle_taskbar_click();
                return;
            }

            // Check window title bar clicks (front to back)
            for i in (0..self.window_count as usize).rev() {
                let window = self.windows[i];
                if window.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }

                if self.hit_test_title_bar(&window) {
                    // Check close button
                    if self.hit_test_close_button(&window) {
                        self.close_window(window.task_id);
                        return;
                    }

                    // Check minimize button
                    if self.hit_test_minimize_button(&window) {
                        sys_set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                        return;
                    }

                    // Start drag
                    self.start_drag(&window);
                    sys_raise_window(window.task_id);
                    sys_tty_set_focus(window.task_id);
                    self.focused_task = window.task_id;
                    return;
                }
            }
        }
    }

    /// Test if mouse is over window's title bar
    fn hit_test_title_bar(&self, window: &UserWindowInfo) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= title_y
            && self.mouse_y < window.y
    }

    /// Test if mouse is over close button
    fn hit_test_close_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING;
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    /// Test if mouse is over minimize button
    fn hit_test_minimize_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    /// Start dragging a window
    fn start_drag(&mut self, window: &UserWindowInfo) {
        self.dragging = true;
        self.drag_task = window.task_id;
        self.drag_offset_x = self.mouse_x - window.x;
        self.drag_offset_y = self.mouse_y - window.y;
    }

    /// Stop dragging
    fn stop_drag(&mut self) {
        self.dragging = false;
        self.drag_task = 0;
    }

    /// Update drag position
    fn update_drag(&mut self) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        sys_set_window_position(self.drag_task, new_x, new_y);
    }

    /// Close a window (set to minimized for now)
    fn close_window(&mut self, task_id: u32) {
        sys_set_window_state(task_id, WINDOW_STATE_MINIMIZED);
    }

    /// Handle taskbar click
    fn handle_taskbar_click(&mut self) {
        let mut x = TASKBAR_BUTTON_PADDING;
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            let button_width = TASKBAR_BUTTON_WIDTH;

            if self.mouse_x >= x && self.mouse_x < x + button_width {
                // Restore if minimized, minimize if normal
                let new_state = if window.state == WINDOW_STATE_MINIMIZED {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MINIMIZED
                };
                sys_set_window_state(window.task_id, new_state);
                if new_state == WINDOW_STATE_NORMAL {
                    sys_raise_window(window.task_id);
                    sys_tty_set_focus(window.task_id);
                    self.focused_task = window.task_id;
                }
                return;
            }

            x += button_width + TASKBAR_BUTTON_PADDING;
        }
    }

    /// Draw all window decorations
    fn draw_decorations(&self, fb: &UserFbInfo) {
        // Draw title bars for all visible windows (back to front)
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            if window.state != WINDOW_STATE_MINIMIZED {
                self.draw_title_bar(window);
            }
        }

        // Draw taskbar
        self.draw_taskbar(fb);

        // Draw cursor
        self.draw_cursor();
    }

    /// Draw window title bar
    fn draw_title_bar(&self, window: &UserWindowInfo) {
        let focused = window.task_id == self.focused_task;
        let color = if focused {
            COLOR_TITLE_BAR_FOCUSED
        } else {
            COLOR_TITLE_BAR
        };

        // Title bar background
        let title_bar = UserRect {
            x: window.x,
            y: window.y - TITLE_BAR_HEIGHT,
            width: window.width as i32,
            height: TITLE_BAR_HEIGHT,
            color,
        };
        sys_fb_fill_rect(&title_bar);

        // Window title text
        let title_text = UserText {
            x: window.x + 8,
            y: window.y - TITLE_BAR_HEIGHT + 4,
            fg_color: COLOR_TEXT,
            bg_color: color,
            str_ptr: window.title.as_ptr(),
            len: title_strlen(&window.title),
        };
        sys_fb_font_draw(&title_text);

        // Close button (X)
        self.draw_button(
            window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING,
            window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING,
            BUTTON_SIZE,
            "X",
            self.hit_test_close_button(window),
        );

        // Minimize button (_)
        self.draw_button(
            window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2),
            window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING,
            BUTTON_SIZE,
            "_",
            self.hit_test_minimize_button(window),
        );
    }

    /// Draw a button
    fn draw_button(&self, x: i32, y: i32, size: i32, label: &str, hover: bool) {
        let color = if hover && label == "X" {
            COLOR_BUTTON_CLOSE_HOVER
        } else if hover {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        let button = UserRect {
            x,
            y,
            width: size,
            height: size,
            color,
        };
        sys_fb_fill_rect(&button);

        // Draw label
        let label_bytes = label.as_bytes();
        let text = UserText {
            x: x + size / 4,
            y: y + size / 4,
            fg_color: COLOR_TEXT,
            bg_color: color,
            str_ptr: label_bytes.as_ptr() as *const c_char,
            len: label_bytes.len() as u32,
        };
        sys_fb_font_draw(&text);
    }

    /// Draw taskbar
    fn draw_taskbar(&self, fb: &UserFbInfo) {
        // Taskbar background
        let taskbar = UserRect {
            x: 0,
            y: fb.height as i32 - TASKBAR_HEIGHT,
            width: fb.width as i32,
            height: TASKBAR_HEIGHT,
            color: COLOR_TASKBAR,
        };
        sys_fb_fill_rect(&taskbar);

        // Draw app buttons
        let mut x = TASKBAR_BUTTON_PADDING;
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            let focused = window.task_id == self.focused_task;
            let btn_color = if focused {
                COLOR_BUTTON_HOVER
            } else {
                COLOR_BUTTON
            };

            let button = UserRect {
                x,
                y: fb.height as i32 - TASKBAR_HEIGHT + TASKBAR_BUTTON_PADDING,
                width: TASKBAR_BUTTON_WIDTH,
                height: TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2),
                color: btn_color,
            };
            sys_fb_fill_rect(&button);

            // Button text
            let text = UserText {
                x: x + 4,
                y: fb.height as i32 - TASKBAR_HEIGHT + TASKBAR_BUTTON_PADDING + 4,
                fg_color: COLOR_TEXT,
                bg_color: btn_color,
                str_ptr: window.title.as_ptr(),
                len: title_strlen(&window.title).min(14), // Truncate to fit
            };
            sys_fb_font_draw(&text);

            x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }
    }

    /// Draw mouse cursor
    fn draw_cursor(&self) {
        // Simple crosshair cursor
        let h_line = UserRect {
            x: self.mouse_x - 4,
            y: self.mouse_y,
            width: 9,
            height: 1,
            color: COLOR_CURSOR,
        };
        sys_fb_fill_rect(&h_line);

        let v_line = UserRect {
            x: self.mouse_x,
            y: self.mouse_y - 4,
            width: 1,
            height: 9,
            color: COLOR_CURSOR,
        };
        sys_fb_fill_rect(&v_line);
    }
}

/// Get length of null-terminated c_char array
fn title_strlen(title: &[c_char; 32]) -> u32 {
    let mut len = 0u32;
    for &ch in title.iter() {
        if ch == 0 {
            break;
        }
        len += 1;
    }
    len
}

#[unsafe(link_section = ".user_text")]
pub fn compositor_user_main(_arg: *mut c_void) {
    let mut wm = WindowManager::new();
    let mut fb_info = UserFbInfo::default();

    // Get framebuffer info
    if sys_fb_info(&mut fb_info) < 0 {
        // Failed to get framebuffer, just yield forever
        loop {
            sys_yield();
        }
    }

    loop {
        // 1. Update mouse state
        wm.update_mouse();

        // 2. Refresh window list
        wm.refresh_windows();

        // 3. Handle mouse events
        wm.handle_mouse_events(&fb_info);

        // 4. Present surfaces (kernel composites them)
        sys_compositor_present();

        // 5. Draw decorations on top
        wm.draw_decorations(&fb_info);

        // 6. Sleep and yield
        sys_sleep_ms(16); // ~60fps
        sys_yield();
    }
}
