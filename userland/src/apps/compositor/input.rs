use crate::program_registry;
use crate::syscall::{UserWindowInfo, input, process, tty, window};
use crate::theme::*;
use std::time::Instant;

use super::MAX_WINDOWS;
use super::output::WINDOW_STATE_MINIMIZED;
use super::taskbar::{self, START_MENU_ITEMS};

const WINDOW_STATE_NORMAL: u8 = 0;
const CLOSE_REQUEST_GRACE_MS: u64 = 1500;
const MAX_CURSOR_TRAIL: usize = 16;

pub struct InputHandler {
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub mouse_buttons: u8,
    mouse_buttons_prev: u8,

    pub dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,

    pub start_menu_open: bool,
    pub focused_task: u32,
    /// The task that the kernel currently routes keyboard events to.
    /// Kept in sync with `focused_task`; the syscall is only issued when
    /// these two diverge, avoiding redundant kernel calls every frame.
    kernel_keyboard_focus: u32,
    pub needs_full_redraw: bool,

    pub cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    pub cursor_trail_count: usize,

    pending_close_tasks: [u32; MAX_WINDOWS],
    pending_close_deadlines: [u64; MAX_WINDOWS],
    pending_close_count: usize,
    clock_origin: Instant,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            mouse_buttons_prev: 0,
            dragging: false,
            drag_task: 0,
            drag_offset_x: 0,
            drag_offset_y: 0,
            start_menu_open: false,
            focused_task: 0,
            kernel_keyboard_focus: 0,
            needs_full_redraw: false,
            cursor_trail: [(0, 0); MAX_CURSOR_TRAIL],
            cursor_trail_count: 0,
            pending_close_tasks: [0; MAX_WINDOWS],
            pending_close_deadlines: [0; MAX_WINDOWS],
            pending_close_count: 0,
            clock_origin: Instant::now(),
        }
    }

    #[inline]
    fn now_ms(&self) -> u64 {
        self.clock_origin.elapsed().as_millis() as u64
    }

    pub fn update_mouse(&mut self) {
        self.cursor_trail_count = 0;

        let old_x = self.mouse_x;
        let old_y = self.mouse_y;

        let (new_x, new_y) = input::get_pointer_pos();
        if new_x != self.mouse_x || new_y != self.mouse_y {
            if self.cursor_trail_count < MAX_CURSOR_TRAIL {
                self.cursor_trail[self.cursor_trail_count] = (old_x, old_y);
                self.cursor_trail_count += 1;
            }
            self.mouse_x = new_x;
            self.mouse_y = new_y;
        }

        self.mouse_buttons_prev = self.mouse_buttons;
        self.mouse_buttons = input::get_button_state();
    }

    fn mouse_clicked(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0 && (self.mouse_buttons_prev & 0x01) == 0
    }

    fn mouse_pressed(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0
    }

    /// Update pointer focus to the topmost visible window under the cursor.
    ///
    /// Following the Wayland compositor pattern (wlroots `tinywl.c`), pointer
    /// focus is tracked **continuously on every frame** — not only on click.
    /// This ensures the correct window already has focus by the time a PS/2
    /// button IRQ fires, so button events are routed to the right client.
    pub fn update_pointer_focus(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        for i in (0..window_count as usize).rev() {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }
            if self.hit_test_content_area(&window) {
                input::set_pointer_focus_with_offset(window.task_id, window.x, window.y);
                return;
            }
        }
        input::set_pointer_focus(0);
    }

    pub fn sync_keyboard_focus(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        prev_windows: &[UserWindowInfo; MAX_WINDOWS],
        prev_window_count: u32,
    ) {
        // Auto-focus newly appeared windows (not in prev snapshot).
        for i in 0..window_count as usize {
            let task_id = windows[i].task_id;
            if windows[i].state == WINDOW_STATE_MINIMIZED {
                continue;
            }
            let existed_before =
                (0..prev_window_count as usize).any(|j| prev_windows[j].task_id == task_id);
            if !existed_before {
                self.focused_task = task_id;
                break;
            }
        }

        // If the focused window no longer exists or is minimized, pick the
        // topmost visible window (last in the z-ordered array).
        if self.focused_task != 0 {
            let still_visible = (0..window_count as usize).any(|i| {
                windows[i].task_id == self.focused_task
                    && windows[i].state != WINDOW_STATE_MINIMIZED
            });
            if !still_visible {
                self.focused_task = 0;
                for i in (0..window_count as usize).rev() {
                    if windows[i].state != WINDOW_STATE_MINIMIZED {
                        self.focused_task = windows[i].task_id;
                        break;
                    }
                }
            }
        }

        // Only issue the kernel syscall when focus actually changed.
        if self.focused_task != self.kernel_keyboard_focus {
            input::set_keyboard_focus(self.focused_task);
            self.kernel_keyboard_focus = self.focused_task;
            self.needs_full_redraw = true;
        }
    }

    pub fn handle_mouse_events(
        &mut self,
        fb_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        let clicked = self.mouse_clicked();

        if self.dragging {
            if !self.mouse_pressed() {
                self.stop_drag();
            } else {
                self.update_drag();
            }
            return;
        }

        if !clicked {
            return;
        }

        if self.start_menu_open && self.hit_test_start_menu(fb_height) {
            if let Some(item_idx) = self.hit_test_start_menu_item(fb_height) {
                self.activate_start_menu_item(item_idx, windows, window_count);
            }
            return;
        }

        if self.start_menu_open
            && !self.hit_test_start_button(fb_height)
            && !self.hit_test_start_menu(fb_height)
        {
            self.start_menu_open = false;
            self.needs_full_redraw = true;
        }

        if self.mouse_y >= fb_height - TASKBAR_HEIGHT {
            self.handle_taskbar_click(fb_height, windows, window_count);
            return;
        }

        for i in (0..window_count as usize).rev() {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            if self.hit_test_title_bar(&window) {
                if self.hit_test_close_button(&window) {
                    self.request_window_close(window.task_id, windows, window_count);
                    return;
                }

                if self.hit_test_minimize_button(&window) {
                    window::set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                    return;
                }

                self.start_drag(&window);
                window::raise_window(window.task_id);
                self.set_focused(window.task_id);
                return;
            }

            if self.hit_test_content_area(&window) {
                window::raise_window(window.task_id);
                input::set_pointer_focus_with_offset(window.task_id, window.x, window.y);
                self.set_focused(window.task_id);
                return;
            }
        }

        // Clicked on desktop background (no window hit) — clear GUI keyboard
        // focus so keystrokes go to the VConsole/TTY instead.
        self.set_focused(0);
    }

    pub fn process_pending_close_requests(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        if self.pending_close_count == 0 {
            return;
        }

        let now = self.now_ms();
        let mut i = 0usize;
        while i < self.pending_close_count {
            let task_id = self.pending_close_tasks[i];

            if !window_exists(windows, window_count, task_id) {
                self.remove_pending_close_at(i);
                continue;
            }

            if now >= self.pending_close_deadlines[i] {
                let _ = process::terminate_task(task_id);
                self.remove_pending_close_at(i);
                self.needs_full_redraw = true;
                continue;
            }

            i += 1;
        }
    }

    pub fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.height as i32
    }

    pub fn hit_test_title_bar(&self, window: &UserWindowInfo) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= title_y
            && self.mouse_y < window.y
    }

    pub fn hit_test_close_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING;
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    pub fn hit_test_minimize_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    pub fn hit_test_start_button(&self, fb_height: i32) -> bool {
        let btn_x = taskbar::start_button_x();
        let btn_y = taskbar::start_button_y(fb_height);
        let btn_h = taskbar::start_button_height();
        self.mouse_x >= btn_x
            && self.mouse_x < btn_x + START_BUTTON_WIDTH
            && self.mouse_y >= btn_y
            && self.mouse_y < btn_y + btn_h
    }

    pub fn hit_test_start_menu(&self, fb_height: i32) -> bool {
        let menu_x = taskbar::start_menu_x();
        let menu_y = taskbar::start_menu_y(fb_height);
        let menu_h = taskbar::start_menu_height();
        self.mouse_x >= menu_x
            && self.mouse_x < menu_x + START_MENU_WIDTH
            && self.mouse_y >= menu_y
            && self.mouse_y < menu_y + menu_h
    }

    pub fn hit_test_start_menu_item(&self, fb_height: i32) -> Option<usize> {
        if !self.start_menu_open || !self.hit_test_start_menu(fb_height) {
            return None;
        }

        let menu_y = taskbar::start_menu_y(fb_height) + START_MENU_PADDING;
        let rel_y = self.mouse_y - menu_y;
        if rel_y < 0 {
            return None;
        }
        let idx = (rel_y / START_MENU_ITEM_HEIGHT) as usize;
        if idx < START_MENU_ITEMS.len() {
            Some(idx)
        } else {
            None
        }
    }

    fn set_focused(&mut self, task_id: u32) {
        self.focused_task = task_id;
        if task_id != self.kernel_keyboard_focus {
            input::set_keyboard_focus(task_id);
            self.kernel_keyboard_focus = task_id;
        }
    }

    fn start_drag(&mut self, window: &UserWindowInfo) {
        self.dragging = true;
        self.drag_task = window.task_id;
        self.drag_offset_x = self.mouse_x - window.x;
        self.drag_offset_y = self.mouse_y - window.y;
    }

    fn stop_drag(&mut self) {
        self.dragging = false;
        self.drag_task = 0;
    }

    fn update_drag(&mut self) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        window::set_window_position(self.drag_task, new_x, new_y);
        self.needs_full_redraw = true;
    }

    fn request_window_close(
        &mut self,
        task_id: u32,
        _windows: &[UserWindowInfo; MAX_WINDOWS],
        _window_count: u32,
    ) {
        if let Some(idx) = self.pending_close_index(task_id) {
            let _ = process::terminate_task(task_id);
            self.remove_pending_close_at(idx);
            self.needs_full_redraw = true;
            return;
        }

        let now = self.now_ms();
        let requested = input::request_close(task_id) == 0;

        if !requested || self.pending_close_count >= MAX_WINDOWS {
            let _ = process::terminate_task(task_id);
            self.needs_full_redraw = true;
            return;
        }

        let idx = self.pending_close_count;
        self.pending_close_tasks[idx] = task_id;
        self.pending_close_deadlines[idx] = now.saturating_add(CLOSE_REQUEST_GRACE_MS);
        self.pending_close_count += 1;
        self.needs_full_redraw = true;
    }

    fn pending_close_index(&self, task_id: u32) -> Option<usize> {
        (0..self.pending_close_count).find(|&i| self.pending_close_tasks[i] == task_id)
    }

    fn remove_pending_close_at(&mut self, idx: usize) {
        if idx >= self.pending_close_count {
            return;
        }

        let last = self.pending_close_count - 1;
        self.pending_close_tasks[idx] = self.pending_close_tasks[last];
        self.pending_close_deadlines[idx] = self.pending_close_deadlines[last];
        self.pending_close_tasks[last] = 0;
        self.pending_close_deadlines[last] = 0;
        self.pending_close_count -= 1;
    }

    fn launch_or_raise_program(
        &mut self,
        window_title: Option<&[u8]>,
        program_name: &str,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        if let Some(title) = window_title {
            if let Some(task_id) = find_window_by_title(windows, window_count, title) {
                window::raise_window(task_id);
                self.set_focused(task_id);
                return;
            }
        }

        if let Some(spec) = program_registry::resolve_program(program_name) {
            let tid =
                process::spawn_path_with_attrs(spec.path.as_bytes(), spec.priority, spec.flags);
            if tid <= 0 {
                tty::write(b"COMPOSITOR: spawn failed for program\n");
            } else {
                // Window doesn't exist yet; sync_keyboard_focus will pick it
                // up once it appears in the next refresh_windows cycle.
                self.focused_task = tid as u32;
            }
        } else {
            tty::write(b"COMPOSITOR: unknown program in start menu\n");
        }
    }

    fn activate_start_menu_item(
        &mut self,
        item_idx: usize,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        if let Some(item) = START_MENU_ITEMS.get(item_idx) {
            self.launch_or_raise_program(
                item.window_title,
                item.program_name,
                windows,
                window_count,
            );
            self.start_menu_open = false;
            self.needs_full_redraw = true;
        }
    }

    fn handle_taskbar_click(
        &mut self,
        fb_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        if self.hit_test_start_button(fb_height) {
            self.start_menu_open = !self.start_menu_open;
            self.needs_full_redraw = true;
            return;
        }

        if let Some(item_idx) = self.hit_test_start_menu_item(fb_height) {
            self.activate_start_menu_item(item_idx, windows, window_count);
            return;
        }

        let mut x = taskbar::app_buttons_start_x();
        for i in 0..window_count as usize {
            let w = &windows[i];
            if self.mouse_x >= x && self.mouse_x < x + TASKBAR_BUTTON_WIDTH {
                let new_state = if w.state == WINDOW_STATE_MINIMIZED {
                    WINDOW_STATE_NORMAL
                } else {
                    WINDOW_STATE_MINIMIZED
                };
                window::set_window_state(w.task_id, new_state);
                if new_state == WINDOW_STATE_NORMAL {
                    window::raise_window(w.task_id);
                    self.set_focused(w.task_id);
                }
                self.needs_full_redraw = true;
                return;
            }

            x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }

        if self.start_menu_open {
            self.start_menu_open = false;
            self.needs_full_redraw = true;
        }
    }
}

fn window_exists(windows: &[UserWindowInfo; MAX_WINDOWS], count: u32, task_id: u32) -> bool {
    (0..count as usize).any(|i| windows[i].task_id == task_id)
}

fn find_window_by_title(
    windows: &[UserWindowInfo; MAX_WINDOWS],
    count: u32,
    title: &[u8],
) -> Option<u32> {
    let title_len = title.iter().position(|&b| b == 0).unwrap_or(title.len());
    for i in 0..count as usize {
        let win_title_len = windows[i].title.iter().position(|&b| b == 0).unwrap_or(32);
        if title_len == win_title_len && windows[i].title[..win_title_len] == title[..title_len] {
            return Some(windows[i].task_id);
        }
    }
    None
}
