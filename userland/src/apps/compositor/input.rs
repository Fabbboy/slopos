use slopos_abi::InputEvent;
use slopos_abi::input::{MODIFIER_ALT, MODIFIER_CTRL, MODIFIER_SHIFT, MODIFIER_SUPER};
use slopos_abi::task::TaskPriority;
use slopos_abi::window::{CURSOR_SHAPE_DEFAULT, CURSOR_SHAPE_GRAB, CURSOR_SHAPE_GRABBING};

use crate::program_registry;
use crate::syscall::{UserWindowInfo, input, process, tty};
use crate::theme::*;
use std::time::Instant;

use super::protocol::ProtocolBridge;

use super::MAX_WINDOWS;
use super::decorations;
use super::dock::LauncherShelf;
use super::menu_bar::SystemBar;
use super::output::WINDOW_STATE_MINIMIZED;

const WINDOW_STATE_NORMAL: u8 = 0;
const CLOSE_REQUEST_GRACE_MS: u64 = 1500;
const MAX_CURSOR_TRAIL: usize = 16;

// ---------------------------------------------------------------------------
// Resize edge bitfield (Wayland convention: TOP=1, BOTTOM=2, LEFT=4, RIGHT=8)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdge(u8);

impl ResizeEdge {
    pub const NONE: Self = Self(0);
    pub const TOP: Self = Self(1);
    pub const BOTTOM: Self = Self(2);
    pub const LEFT: Self = Self(4);
    pub const RIGHT: Self = Self(8);
    pub const TOP_LEFT: Self = Self(1 | 4);
    pub const TOP_RIGHT: Self = Self(1 | 8);
    pub const BOTTOM_LEFT: Self = Self(2 | 4);
    pub const BOTTOM_RIGHT: Self = Self(2 | 8);

    #[inline]
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
    #[inline]
    pub fn has_top(self) -> bool {
        self.0 & 1 != 0
    }
    #[inline]
    pub fn has_bottom(self) -> bool {
        self.0 & 2 != 0
    }
    #[inline]
    pub fn has_left(self) -> bool {
        self.0 & 4 != 0
    }
    #[inline]
    pub fn has_right(self) -> bool {
        self.0 & 8 != 0
    }

    /// Map resize edge to the appropriate cursor shape constant.
    pub fn cursor_shape(self) -> u8 {
        use slopos_abi::window::*;
        match self.0 {
            1 => CURSOR_SHAPE_N_RESIZE,
            2 => CURSOR_SHAPE_S_RESIZE,
            4 => CURSOR_SHAPE_W_RESIZE,
            8 => CURSOR_SHAPE_E_RESIZE,
            5 => CURSOR_SHAPE_NW_RESIZE,
            9 => CURSOR_SHAPE_NE_RESIZE,
            6 => CURSOR_SHAPE_SW_RESIZE,
            10 => CURSOR_SHAPE_SE_RESIZE,
            _ => CURSOR_SHAPE_DEFAULT,
        }
    }
}

pub struct InputHandler {
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub mouse_buttons: u8,

    pub dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,

    // Resize state (wlroots model: grab snapshot + active edges)
    pub resizing: bool,
    resize_task: u32,
    resize_edges: ResizeEdge,
    resize_grab_x: i32,
    resize_grab_y: i32,
    resize_grab_w: u32,
    resize_grab_h: u32,
    resize_grab_mouse_x: i32,
    resize_grab_mouse_y: i32,
    /// Timestamp of last configure event sent during resize (for throttling).
    resize_last_configure_ms: u64,

    /// Compositor-side cursor shape override for resize hover feedback.
    /// Non-zero overrides the per-window cursor_shape in the render path.
    pub compositor_cursor_override: u8,

    /// Pre-maximize geometry for restore: (task_id, x, y, w, h).
    restore_geometry: [(u32, i32, i32, u32, u32); MAX_WINDOWS],

    /// The task that currently has keyboard focus.  Private — all changes
    /// go through `set_focused()`.  Read via `focused_task()`.  This is
    /// the Mutter/KWin pattern: a single entry-point for focus changes
    /// prevents desync by design.
    focused_task: u32,
    pub needs_full_redraw: bool,

    pub cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    pub cursor_trail_count: usize,

