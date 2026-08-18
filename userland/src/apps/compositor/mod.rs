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

        // Register signal button group hover for each visible window.
        // We track the group bounding box for the focused window so the
        // renderer can show/hide button glyphs on hover.
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

        // Status items, from the same layout the bar draws from.
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

    /// The index this `task_id` held in the previous frame's z-ordered window
    /// list, or `None` if it was not present. The index doubles as the window's
    /// previous stacking rank (bottom = 0), so the caller can detect restacks.
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

    /// Dispatch the frame's raw input events IN STREAM ORDER.
    ///
    /// This is the wlroots/Wayland event model: each event is processed at
    /// the input state accumulated up to that event. Folding the batch into
    /// final state first (the old model) hit-tested button presses at the
    /// end-of-batch pointer position — when the press and its subsequent
    /// drag motion arrived within one 16 ms frame batch, the press was
    /// tested at post-drag coordinates and landed on the wrong target
    /// (observed live: a resize-corner grab becoming a content/desktop
    /// click, the latter leaving keyboard focus pointing at nothing).
    fn process_input_events(&mut self, fb_width: i32, fb_height: i32, shelf_height: i32) {
        use slopos_abi::InputEventType;

        // Take the protocol bridge out so it can be passed mutably into
        // InputHandler methods without borrowing all of `self`.
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
                    // Adopt the kernel's authoritative per-event modifier snapshot.
                    let mods = event.key_modifiers();
                    self.input.set_modifier_state(mods);
                    // Route to the keyboard focus AS OF THIS EVENT: a click
                    // earlier in the batch already retargeted focused_task.
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

        // Final per-frame sync: pointer focus can also change without any
        // motion event (a window moved/appeared/vanished under a static
        // cursor), so recompute once per frame after window-state changes.
        self.sync_pointer_focus(proto_box.as_deref_mut());

        self.protocol = proto_box;
    }

    /// Recompute pointer focus and emit protocol enter/leave events on
    /// transitions. A client holds the pointer only while the cursor is over
    /// its content; decorations and the desktop belong to the compositor, so a
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
                // Serial 0 is the "never entered" sentinel the cursor gate
                // rejects, so an enter must never carry it.
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
            // Find this window's pre-input z-rank (array index = stacking rank).
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
                // A raise/lower happens during input processing, so a restack is
                // only visible here, in the post-input re-fetch: a window's
                // pre-input rank `j` differs from its post-input rank `i`. Every
                // window whose rank shifted (the raised one and any it passed)
                // gets its bounds damaged, repainting the full overlap region.
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

    /// Add damage for a window's title bar + shadow area by task_id.
    /// Used for targeted focus-change damage instead of full-screen redraw.
    fn add_title_bar_damage_for_task(&mut self, task_id: u32) {
        if task_id == 0 {
            return;
        }
        for i in 0..self.window_count as usize {
            if self.windows[i].task_id == task_id {
                let w = &self.windows[i];
                let frame_y = w.y - TITLE_BAR_HEIGHT;
                // Damage the title bar area including shadow overlap.
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

    /// Run one frame: gather input, refresh window state, render+present if
    /// dirty, emit frame_done events, then reap disconnected clients and
    /// flush per-client write buffers. The accept path and frame pacing live
    /// in the async driver; this is the synchronous per-frame work the timer
    /// arm invokes. Logic is identical to the legacy per-frame body.
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
        // Snapshot focus *before* any input processing so we can detect
        // changes from any source (sync, mouse click, shelf, spawn) in a
        // single comparison afterwards.  This is order-independent and
        // mirrors the Mutter/KWin pattern where focus transitions always
        // trigger targeted title-bar redraws.
        let focus_before = wm.input.focused_task();

        wm.input.sync_keyboard_focus(
            &wm.windows,
            wm.window_count,
            &wm.prev_windows,
            wm.prev_window_count,
        );
        wm.input
            .process_pending_close_requests(&wm.windows, wm.window_count);
        // Shelf height for maximize: pill + bottom margin + running dots
        let shelf_h = SHELF_ICON_SIZE
            + 2 * SHELF_PILL_PADDING_Y
            + SHELF_BOTTOM_MARGIN
            + SHELF_DOT_DIAMETER
            + SHELF_DOT_MARGIN_Y;

        // Dispatch the frame's input events in stream order: every button
        // press is hit-tested at the pointer position accumulated up to
        // that event, and every key is routed to the keyboard focus as of
        // that moment (a click earlier in the same batch retargets the
        // keys that follow it). Also forwards each event to the protocol
        // clients with in-order coordinates.
        wm.process_input_events(fb_info.width as i32, fb_info.height as i32, shelf_h);

        // Cursor trail + shelf + hover damage — needs the trail the input
        // dispatch just accumulated.
        wm.add_pointer_damage();

        // Resolve what the pointer is over once; the cursor shape and the
        // signal-button hover both read this single hit, against this window
        // snapshot, before the post-input resync can reorder the array.
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

        // When cursor shape changes, damage the cursor position so the old
        // shape gets erased and the new one gets drawn.
        if cursor_shape != wm.prev_cursor_shape {
            wm.add_cursor_damage_at(wm.input.mouse_x, wm.input.mouse_y);
            wm.prev_cursor_shape = cursor_shape;
        }

        // Hardware cursor (virtio-gpu overlay): upload the image on shape
        // change and reposition it when the pointer moves. When active, the
        // renderer omits the software cursor; without a virtio-gpu device the
        // overlay calls are no-ops and the software cursor is composited.
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

        // After all input: if focus moved, damage both the old and new
        // title bars.  No flags to manage — just compare the snapshot.
        let focus_after = wm.input.focused_task();
        if focus_after != focus_before {
            wm.add_title_bar_damage_for_task(focus_before);
            wm.add_title_bar_damage_for_task(focus_after);
            // The bar names the focused application, and the title-bar damage
            // above covers the windows only — nothing else in the frame
            // touches the bar strip, so the name would stay stale until some
            // unrelated change repainted `y < 24`.
            let name_rect = wm.system_bar.app_name_damage(output.width);
            wm.output_damage
                .add_rect(name_rect.x0, name_rect.y0, name_rect.x1, name_rect.y1);
        }

        wm.resync_windows_post_input();

        // Fallback path only: with a `net_monitor` fd armed, refreshes are
        // driven by the kernel telling us something changed, and this timer
        // never runs. It exists for the case where the monitor could not be
        // opened at all.
        if !wm.net_event_driven
            && wm
                .net_last_poll
                .is_none_or(|last| last.elapsed().as_millis() >= NET_POLL_INTERVAL_MS)
        {
            wm.net_last_poll = Some(Instant::now());
            wm.refresh_from_kernel();
        }

        // Apply the press the input dispatch recorded. Done here rather than
        // in the dispatch because that path is handed an immutable view of the
        // chrome; this is the same shape `process_pending_close_requests` uses.
        if let Some(request) = wm.input.take_chrome_request() {
            let screen_w = output.width;
            let screen_h = output.height;
            match request {
                input::ChromeRequest::PopoverToggle(kind) => {
                    if let Some(item) = wm.system_bar.item_rect(screen_w, kind) {
                        // The model must be read out first: `toggle` sizes the
                        // panel from it, and `wm` is borrowed mutably below.
                        let model = wm.net_cache;
                        wm.popover.toggle(kind, item, &model, screen_w, screen_h);
                    }
                }
                input::ChromeRequest::PopoverDismiss => wm.popover.dismiss(),
                // A press the panel does not claim is still swallowed by the
                // grab rather than falling through to a window.
                input::ChromeRequest::PopoverPress { x, y } => {
                    let _ = wm.popover.press(x, y, &wm.net_cache);
                }
            }
        }
        // Settle or abandon an outstanding switch request before the panel is
        // measured, so a request that timed out this frame reverts in the same
        // frame it expires rather than one later.
        wm.popover.settle(&wm.net_cache);
        wm.popover
            .fit_to(&wm.net_cache, output.width, output.height);

        // Whatever the popover covered and now covers. Emitted only when the
        // two differ, so an open popover over an idle network is free.
        let mut popover_damage = [DamageRect::invalid(); 2];
        let popover_damage_count = wm.popover.take_damage(&mut popover_damage);
        for rect in &popover_damage[..popover_damage_count] {
            wm.output_damage
                .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
        }

        // System-bar damage: the clock ticking over, an indicator changing
        // state, or the items moving. A frame where none of those happened
        // reports nothing.
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

            // The precise, disjoint region to repaint this frame.
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

            // A coalesced SUPERSET of the painted region for the kernel flip
            // (never fewer pixels than were repainted), so the back-buffer →
            // scanout copy always covers everything that changed.
            let (damage_arr, damage_n) = frame_damage.to_bounded::<MAX_OUTPUT_DAMAGE_REGIONS>();

            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);

                // Determine the active app name for the system bar.
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

            // A full redraw presents the whole buffer (empty damage = full flip).
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

                // Only surfaces whose committed content actually reached the
                // screen this frame are cleared; a commit that landed after the
                // snapshot keeps its dirty flag and is exported next frame.
                let presented = core::mem::take(&mut wm.frame_dirty_surfaces);
                if let Some(ref mut proto) = wm.protocol {
                    proto.mark_frames_done(present_time);
                    proto.clear_presented(&presented);
                }
            } else {
                // Present never reached the screen — carry the damage forward so
                // it is repainted and re-flushed next frame. Surfaces stay dirty
                // (not cleared) so their content is not lost.
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

        // What the last window of frames cost. Deltas, so a steady desktop
        // reads as `frames=0 bytes=0` and a regression in damage shows up as a
        // number rather than as a feeling about the frame rate.
        if let Some((frames, bytes)) =
            metrics.take_window(time_origin.elapsed().as_millis() as u64, METRICS_REPORT_MS)
        {
            let mut line = std::string::String::new();
            use core::fmt::Write;
            let _ = write!(line, "COMPOSITOR: frames={frames} bytes={bytes}\n");
            tty::write(line.as_bytes());
        }

        // Parse + reap + flush once per frame.
        //
        // `process_requests()` here is LOAD-BEARING, not an optimisation:
        // `cleanup_disconnected`'s close-probe does a non-blocking recv that
        // pulls any in-flight bytes off the client socket into the Server's
        // read buffer. That read empties the kernel-side socket, so the
        // per-client poll-readiness stream never fires again for those
        // bytes — without an every-frame parse they rot in the buffer
        // forever. Observed live as a terminal whose SurfaceCommit (sent
        // between its poll task's drain and the probe) was never processed:
        // surface attached but never visible, window never appeared. The
        // legacy sync loop parsed every client every frame for exactly this
        // reason; the async per-client tasks only add latency-reduction on
        // top of this guarantee.
        if let Some(ref mut proto) = wm.protocol {
            proto.process_requests();
            proto.cleanup_disconnected();
            proto.flush_all();
        }

        // Dock and shelf launches are fire-and-forget — `spawn_program` keeps
        // no tid — and the compositor outlives every one of them, so it is
        // the reaper of record for each. Non-blocking, so a frame with no
        // exited child costs one syscall that returns immediately.
        process::reap_exited_children();
    }
}

