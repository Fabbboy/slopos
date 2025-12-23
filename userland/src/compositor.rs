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

// Cursor constants
const CURSOR_SIZE: i32 = 9;
const CURSOR_HALF: i32 = CURSOR_SIZE / 2;

/// Tracks state for conditional taskbar redraws
#[derive(Clone, Copy, PartialEq, Eq)]
struct TaskbarState {
    window_count: u32,
    focused_task: u32,
    // Packed window states (minimized/normal) as bits
    window_states: u32,
}

impl TaskbarState {
    const fn empty() -> Self {
        Self {
            window_count: 0,
            focused_task: 0,
            window_states: 0,
        }
    }

    fn from_windows(windows: &[UserWindowInfo; MAX_WINDOWS], count: u32, focused: u32) -> Self {
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
        }
    }
}

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
    // Previous window state for move damage tracking
    prev_windows: [UserWindowInfo; MAX_WINDOWS],
    prev_window_count: u32,
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
    // Taskbar state tracking for conditional redraws
    prev_taskbar_state: TaskbarState,
    taskbar_needs_redraw: bool,
}

impl WindowManager {
    fn new() -> Self {
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [UserWindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
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
            prev_taskbar_state: TaskbarState::empty(),
            taskbar_needs_redraw: true, // First frame always needs taskbar
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

    /// Refresh window list from kernel and detect taskbar state changes
    fn refresh_windows(&mut self) {
        // Save previous window state for move damage tracking
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;

        // Get current window state from kernel
        self.window_count = sys_enumerate_windows(&mut self.windows) as u32;

        // Check if taskbar state changed (window count, focus, or minimize states)
        let new_state = TaskbarState::from_windows(&self.windows, self.window_count, self.focused_task);
        if new_state != self.prev_taskbar_state {
            self.taskbar_needs_redraw = true;
            self.prev_taskbar_state = new_state;
        }
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

    /// Calculate damage regions for windows and taskbar (NOT cursor).
    /// Cursor damage is handled separately without clearing.
    /// Returns (regions, count).
    fn get_content_damage_regions(&self, fb: &UserFbInfo) -> ([UserDamageRegion; 64], usize) {
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
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            if window.state != WINDOW_STATE_MINIMIZED && window.dirty != 0 {
                let dirty_w = window.dirty_x1 - window.dirty_x0 + 1;
                let dirty_h = window.dirty_y1 - window.dirty_y0 + 1;

                if dirty_w > 0 && dirty_h > 0 && count < 64 {
                    regions[count] = UserDamageRegion {
                        x: window.x + window.dirty_x0,
                        y: window.y + window.dirty_y0,
                        width: dirty_w,
                        height: dirty_h,
                    };
                    count += 1;
                }
            }
        }

        // TASKBAR DAMAGE: Only when taskbar state actually changed
        if self.taskbar_needs_redraw && count < 64 {
            regions[count] = UserDamageRegion {
                x: 0,
                y: fb.height as i32 - TASKBAR_HEIGHT,
                width: fb.width as i32,
                height: TASKBAR_HEIGHT,
            };
            count += 1;
        }

        (regions, count)
    }

    /// Get the old cursor damage region (if cursor moved)
    fn get_cursor_damage_region(&self) -> Option<UserDamageRegion> {
        if self.prev_mouse_x != self.mouse_x || self.prev_mouse_y != self.mouse_y {
            Some(UserDamageRegion {
                x: (self.prev_mouse_x - CURSOR_HALF).max(0),
                y: (self.prev_mouse_y - CURSOR_HALF).max(0),
                width: CURSOR_SIZE,
                height: CURSOR_SIZE,
            })
        } else {
            None
        }
    }

    /// Calculate damage regions for windows that moved (position changed but content same).
    /// Returns (regions, count).
    fn get_move_damage_regions(&self) -> ([UserDamageRegion; 64], usize) {
        let mut regions = [UserDamageRegion::default(); 64];
        let mut count = 0;

        // For each current window, check if it moved from previous frame
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Find this window in previous frame
            for j in 0..self.prev_window_count as usize {
                let prev = &self.prev_windows[j];
                if prev.task_id == window.task_id && prev.state != WINDOW_STATE_MINIMIZED {
                    // Check if position changed
                    if prev.x != window.x || prev.y != window.y {
                        // Add OLD position as damage (clear what was there)
                        if count < 64 {
                            regions[count] = UserDamageRegion {
                                x: prev.x,
                                y: prev.y,
                                width: prev.width as i32,
                                height: prev.height as i32,
                            };
                            count += 1;
                        }
                        // Add NEW position as damage (draw window there)
                        if count < 64 {
                            regions[count] = UserDamageRegion {
                                x: window.x,
                                y: window.y,
                                width: window.width as i32,
                                height: window.height as i32,
                            };
                            count += 1;
                        }
                    }
                    break;
                }
            }
        }

        (regions, count)
    }

    /// Check if a rectangle overlaps with any damage region
    fn overlaps_damage(
        x: i32, y: i32, w: i32, h: i32,
        regions: &[UserDamageRegion], count: usize
    ) -> bool {
        let r1_x1 = x + w - 1;
        let r1_y1 = y + h - 1;

        for i in 0..count {
            let r = &regions[i];
            let r2_x1 = r.x + r.width - 1;
            let r2_y1 = r.y + r.height - 1;

            // Rectangle overlap test
            if x <= r2_x1 && r1_x1 >= r.x && y <= r2_y1 && r1_y1 >= r.y {
                return true;
            }
        }
        false
    }

    /// Check if taskbar overlaps any damage region
    fn taskbar_overlaps_damage(&self, fb: &UserFbInfo, regions: &[UserDamageRegion], count: usize) -> bool {
        let taskbar_y = fb.height as i32 - TASKBAR_HEIGHT;
        Self::overlaps_damage(0, taskbar_y, fb.width as i32, TASKBAR_HEIGHT, regions, count)
    }

    /// Check if taskbar overlaps a specific region
    fn taskbar_overlaps_region(fb: &UserFbInfo, region: &UserDamageRegion) -> bool {
        let taskbar_y = fb.height as i32 - TASKBAR_HEIGHT;
        Self::rects_overlap(0, taskbar_y, fb.width as i32, TASKBAR_HEIGHT,
                           region.x, region.y, region.width, region.height)
    }

    /// Check if a window's title bar overlaps any damage region
    fn title_bar_overlaps_damage(window: &UserWindowInfo, regions: &[UserDamageRegion], count: usize) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        Self::overlaps_damage(window.x, title_y, window.width as i32, TITLE_BAR_HEIGHT, regions, count)
    }

