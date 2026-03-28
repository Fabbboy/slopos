use crate::program_registry;
use crate::syscall::{UserWindowInfo, input, process, tty, window};
use crate::theme::*;
use std::time::Instant;

use super::MAX_WINDOWS;
use super::decorations;
use super::dock::LauncherShelf;
use super::menu_bar::SystemBar;
use super::output::WINDOW_STATE_MINIMIZED;

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

    /// The task that currently has keyboard focus.  Private — all changes
    /// go through `set_focused()` so the kernel syscall is always issued.
    /// Read via `focused_task()`.  This is the Mutter/KWin pattern: a
    /// single entry-point for focus changes prevents desync by design.
    focused_task: u32,
    /// Mirror of the last value sent to the kernel via
    /// `input::set_keyboard_focus()`.  Compared against `focused_task`
    /// inside `set_focused()` to skip redundant syscalls.
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

    /// Read the currently focused task.  All mutations go through
    /// `set_focused()` which keeps the kernel in sync.
    pub fn focused_task(&self) -> u32 {
        self.focused_task
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
    /// focus is tracked **continuously on every frame** -- not only on click.
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
                self.set_focused(task_id);
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
                let mut new_focus = 0u32;
                for i in (0..window_count as usize).rev() {
                    if windows[i].state != WINDOW_STATE_MINIMIZED {
                        new_focus = windows[i].task_id;
                        break;
                    }
                }
                self.set_focused(new_focus);
            }
        }
    }

    /// Hit-test priority chain (spec section 8):
    /// 1. system_bar::hit_test() -> consume click (no action)
    /// 2. shelf.hit_test()       -> handle shelf click
    /// 3. decorations::hit_test_signal_button() -> close/min/expand
    /// 4. decorations::hit_test_title_bar()     -> drag
    /// 5. hit_test_content_area() -> raise + focus + forward
    /// 6. desktop -> deselect
    pub fn handle_mouse_events(
        &mut self,
        _fb_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        shelf: &LauncherShelf,
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

        // 1. System bar -- consume click, no action
        if SystemBar::hit_test(self.mouse_x, self.mouse_y) {
            return;
        }

        // 2. Shelf click
        if let Some(idx) = shelf.hit_test(self.mouse_x, self.mouse_y) {
            self.handle_shelf_click(idx, shelf, windows, window_count);
            return;
        }

        // 3-4. Window decorations and content (top-to-bottom z-order)
        for i in (0..window_count as usize).rev() {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Frame top (title bar) is above the kernel's window.y.
            let frame_y = window.y - TITLE_BAR_HEIGHT;

            // 3. Signal buttons (close/minimize/expand)
            if let Some(button_id) =
                decorations::hit_test_signal_button(window.x, frame_y, self.mouse_x, self.mouse_y)
            {
                match button_id {
                    0 => {
                        // Close
                        self.request_window_close(window.task_id, windows, window_count);
                    }
                    1 => {
                        // Minimize
                        window::set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                    }
                    2 => {
                        // Expand -- no-op in Phase 4
                    }
                    _ => {}
                }
                return;
            }

            // 4. Title bar (drag)
            if decorations::hit_test_title_bar(
                window.x,
                frame_y,
                window.width,
                self.mouse_x,
                self.mouse_y,
            ) {
                self.start_drag(&window);
                window::raise_window(window.task_id);
                self.set_focused(window.task_id);
                return;
            }

            // 5. Content area
            if self.hit_test_content_area(&window) {
                window::raise_window(window.task_id);
                input::set_pointer_focus_with_offset(window.task_id, window.x, window.y);
                self.set_focused(window.task_id);
                return;
            }
        }

        // 6. Desktop background -- clear focus
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

    /// Hit-test the client content area of a window. The kernel stores
    /// `window.y` as the content top; the title bar is above it.
    pub fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.height as i32
    }

    /// Single entry-point for all focus changes (KWin `activateClient`
    /// pattern).  The field is private so every mutation is forced through
    /// here at compile time, guaranteeing the kernel syscall is issued.
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
        // Don't set needs_full_redraw — the per-window bounds change
        // detection in refresh_windows() already damages old + new positions,
        // following the wlroots scene_node_set_position() pattern.
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

    fn handle_shelf_click(
        &mut self,
        idx: usize,
        shelf: &LauncherShelf,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        let entry = match shelf.entry(idx) {
            Some(e) => e,
            None => return,
        };

        if entry.running && entry.task_id != 0 {
            // Check if the window is minimized; if so, unminimize it.
            for i in 0..window_count as usize {
                if windows[i].task_id == entry.task_id {
                    if windows[i].state == WINDOW_STATE_MINIMIZED {
                        window::set_window_state(entry.task_id, WINDOW_STATE_NORMAL);
                    }
                    window::raise_window(entry.task_id);
                    self.set_focused(entry.task_id);
                    self.needs_full_redraw = true;
                    return;
                }
            }
        }

        // Not running -- spawn the program.
        let path_len = entry.path_len.min(entry.program_path.len());
        let path_bytes = &entry.program_path[..path_len];
        if let Ok(path_str) = core::str::from_utf8(path_bytes) {
            self.spawn_program(path_str);
        }
    }

    fn spawn_program(&mut self, path: &str) {
        if let Some(spec) = program_registry::resolve_program_path(path) {
            let tid =
                process::spawn_path_with_attrs(spec.path.as_bytes(), spec.priority, spec.flags);
            if tid <= 0 {
                tty::write(b"COMPOSITOR: spawn failed for program\n");
            } else {
                self.set_focused(tid as u32);
            }
        } else {
            // Fall back to direct path spawn with default attrs.
            let tid = process::spawn_path_with_attrs(path.as_bytes(), 4, 0);
            if tid <= 0 {
                tty::write(b"COMPOSITOR: spawn failed for program\n");
            } else {
                self.set_focused(tid as u32);
            }
        }
    }
}

fn window_exists(windows: &[UserWindowInfo; MAX_WINDOWS], count: u32, task_id: u32) -> bool {
    (0..count as usize).any(|i| windows[i].task_id == task_id)
}