pub fn compositor_user_main() {
    tty::write(b"COMPOSITOR: starting\n");
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

    // Tell the protocol bridge the display dimensions so it can send
    // OutputInfo to new clients on accept.
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

    // Drive the compositor as an async root on a single ring. `block_on`
    // never nests here: `compositor_user_main` is the top-level process entry,
    // not invoked from another async context. The ring is sized for the peak
    // armed-row count: one accept-readiness stream + one per connected client
    // (MAX_CLIENTS) + the frame timer ⇒ 64 entries is comfortable headroom.
    let ring = Ring::setup(64).expect("COMPOSITOR: ring setup failed");
    slopfut::block_on(
        ring,
        compositor_async(wm, output, fb_info, pixel_format, readiness),
    );
}

const TARGET_FRAME_MS: u64 = 16;

/// How often the compositor asks the kernel about the network **when it has no
/// monitor fd**. Two seconds is slow enough that the syscall is free at 60 Hz
/// and fast enough that a cable pulled out is reflected before anyone looks
/// twice at the bar. With a monitor fd this never runs.
const NET_POLL_INTERVAL_MS: u128 = 2000;

/// How often the compositor reports what its frames cost.
///
/// `FrameMetrics` is otherwise write-only: `record` accumulates and nothing
/// reads it back, so "did this change make the compositor busier" is not a
/// question anyone can answer from a boot log. Five seconds is long enough that
/// the line is rare and short enough that a capture of ordinary length contains
/// several.
///
/// Off unless `SLOPOS_COMPOSITOR_METRICS` is set, because the report goes to the
/// TTY: on by default it writes over whatever the user is doing, every five
/// seconds, forever.
const METRICS_REPORT_MS: u64 = 5000;

