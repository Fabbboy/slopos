pub mod decorations;
pub mod dock;
mod hover;
mod input;
pub mod menu_bar;
mod output;
pub mod protocol;
mod renderer;
mod surface_cache;

use crate::gfx::{DamageRect, DamageTracker};
use crate::syscall::{DisplayInfo, UserWindowInfo, core as sys_core, tty, window};
use crate::theme::*;
use std::thread;
use std::time::{Duration, Instant};

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

    /// Protocol bridge for AF_UNIX socket-based clients (None if bind failed).
    protocol: Option<Box<ProtocolBridge>>,

    /// Monotonic serial counter for protocol input events (Wayland convention).
    protocol_serial: u32,
    /// Task ID of the protocol surface that last had pointer focus (for enter/leave).
    protocol_pointer_focus: u32,

    first_frame: bool,
    output_damage: DamageTracker,
    /// Unfulfilled damage from a failed fb_flip — carried forward until
    /// successfully presented (Wayland buffer-age pattern).
    pending_damage: DamageTracker,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    prev_uptime_secs: u64,
    prev_cursor_shape: u8,
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
            protocol,
            protocol_serial: 0,
            protocol_pointer_focus: 0,
            first_frame: true,
            output_damage: DamageTracker::new(),
            pending_damage: DamageTracker::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            prev_uptime_secs: u64::MAX, // force first-frame clock damage
            prev_cursor_shape: 0,
        }
    }

    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;
        let saved_bounds = self.prev_window_bounds;

        // Populate window list from ProtocolBridge local state.
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

        // Sync running apps into the launcher shelf.
        self.shelf
            .sync_running_apps(&self.windows, self.window_count);

        self.output_damage.clear();

        // Carry forward unfulfilled damage from a previous failed flip.
        if self.pending_damage.is_dirty() {
            for rect in self.pending_damage.regions() {
                self.output_damage
                    .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
            }
            if self.pending_damage.is_full_damage() {
                self.output_damage.set_full_damage();
            }
            self.pending_damage.clear();
        }

        for i in 0..self.window_count as usize {
            let window = self.windows[i];
            let curr_bounds = WindowBounds::from_window(&window);

            let prev_bounds = self.find_prev_bounds_in(&saved_bounds, window.task_id);

            if let Some(old) = prev_bounds {
                if old.x != curr_bounds.x
                    || old.y != curr_bounds.y
                    || old.width != curr_bounds.width
                    || old.height != curr_bounds.height
                    || old.visible != curr_bounds.visible
                {
                    self.add_bounds_damage(&old);
                    self.add_bounds_damage(&curr_bounds);
                }
            } else if curr_bounds.visible {
                self.input.needs_full_redraw = true;
            }

            self.prev_window_bounds[i] = curr_bounds;

            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            if window.is_dirty() {
                self.add_window_damage(&window);
            }
        }

        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.task_id) {
                self.add_bounds_damage(&saved_bounds[i]);
            }
        }

        // Add shelf bounds to damage only when its visual output changes:
        // cursor moved (magnification/hover may change) or content changed
        // (app opened/closed). Following the Mutter/wlroots pattern where
        // panels produce zero damage when idle.
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

    fn find_prev_bounds_in(
        &self,
        bounds: &[WindowBounds; MAX_WINDOWS],
        task_id: u32,
    ) -> Option<WindowBounds> {
        for i in 0..self.prev_window_count as usize {
            if self.prev_windows[i].task_id == task_id {
                return Some(bounds[i]);
            }
        }
        None
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
        self.first_frame || self.input.needs_full_redraw || self.output_damage.is_dirty()
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

    let mut output = match CompositorOutput::new(&fb_info) {
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
    let mut readiness = crate::readiness::ReadinessNotifier::acquire();

    const TARGET_FRAME_MS: u64 = 16;
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new();
    let time_origin = Instant::now();

    loop {
        let frame_start = Instant::now();

        if let Some(ref mut proto) = wm.protocol {
            proto.accept_clients();
            proto.process_requests();
        }

        // Signal readiness after the first accept pass so any
        // connections queued during init are already handled.
        if let Some(n) = readiness.take() {
            n.signal_ready();
        }

        wm.input.update_from_raw_events();
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
        wm.input.update_pointer_focus(&wm.windows, wm.window_count);
        wm.input
            .process_pending_close_requests(&wm.windows, wm.window_count);
        // Shelf height for maximize: pill + bottom margin + running dots
        let shelf_h = SHELF_ICON_SIZE
            + 2 * SHELF_PILL_PADDING_Y
            + SHELF_BOTTOM_MARGIN
            + SHELF_DOT_DIAMETER
            + SHELF_DOT_MARGIN_Y;
        // Temporarily take the protocol bridge out so we can pass it mutably
        // into InputHandler methods without borrowing all of `wm`.
        let mut proto_box = wm.protocol.take();
        let proto_ref = proto_box.as_deref_mut();
        wm.input.handle_mouse_events(
            fb_info.width as i32,
            fb_info.height as i32,
            shelf_h,
            &wm.windows,
            wm.window_count,
            &wm.shelf,
            proto_ref,
        );
        wm.protocol = proto_box;

        // Update resize cursor hover feedback (must run after handle_mouse_events).
        wm.input.update_resize_cursor(&wm.windows, wm.window_count);

        // Forward raw input events to protocol clients.
        if let Some(ref mut proto) = wm.protocol {
            // Determine pointer focus for protocol surfaces: find the topmost
            // protocol surface under the cursor.
            let mut new_ptr_focus: u32 = 0;
            for i in (0..wm.window_count as usize).rev() {
                let w = wm.windows[i];
                if w.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }
                if wm.input.hit_test_content_area(&w) {
                    new_ptr_focus = w.task_id;
                    break;
                }
            }

            // Send pointer enter/leave events on focus transitions.
            if new_ptr_focus != wm.protocol_pointer_focus {
                if wm.protocol_pointer_focus != 0 {
                    wm.protocol_serial = wm.protocol_serial.wrapping_add(1);
                    proto
                        .send_pointer_leave_for_task(wm.protocol_pointer_focus, wm.protocol_serial);
                }
                if new_ptr_focus != 0 {
                    wm.protocol_serial = wm.protocol_serial.wrapping_add(1);
                    proto.send_pointer_enter_for_task(
                        new_ptr_focus,
                        wm.protocol_serial,
                        wm.input.mouse_x,
                        wm.input.mouse_y,
                    );
                }
                wm.protocol_pointer_focus = new_ptr_focus;
            }

            // Forward raw events to focused protocol surfaces.
            let raw_events = wm.input.raw_events();
            if !raw_events.is_empty() {
                let kbd_focus = wm.input.focused_task();
                let mods = wm.input.modifier_state();
                proto.forward_input_events(
                    raw_events,
                    new_ptr_focus,
                    kbd_focus,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    mods,
                    &mut wm.protocol_serial,
                );
            }
        }

        // Compute effective cursor shape early so we can detect shape changes
        // and add damage before the render decision.
        let cursor_shape = if wm.input.compositor_cursor_override != 0 {
            wm.input.compositor_cursor_override
        } else {
            let mut shape = 0u8;
            for i in (0..wm.window_count as usize).rev() {
                if wm.windows[i].state == WINDOW_STATE_MINIMIZED {
                    continue;
                }
                if wm.input.hit_test_content_area(&wm.windows[i]) {
                    shape = wm.windows[i].cursor_shape;
                    break;
                }
            }
            shape
        };

        // When cursor shape changes, damage the cursor position so the old
        // shape gets erased and the new one gets drawn.
        if cursor_shape != wm.prev_cursor_shape {
            wm.add_cursor_damage_at(wm.input.mouse_x, wm.input.mouse_y);
            wm.prev_cursor_shape = cursor_shape;
        }

        // After all input: if focus moved, damage both the old and new
        // title bars.  No flags to manage — just compare the snapshot.
        let focus_after = wm.input.focused_task();
        if focus_after != focus_before {
            wm.add_title_bar_damage_for_task(focus_before);
            wm.add_title_bar_damage_for_task(focus_after);
        }

        // Compute uptime for the system bar clock.
        let uptime_secs = time_origin.elapsed().as_secs();

        // Add system bar clock damage each second.
        if uptime_secs != wm.prev_uptime_secs {
            wm.prev_uptime_secs = uptime_secs;
            if let Some(clock_rect) = wm.system_bar.clock_damage(output.width) {
                wm.output_damage.add_rect(
                    clock_rect.x0,
                    clock_rect.y0,
                    clock_rect.x1,
                    clock_rect.y1,
                );
            }
        }

        if wm.needs_redraw() {
            let force_full =
                wm.first_frame || wm.input.needs_full_redraw || wm.output_damage.is_full_damage();

            let mut mode = RenderMode::Full;
            let mut damage_snapshot = [DamageRect::invalid(); 8];
            let mut damage_count = 0usize;

            if !force_full {
                for rect in wm.output_damage.regions() {
                    if damage_count >= damage_snapshot.len() {
                        break;
                    }
                    damage_snapshot[damage_count] = *rect;
                    damage_count += 1;
                }
            }

            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);

                // Determine the active app name for the system bar.
                let active_app_name =
                    active_window_title(&wm.windows, wm.window_count, wm.input.focused_task());

                // Compute per-window signal group hover state.
                let signal_hovered_task = signal_hovered_task_id(
                    &wm.windows,
                    wm.window_count,
                    wm.input.focused_task(),
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                );

                mode = wm.renderer.render(
                    &mut buf,
                    &wm.windows,
                    wm.window_count as usize,
                    wm.input.focused_task(),
                    signal_hovered_task,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    cursor_shape,
                    &wm.hover_registry,
                    &mut wm.surface_cache,
                    &mut wm.system_bar,
                    &mut wm.shelf,
                    active_app_name,
                    uptime_secs,
                    force_full,
                    &damage_snapshot[..damage_count],
                );
            }

            let damage_slice = if mode == RenderMode::Partial {
                &damage_snapshot[..damage_count]
            } else {
                &[]
            };

            let flip_result = output.present(damage_slice);
            if frame_count < 3 {
                if flip_result {
                    tty::write(b"COMPOSITOR: fb_flip ok\n");
                } else {
                    tty::write(b"COMPOSITOR: fb_flip FAILED\n");
                }
            }
            frame_count = frame_count.saturating_add(1);
            if flip_result {
                let present_time = time_origin.elapsed().as_millis() as u64;

                // Send frame_done events to protocol clients.
                if let Some(ref mut proto) = wm.protocol {
                    proto.mark_frames_done(present_time);
                    proto.clear_dirty();
                }
            } else {
                // Flip failed — save damage for retry on next frame.
                // The back buffer is correct; the framebuffer is stale.
                if mode == RenderMode::Full {
                    wm.pending_damage.set_full_damage();
                } else {
                    for rect in &damage_snapshot[..damage_count] {
                        wm.pending_damage
                            .add_rect(rect.x0, rect.y0, rect.x1, rect.y1);
                    }
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
            metrics.record(mode, copied, frame_time, TARGET_FRAME_MS, flip_result);

            wm.input.needs_full_redraw = false;
            wm.first_frame = false;
        }

        // Accept connections + flush/cleanup BEFORE the frame-timing poll.
        // This is critical: after signal_ready(), the shell may connect during
        // the first frame's rendering. Without this, the connection sits in
        // the backlog until the NEXT loop iteration's accept_clients() at the
        // top — which may be > 10s if the first frame is slow (desktop render).
        if let Some(ref mut proto) = wm.protocol {
            proto.accept_clients();
            proto.cleanup_disconnected();
            proto.flush_all();
        }

        let frame_elapsed = frame_start.elapsed();
        let target_frame = Duration::from_millis(TARGET_FRAME_MS);
        if frame_elapsed < target_frame {
            let remaining_ms = (target_frame - frame_elapsed).as_millis() as i64;

            // Instead of sleeping, poll for protocol activity.
            // This wakes the compositor when client data arrives,
            // matching the Wayland event-driven dispatch pattern.
            if let Some(ref mut proto) = wm.protocol {
                let mut poll_fds = [slopos_abi::syscall::types::UserPollFd::default(); 33];
                let poll_count = proto.server_poll_fds(&mut poll_fds);

                if poll_count > 0 {
                    let result =
                        crate::syscall::fs::poll(&mut poll_fds[..poll_count], remaining_ms);

                    // If data arrived, process requests immediately
                    if result.unwrap_or(0) > 0 {
                        proto.accept_clients();
                        proto.process_requests();
                    }
                }
            } else {
                thread::sleep(target_frame - frame_elapsed);
            }
        }
    }
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

/// Return the task_id of the focused window whose signal group is hovered,
/// or 0 if no signal group is hovered.
fn signal_hovered_task_id(
    windows: &[UserWindowInfo; MAX_WINDOWS],
    count: u32,
    focused_task: u32,
    mx: i32,
    my: i32,
) -> u32 {
    if focused_task == 0 {
        return 0;
    }
    for i in (0..count as usize).rev() {
        let w = &windows[i];
        if w.task_id == focused_task && w.state != WINDOW_STATE_MINIMIZED {
            let frame_y = w.y - TITLE_BAR_HEIGHT;
            if decorations::hit_test_signal_group(w.x, frame_y, mx, my) {
                return w.task_id;
            }
            break;
        }
    }
    0
}
