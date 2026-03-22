mod hover;
mod input;
mod output;
mod renderer;
mod surface_cache;
mod taskbar;

use crate::gfx::{DamageRect, DamageTracker};
use crate::syscall::{
    DisplayInfo, UserWindowInfo, core as sys_core, input as sys_input, tty, window,
};
use crate::theme::*;
use std::thread;
use std::time::{Duration, Instant};

use hover::{
    HOVER_APP_BTN_BASE, HOVER_CLOSE_BASE, HOVER_MENU_ITEM_BASE, HOVER_MINIMIZE_BASE,
    HOVER_START_BTN, HoverRegistry,
};
use input::InputHandler;
use output::{
    CompositorOutput, FrameMetrics, RenderMode, WINDOW_STATE_MINIMIZED, WindowBounds,
    estimate_present_bytes,
};
use renderer::Renderer;
use surface_cache::ClientSurfaceCache;
use taskbar::{START_MENU_ITEMS, TaskbarState};

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

    first_frame: bool,
    prev_taskbar_state: TaskbarState,
    taskbar_needs_redraw: bool,
    output_damage: DamageTracker,
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
}

impl WindowManager {
    fn new() -> Self {
        Self {
            windows: [UserWindowInfo::default(); MAX_WINDOWS],
            window_count: 0,
            prev_windows: [UserWindowInfo::default(); MAX_WINDOWS],
            prev_window_count: 0,
            input: InputHandler::new(),
            renderer: Renderer::new(),
            hover_registry: HoverRegistry::new(),
            surface_cache: ClientSurfaceCache::new(),
            first_frame: true,
            prev_taskbar_state: TaskbarState::empty(),
            taskbar_needs_redraw: true,
            output_damage: DamageTracker::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
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

        let new_state = TaskbarState::from_windows(
            &self.windows,
            self.window_count,
            self.input.focused_task,
            self.input.start_menu_open,
        );
        if new_state != self.prev_taskbar_state {
            self.taskbar_needs_redraw = true;
            self.prev_taskbar_state = new_state;
        }

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

        if self.taskbar_needs_redraw {
            self.add_taskbar_damage();
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
        let fb_h = self.renderer.output_height as i32;

        let btn_x = taskbar::start_button_x();
        let btn_y = taskbar::start_button_y(fb_h);
        let btn_h = taskbar::start_button_height();
        self.hover_registry.register(
            HOVER_START_BTN,
            DamageRect {
                x0: btn_x,
                y0: btn_y,
                x1: btn_x + START_BUTTON_WIDTH - 1,
                y1: btn_y + btn_h - 1,
            },
            self.input.hit_test_start_button(fb_h),
        );

        if self.input.start_menu_open {
            let menu_y = taskbar::start_menu_y(fb_h) + START_MENU_PADDING;
            let menu_x = taskbar::start_menu_x();
            let hovered_item = self.input.hit_test_start_menu_item(fb_h);
            for idx in 0..START_MENU_ITEMS.len() {
                let item_y = menu_y + (idx as i32 * START_MENU_ITEM_HEIGHT);
                let hovered = hovered_item == Some(idx);
                self.hover_registry.register(
                    HOVER_MENU_ITEM_BASE | idx as u32,
                    DamageRect {
                        x0: menu_x + START_MENU_PADDING,
                        y0: item_y,
                        x1: menu_x + START_MENU_WIDTH - START_MENU_PADDING - 1,
                        y1: item_y + START_MENU_ITEM_HEIGHT - 1,
                    },
                    hovered,
                );
            }
        }

        let mut app_x = taskbar::app_buttons_start_x();
        let taskbar_y = fb_h - TASKBAR_HEIGHT;
        let app_btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
        let app_btn_h = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);
        for i in 0..self.window_count as usize {
            let w = self.windows[i];
            let hovered = self.input.mouse_x >= app_x
                && self.input.mouse_x < app_x + TASKBAR_BUTTON_WIDTH
                && self.input.mouse_y >= app_btn_y
                && self.input.mouse_y < app_btn_y + app_btn_h;
            self.hover_registry.register(
                HOVER_APP_BTN_BASE | w.task_id,
                DamageRect {
                    x0: app_x,
                    y0: app_btn_y,
                    x1: app_x + TASKBAR_BUTTON_WIDTH - 1,
                    y1: app_btn_y + app_btn_h - 1,
                },
                hovered,
            );
            app_x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }

        let mut deco_hit_consumed = false;
        for i in (0..self.window_count as usize).rev() {
            let w = self.windows[i];
            if w.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let close_x = w.x + w.width as i32 - BUTTON_SIZE - BUTTON_PADDING;
            let close_y = w.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
            let min_x = w.x + w.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
            let min_y = w.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;

            let on_title_bar = !deco_hit_consumed && self.input.hit_test_title_bar(&w);
            let close_hover = on_title_bar && self.input.hit_test_close_button(&w);
            let min_hover = on_title_bar && self.input.hit_test_minimize_button(&w);

            if on_title_bar {
                deco_hit_consumed = true;
            }

            self.hover_registry.register(
                HOVER_CLOSE_BASE | w.task_id,
                DamageRect {
                    x0: close_x,
                    y0: close_y,
                    x1: close_x + BUTTON_SIZE - 1,
                    y1: close_y + BUTTON_SIZE - 1,
                },
                close_hover,
            );
            self.hover_registry.register(
                HOVER_MINIMIZE_BASE | w.task_id,
                DamageRect {
                    x0: min_x,
                    y0: min_y,
                    x1: min_x + BUTTON_SIZE - 1,
                    y1: min_y + BUTTON_SIZE - 1,
                },
                min_hover,
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

    fn add_taskbar_damage(&mut self) {
        if self.renderer.output_width == 0 || self.renderer.output_height == 0 {
            return;
        }
        let fb_height = self.renderer.output_height as i32;
        self.output_damage.add_rect(
            0,
            fb_height - TASKBAR_HEIGHT,
            self.renderer.output_width as i32 - 1,
            fb_height - 1,
        );
        if self.input.start_menu_open {
            self.add_start_menu_damage();
        }
    }

    fn add_start_menu_damage(&mut self) {
        if self.renderer.output_width == 0 || self.renderer.output_height == 0 {
            return;
        }
        let fb_height = self.renderer.output_height as i32;
        let menu_h = taskbar::start_menu_height();
        self.output_damage.add_rect(
            taskbar::start_menu_x(),
            taskbar::start_menu_y(fb_height),
            taskbar::start_menu_x() + START_MENU_WIDTH - 1,
            taskbar::start_menu_y(fb_height) + menu_h - 1,
        );
    }

    fn add_cursor_damage_at(&mut self, x: i32, y: i32) {
        self.output_damage.add_rect(x - 4, y - 8, x + 4, y + 8);
    }

    fn needs_redraw(&self) -> bool {
        self.first_frame
            || self.input.needs_full_redraw
            || self.taskbar_needs_redraw
            || self.output_damage.is_dirty()
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
        wm.input.sync_keyboard_focus(
            &wm.windows,
            wm.window_count,
            &wm.prev_windows,
            wm.prev_window_count,
        );
        wm.input.update_pointer_focus(&wm.windows, wm.window_count);
        wm.input
            .process_pending_close_requests(&wm.windows, wm.window_count);
        wm.input
            .handle_mouse_events(fb_info.height as i32, &wm.windows, wm.window_count);

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

                mode = wm.renderer.render(
                    &mut buf,
                    &wm.windows,
                    wm.window_count as usize,
                    wm.input.focused_task,
                    wm.input.start_menu_open,
                    wm.input.mouse_x,
                    wm.input.mouse_y,
                    cursor_shape,
                    &wm.hover_registry,
                    &mut wm.surface_cache,
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
            wm.taskbar_needs_redraw = false;
        }

        let frame_elapsed = frame_start.elapsed();
        let target_frame = Duration::from_millis(TARGET_FRAME_MS);
        if frame_elapsed < target_frame {
            thread::sleep(target_frame - frame_elapsed);
        }
    }
}