/// Whether to print the periodic damage report. Read once — the environment
/// does not change under a running compositor, and this is consulted every
/// frame.
fn metrics_reporting_enabled() -> bool {
    std::env::var("SLOPOS_COMPOSITOR_METRICS").is_ok_and(|v| v != "0")
}

/// Maximum rect count handed to the kernel flip per frame. The precise damage
/// region is coalesced down to this many bounding rects (a superset of what was
/// painted) so the back-buffer → scanout copy stays bounded while still covering
/// every changed pixel. MUST match the kernel's per-flip damage capacity: the
/// kernel drops rects beyond it, which would leave painted pixels un-scanned-out.
const MAX_OUTPUT_DAMAGE_REGIONS: usize = slopos_abi::damage::MAX_DAMAGE_REGIONS;

/// Maximum new connections drained from the listen backlog in one accept wake.
const ACCEPT_BATCH: usize = MAX_CLIENTS;

/// Async compositor driver (tokio-style accept-loop + task-per-client + a
/// frame-timer arm, all on the single-threaded `!Send` executor — shared
/// state via `Rc<RefCell<…>>` is sound because borrows are short and never
/// held across an `.await`).
///
/// - The frame-timer arm (this root future) owns the framebuffer locals and
///   ticks the render/commit cadence every [`TARGET_FRAME_MS`].
/// - The accept task arms `poll_add_multishot(listen_fd, POLLIN)`; each yield
///   drains the backlog via the existing sync accept path and spawns a
///   per-client task. Listen-socket readiness — not the synchronous ring
///   `accept_multishot` — is used so the existing `Server::accept()` stays the
///   sole client-slot allocator (slop-protocol unchanged); fds are never
///   accepted out from under it.
/// - Each per-client task arms `poll_add_multishot(client_fd, POLLIN)` and on
///   each yield drains that client's requests with the EXISTING sync
///   `Server::recv_request` (via `ProtocolBridge::process_client`). On
///   disconnect it exits, dropping its stream → `OP_CANCEL` retires the armed
///   row, so connect→disconnect→reconnect never leaks rows in the 64-slot ring.
///
/// Accept-before-frame ordering is preserved structurally: the accept task is
/// woken by listen-socket readiness independently of the frame timer, so a new
/// client is accepted and serviced as soon as it connects, never gated on a
/// full frame.
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

    // Initial accept pass + request drain BEFORE signalling readiness, exactly
    // as the legacy top-of-loop did: connections queued during init are
    // greeted and serviced, and a per-client task is spawned for each, before
    // init is told the compositor is up.
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

    // Signal readiness now that the first accept pass is done (boot-critical:
    // init blocks on this byte before launching the shell).
    if let Some(n) = readiness.take() {
        n.signal_ready();
    }

    // Network indicator: driven by a monitor fd when one can be opened, and by
    // the per-frame timer when one cannot.
    {
        let event_driven = spawn_net_monitor_task(wm.clone());
        wm.borrow_mut().net_event_driven = event_driven;
    }

    // Accept task: a multishot listen-readiness stream. Each yield drains the
    // full backlog and spawns a per-client task per new connection. Runs
    // concurrently with the frame timer, so newly-connected clients are
    // serviced without waiting for a frame.
    //
    // The stream is re-armed if it ever ends: a multishot row can die on a
    // transient error (arm failure or an error-terminal CQE), and the listen
    // socket outlives any such transient. Treating stream-end as permanent
    // silently killed ALL future accepts — clients connected into the
    // backlog and waited forever for a greeting (observed live: terminal
    // window never appeared while the compositor kept rendering).
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
                // Pace the re-arm so a persistently failing arm cannot
                // become a hot loop; the per-frame sweep below covers the
                // gap meanwhile.
                slopfut::time::sleep_ms(50).await;
            }
        });
    }

    // Frame-timer arm: tick the render/commit cadence. The full per-frame
    // work (input, refresh, render, present, frame_done, cleanup, flush) runs
    // synchronously under a short `borrow_mut` that is dropped before the next
    // timer await — so per-client/accept tasks observe no overlapping borrow.
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new(metrics_reporting_enabled());
    let time_origin = Instant::now();
    loop {
        let frame_start = Instant::now();

        // Defensive accept sweep (belt-and-braces over the async accept
        // task): a non-blocking accept per frame guarantees a connection
        // queued in the backlog is greeted within one frame even if the
        // accept-readiness stream is between re-arms. `Server::accept()`
        // stays the sole slot allocator, so the two paths cannot
        // double-accept a connection.
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

        // Deadline pacing: sleep only the remainder of this frame's budget so the
        // cadence stays ~TARGET_FRAME_MS regardless of how long the frame's work
        // took. A frame that overran its budget yields once and runs the next
        // immediately.
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
/// The fd is used purely as a change *notification*: every wake re-reads the
/// whole model through `net_query` rather than applying the event's payload as
/// a delta. That is what makes a dropped event harmless — the in-band
/// `NET_EV_OVERFLOW` record wakes us like any other, and the re-query is
/// authoritative regardless of what was missed. Applying deltas would need the
/// `seq`-based handoff the ABI documents, to no benefit for a model this small.
///
/// The monitor is opened, then the first query issued, in that order: an
/// interface that appears between the two shows up as an event rather than
/// being missed by both.
///
/// Returns whether the fd was opened. A failure leaves the caller on the
/// timer.
fn spawn_net_monitor_task(wm: std::rc::Rc<std::cell::RefCell<WindowManager>>) -> bool {
    let fd = match crate::syscall::net::net_monitor(NET_MON_DEFAULT, 0) {
        Ok(fd) => fd,
        Err(_) => {
            tty::write(b"COMPOSITOR: net_monitor unavailable; polling instead\n");
            return false;
        }
    };
    // The task outlives this frame, so the descriptor is handed to it rather
    // than closed here; it lives until the compositor exits.
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
            // A multishot row can die on a transient error; the fd outlives
            // it, so re-arm rather than treating stream-end as permanent.
            //
            // Logged rather than silent: there is no polling backstop, and a
            // monitor that dies and quietly resurrects looks exactly like one
            // that never died. A healthy stream never ends, so the line costs
            // nothing.
            tty::write(b"COMPOSITOR: net_monitor stream ended; re-arming\n");
            // Paced so a persistently failing arm cannot become a hot loop.
            slopfut::time::sleep_ms(500).await;
        }
    });
    true
}

