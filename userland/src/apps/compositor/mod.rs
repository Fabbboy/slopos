pub mod decorations;
pub mod dock;
mod hover;
mod input;
pub mod menu_bar;
mod net_glyph;
mod output;
mod popover;
pub mod protocol;
mod region;
mod renderer;
mod status_item;
mod surface_cache;

use crate::gfx::DamageRect;
use crate::net_query;
use crate::ring::{Ring, slopfut};
use crate::syscall::{DisplayInfo, UserWindowInfo, core as sys_core, process, tty, window};
use crate::theme::*;
use region::Region;
use slopos_abi::net::{
    NET_EVENT_LEN, NET_IFINDEX_NONE, NET_MON_DEFAULT, NET_Q_ADDRS, NET_Q_GLOBAL, NET_Q_IFACES,
    UserAddr, UserIface, UserNetGlobal,
};
use slopos_abi::syscall::POLLIN;
use slopos_chrome_core::status::MAX_STATUS_ITEMS;
use slopos_chrome_core::{
    IfaceKind, IfaceRow, NetIndicatorState, NetPanelModel, indicator_state_for,
};
use slopos_protocol::server::MAX_CLIENTS;
use std::time::Instant;
use std::vec::Vec;

use hover::HoverRegistry;
use input::InputHandler;
use output::{
    CompositorOutput, FrameMetrics, RenderMode, WINDOW_STATE_MINIMIZED, WindowBounds,
    estimate_present_bytes,
};
use protocol::ProtocolBridge;
use renderer::Renderer;
use surface_cache::ClientSurfaceCache;

const MAX_WINDOWS: usize = 32;

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
    prev_windows: [UserWindowInfo; MAX_WINDOWS],
    prev_window_count: u32,

    input: InputHandler,
    renderer: Renderer,
    hover_registry: HoverRegistry,
    surface_cache: ClientSurfaceCache,

    system_bar: menu_bar::SystemBar,
    shelf: dock::LauncherShelf,
    /// Compositor-owned chrome; holds a pointer grab while open.
    popover: popover::Popover,

    /// `None` if the AF_UNIX bind failed.
    protocol: Option<Box<ProtocolBridge>>,

    protocol_serial: u32,
    protocol_pointer_focus: u32,

    first_frame: bool,
    /// This frame's accumulated, disjoint damage region.
    output_damage: Region,
    /// Tracked separately from `output_damage` so that region stays a precise
    /// rect set.
    force_full_redraw: bool,
    /// Damage from a present that did not reach the screen, carried forward
    /// until actually shown.
    pending_damage: Region,
    /// A suppressed/failed *full* present to retry next frame.
    pending_full: bool,
    /// Surfaces (by `task_id`) whose committed damage was folded into this
    /// frame. Only these are cleared after a present that reached the screen —
    /// a commit landing after the snapshot stays dirty and exports next frame.
    frame_dirty_surfaces: Vec<u32>,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    prev_cursor_shape: u8,
    net_cache: NetPanelModel,
    net_indicator: NetIndicatorState,
    /// Only consulted on the fallback path — see [`WindowManager::net_event_driven`].
    net_last_poll: Option<Instant>,
    /// Whether a `net_monitor` fd is driving refreshes. When it is, the poll
    /// timer is switched off entirely rather than kept as a backstop, so a
    /// monitor that stopped firing is noticed instead of masked.
    net_event_driven: bool,
    /// `None` = not yet probed, `Some(true)` = the virtio-gpu overlay owns the
    /// pointer (software cursor suppressed), `Some(false)` = unavailable.
    hw_cursor: Option<bool>,
    /// Cursor shape last uploaded to the hardware overlay (`-1` = none).
    hw_cursor_shape: i32,
    /// Last position sent to the hardware cursor (`i32::MIN` = never), so the
    /// blocking move is only issued when the pointer actually moved.
    hw_cursor_last_x: i32,
    hw_cursor_last_y: i32,
}

impl WindowManager {
    fn new() -> Self {
        let mut shelf = dock::LauncherShelf::new();
        shelf.init_defaults();
        let protocol = ProtocolBridge::new().map(Box::new);
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [UserWindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
            input: InputHandler::new(),
            renderer: Renderer::new(),
            hover_registry: HoverRegistry::new(),
            surface_cache: ClientSurfaceCache::new(),
            system_bar: menu_bar::SystemBar::new(),
            shelf,
            popover: popover::Popover::new(),
            protocol,
            protocol_serial: 0,
            protocol_pointer_focus: 0,
            first_frame: true,
            output_damage: Region::new(),
            force_full_redraw: false,
            pending_damage: Region::new(),
            pending_full: false,
            frame_dirty_surfaces: Vec::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            prev_cursor_shape: 0,
            net_cache: NetPanelModel::EMPTY,
            net_indicator: NetIndicatorState::Disconnected,
            net_last_poll: None,
            net_event_driven: false,
            hw_cursor: None,
            hw_cursor_shape: -1,
            hw_cursor_last_x: i32::MIN,
            hw_cursor_last_y: i32::MIN,
        }
    }

