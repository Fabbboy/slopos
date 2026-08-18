use slopos_abi::InputEvent;
use slopos_abi::input::MODIFIER_SUPER;
use slopos_abi::task::TaskPriority;
use slopos_chrome_core::Rect as ChromeRect;
use slopos_chrome_core::status::StatusKind;

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

/// What a press on compositor chrome asks the frame loop to do.
///
/// Input dispatch sees an immutable view of the chrome, so a press records its
/// intent here and `render_frame` applies it. At most one is outstanding: two
/// presses in one batch mean the second is what the person meant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChromeRequest {
    PopoverToggle(StatusKind),
    PopoverDismiss,
    /// A press inside the open popover, in screen coordinates.
    PopoverPress {
        x: i32,
        y: i32,
    },
}

const WINDOW_STATE_NORMAL: u8 = 0;
const CLOSE_REQUEST_GRACE_MS: u64 = 1500;
const MAX_CURSOR_TRAIL: usize = 16;

// Edge bits follow the Wayland convention: TOP=1, BOTTOM=2, LEFT=4, RIGHT=8.
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

/// What the pointer is over, resolved by a single top-of-z-order walk that
/// stops at the first hit. Cursor shape, pointer focus, hover feedback, and
/// click routing all derive from this one answer, so they always agree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorPart {
    /// The popover holds a pointer grab, so this outranks every window beneath.
    Popover,
    /// Light dismiss: the press is spent closing the popover and must not also
    /// reach whatever is under it.
    PopoverOutside,
    SystemBar(Option<StatusKind>),
    Shelf(usize),
    ResizeEdge(ResizeEdge),
    /// A window's signal button: 0 = close, 1 = minimize, 2 = expand.
    SignalButton(u8),
    /// A window's draggable title bar: the frame strip minus the buttons.
    TitleBar,
    Content,
    Desktop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CursorHit {
    pub part: CursorPart,
    /// Index into the window array for window parts; `usize::MAX` otherwise.
    pub window_idx: usize,
    /// Task id of the hit window, or 0 for non-window parts.
    pub task_id: u32,
}

impl CursorHit {
    fn ui(part: CursorPart) -> Self {
        Self {
            part,
            window_idx: usize::MAX,
            task_id: 0,
        }
    }
    fn window(part: CursorPart, idx: usize, task_id: u32) -> Self {
        Self {
            part,
            window_idx: idx,
            task_id,
        }
    }
    pub fn is_window(self) -> bool {
        self.window_idx != usize::MAX
    }
}

pub struct InputHandler {
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub mouse_buttons: u8,

    chrome_request: Option<ChromeRequest>,

    pub dragging: bool,
    drag_task: u32,
    drag_offset_x: i32,
    drag_offset_y: i32,

    pub resizing: bool,
    resize_task: u32,
    resize_edges: ResizeEdge,
    resize_grab_x: i32,
    resize_grab_y: i32,
    resize_grab_w: u32,
    resize_grab_h: u32,
    resize_grab_mouse_x: i32,
    resize_grab_mouse_y: i32,
    resize_last_configure_ms: u64,

    /// Pre-maximize geometry for restore: (task_id, x, y, w, h).
    restore_geometry: [(u32, i32, i32, u32, u32); MAX_WINDOWS],

    /// Keyboard focus. Private so every change goes through `set_focused()`.
    focused_task: u32,
    pub needs_full_redraw: bool,

    pub cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    pub cursor_trail_count: usize,

    pending_close_tasks: [u32; MAX_WINDOWS],
    pending_close_deadlines: [u64; MAX_WINDOWS],
    pending_close_count: usize,
    clock_origin: Instant,

    local_modifier_state: u8,
    pub raw_event_buf: [InputEvent; 64],
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
            restore_geometry: [(0, 0, 0, 0, 0); MAX_WINDOWS],
            focused_task: 0,
            needs_full_redraw: false,
            chrome_request: None,
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

    pub fn focused_task(&self) -> u32 {
        self.focused_task
    }

    #[inline]
    fn now_ms(&self) -> u64 {
        self.clock_origin.elapsed().as_millis() as u64
    }

    /// Drain raw input events from the kernel queue into the frame buffer.
    ///
    /// State is not folded here: the main loop dispatches events in stream
    /// order, so each press is hit-tested at the pointer position accumulated
    /// up to that event, never at the end-of-batch position.
    pub fn drain_events(&mut self) {
        self.cursor_trail_count = 0;
        self.raw_event_count = input::poll_batch(&mut self.raw_event_buf);
    }

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

