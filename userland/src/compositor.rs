use core::ffi::{c_char, c_void};

use crate::syscall::{
    sys_compositor_present_damage, sys_enumerate_windows, sys_fb_fill_rect, sys_fb_font_draw,
    sys_fb_info, sys_mouse_read, sys_raise_window, sys_set_window_position,
    sys_set_window_state, sys_tty_set_focus, sys_yield, UserDamageRegion, UserFbInfo,
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
const COLOR_BACKGROUND: u32 = 0x001122FF; // Background color for cursor clearing

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
    prev_mouse_x: i32,
    prev_mouse_y: i32,
    first_frame: bool,
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
            prev_mouse_x: 0,
            prev_mouse_y: 0,
            first_frame: true,
        }
    }

    /// Update mouse state from kernel
    fn update_mouse(&mut self) {
        let mut event = UserMouseEvent::default();
        if sys_mouse_read(&mut event) > 0 {
            self.mouse_buttons_prev = self.mouse_buttons;
            // Save previous position for damage tracking
            self.prev_mouse_x = self.mouse_x;
            self.prev_mouse_y = self.mouse_y;
            // Update to new position
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
        // Note: Framebuffer is already cleared, no need to clear old cursor!
        // We follow the Wayland pattern: clear → composite → decorate

        // Draw title bars for all visible windows (back to front)
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            if window.state != WINDOW_STATE_MINIMIZED {
                self.draw_title_bar(window);
            }
        }

        // Draw taskbar
        self.draw_taskbar(fb);

        // Draw cursor on top of everything
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

    /// Calculate ALL damage regions: first frame, windows, and cursor
    /// This ensures we never miss dirty windows or leave artifacts on screen
    fn get_all_damage_regions(&self, fb: &UserFbInfo) -> ([UserDamageRegion; 64], usize) {
        let mut regions = [UserDamageRegion::default(); 64];
        let mut count = 0;

        // FIRST FRAME: Clear entire screen to remove boot artifacts (roulette, etc.)
        if self.first_frame {
            regions[count] = UserDamageRegion {
                x: 0,
                y: 0,
                width: fb.width as i32,
                height: fb.height as i32,
            };
            count += 1;
            return (regions, count);
        }

        // WINDOW DAMAGE: Only add windows that are actually dirty
        // Use actual dirty rectangle bounds (surface-relative), not entire window
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            if window.state != WINDOW_STATE_MINIMIZED && window.dirty != 0 {
                // Convert surface-relative dirty bounds to screen coordinates
                let dirty_w = window.dirty_x1 - window.dirty_x0 + 1;
                let dirty_h = window.dirty_y1 - window.dirty_y0 + 1;

                if dirty_w > 0 && dirty_h > 0 {
                    regions[count] = UserDamageRegion {
                        x: window.x + window.dirty_x0,
                        y: window.y + window.dirty_y0,
                        width: dirty_w,
                        height: dirty_h,
                    };
                    count += 1;
                    if count >= 62 { // Leave room for cursor and taskbar damage
                        break;
                    }
                }
            }
        }

        // TASKBAR DAMAGE: Always redraw taskbar (reflects window state changes)
        if count < 63 {
            regions[count] = UserDamageRegion {
                x: 0,
                y: fb.height as i32 - TASKBAR_HEIGHT,
                width: fb.width as i32,
                height: TASKBAR_HEIGHT,
            };
            count += 1;
        }

        // CURSOR DAMAGE: Previous and current cursor positions
        const CURSOR_SIZE: i32 = 9;
        const CURSOR_HALF: i32 = CURSOR_SIZE / 2;

        // Add previous cursor position as damage (if it moved)
        if self.prev_mouse_x != self.mouse_x || self.prev_mouse_y != self.mouse_y {
            if count < 64 {
                regions[count] = UserDamageRegion {
                    x: (self.prev_mouse_x - CURSOR_HALF).max(0),
                    y: (self.prev_mouse_y - CURSOR_HALF).max(0),
                    width: CURSOR_SIZE,
                    height: CURSOR_SIZE,
                };
                count += 1;
            }
        }

        // Add current cursor position as damage
        if count < 64 {
            regions[count] = UserDamageRegion {
                x: (self.mouse_x - CURSOR_HALF).max(0),
                y: (self.mouse_y - CURSOR_HALF).max(0),
                width: CURSOR_SIZE,
                height: CURSOR_SIZE,
            };
            count += 1;
        }

        (regions, count)
    }

    /// Draw mouse cursor
    fn draw_cursor(&self) {
        // Simple crosshair cursor with bounds checking
        // Clamp coordinates to ensure cursor is always visible and within framebuffer

        // Horizontal line (9px wide centered on cursor)
        let h_x = (self.mouse_x - 4).max(0);
        let h_width = if self.mouse_x < 4 {
            // Cursor near left edge - draw shorter line
            (5 + self.mouse_x).min(9)
        } else {
            9
        };

        let h_line = UserRect {
            x: h_x,
            y: self.mouse_y,
            width: h_width,
            height: 1,
            color: COLOR_CURSOR,
        };
        sys_fb_fill_rect(&h_line);

        // Vertical line (9px tall centered on cursor)
        let v_y = (self.mouse_y - 4).max(0);
        let v_height = if self.mouse_y < 4 {
            // Cursor near top edge - draw shorter line
            (5 + self.mouse_y).min(9)
        } else {
            9
        };

        let v_line = UserRect {
            x: self.mouse_x,
            y: v_y,
            width: 1,
            height: v_height,
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

    // High-performance compositor loop
    // - NO artificial framerate limiting (no sleep)
    // - Physics/logic runs every frame (deterministic behavior)
    // - Rendering runs as fast as the CPU can handle
    // - Yields to scheduler for cooperative multitasking
    //
    // This ensures:
    // - Fast machines get high framerate and responsive feel
    // - Slow machines get consistent physics (no skip/lag)
    // - Same behavior across different hardware
    let mut frame_count: u64 = 0;

    loop {
        // === PHYSICS/LOGIC PHASE ===
        // Always runs exactly once per frame for deterministic behavior

        // 1. Update mouse state from kernel driver
        wm.update_mouse();

        // 2. Refresh window list from kernel
        wm.refresh_windows();

        // 3. Handle mouse events (dragging, clicking, etc.)
        wm.handle_mouse_events(&fb_info);

        // === RENDERING PHASE ===
        // Wayland-style damage tracking: clear → composite → decorate
        //
        // DAMAGE SOURCES:
        // 1. First frame: Full screen (clears boot artifacts like roulette)
        // 2. Window damage: All visible windows (ensures dirty windows render)
        // 3. Cursor damage: Old and new cursor positions
        //
        // This ensures correctness while still being more efficient than
        // full-screen clear on every frame after the first one.

        // 4. Get ALL damage regions (first frame, windows, cursor)
        let (damage_regions, damage_count) = wm.get_all_damage_regions(&fb_info);

        // 5. Clear only damaged regions (removes old content from background)
        for i in 0..damage_count {
            let region = &damage_regions[i];
            let clear_rect = UserRect {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
                color: COLOR_BACKGROUND,
            };
            sys_fb_fill_rect(&clear_rect);
        }

        // 6. Composite windows in damaged regions only
        //    Kernel checks dirty flags and only blits windows that overlap damage
        sys_compositor_present_damage(&damage_regions[..damage_count]);

        // 7. Mark first frame complete (subsequent frames use incremental damage)
        if wm.first_frame {
            wm.first_frame = false;
        }

        // 7. Draw decorations on top (title bars, taskbar, cursor)
        //    Taskbar covers its area, title bars cover theirs
        wm.draw_decorations(&fb_info);

        // 8. Yield to scheduler for cooperative multitasking
        // This allows other tasks to run without blocking
        sys_yield();

        frame_count = frame_count.wrapping_add(1);
    }
}