    /// Drive the virtio-gpu hardware cursor: probe lazily, re-upload on shape
    /// change, and report whether the overlay owns the pointer (so the renderer
    /// skips compositing a software cursor into the frame).
    fn update_hw_cursor(&mut self, shape: u8) -> bool {
        match self.hw_cursor {
            Some(false) => return false,
            Some(true) if self.hw_cursor_shape == shape as i32 => return true,
            _ => {}
        }
        const N: usize = (renderer::HW_CURSOR_DIM * renderer::HW_CURSOR_DIM * 4) as usize;
        let mut pixels = [0u8; N];
        self.renderer.render_cursor_image(shape, &mut pixels);
        let ok = window::cursor_set_image(
            &pixels,
            renderer::HW_CURSOR_HOTSPOT,
            renderer::HW_CURSOR_HOTSPOT,
        ) == 0;
        if ok {
            self.hw_cursor = Some(true);
            self.hw_cursor_shape = shape as i32;
            return true;
        }
        // A failed re-upload after activation stays on hardware, keeping the
        // last good overlay shape, so overlay and software cursor never coexist.
        if self.hw_cursor.is_none() {
            self.hw_cursor = Some(false);
            false
        } else {
            true
        }
    }

    /// Rebuild the network model from the kernel and republish the indicator.
    ///
    /// A failed query leaves the previous model in place rather than falling
    /// back to `EMPTY`: a transient error is not evidence the network went away.
    fn refresh_from_kernel(&mut self) {
        let Some(model) = read_net_model() else {
            return;
        };
        self.net_indicator = indicator_state_for(&model);
        self.net_cache = model;
        self.system_bar.set_network(true, self.net_indicator);
    }

    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;
        let saved_bounds = self.prev_window_bounds;

        let raw_count = if let Some(ref proto) = self.protocol {
            proto.get_windows(&mut self.windows) as i64
        } else {
            0
        };
        self.window_count = if raw_count > 0 {
            (raw_count as usize).min(MAX_WINDOWS) as u32
        } else {
            0
        };

        self.surface_cache
            .cleanup_stale(&self.windows, self.window_count);

        self.shelf
            .sync_running_apps(&self.windows, self.window_count);

        self.output_damage.clear();
        self.frame_dirty_surfaces.clear();

        if self.pending_full {
            self.force_full_redraw = true;
            self.pending_full = false;
        }
        if !self.pending_damage.is_empty() {
            let pending = core::mem::take(&mut self.pending_damage);
            self.output_damage.push_region(&pending);
        }