    /// Called per motion event while a grab is active.
    pub fn apply_grab_motion(&mut self, proto: Option<&mut ProtocolBridge>) {
        if self.dragging {
            self.update_drag(proto);
        } else if self.resizing {
            self.update_resize(proto);
        }
    }

    /// The kernel's `MODIFIER_*` snapshot is authoritative; this caches it.
    pub(super) fn set_modifier_state(&mut self, mods: u8) {
        self.local_modifier_state = mods;
    }

    pub fn sync_keyboard_focus(
        &mut self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        prev_windows: &[UserWindowInfo; MAX_WINDOWS],
        prev_window_count: u32,
    ) {
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

        // Topmost visible is last in the z-ordered array.
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
        // visible: any path that leaves focus at 0 would drop every
        // subsequent keystroke.
        if self.focused_task == 0 {
            for i in (0..window_count as usize).rev() {
                if windows[i].state != WINDOW_STATE_MINIMIZED {
                    self.set_focused(windows[i].task_id);
                    break;
                }
            }
        }
    }

    /// Ends any active grab at the position accumulated up to this event.
    pub fn on_button_release(&mut self, button: u8, proto: Option<&mut ProtocolBridge>) -> bool {
        self.mouse_buttons &= !button;
        if button & 0x01 == 0 {
            return true;
        }
        if self.dragging {
            self.stop_drag();
            return false;
        } else if self.resizing {
            self.stop_resize(proto);
            return false;
        }
        true
    }

    /// Hit-tests at the pointer position accumulated up to this event, routing
    /// from `resolve_cursor_hit` so a click lands on whatever the cursor is
    /// visually over. Returns `true` only for a plain content click, which the
    /// caller forwards to the client; every other part the compositor consumes.
    pub fn on_button_press(
        &mut self,
        button: u8,
        fb_width: i32,
        fb_height: i32,
        shelf_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        shelf: &LauncherShelf,
        popover: Option<ChromeRect>,
        screen_width: u32,
        bar: &SystemBar,
        mut proto: Option<&mut ProtocolBridge>,
    ) -> bool {
        self.mouse_buttons |= button;
        if button & 0x01 == 0 {
            return true;
        }

        // A press while a grab is active means the matching release was lost
        // (kernel queue overwritten under a motion flood), so end the stale
        // grab and process the press normally.
        if self.dragging {
            self.stop_drag();
        }
        if self.resizing {
            self.stop_resize(proto.as_deref_mut());
        }

        let hit = self.resolve_cursor_hit(windows, window_count, shelf, popover, screen_width, bar);
        match hit.part {
            // Coordinates travel with the request: the panel resolves the
            // widget under them from the same rect the renderer drew.
            CursorPart::Popover => {
                self.chrome_request = Some(ChromeRequest::PopoverPress {
                    x: self.mouse_x,
                    y: self.mouse_y,
                });
                false
            }
            CursorPart::PopoverOutside => {
                self.chrome_request = Some(ChromeRequest::PopoverDismiss);
                false
            }
            // The bar consumes every click; only a named item opens a popover.
            CursorPart::SystemBar(item) => {
                if let Some(kind) = item {
                    self.chrome_request = Some(ChromeRequest::PopoverToggle(kind));
                }
                false
            }
            CursorPart::Shelf(idx) => {
                self.handle_shelf_click(idx, shelf, windows, window_count, &mut proto);
                false
            }
            CursorPart::ResizeEdge(edge) => {
                let window = windows[hit.window_idx];
                self.start_resize(&window, edge);
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                false
            }
            CursorPart::SignalButton(btn) => {
                let window = windows[hit.window_idx];
                self.handle_signal_button(
                    btn,
                    &window,
                    fb_width,
                    fb_height,
                    shelf_height,
                    windows,
                    window_count,
                    &mut proto,
                );
                false
            }
            CursorPart::TitleBar => {
                let window = windows[hit.window_idx];
                self.start_drag(&window);
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                false
            }
            CursorPart::Content => {
                let window = windows[hit.window_idx];
                // Super+LMB on content moves a non-maximized window.
                if self.local_modifier_state & MODIFIER_SUPER != 0 && window.state != 2 {
                    self.start_drag(&window);
                    if let Some(ref mut p) = proto {
                        p.raise_window(window.task_id);
                    }
                    self.set_focused(window.task_id);
                    return false;
                }
                if let Some(ref mut p) = proto {
                    p.raise_window(window.task_id);
                }
                self.set_focused(window.task_id);
                true
            }
            // Focus stays on the last focused window so keystrokes survive.
            CursorPart::Desktop => false,
        }
    }