    pending_close_tasks: [u32; MAX_WINDOWS],
    pending_close_deadlines: [u64; MAX_WINDOWS],
    pending_close_count: usize,
    clock_origin: Instant,

    local_modifier_state: u8,
    /// Raw event buffer for poll_batch reads from the kernel queue.
    pub raw_event_buf: [InputEvent; 64],
    /// Number of raw events read in the current frame.
    pub raw_event_count: usize,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            dragging: false,
            drag_task: 0,
            drag_offset_x: 0,
            drag_offset_y: 0,
            resizing: false,
            resize_task: 0,
            resize_edges: ResizeEdge::NONE,
            resize_grab_x: 0,
            resize_grab_y: 0,
            resize_grab_w: 0,
            resize_grab_h: 0,
            resize_grab_mouse_x: 0,
            resize_grab_mouse_y: 0,
            resize_last_configure_ms: 0,
            compositor_cursor_override: 0,
            restore_geometry: [(0, 0, 0, 0, 0); MAX_WINDOWS],
            focused_task: 0,
            needs_full_redraw: false,
            cursor_trail: [(0, 0); MAX_CURSOR_TRAIL],
            cursor_trail_count: 0,
            pending_close_tasks: [0; MAX_WINDOWS],
            pending_close_deadlines: [0; MAX_WINDOWS],
            pending_close_count: 0,
            clock_origin: Instant::now(),
            local_modifier_state: 0,
            raw_event_buf: [InputEvent::default(); 64],
            raw_event_count: 0,
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

    /// Drain raw input events from the kernel queue into the frame buffer.
    ///
    /// State is NOT folded here: events are dispatched IN STREAM ORDER by
    /// the main loop (`WindowManager::process_input_events`), so every
    /// button press is hit-tested at the pointer position accumulated *up
    /// to that event* — never at the end-of-batch position. Folding the
    /// batch first (the old model) hit-tested presses at coordinates that
    /// included motion *after* the press; a resize-corner grab whose drag
    /// motion arrived in the same 16 ms batch was hit-tested at the
    /// post-drag position and silently became a content or desktop click.
    pub fn drain_events(&mut self) {
        self.cursor_trail_count = 0;
        self.raw_event_count = input::poll_batch(&mut self.raw_event_buf);
    }

    /// Apply one pointer-motion event: update position + cursor trail.
    pub fn apply_motion(&mut self, event: &InputEvent) {
        let new_x = event.pointer_x();
        let new_y = event.pointer_y();
        if new_x != self.mouse_x || new_y != self.mouse_y {
            if self.cursor_trail_count < MAX_CURSOR_TRAIL {
                self.cursor_trail[self.cursor_trail_count] = (self.mouse_x, self.mouse_y);
                self.cursor_trail_count += 1;
            }
            self.mouse_x = new_x;
            self.mouse_y = new_y;
        }
    }

    /// Apply an in-flight drag/resize to the current pointer position.
    /// Called per motion event while a grab is active (wlroots model).
    pub fn apply_grab_motion(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.dragging {
            self.update_drag(proto);
        } else if self.resizing {
            self.update_resize(proto);
        }
    }

    /// Update local modifier state from a raw scancode.
    ///
    /// The kernel routes make codes (press/release rides the event type),
    /// but break codes (make | 0x80) are also accepted defensively: a
    /// modifier release that slipped through with its raw break code must
    /// still clear the bit, or the modifier sticks for the rest of the
    /// session and silently reclassifies every chord (a stuck SHIFT turns
    /// Ctrl+C into the Ctrl+Shift+C copy chord — no SIGINT ever again).
    /// The modifier break codes (0x9D/0xAA/0xB6/0xB8/0xDB/0xDC) sit above
    /// the 0x80–0x97 pseudo-scancode range, so matching them cannot
    /// misread an arrow/nav pseudo-code.
    pub(super) fn update_modifier_from_scancode(&mut self, scancode: u8, pressed: bool) {
        // PS/2 scan code set 1 modifier make codes (+ break-code aliases).
        let bit = match scancode {
            0x2A | 0x36 | 0xAA | 0xB6 => MODIFIER_SHIFT, // Left/Right Shift
            0x1D | 0x9D => MODIFIER_CTRL,                // Left Ctrl
            0x38 | 0xB8 => MODIFIER_ALT,                 // Left Alt
            0x5B | 0x5C | 0xDB | 0xDC => MODIFIER_SUPER, // Left/Right Super
            _ => return,
        };
        // A break code is a release regardless of the event type it rode in
        // on; modifier make codes are all < 0x80, so the high bit is an
        // unambiguous release marker here.
        if pressed && scancode < 0x80 {
            self.local_modifier_state |= bit;
        } else {
            self.local_modifier_state &= !bit;
        }
    }