        for i in 0..self.window_count as usize {
            let window = self.windows[i];
            let curr_bounds = WindowBounds::from_window(&window);

            let prev = self.find_prev_index(window.task_id);

            if let Some(prev_idx) = prev {
                let old = saved_bounds[prev_idx];
                let geometry_changed = old.x != curr_bounds.x
                    || old.y != curr_bounds.y
                    || old.width != curr_bounds.width
                    || old.height != curr_bounds.height
                    || old.visible != curr_bounds.visible;
                // A pure restack changes no geometry but changes occlusion, so
                // the covered/revealed area must be repainted.
                let restacked = prev_idx != i;
                if geometry_changed || restacked {
                    self.add_bounds_damage(&old);
                    self.add_bounds_damage(&curr_bounds);
                }
            } else if curr_bounds.visible {
                self.force_full_redraw = true;
            }

            self.prev_window_bounds[i] = curr_bounds;

            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            if window.is_dirty() {
                self.add_window_damage(&window);
                self.frame_dirty_surfaces.push(window.task_id);
            }
        }

        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.task_id) {
                self.add_bounds_damage(&saved_bounds[i]);
            }
        }
    }

    /// Cursor-trail / shelf / hover damage. Must run after the frame's input
    /// events are dispatched: `process_input_events` populates the cursor trail.
    fn add_pointer_damage(&mut self) {
        let cursor_moved = self.input.cursor_trail_count > 0;
        let shelf_content_changed = self.shelf.take_content_dirty();
        if cursor_moved || shelf_content_changed {
            let shelf_bounds = self.shelf.bounds();
            if shelf_bounds.is_valid() {
                self.output_damage.add_rect(
                    shelf_bounds.x0,
                    shelf_bounds.y0,
                    shelf_bounds.x1,
                    shelf_bounds.y1,
                );
            }
        }

        if self.input.cursor_trail_count > 0 {
            for i in 0..self.input.cursor_trail_count {
                let (x, y) = self.input.cursor_trail[i];
                self.add_cursor_damage_at(x, y);
            }
            self.add_cursor_damage_at(self.input.mouse_x, self.input.mouse_y);
        }

        self.register_hover_regions();
    }

    fn register_hover_regions(&mut self) {
        self.hover_registry.begin_frame();

        if self.renderer.output_height == 0 {
            return;
        }

        // The renderer shows/hides the button glyphs from this hover state.
        for i in (0..self.window_count as usize).rev() {
            let w = self.windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let frame_y = w.y - TITLE_BAR_HEIGHT;
            let group_hovered = decorations::hit_test_signal_group(
                w.x,
                frame_y,
                self.input.mouse_x,
                self.input.mouse_y,
            );

            let gx = w.x + SIGNAL_GROUP_X;
            let gy = frame_y + SIGNAL_GROUP_Y;
            self.hover_registry.register(
                hover::HOVER_SIGNAL_GROUP_BASE | w.task_id,
                DamageRect {
                    x0: gx,
                    y0: gy,
                    x1: gx + SIGNAL_GROUP_W - 1,
                    y1: gy + SIGNAL_GROUP_H - 1,
                },
                group_hovered,
            );
        }

        let mut status_regions = [(0u32, DamageRect::invalid(), false); MAX_STATUS_ITEMS];
        let status_count = self.system_bar.hover_regions(
            self.renderer.output_width,
            self.input.mouse_x,
            self.input.mouse_y,
            &mut status_regions,
        );
        for &(id, rect, hovered) in &status_regions[..status_count] {
            self.hover_registry.register(id, rect, hovered);
        }

        let mut hover_damage = [DamageRect::invalid(); 32];
        let hover_damage_count = self.hover_registry.changed_regions(&mut hover_damage);
        for i in 0..hover_damage_count {
            self.output_damage.add_rect(
                hover_damage[i].x0,
                hover_damage[i].y0,
                hover_damage[i].x1,
                hover_damage[i].y1,
            );
        }
    }

    /// The index this `task_id` held in the previous frame's window list. The
    /// index doubles as its stacking rank (bottom = 0), so callers detect
    /// restacks by comparing it against the current index.
    fn find_prev_index(&self, task_id: u32) -> Option<usize> {
        (0..self.prev_window_count as usize).find(|&i| self.prev_windows[i].task_id == task_id)
    }

    fn window_exists(&self, task_id: u32) -> bool {
        (0..self.window_count as usize).any(|i| self.windows[i].task_id == task_id)
    }

    fn add_bounds_damage(&mut self, bounds: &WindowBounds) {
        let rect = bounds.to_damage_rect();
        if rect.is_valid() {
            self.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }
    }

    /// Dispatch the frame's raw input events in stream order: each event is
    /// processed against the input state accumulated up to that event, so a
    /// press is hit-tested at its own pointer position rather than at the
    /// end-of-batch one.
    fn process_input_events(&mut self, fb_width: i32, fb_height: i32, shelf_height: i32) {
        use slopos_abi::InputEventType;

        // Taken out so it can be passed mutably into `InputHandler` methods
        // without borrowing all of `self`.
        let mut proto_box = self.protocol.take();

        for i in 0..self.input.raw_event_count {
            let event = self.input.raw_event_buf[i];
            let time = event.timestamp_ms as u32;
            match event.event_type {
                InputEventType::PointerMotion => {
                    self.input.apply_motion(&event);
                    self.input.apply_grab_motion(proto_box.as_deref_mut());
                    self.sync_pointer_focus(proto_box.as_deref_mut());
                    if self.protocol_pointer_focus != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_motion_for_task(
                                self.protocol_pointer_focus,
                                time,
                                self.input.mouse_x,
                                self.input.mouse_y,
                            );
                        }
                    }
                }
                InputEventType::PointerButtonPress => {
                    let button = event.data.data0 as u8;
                    let popover_rect = self.popover.rect();
                    let screen_w = self.renderer.output_width;
                    let should_forward = self.input.on_button_press(
                        button,
                        fb_width,
                        fb_height,
                        shelf_height,
                        &self.windows,
                        self.window_count,
                        &self.shelf,
                        popover_rect,
                        screen_w,
                        &self.system_bar,
                        proto_box.as_deref_mut(),
                    );
                    if should_forward && self.protocol_pointer_focus != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            self.protocol_serial = self.protocol_serial.wrapping_add(1);
                            p.send_pointer_button_for_task(
                                self.protocol_pointer_focus,
                                self.protocol_serial,
                                time,
                                event.data.data0,
                                1,
                            );
                        }
                    }
                }
                InputEventType::PointerButtonRelease => {
                    let button = event.data.data0 as u8;
                    let should_forward = self
                        .input
                        .on_button_release(button, proto_box.as_deref_mut());
                    if should_forward && self.protocol_pointer_focus != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            self.protocol_serial = self.protocol_serial.wrapping_add(1);
                            p.send_pointer_button_for_task(
                                self.protocol_pointer_focus,
                                self.protocol_serial,
                                time,
                                event.data.data0,
                                0,
                            );
                        }
                    }
                }
                InputEventType::PointerAxis => {
                    if self.protocol_pointer_focus != 0 {
                        if let Some(p) = proto_box.as_deref_mut() {
                            p.send_pointer_axis_for_task(
                                self.protocol_pointer_focus,
                                time,
                                event.axis_id(),
                                event.axis_value_v120(),
                            );
                        }
                    }
                }
                InputEventType::KeyPress | InputEventType::KeyRelease => {
                    let pressed = event.event_type == InputEventType::KeyPress;
                    let mods = event.key_modifiers();
                    self.input.set_modifier_state(mods);
                    let kbd_focus = self.input.focused_task();
                    if let Some(p) = proto_box.as_deref_mut() {
                        p.forward_key_event(
                            kbd_focus,
                            &event,
                            pressed,
                            mods,
                            &mut self.protocol_serial,
                        );
                    }
                }
                _ => {}
            }
        }

        // Pointer focus can change without a motion event — a window moved,
        // appeared or vanished under a static cursor.
        self.sync_pointer_focus(proto_box.as_deref_mut());

        self.protocol = proto_box;
    }

    /// Recompute pointer focus and emit protocol enter/leave events on
    /// transitions. Decorations and the desktop belong to the compositor, so a
    /// non-`Content` hit drops focus to none.
    fn sync_pointer_focus(&mut self, proto: Option<&mut ProtocolBridge>) {
        let hit = self.input.resolve_cursor_hit(
            &self.windows,
            self.window_count,
            &self.shelf,
            self.popover.rect(),
            self.renderer.output_width,
            &self.system_bar,
        );
        let new_ptr_focus = match hit.part {
            input::CursorPart::Content => hit.task_id,
            _ => 0,
        };
        if new_ptr_focus == self.protocol_pointer_focus {
            return;
        }
        if let Some(p) = proto {
            if self.protocol_pointer_focus != 0 {
                self.protocol_serial = self.protocol_serial.wrapping_add(1);
                p.send_pointer_leave_for_task(self.protocol_pointer_focus, self.protocol_serial);
            }
            if new_ptr_focus != 0 {
                // Serial 0 is the "never entered" sentinel the cursor gate rejects.
                self.protocol_serial = self.protocol_serial.wrapping_add(1).max(1);
                p.send_pointer_enter_for_task(
                    new_ptr_focus,
                    self.protocol_serial,
                    self.input.mouse_x,
                    self.input.mouse_y,
                );
            }
        }
        self.protocol_pointer_focus = new_ptr_focus;
    }

    fn resync_windows_post_input(&mut self) {
        let pre_count = self.window_count as usize;
        let mut pre_windows = [UserWindowInfo::default(); MAX_WINDOWS];
        let mut pre_bounds = [WindowBounds::default(); MAX_WINDOWS];
        for i in 0..pre_count {
            pre_windows[i] = self.windows[i];
            pre_bounds[i] = WindowBounds::from_window(&self.windows[i]);
        }

        let raw_count = if let Some(ref proto) = self.protocol {
            proto.get_windows(&mut self.windows) as i64
        } else {
            return;
        };
        self.window_count = if raw_count > 0 {
            (raw_count as usize).min(MAX_WINDOWS) as u32
        } else {
            0
        };

        for i in 0..self.window_count as usize {
            let curr_bounds = WindowBounds::from_window(&self.windows[i]);
            let task_id = self.windows[i].task_id;
            let prev_idx = pre_windows[..pre_count]
                .iter()
                .position(|w| w.task_id == task_id);
            if let Some(j) = prev_idx {
                let p = pre_bounds[j];
                let geometry_changed = p.x != curr_bounds.x
                    || p.y != curr_bounds.y
                    || p.width != curr_bounds.width
                    || p.height != curr_bounds.height
                    || p.visible != curr_bounds.visible;
                // A raise/lower happens during input processing, so a restack
                // is only visible here, in the post-input re-fetch.
                let restacked = j != i;
                if geometry_changed || restacked {
                    self.add_bounds_damage(&p);
                    self.add_bounds_damage(&curr_bounds);
                }
            }
            self.prev_window_bounds[i] = curr_bounds;
        }
    }

    fn add_window_damage(&mut self, window: &UserWindowInfo) {
        if window.damage_count == u8::MAX {
            let bounds = WindowBounds::from_window(window);
            self.add_bounds_damage(&bounds);
            return;
        }

        for i in 0..window.damage_count as usize {
            let region = &window.damage_regions[i];
            if region.is_valid() {
                self.output_damage.add_rect(
                    window.x + region.x0,
                    window.y + region.y0,
                    window.x + region.x1,
                    window.y + region.y1,
                );
            }
        }
    }

    fn add_cursor_damage_at(&mut self, x: i32, y: i32) {
        // Conservative rect covering ALL cursor shapes:
        // default arrow (12×17 at x,y) and resize arrows (up to 17×17 centered).
        self.output_damage.add_rect(x - 9, y - 9, x + 12, y + 17);
    }

    /// Targeted focus-change damage, in place of a full-screen redraw.
    fn add_title_bar_damage_for_task(&mut self, task_id: u32) {
        if task_id == 0 {
            return;
        }
        for i in 0..self.window_count as usize {
            if self.windows[i].task_id == task_id {
                let w = &self.windows[i];
                let frame_y = w.y - TITLE_BAR_HEIGHT;
                self.output_damage.add_rect(
                    w.x - SHADOW_SPREAD,
                    frame_y - SHADOW_SPREAD,
                    w.x + w.width as i32 - 1 + SHADOW_SPREAD,
                    w.y - 1,
                );
                return;
            }
        }
    }

    fn needs_redraw(&self) -> bool {
        self.first_frame
            || self.input.needs_full_redraw
            || self.force_full_redraw
            || !self.output_damage.is_empty()
    }

    /// The synchronous per-frame work the timer arm invokes: gather input,
    /// refresh window state, render+present if dirty, emit frame_done, reap
    /// disconnected clients and flush. Accept and pacing live in the driver.
    fn render_frame(
        &mut self,
        output: &mut CompositorOutput,
        fb_info: &DisplayInfo,
        pixel_format: slopos_abi::PixelFormat,
        frame_count: &mut u32,
        metrics: &mut FrameMetrics,
        time_origin: Instant,
        frame_start: Instant,
        target_frame_ms: u64,
    ) {
        let wm = self;
        wm.input.drain_events();
        wm.refresh_windows();
        // Snapshot before any input processing so a change from any source —
        // sync, click, shelf, spawn — is one comparison afterwards.
        let focus_before = wm.input.focused_task();

        wm.input.sync_keyboard_focus(
            &wm.windows,
            wm.window_count,
            &wm.prev_windows,
            wm.prev_window_count,
        );
        wm.input
            .process_pending_close_requests(&wm.windows, wm.window_count);
        let shelf_h = SHELF_ICON_SIZE
            + 2 * SHELF_PILL_PADDING_Y
            + SHELF_BOTTOM_MARGIN
            + SHELF_DOT_DIAMETER
            + SHELF_DOT_MARGIN_Y;

        wm.process_input_events(fb_info.width as i32, fb_info.height as i32, shelf_h);

        wm.add_pointer_damage();

        // Resolved once, against this window snapshot, before the post-input
        // resync can reorder the array.
        let cursor_hit = wm.input.resolve_cursor_hit(
            &wm.windows,
            wm.window_count,
            &wm.shelf,
            wm.popover.rect(),
            wm.renderer.output_width,
            &wm.system_bar,
        );
        let cursor_shape = wm.input.cursor_shape_for(&cursor_hit, &wm.windows);
        let signal_hovered_task =
            wm.input
                .signal_hovered_task(&cursor_hit, &wm.windows, wm.input.focused_task());

        if cursor_shape != wm.prev_cursor_shape {
            wm.add_cursor_damage_at(wm.input.mouse_x, wm.input.mouse_y);
            wm.prev_cursor_shape = cursor_shape;
        }

        let hw_cursor = wm.update_hw_cursor(cursor_shape);
        if hw_cursor
            && (wm.input.mouse_x != wm.hw_cursor_last_x || wm.input.mouse_y != wm.hw_cursor_last_y)
        {
            let _ = window::cursor_move(
                wm.input.mouse_x.max(0) as u32,
                wm.input.mouse_y.max(0) as u32,
            );
            wm.hw_cursor_last_x = wm.input.mouse_x;
            wm.hw_cursor_last_y = wm.input.mouse_y;
        }

        let focus_after = wm.input.focused_task();
        if focus_after != focus_before {
            wm.add_title_bar_damage_for_task(focus_before);
            wm.add_title_bar_damage_for_task(focus_after);
            // The bar names the focused application and nothing else in the
            // frame touches the bar strip, so its name would otherwise stay
            // stale until an unrelated change repainted it.
            let name_rect = wm.system_bar.app_name_damage(output.width);
            wm.output_damage
                .add_rect(name_rect.x0, name_rect.y0, name_rect.x1, name_rect.y1);
        }

        wm.resync_windows_post_input();

        if !wm.net_event_driven
            && wm
                .net_last_poll
                .is_none_or(|last| last.elapsed().as_millis() >= NET_POLL_INTERVAL_MS)
        {
            wm.net_last_poll = Some(Instant::now());
            wm.refresh_from_kernel();
        }

        // Not applied in the input dispatch: that path is handed an immutable
        // view of the chrome.
        if let Some(request) = wm.input.take_chrome_request() {
            let screen_w = output.width;
            let screen_h = output.height;
            match request {
                input::ChromeRequest::PopoverToggle(kind) => {
                    if let Some(item) = wm.system_bar.item_rect(screen_w, kind) {
                        // Read out first: `toggle` sizes the panel from it and
                        // borrows `wm` mutably.
                        let model = wm.net_cache;
                        wm.popover.toggle(kind, item, &model, screen_w, screen_h);
                    }
                }
                input::ChromeRequest::PopoverDismiss => wm.popover.dismiss(),
                // A press the panel does not claim is swallowed by the grab
                // rather than falling through to a window.
                input::ChromeRequest::PopoverPress { x, y } => {
                    let _ = wm.popover.press(x, y, &wm.net_cache);
                }
            }
        }
        // Before the panel is measured, so a switch request that timed out
        // this frame reverts in the same frame rather than one later.
        wm.popover.settle(&wm.net_cache);
        wm.popover
            .fit_to(&wm.net_cache, output.width, output.height);

        let mut popover_damage = [DamageRect::invalid(); 2];
        let popover_damage_count = wm.popover.take_damage(&mut popover_damage);
        for rect in &popover_damage[..popover_damage_count] {
            wm.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }

        let uptime_secs = time_origin.elapsed().as_secs();
        let mut bar_damage = [DamageRect::invalid(); menu_bar::MAX_BAR_DAMAGE];
        let bar_damage_count =
            wm.system_bar
                .take_damage(output.width, uptime_secs, &mut bar_damage);
        for rect in &bar_damage[..bar_damage_count] {
            wm.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }

        if wm.needs_redraw() {
            let force_full = wm.first_frame || wm.input.needs_full_redraw || wm.force_full_redraw;

            let frame_damage = if force_full {
                Region::full(output.width, output.height)
            } else {
                wm.output_damage.clone()
            };
            let mode = if force_full {
                RenderMode::Full
            } else {
                RenderMode::Partial
            };

            // A superset of the painted region: never fewer pixels than were
            // repainted, so the scanout copy covers everything that changed.
            let (damage_arr, damage_n) = frame_damage.to_bounded::<MAX_OUTPUT_DAMAGE_REGIONS>();

            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);

                let active_app_name =
                    active_window_title(&wm.windows, wm.window_count, wm.input.focused_task());

                wm.renderer.render(
                    &mut buf,
                    &wm.windows,
                    wm.window_count as usize,
                    wm.input.focused_task(),
                    signal_hovered_task,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    cursor_shape,
                    &mut wm.surface_cache,
                    &wm.system_bar,
                    &mut wm.shelf,
                    &mut wm.popover,
                    &wm.net_cache,
                    active_app_name,
                    &frame_damage,
                    hw_cursor,
                );
            }

            // Empty damage means a full flip.
            let damage_slice: &[DamageRect] = if force_full {
                &[]
            } else {
                &damage_arr[..damage_n]
            };

            let flip_result = output.present(damage_slice);
            if *frame_count < 3 {
                if flip_result {
                    tty::write(b"COMPOSITOR: fb_flip ok\n");
                } else {
                    tty::write(b"COMPOSITOR: fb_flip FAILED\n");
                }
            }
            *frame_count = frame_count.saturating_add(1);
            if flip_result {
                let present_time = time_origin.elapsed().as_millis() as u64;

                let presented = core::mem::take(&mut wm.frame_dirty_surfaces);
                if let Some(ref mut proto) = wm.protocol {
                    proto.mark_frames_done(present_time);
                    proto.clear_presented(&presented);
                }
            } else {
                if force_full {
                    wm.pending_full = true;
                } else {
                    wm.pending_damage.push_region(&frame_damage);
                }
            }

            let frame_time = frame_start.elapsed().as_millis() as u64;
            let copied = estimate_present_bytes(
                output.width,
                output.height,
                output.bytes_pp,
                output.pitch,
                mode,
                damage_slice,
            );
            metrics.record(mode, copied, frame_time, target_frame_ms, flip_result);

            wm.input.needs_full_redraw = false;
            wm.force_full_redraw = false;
            wm.first_frame = false;
        }

        if let Some((frames, bytes)) =
            metrics.take_window(time_origin.elapsed().as_millis() as u64, METRICS_REPORT_MS)
        {
            let mut line = std::string::String::new();
            use core::fmt::Write;
            let _ = write!(line, "COMPOSITOR: frames={frames} bytes={bytes}\n");
            tty::write(line.as_bytes());
        }

        // `process_requests()` is load-bearing, not an optimisation:
        // `cleanup_disconnected`'s close-probe recv empties the kernel-side
        // socket, so the per-client readiness stream never fires again for
        // bytes it pulled into the read buffer. Without an every-frame parse
        // those bytes are never handled.
        if let Some(ref mut proto) = wm.protocol {
            proto.process_requests();
            proto.cleanup_disconnected();
            proto.flush_all();
        }

        // Dock and shelf launches are fire-and-forget — `spawn_program` keeps
        // no tid — and the compositor outlives them, so it reaps them all.
        process::reap_exited_children();
    }
}