    /// `button_id`: 0 = close, 1 = minimize, 2 = expand (maximize/restore).
    #[allow(clippy::too_many_arguments)]
    fn handle_signal_button(
        &mut self,
        button_id: u8,
        window: &UserWindowInfo,
        fb_width: i32,
        fb_height: i32,
        shelf_height: i32,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        proto: &mut Option<&mut ProtocolBridge>,
    ) {
        match button_id {
            0 => {
                self.request_window_close(window.task_id, windows, window_count, proto);
            }
            1 => {
                if let Some(p) = proto.as_deref_mut() {
                    p.set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                }
            }
            2 => {
                const WINDOW_STATE_MAXIMIZED: u8 = 2;
                if window.state == WINDOW_STATE_MAXIMIZED {
                    if let Some(geo) = self.restore_geometry.iter().find(|g| g.0 == window.task_id)
                    {
                        if let Some(p) = proto.as_deref_mut() {
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
                    if let Some(p) = proto.as_deref_mut() {
                        p.set_window_state(window.task_id, WINDOW_STATE_NORMAL);
                    }
                } else {
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
                    // Fill the screen between the system bar and the shelf.
                    let max_y = SYSTEM_BAR_HEIGHT + TITLE_BAR_HEIGHT;
                    let max_w = fb_width as u32;
                    let max_h =
                        (fb_height - SYSTEM_BAR_HEIGHT - TITLE_BAR_HEIGHT - shelf_height) as u32;
                    if let Some(p) = proto.as_deref_mut() {
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
                // No kernel terminate_task: protocol cleanup destroys the
                // surface on disconnect.
                self.remove_pending_close_at(i);
                self.needs_full_redraw = true;
                continue;
            }

            i += 1;
        }
    }

    /// `window.y` is the content top; the title bar sits above it.
    pub fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.effective_width() as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.effective_height() as i32
    }

    /// Content area plus the decorations `TITLE_BAR_HEIGHT` above `window.y`,
    /// so a top window's whole frame occludes what sits beneath it.
    pub fn hit_test_frame_area(&self, window: &UserWindowInfo) -> bool {
        let frame_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.effective_width() as i32
            && self.mouse_y >= frame_y
            && self.mouse_y < window.y + window.effective_height() as i32
    }

    /// The single entry point for focus changes. Focus is tracked locally;
    /// `ProtocolBridge::forward_input_events` routes keys to the focused
    /// surface from the main loop.
    fn set_focused(&mut self, task_id: u32) {
        self.focused_task = task_id;
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

    fn update_drag(&mut self, proto: Option<&mut ProtocolBridge>) {
        let new_x = self.mouse_x - self.drag_offset_x;
        let new_y = self.mouse_y - self.drag_offset_y;
        if let Some(p) = proto {
            p.set_window_position(self.drag_task, new_x, new_y);
        }
    }

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

        // Clamping keeps the anchored corner fixed.
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

            // Throttled to ~100 ms so clients re-render during the drag rather
            // than only at the end.
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
    }

    pub fn take_chrome_request(&mut self) -> Option<ChromeRequest> {
        self.chrome_request.take()
    }

    /// The single source of truth for cursor shape, pointer focus, hover
    /// feedback and click routing. A window's whole frame — content and
    /// decorations — occludes everything beneath, so the walk never falls
    /// through to a lower window once inside a frame.
    pub fn resolve_cursor_hit(
        &self,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        window_count: u32,
        shelf: &LauncherShelf,
        popover: Option<ChromeRect>,
        screen_width: u32,
        bar: &SystemBar,
    ) -> CursorHit {
        let (mx, my) = (self.mouse_x, self.mouse_y);

        // An open popover holds a pointer grab, so nothing below is consulted.
        // The client underneath gets a correct `PointerLeave` because
        // `sync_pointer_focus` drops focus for any part that is not `Content`.
        if let Some(rect) = popover {
            if rect.contains(mx, my) {
                return CursorHit::ui(CursorPart::Popover);
            }
            return CursorHit::ui(CursorPart::PopoverOutside);
        }

        // Compositor chrome is drawn above all windows, so it wins here too.
        if SystemBar::covers(my) {
            return CursorHit::ui(CursorPart::SystemBar(bar.hit_test(screen_width, mx, my)));
        }
        if let Some(idx) = shelf.hit_test(mx, my) {
            return CursorHit::ui(CursorPart::Shelf(idx));
        }

        for i in (0..window_count as usize).rev() {
            let w = windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // The resize grab zone is the shadow around the frame; state 2 is
            // maximized, which has none.
            if w.state != 2 {
                let edge = decorations::hit_test_resize_edge(
                    w.x,
                    w.y,
                    w.effective_width(),
                    w.effective_height(),
                    mx,
                    my,
                );
                if !edge.is_none() {
                    return CursorHit::window(CursorPart::ResizeEdge(edge), i, w.task_id);
                }
            }

            let frame_y = w.y - TITLE_BAR_HEIGHT;

            // Signal buttons sit highest within the title bar.
            if let Some(btn) = decorations::hit_test_signal_button(w.x, frame_y, mx, my) {
                return CursorHit::window(CursorPart::SignalButton(btn), i, w.task_id);
            }
            if self.hit_test_content_area(&w) {
                return CursorHit::window(CursorPart::Content, i, w.task_id);
            }
            // Anything else inside the frame is the draggable title bar.
            if self.hit_test_frame_area(&w) {
                return CursorHit::window(CursorPart::TitleBar, i, w.task_id);
            }
        }

        CursorHit::ui(CursorPart::Desktop)
    }

    /// An active grab takes precedence over the resolved hit.
    pub fn cursor_shape_for(&self, hit: &CursorHit, windows: &[UserWindowInfo; MAX_WINDOWS]) -> u8 {
        use slopos_abi::window::*;
        if self.dragging {
            return CURSOR_SHAPE_GRABBING;
        }
        if self.resizing {
            return self.resize_edges.cursor_shape();
        }
        match hit.part {
            CursorPart::ResizeEdge(edge) => edge.cursor_shape(),
            CursorPart::TitleBar => CURSOR_SHAPE_GRAB,
            // The client owns the cursor only over its own content.
            CursorPart::Content => windows[hit.window_idx].cursor_shape,
            CursorPart::SignalButton(_)
            | CursorPart::SystemBar(_)
            | CursorPart::Shelf(_)
            | CursorPart::Popover
            | CursorPart::PopoverOutside
            | CursorPart::Desktop => CURSOR_SHAPE_DEFAULT,
        }
    }

    /// Task id whose signal-button cluster should reveal its glyphs. Gated by
    /// the resolved topmost hit so an occluded window never lights up.
    pub fn signal_hovered_task(
        &self,
        hit: &CursorHit,
        windows: &[UserWindowInfo; MAX_WINDOWS],
        focused_task: u32,
    ) -> u32 {
        if focused_task == 0 || !hit.is_window() || hit.task_id != focused_task {
            return 0;
        }
        let w = &windows[hit.window_idx];
        let frame_y = w.y - TITLE_BAR_HEIGHT;
        if decorations::hit_test_signal_group(w.x, frame_y, self.mouse_x, self.mouse_y) {
            hit.task_id
        } else {
            0
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
            // Second close click: protocol cleanup destroys the surface, so
            // dropping the pending entry is all that is left to do.
            self.remove_pending_close_at(idx);
            self.needs_full_redraw = true;
            return;
        }

        let close_sent = if let Some(p) = proto.as_deref_mut() {
            p.send_close_for_task(task_id)
        } else {
            false
        };

        if !close_sent || self.pending_close_count >= MAX_WINDOWS {
            // Nothing more to do: protocol cleanup destroys the surface on
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

        let path_len = entry.path_len.min(entry.program_path.len());
        let path_bytes = &entry.program_path[..path_len];
        if let Ok(path_str) = core::str::from_utf8(path_bytes) {
            self.spawn_program(path_str);
        }
    }

    /// Focus is not set here: spawn returns a kernel task id while focus is
    /// keyed by the protocol bridge's surface pseudo-ids, and the two id spaces
    /// never match. `sync_keyboard_focus` picks the window up once it maps.
    fn spawn_program(&mut self, path: &str) {
        let tid = if let Some(spec) = program_registry::resolve_program_path(path) {
            process::spawn_path_with_attrs(spec.path.as_bytes(), spec.priority, spec.flags)
        } else {
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