/// Spawn a per-client task that services `idx` (socket `fd`, connection
/// generation `generation`) until it disconnects. Loops over
/// `poll_add_multishot(fd, POLLIN)` yields, draining the client's requests with
/// the existing sync recv path on each readiness. Exits — dropping its stream →
/// `OP_CANCEL` — when the client disconnects (recv reports it, or the poll
/// terminates on `POLLHUP`/`POLLERR`).
fn spawn_client_task(
    wm: std::rc::Rc<std::cell::RefCell<WindowManager>>,
    idx: usize,
    fd: i32,
    generation: u64,
) {
    if fd < 0 {
        return;
    }
    // Identity = (fd, generation). The slot index *and* the fd number are both
    // recycled by the kernel/Server across disconnect→reconnect, so a successor
    // client can inherit our exact slot+fd; only the monotonic generation
    // distinguishes it. A stale task whose `(fd, generation)` no longer matches
    // exits without touching the successor.
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
                    // Terminal CQE (POLLHUP/POLLERR / cancel). The kernel
                    // coalesces POLLHUP with a final POLLIN on peer close, and
                    // the multishot terminal CQE surfaces as `None` *without*
                    // yielding that last data edge — so drain once before
                    // teardown to recover a trailing request burst the client
                    // sent immediately before closing (the legacy sync loop
                    // drained every client each frame before cleanup).
                    let mut w = wm.borrow_mut();
                    if let Some(ref mut proto) = w.protocol {
                        if owns(proto) {
                            // `process_client` drains to EOF and, on the
                            // Disconnected path, already runs the teardown
                            // funnel and returns false — so only disconnect
                            // here if it drained without seeing the close.
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

/// Read the whole network model out of `net_query`.
///
/// Three queries, joined on `ifindex`: the interface table, the addresses, and
/// the one global record. The resolver is not among them — nothing the bar or
/// the panel draws reads the model's nameserver list, so it stays empty.
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
        // The first address on the interface. The indicator asks only whether
        // there is one; which one it picks matters to the panel, and the panel
        // reads the full list from `net_cache` rather than this summary.
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

/// Return the title of the focused window as a `&str`, or `""` if none.
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