pub fn compositor_user_main() {
    tty::write(b"COMPOSITOR: starting\n");

    // Ownership of the screen and the input stream is announced here, once,
    // rather than re-asserted by every frame. Both fds are deliberately leaked
    // for the process lifetime: closing one would not release the seat anyway
    // (release is arbiter revocation at task exit), and holding them keeps the
    // descriptor's existence honest about what this process owns.
    if window::screen_acquire(window::SEAT_COMPOSITOR_PRIMARY) < 0 {
        tty::write(b"COMPOSITOR: screen seat unavailable\n");
    }
    if window::input_sink_acquire(window::SEAT_COMPOSITOR_PRIMARY) < 0 {
        tty::write(b"COMPOSITOR: input seat unavailable\n");
    }

    let mut wm = WindowManager::new();
    let mut fb_info = DisplayInfo::default();

    if window::fb_info(&mut fb_info) < 0 {
        tty::write(b"COMPOSITOR: fb_info failed\n");
        loop {
            sys_core::yield_now();
        }
    }
    tty::write(b"COMPOSITOR: fb_info ok\n");

    let output = match CompositorOutput::new(&fb_info) {
        Some(out) => out,
        None => {
            tty::write(b"COMPOSITOR: output alloc failed\n");
            loop {
                sys_core::yield_now();
            }
        }
    };
    tty::write(b"COMPOSITOR: output allocated\n");

    wm.renderer
        .set_output_info(output.width, output.height, output.bytes_pp, output.pitch);

    if let Some(ref mut proto) = wm.protocol {
        proto.set_display_info(
            output.width,
            output.height,
            fb_info.format as u32,
            output.pitch as u32,
        );
    }

    let pixel_format = fb_info.format;
    let readiness = crate::readiness::ReadinessNotifier::acquire();

    // Ring sized for the peak armed-row count: one accept-readiness stream,
    // one per connected client (MAX_CLIENTS), and the frame timer.
    let ring = Ring::setup(64).expect("COMPOSITOR: ring setup failed");
    slopfut::block_on(
        ring,
        compositor_async(wm, output, fb_info, pixel_format, readiness),
    );
}