    /// Return the locally tracked modifier state.
    pub fn modifier_state(&self) -> u8 {
        self.local_modifier_state
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

        // Keyboard input must never fall into a void while a window is
        // visible: whenever nothing is focused, re-acquire the topmost
        // visible window. Without this, any path that left focus at 0
        // (historically: a mis-hit-tested desktop click) silently dropped
        // every subsequent keystroke until the user happened to click a
        // window again.
        if self.focused_task == 0 {
            for i in (0..window_count as usize).rev() {
                if windows[i].state != WINDOW_STATE_MINIMIZED {
                    self.set_focused(windows[i].task_id);
                    break;
                }
            }
        }
    }

    /// Handle one left-button release event in stream order: end any
    /// active drag/resize grab at the position accumulated so far.
    pub fn on_button_release(&mut self, button: u8, proto: Option<&mut ProtocolBridge>) {
        self.mouse_buttons &= !button;
        if button & 0x01 == 0 {
            return;
        }
        if self.dragging {
            self.stop_drag();
        } else if self.resizing {
            self.stop_resize(proto);
        }
    }

    /// Handle one button-press event in stream order, hit-testing at the
    /// pointer position accumulated up to THIS event.
    ///
    /// Hit-test priority chain (spec section 8):
    /// 1. system_bar::hit_test() -> consume click (no action)
    /// 2. shelf.hit_test()       -> handle shelf click
    /// 3. decorations::hit_test_signal_button() -> close/min/expand
    /// 4. decorations::hit_test_title_bar()     -> drag
    /// 5. hit_test_content_area() -> raise + focus + forward
    /// 6. desktop -> no-op (keyboard focus is sticky; see sync_keyboard_focus)
    pub fn on_button_press(
        &mut self,
        button: u8,
        fb_width: i32,
        fb_height: i32,
        shelf_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        shelf: &LauncherShelf,
        mut proto: Option<&mut ProtocolBridge>,
    ) {
        self.mouse_buttons |= button;
        if button & 0x01 == 0 {
            return;
        }

        // A press while a grab is active means the matching release event
        // was lost (e.g. kernel queue overwrote it under a motion flood).
        // The kernel saw a physical release before this press, so end the
        // stale grab and process the press normally.
        if self.dragging {
            self.stop_drag();
        }
        if self.resizing {
            self.stop_resize(proto.as_deref_mut());
        }

        // 1. System bar -- consume click, no action
        if SystemBar::hit_test(self.mouse_x, self.mouse_y) {
            return;
        }

        // 2. Shelf click
        if let Some(idx) = shelf.hit_test(self.mouse_x, self.mouse_y) {
            self.handle_shelf_click(idx, shelf, windows, window_count, &mut proto);
            return;
        }

        // 3-4. Window decorations and content (top-to-bottom z-order)
        for i in (0..window_count as usize).rev() {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Resize edges (shadow grab zone) — skip for maximized windows
            if window.state != 2 {
                let edge = decorations::hit_test_resize_edge(
                    window.x,
                    window.y,
                    window.effective_width(),
                    window.effective_height(),
                    self.mouse_x,
                    self.mouse_y,
                );
                if !edge.is_none() {
                    self.start_resize(&window, edge);
                    if let Some(ref mut p) = proto {
                        p.raise_window(window.task_id);
                    }
                    self.set_focused(window.task_id);
                    return;
                }
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
                        self.request_window_close(
                            window.task_id,
                            windows,
                            window_count,
                            &mut proto,
                        );
                    }
                    1 => {
                        // Minimize
                        if let Some(ref mut p) = proto {
                            p.set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                        }
                    }
                    2 => {
                        // Expand — toggle maximize/restore
                        const WINDOW_STATE_MAXIMIZED: u8 = 2;
                        if window.state == WINDOW_STATE_MAXIMIZED {
                            // Restore saved geometry
                            if let Some(geo) =
                                self.restore_geometry.iter().find(|g| g.0 == window.task_id)
                            {
                                if let Some(ref mut p) = proto {
                                    p.set_window_position(window.task_id, geo.1, geo.2);
                                    p.set_window_size(window.task_id, geo.3, geo.4);
                                    p.send_configure_for_task(
                                        window.task_id,
                                        geo.3,
                                        geo.4,
                                        slopos_protocol::toplevel_state::ACTIVATED,
                                    );
                                }
                            }
                            if let Some(ref mut p) = proto {
                                p.set_window_state(window.task_id, WINDOW_STATE_NORMAL);
                            }
                        } else {
                            // Save current geometry for restore
                            if let Some(slot) = self
                                .restore_geometry
                                .iter_mut()
                                .find(|g| g.0 == 0 || g.0 == window.task_id)
                            {
                                *slot = (
                                    window.task_id,
                                    window.x,
                                    window.y,
                                    window.effective_width(),
                                    window.effective_height(),
                                );
                            }
                            // Maximize: fill screen between system bar and shelf
                            let max_y = SYSTEM_BAR_HEIGHT + TITLE_BAR_HEIGHT;
                            let max_w = fb_width as u32;
                            let max_h =
                                (fb_height - SYSTEM_BAR_HEIGHT - TITLE_BAR_HEIGHT - shelf_height)
                                    as u32;
                            if let Some(ref mut p) = proto {
                                p.set_window_position(window.task_id, 0, max_y);
                                p.set_window_size(window.task_id, max_w, max_h);
                                p.set_window_state(window.task_id, WINDOW_STATE_MAXIMIZED);
                                p.send_configure_for_task(
                                    window.task_id,
                                    max_w,
                                    max_h,
                                    slopos_protocol::toplevel_state::ACTIVATED
                                        | slopos_protocol::toplevel_state::MAXIMIZED,
                                );
                            }
                        }
                        self.needs_full_redraw = true;
                    }
                    _ => {}
                }
                return;
            }