    /// Check if a window's title bar overlaps a specific region
    fn title_bar_overlaps_region(window: &UserWindowInfo, region: &UserDamageRegion) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        Self::rects_overlap(window.x, title_y, window.width as i32, TITLE_BAR_HEIGHT,
                           region.x, region.y, region.width, region.height)
    }

    /// Check if two rectangles overlap
    fn rects_overlap(x1: i32, y1: i32, w1: i32, h1: i32, x2: i32, y2: i32, w2: i32, h2: i32) -> bool {
        let r1_x1 = x1 + w1 - 1;
        let r1_y1 = y1 + h1 - 1;
        let r2_x1 = x2 + w2 - 1;
        let r2_y1 = y2 + h2 - 1;
        x1 <= r2_x1 && r1_x1 >= x2 && y1 <= r2_y1 && r1_y1 >= y2
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

        // 2. Refresh window list from kernel (also detects taskbar state changes)
        wm.refresh_windows();

        // 3. Handle mouse events (dragging, clicking, etc.)
        wm.handle_mouse_events(&fb_info);

        // === RENDERING PHASE ===
        // Layer-based compositing with smart cursor handling:
        //   - Content damage (windows, taskbar state): CLEAR then redraw
        //   - Move damage (window position changes): CLEAR then recomposite existing pixels
        //   - Cursor damage (old position): NO CLEAR, directly redraw layers
        // This prevents the blue rectangle artifact when cursor moves.

        // 4. Get content damage regions (windows + taskbar state changes)
        let (content_damage, content_count) = wm.get_content_damage_regions(&fb_info);

        // 4a. Get move damage regions (windows that changed position but not content)
        let (move_damage, move_count) = wm.get_move_damage_regions();

        // 5. Get cursor damage region (old cursor position, if moved)
        let cursor_damage = wm.get_cursor_damage_region();

        // 6. Clear content AND move damage regions (NOT cursor damage)
        for i in 0..content_count {
            let region = &content_damage[i];
            let clear_rect = UserRect {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
                color: COLOR_BACKGROUND,
            };
            sys_fb_fill_rect(&clear_rect);
        }
        for i in 0..move_count {
            let region = &move_damage[i];
            let clear_rect = UserRect {
                x: region.x,
                y: region.y,
                width: region.width,
                height: region.height,
                color: COLOR_BACKGROUND,
            };
            sys_fb_fill_rect(&clear_rect);
        }

        // 7. Build combined damage list for window compositing
        // Kernel needs to know about content, move, and cursor damage to composite windows
        let mut all_damage = [UserDamageRegion::default(); 64];
        let mut all_count = 0;
        for i in 0..content_count {
            if all_count < 64 {
                all_damage[all_count] = content_damage[i];
                all_count += 1;
            }
        }
        for i in 0..move_count {
            if all_count < 64 {
                all_damage[all_count] = move_damage[i];
                all_count += 1;
            }
        }
        if let Some(ref cursor_region) = cursor_damage {
            if all_count < 64 {
                all_damage[all_count] = *cursor_region;
                all_count += 1;
            }
        }

        // 8. Composite windows in all damaged regions
        if all_count > 0 {
            sys_compositor_present_damage(&all_damage[..all_count]);
        }

        // 9. Mark first frame complete
        if wm.first_frame {
            wm.first_frame = false;
        }

        // 10. Redraw title bars that overlap any damage (content, move, OR cursor)
        for i in 0..wm.window_count as usize {
            let window = &wm.windows[i];
            if window.state != WINDOW_STATE_MINIMIZED {
                let overlaps_content = WindowManager::title_bar_overlaps_damage(window, &content_damage, content_count);
                let overlaps_move = WindowManager::title_bar_overlaps_damage(window, &move_damage, move_count);
                let overlaps_cursor = cursor_damage.as_ref()
                    .map(|r| WindowManager::title_bar_overlaps_region(window, r))
                    .unwrap_or(false);
                if overlaps_content || overlaps_move || overlaps_cursor {
                    wm.draw_title_bar(window);
                }
            }
        }

        // 11. Redraw taskbar if it overlaps any damage OR state changed
        let taskbar_overlaps_content = wm.taskbar_overlaps_damage(&fb_info, &content_damage, content_count);
        let taskbar_overlaps_move = wm.taskbar_overlaps_damage(&fb_info, &move_damage, move_count);
        let taskbar_overlaps_cursor = cursor_damage.as_ref()
            .map(|r| WindowManager::taskbar_overlaps_region(&fb_info, r))
            .unwrap_or(false);
        if taskbar_overlaps_content || taskbar_overlaps_move || taskbar_overlaps_cursor || wm.taskbar_needs_redraw {
            wm.draw_taskbar(&fb_info);
            wm.taskbar_needs_redraw = false;
        }

        // 12. Draw cursor on top of everything
        wm.draw_cursor();

        // 12. Yield to scheduler for cooperative multitasking
        sys_yield();

        frame_count = frame_count.wrapping_add(1);
    }
}