const TARGET_FRAME_MS: u64 = 16;

/// Network poll cadence for the fallback path only; with a monitor fd this
/// never runs. Free at 60 Hz, and fast enough to reflect a pulled cable.
const NET_POLL_INTERVAL_MS: u128 = 2000;

/// Frame-cost report cadence. Off unless `SLOPOS_COMPOSITOR_METRICS` is set,
/// because the report goes to the TTY over whatever the user is doing.
const METRICS_REPORT_MS: u64 = 5000;

/// Whether to print the periodic damage report. Read once by the caller — the
/// environment does not change under a running compositor.
fn metrics_reporting_enabled() -> bool {
    std::env::var("SLOPOS_COMPOSITOR_METRICS").is_ok_and(|v| v != "0")
}

/// Maximum rect count handed to the kernel flip per frame. Must match the
/// kernel's per-flip damage capacity: it drops rects beyond that, which would
/// leave painted pixels un-scanned-out.
const MAX_OUTPUT_DAMAGE_REGIONS: usize = slopos_abi::damage::MAX_DAMAGE_REGIONS;

/// Maximum new connections drained from the listen backlog in one accept wake.
const ACCEPT_BATCH: usize = MAX_CLIENTS;

/// Async compositor driver: an accept task, a task per client, and this root
/// future as the frame-timer arm, all on the single-threaded `!Send` executor.
/// Sharing state through `Rc<RefCell<…>>` is sound because no borrow is held
/// across an `.await`.
///
/// The accept task waits on listen-socket readiness rather than the ring's
/// `accept_multishot`, so `Server::accept()` stays the sole client-slot
/// allocator and fds are never accepted out from under it. It is woken
/// independently of the frame timer, so a new client is never gated on a frame.
async fn compositor_async(
    wm: WindowManager,
    mut output: CompositorOutput,
    fb_info: DisplayInfo,
    pixel_format: slopos_abi::PixelFormat,
    mut readiness: Option<crate::readiness::ReadinessNotifier>,
) {
    use std::cell::RefCell;
    use std::rc::Rc;

    let wm = Rc::new(RefCell::new(wm));

    // Accept pass and request drain before signalling readiness, so a
    // connection queued during init is greeted before init is told the
    // compositor is up.
    let listen_fd = {
        let mut w = wm.borrow_mut();
        let listen_fd = w.protocol.as_ref().map(|p| p.listen_fd());
        if let Some(ref mut proto) = w.protocol {
            let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
            let n = proto.accept_and_collect(&mut accepted);
            proto.process_requests();
            drop(w);
            for &(idx, fd, generation) in &accepted[..n] {
                spawn_client_task(wm.clone(), idx, fd, generation);
            }
        } else {
            drop(w);
        }
        listen_fd
    };

    // Boot-critical: init blocks on this byte before launching the shell.
    if let Some(n) = readiness.take() {
        n.signal_ready();
    }

    {
        let event_driven = spawn_net_monitor_task(wm.clone());
        wm.borrow_mut().net_event_driven = event_driven;
    }

    // A multishot row can die on a transient error and the listen socket
    // outlives any such transient, so stream-end re-arms rather than ending
    // all future accepts.
    if let Some(listen_fd) = listen_fd {
        let wm_accept = wm.clone();
        slopfut::spawn(async move {
            loop {
                let mut stream = slopfut::poll_add_multishot(listen_fd, POLLIN);
                while stream.next().await.is_some() {
                    let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
                    let n = {
                        let mut w = wm_accept.borrow_mut();
                        match w.protocol {
                            Some(ref mut proto) => proto.accept_and_collect(&mut accepted),
                            None => 0,
                        }
                    };
                    for &(idx, fd, generation) in &accepted[..n] {
                        spawn_client_task(wm_accept.clone(), idx, fd, generation);
                    }
                }
                tty::write(b"COMPOSITOR: accept stream ended; re-arming\n");
                // Paced so a persistently failing arm cannot become a hot loop;
                // the per-frame sweep below covers the gap meanwhile.
                slopfut::time::sleep_ms(50).await;
            }
        });
    }

    // The per-frame work runs synchronously under a `borrow_mut` dropped
    // before the next timer await, so no other task sees an overlapping borrow.
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new(metrics_reporting_enabled());
    let time_origin = Instant::now();
    loop {
        let frame_start = Instant::now();

        // Covers the accept task while its readiness stream is between
        // re-arms. `Server::accept()` stays the sole slot allocator, so the
        // two paths cannot double-accept a connection.
        {
            let mut accepted = [(0usize, -1i32, 0u64); ACCEPT_BATCH];
            let n = {
                let mut w = wm.borrow_mut();
                match w.protocol {
                    Some(ref mut proto) => proto.accept_and_collect(&mut accepted),
                    None => 0,
                }
            };
            for &(idx, fd, generation) in &accepted[..n] {
                spawn_client_task(wm.clone(), idx, fd, generation);
            }
        }

        wm.borrow_mut().render_frame(
            &mut output,
            &fb_info,
            pixel_format,
            &mut frame_count,
            &mut metrics,
            time_origin,
            frame_start,
            TARGET_FRAME_MS,
        );

        let elapsed = frame_start.elapsed().as_millis() as u64;
        let remaining = TARGET_FRAME_MS.saturating_sub(elapsed);
        if remaining > 0 {
            slopfut::time::sleep_ms(remaining).await;
        } else {
            slopfut::yield_now().await;
        }
    }
}

