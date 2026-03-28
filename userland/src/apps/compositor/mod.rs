pub mod decorations;
pub mod dock;
mod hover;
mod input;
pub mod menu_bar;
mod output;
mod renderer;
mod surface_cache;

use crate::gfx::{DamageRect, DamageTracker};
use crate::syscall::{
    DisplayInfo, UserWindowInfo, core as sys_core, input as sys_input, tty, window,
};
use crate::theme::*;
use std::thread;
use std::time::{Duration, Instant};

use hover::HoverRegistry;
use input::InputHandler;
use output::{
    CompositorOutput, FrameMetrics, RenderMode, WINDOW_STATE_MINIMIZED, WindowBounds,
    estimate_present_bytes,
};
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

    first_frame: bool,
    output_damage: DamageTracker,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    prev_uptime_secs: u64,
}

impl WindowManager {
    fn new() -> Self {
        let mut shelf = dock::LauncherShelf::new();
        shelf.init_defaults();
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
            first_frame: true,
            output_damage: DamageTracker::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            prev_uptime_secs: u64::MAX, // force first-frame clock damage
        }
    }

    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;
        let saved_bounds = self.prev_window_bounds;

        let raw_count = window::enumerate_windows(&mut self.windows);
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
        self.output_damage.add_rect(x - 3, y - 9, x + 12, y + 17);
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

    let pixel_format = fb_info.format;

    const TARGET_FRAME_MS: u64 = 16;
    let mut frame_count: u32 = 0;
    let mut metrics = FrameMetrics::new();
    let time_origin = Instant::now();

    loop {
        let frame_start = Instant::now();

        window::drain_queue();
        sys_input::drain_queue();

        wm.input.update_mouse();
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
        wm.input.handle_mouse_events(
            fb_info.height as i32,
            &wm.windows,
            wm.window_count,
            &wm.shelf,
        );

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

                let cursor_shape = {
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
                window::mark_frames_done(present_time);
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

        let frame_elapsed = frame_start.elapsed();
        let target_frame = Duration::from_millis(TARGET_FRAME_MS);
        if frame_elapsed < target_frame {
            thread::sleep(target_frame - frame_elapsed);
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