            // 4. Title bar (drag)
            if decorations::hit_test_title_bar(
                window.x,
                frame_y,
                window.effective_width(),
                self.mouse_x,
                self.mouse_y,
            ) {
                self.start_drag(&window);
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                return;
            }

            // 5. Content area
            if self.hit_test_content_area(&window) {
                // Super+LMB on content: interactive move (wlroots/Sway pattern).
                // The modifier check uses local state from raw events.
                if self.local_modifier_state & MODIFIER_SUPER != 0 && window.state != 2 {
                    self.start_drag(&window);
                    if let Some(ref mut p) = proto {
                        p.raise_window(window.task_id);
                    }
                    self.set_focused(window.task_id);
                    return;
                }
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                return;
            }
        }

        // 6. Desktop background — keyboard focus is sticky (stays on the
        // last focused window). Clearing it here created an input black
        // hole: every keystroke was dropped until the next window click.
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
                // All windows are protocol surfaces identified by surface index.
                // The protocol cleanup handles surface destruction on disconnect;
                // no kernel terminate_task needed.
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
            && self.mouse_x < window.x + window.effective_width() as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.effective_height() as i32
    }

    /// Single entry-point for all focus changes (KWin `activateClient`
    /// pattern).  The field is private so every mutation is forced through
    /// here at compile time.  With the protocol migration, keyboard focus
    /// is tracked locally; key events are routed to the focused surface
    /// via ProtocolBridge::forward_input_events in the main loop.
    fn set_focused(&mut self, task_id: u32) {
        self.focused_task = task_id;
    }

    fn start_drag(&mut self, window: &UserWindowInfo) {
        self.dragging = true;
        self.drag_task = window.task_id;
        self.drag_offset_x = self.mouse_x - window.x;
        self.drag_offset_y = self.mouse_y - window.y;
        self.compositor_cursor_override = CURSOR_SHAPE_GRABBING;
    }

    fn stop_drag(&mut self) {
        self.dragging = false;
        self.drag_task = 0;
        self.compositor_cursor_override = CURSOR_SHAPE_DEFAULT;
    }

    fn update_drag(&mut self, proto: Option<&mut ProtocolBridge>) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        if let Some(p) = proto {
            p.set_window_position(self.drag_task, new_x, new_y);
        }
    }

    // -- Resize state machine (wlroots model) --------------------------------

    fn start_resize(&mut self, window: &UserWindowInfo, edges: ResizeEdge) {
        self.resizing = true;
        self.resize_task = window.task_id;
        self.resize_edges = edges;
        self.resize_grab_x = window.x;
        self.resize_grab_y = window.y;
        self.resize_grab_w = window.width;
        self.resize_grab_h = window.height;
        self.resize_grab_mouse_x = self.mouse_x;
        self.resize_grab_mouse_y = self.mouse_y;
    }

    fn update_resize(&mut self, proto: Option<&mut ProtocolBridge>) {
        let dx = self.mouse_x - self.resize_grab_mouse_x;
        let dy = self.mouse_y - self.resize_grab_mouse_y;

        let mut new_x = self.resize_grab_x;
        let mut new_y = self.resize_grab_y;
        let mut new_w = self.resize_grab_w as i32;
        let mut new_h = self.resize_grab_h as i32;

        if self.resize_edges.has_right() {
            new_w += dx;
        }
        if self.resize_edges.has_left() {
            new_w -= dx;
            new_x += dx;
        }
        if self.resize_edges.has_bottom() {
            new_h += dy;
        }
        if self.resize_edges.has_top() {
            new_h -= dy;
            new_y += dy;
        }

        // Clamp to minimum, keeping the anchored corner fixed
        if new_w < MIN_WINDOW_WIDTH {
            if self.resize_edges.has_left() {
                new_x -= MIN_WINDOW_WIDTH - new_w;
            }
            new_w = MIN_WINDOW_WIDTH;
        }
        if new_h < MIN_WINDOW_HEIGHT {
            if self.resize_edges.has_top() {
                new_y -= MIN_WINDOW_HEIGHT - new_h;
            }
            new_h = MIN_WINDOW_HEIGHT;
        }

        if let Some(p) = proto {
            p.set_window_position(self.resize_task, new_x, new_y);
            p.set_window_size(self.resize_task, new_w as u32, new_h as u32);

            // Send throttled configure events (~every 100ms) so clients can
            // reallocate and re-render during the drag, not just at the end.
            let now = self.now_ms();
            if now.saturating_sub(self.resize_last_configure_ms) >= 100 {
                self.resize_last_configure_ms = now;
                p.send_configure_for_task(
                    self.resize_task,
                    new_w as u32,
                    new_h as u32,
                    slopos_protocol::toplevel_state::ACTIVATED
                        | slopos_protocol::toplevel_state::RESIZING,
                );
            }
        }
    }

    fn stop_resize(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.resize_task != 0 {
            // Compute the final size from the current state
            let dx = self.mouse_x - self.resize_grab_mouse_x;
            let dy = self.mouse_y - self.resize_grab_mouse_y;
            let mut final_w = self.resize_grab_w as i32;
            let mut final_h = self.resize_grab_h as i32;
            if self.resize_edges.has_right() {
                final_w += dx;
            }
            if self.resize_edges.has_left() {
                final_w -= dx;
            }
            if self.resize_edges.has_bottom() {
                final_h += dy;
            }
            if self.resize_edges.has_top() {
                final_h -= dy;
            }
            if final_w < MIN_WINDOW_WIDTH {
                final_w = MIN_WINDOW_WIDTH;
            }
            if final_h < MIN_WINDOW_HEIGHT {
                final_h = MIN_WINDOW_HEIGHT;
            }
            if let Some(p) = proto {
                p.send_configure_for_task(
                    self.resize_task,
                    final_w as u32,
                    final_h as u32,
                    slopos_protocol::toplevel_state::ACTIVATED,
                );
            }
        }
        self.resizing = false;
        self.resize_task = 0;
        self.resize_edges = ResizeEdge::NONE;
        self.compositor_cursor_override = CURSOR_SHAPE_DEFAULT;
    }

    /// Update the compositor cursor override for resize hover feedback.
    /// Called every frame; sets the cursor shape when hovering a resize zone.
    pub fn update_resize_cursor(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
    ) {
        if self.dragging || self.resizing {
            return;
        }

        self.compositor_cursor_override = CURSOR_SHAPE_DEFAULT;

        for i in (0..window_count as usize).rev() {
            let w = windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Skip resize cursor for maximized windows
            if w.state != 2 {
                let edge = decorations::hit_test_resize_edge(
                    w.x,
                    w.y,
                    w.effective_width(),
                    w.effective_height(),
                    self.mouse_x,
                    self.mouse_y,
                );
                if !edge.is_none() {
                    self.compositor_cursor_override = edge.cursor_shape();
                    return;
                }
            }

            // If inside this window, stop searching (z-order priority)
            let frame_y = w.y - TITLE_BAR_HEIGHT;
            if self.mouse_x >= w.x
                && self.mouse_x < w.x + w.effective_width() as i32
                && self.mouse_y >= frame_y
                && self.mouse_y < w.y + w.effective_height() as i32
            {
                // Title bar hover → open hand (grab) for non-maximized windows
                if w.state != 2
                    && decorations::hit_test_title_bar(
                        w.x,
                        frame_y,
                        w.effective_width(),
                        self.mouse_x,
                        self.mouse_y,
                    )
                {
                    self.compositor_cursor_override = CURSOR_SHAPE_GRAB;
                }
                return;
            }
        }
    }

    fn request_window_close(
        &mut self,
        task_id: u32,
        _windows: &[UserWindowInfo; MAX_WINDOWS],
        _window_count: u32,
        proto: &mut Option<&mut ProtocolBridge>,
    ) {
        if let Some(idx) = self.pending_close_index(task_id) {
            // Grace period expired on a second close click — just remove from
            // pending list.  Protocol cleanup handles surface destruction.
            self.remove_pending_close_at(idx);
            self.needs_full_redraw = true;
            return;
        }

        // Send close event via protocol; if the surface exists, the client
        // gets a chance to handle the close gracefully.
        let close_sent = if let Some(p) = proto.as_deref_mut() {
            p.send_close_for_task(task_id)
        } else {
            false
        };

        if !close_sent || self.pending_close_count >= MAX_WINDOWS {
            // Could not send close event or pending list full — nothing more
            // we can do.  Protocol cleanup will destroy the surface on
            // disconnect.
            self.needs_full_redraw = true;
            return;
        }

        let now = self.now_ms();
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
        proto: &mut Option<&mut ProtocolBridge>,
    ) {
        let entry = match shelf.entry(idx) {
            Some(e) => e,
            None => return,
        };

        if entry.running && entry.task_id != 0 {
            // Check if the window is minimized; if so, unminimize it.
            for i in 0..window_count as usize {
                if windows[i].task_id == entry.task_id {
                    if let Some(p) = proto.as_deref_mut() {
                        if windows[i].state == WINDOW_STATE_MINIMIZED {
                            p.set_window_state(entry.task_id, WINDOW_STATE_NORMAL);
                        }
                        p.raise_window(entry.task_id);
                    }
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

    /// Spawn a program from the shelf. Focus is NOT set here: the spawn
    /// syscall returns a KERNEL task id, while window focus is keyed by
    /// the protocol bridge's surface pseudo-ids (surface index + 1) — the
    /// two id spaces never match. The new window's focus is acquired by
    /// `sync_keyboard_focus`'s newly-appeared edge once the client maps
    /// its surface, with the correct pseudo-id.
    fn spawn_program(&mut self, path: &str) {
        let tid = if let Some(spec) = program_registry::resolve_program_path(path) {
            process::spawn_path_with_attrs(spec.path.as_bytes(), spec.priority, spec.flags)
        } else {
            // Fall back to direct path spawn with default attrs.
            process::spawn_path_with_attrs(path.as_bytes(), TaskPriority::Normal, 0)
        };
        if tid <= 0 {
            tty::write(b"COMPOSITOR: spawn failed for program\n");
        }
    }
}

fn window_exists(windows: &[UserWindowInfo; MAX_WINDOWS], count: u32, task_id: u32) -> bool {
    (0..count as usize).any(|i| windows[i].task_id == task_id)
}