/// Drive the network indicator from a `net_monitor` fd instead of a timer.
///
/// The fd is a change *notification* only: every wake re-reads the whole model
/// through `net_query` rather than applying the event payload as a delta, which
/// is what makes a dropped event harmless.
///
/// The monitor is opened before the first query, so an interface appearing
/// between the two shows up as an event rather than being missed by both.
///
/// Returns whether the fd was opened; a failure leaves the caller on the timer.
fn spawn_net_monitor_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) -> bool {
    let fd = match crate::syscall::net::net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => fd,
        Err(_) => {
            tty::write(b"COMPOSITOR: net_monitor unavailable; polling instead\n");
            return false;
        }
    };
    // Handed to the task rather than closed here; it lives until exit.
    let raw = fd.into_raw();

    wm.borrow_mut().refresh_from_kernel();

    slopfut::spawn(async move {
        let mut buf = [0u8; NET_EVENT_LEN * 16];
        loop {
            let mut stream = slopfut::poll_add_multishot(raw, POLLIN);
            while stream.next().await.is_some() {
                // Drain to empty, or POLLIN stays asserted and the stream
                // spins. An empty netmon read is `EAGAIN`, never a block.
                while crate::syscall::fs::read_slice(raw, &mut buf).unwrap_or(0) > 0 {}
                wm.borrow_mut().refresh_from_kernel();
            }
            // The fd outlives a multishot row that died on a transient error,
            // so re-arm. Logged because there is no polling backstop here.
            tty::write(b"COMPOSITOR: net_monitor stream ended; re-arming\n");
            slopfut::time::sleep_ms(500).await;
        }
    });
    true
}

/// Spawn a per-client task that drains `idx`'s requests on each readiness of
/// `fd` until it disconnects, then exits — dropping its stream, which
/// `OP_CANCEL`s the armed ring row.
fn spawn_client_task(
    wm: std::rc::Rc<std::cell::RefCell<WindowManager>>,
    idx: usize,
    fd: i32,
    generation: u64,
) {
    if fd < 0 {
        return;
    }
    // Both the slot index and the fd number are recycled across
    // disconnect→reconnect, so a successor client can inherit this exact
    // slot+fd; only the monotonic generation distinguishes it.
    let owns = move |proto: &ProtocolBridge| {
        proto.client_fd(idx) == Some(fd) && proto.client_gen(idx) == Some(generation)
    };
    slopfut::spawn(async move {
        let mut stream = slopfut::poll_add_multishot(fd, POLLIN);
        loop {
            match stream.next().await {
                Some(_revents) => {
                    let still_connected = {
                        let mut w = wm.borrow_mut();
                        match w.protocol {
                            Some(ref mut proto) if owns(proto) => proto.process_client(idx),
                            _ => false,
                        }
                    };
                    if !still_connected {
                        break;
                    }
                }
                None => {
                    // The kernel coalesces POLLHUP with a final POLLIN on peer
                    // close and the terminal CQE surfaces as `None` without
                    // yielding that data edge, so drain once before teardown
                    // to recover a trailing request burst.
                    let mut w = wm.borrow_mut();
                    if let Some(ref mut proto) = w.protocol {
                        if owns(proto) {
                            // A false return means the teardown funnel already
                            // ran, so only disconnect if it drained clean.
                            if proto.process_client(idx) {
                                proto.disconnect_client(idx);
                            }
                        }
                    }
                    break;
                }
            }
        }
        // `stream` drops here → its armed ring row is cancelled (`OP_CANCEL`).
    });
}

/// Read the whole network model out of `net_query`: interfaces, addresses and
/// the one global record, joined on `ifindex`. The resolver is not queried —
/// nothing the bar or panel draws reads the nameserver list, so it stays empty.
///
/// `None` if any query failed, which the caller reads as "keep what you had".
fn read_net_model() -> Option<NetPanelModel> {
    let ifaces = net_query::fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE).ok()?;
    let addrs = net_query::fetch::<UserAddr>(NET_Q_ADDRS, NET_IFINDEX_NONE).ok()?;
    let global = net_query::fetch::<UserNetGlobal>(NET_Q_GLOBAL, NET_IFINDEX_NONE).ok()?;
    let global = global.records.first()?;

    let mut model = NetPanelModel::EMPTY;
    model.enabled = global.enabled != 0;
    model.connectivity = global.connectivity;
    model.gateway = global.default_gateway;

    for iface in &ifaces.records {
        let mut row = IfaceRow::named(
            net_query::name_of(iface).as_bytes(),
            IfaceKind::from_abi(iface.kind),
        );
        row.admin_up = iface.admin_up != 0;
        row.carrier = iface.carrier != 0;
        row.oper = iface.oper_state;
        // Any address will do: the indicator asks only whether there is one,
        // and the panel reads the full list from `net_cache`, not this summary.
        if let Some(addr) = addrs
            .records
            .iter()
            .find(|addr| addr.ifindex == iface.ifindex)
        {
            row.ipv4 = addr.addr;
            row.prefix_len = addr.prefix_len;
        }
        model.push_iface(row);
    }
    Some(model)
}

fn active_window_title(
    windows: &[UserWindowInfo; MAX_WINDOWS],
    count: u32,
    focused_task: u32,
) -> &str {
    if focused_task == 0 {
        return "";
    }
    for i in 0..count as usize {
        if windows[i].task_id == focused_task {
            let title = &windows[i].title;
            let len = title.iter().position(|&b| b == 0).unwrap_or(32);
            return core::str::from_utf8(&title[..len]).unwrap_or("");
        }
    }
    ""
}
