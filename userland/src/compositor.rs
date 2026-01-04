//! SlopOS Compositor - Wayland-like userland compositor
//!
//! This compositor runs entirely in userland (Ring 3) and uses shared memory
//! buffers for all graphics operations. No kernel drawing calls - all rendering
//! is done with 100% safe Rust via the gfx library.
//!
//! Architecture:
//! - Compositor allocates an output buffer via shared memory
//! - Clients allocate surface buffers via shared memory (Phase 4)
//! - Compositor composites all windows to output buffer
//! - Compositor draws chrome (title bars, taskbar, cursor)
//! - Compositor presents output buffer via sys_fb_flip()

use core::ffi::{c_char, c_void};

use crate::gfx::{self, rgb, DrawBuffer, PixelFormat};
use crate::syscall::{
    sys_drain_queue, sys_enumerate_windows, sys_fb_flip, sys_fb_info, sys_get_time_ms,
    sys_mouse_read, sys_raise_window, sys_set_window_position, sys_set_window_state,
    sys_sleep_ms, sys_tty_set_focus, sys_yield, CachedShmMapping, ShmBuffer,
    UserFbInfo, UserMouseEvent, UserWindowInfo,
};

// UI Constants - Dark Roulette Theme
const TITLE_BAR_HEIGHT: i32 = 24;
const BUTTON_SIZE: i32 = 20;
const BUTTON_PADDING: i32 = 2;
const TASKBAR_HEIGHT: i32 = 32;
const TASKBAR_BUTTON_WIDTH: i32 = 120;
const TASKBAR_BUTTON_PADDING: i32 = 4;

// Colors matching the dark roulette aesthetic
const COLOR_TITLE_BAR: u32 = rgb(0x1E, 0x1E, 0x1E);
const COLOR_TITLE_BAR_FOCUSED: u32 = rgb(0x2D, 0x2D, 0x30);
const COLOR_BUTTON: u32 = rgb(0x3E, 0x3E, 0x42);
const COLOR_BUTTON_HOVER: u32 = rgb(0x50, 0x50, 0x52);
const COLOR_BUTTON_CLOSE_HOVER: u32 = rgb(0xE8, 0x11, 0x23);
const COLOR_TEXT: u32 = rgb(0xE0, 0xE0, 0xE0);
const COLOR_TASKBAR: u32 = rgb(0x25, 0x25, 0x26);
const COLOR_CURSOR: u32 = rgb(0xFF, 0xFF, 0xFF);
const COLOR_BACKGROUND: u32 = rgb(0x00, 0x11, 0x22);

// Window placeholder colors (until clients migrate to shared memory)
const COLOR_WINDOW_PLACEHOLDER: u32 = rgb(0x20, 0x20, 0x30);

const MAX_WINDOWS: usize = 32;

/// Cache entry for a mapped client surface
struct ClientSurfaceEntry {
    task_id: u32,
    token: u32,
    mapping: Option<CachedShmMapping>,
}

impl ClientSurfaceEntry {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            token: 0,
            mapping: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.task_id == 0 && self.mapping.is_none()
    }

    fn matches(&self, task_id: u32, token: u32) -> bool {
        self.task_id == task_id && self.token == token && self.mapping.is_some()
    }
}

/// Cache of mapped client surfaces (100% safe - no raw pointers)
struct ClientSurfaceCache {
    entries: [ClientSurfaceEntry; MAX_WINDOWS],
}

impl ClientSurfaceCache {
    fn new() -> Self {
        // Can't use const fn with Option initialization, so use Default-style init
        Self {
            entries: core::array::from_fn(|_| ClientSurfaceEntry::empty()),
        }
    }

    /// Get the index of a cached mapping for the given task/token, or create one.
    /// Returns the index into entries array, or None if mapping failed.
    fn get_or_create_index(
        &mut self,
        task_id: u32,
        token: u32,
        buffer_size: usize,
    ) -> Option<usize> {
        if token == 0 {
            return None;
        }

        // Check if we already have this mapping
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.matches(task_id, token) {
                return Some(i);
            }
        }

        // Need to create a new mapping
        let mapping = CachedShmMapping::map_readonly(token, buffer_size)?;

        // Find a slot to store the mapping
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.is_empty() {
                *entry = ClientSurfaceEntry {
                    task_id,
                    token,
                    mapping: Some(mapping),
                };
                return Some(i);
            }
        }

        // No slot available
        None
    }

    /// Get a slice view of the cached buffer at the given index.
    fn get_slice(&self, index: usize) -> Option<&[u8]> {
        self.entries.get(index)?.mapping.as_ref().map(|m| m.as_slice())
    }

    /// Invalidate mappings for windows that no longer exist
    fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.task_id == 0 {
                continue;
            }

            let still_exists = (0..window_count as usize)
                .any(|i| windows[i].task_id == entry.task_id);

            if !still_exists {
                // Window no longer exists, clear the entry
                // (CachedShmMapping doesn't unmap on drop - kernel cleans up)
                *entry = ClientSurfaceEntry::empty();
            }
        }
    }
}

const WINDOW_STATE_NORMAL: u8 = 0;
const WINDOW_STATE_MINIMIZED: u8 = 1;

// Cursor constants
const CURSOR_SIZE: i32 = 9;

/// Tracks state for conditional taskbar redraws
#[derive(Clone, Copy, PartialEq, Eq)]
struct TaskbarState {
    window_count: u32,
    focused_task: u32,
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

/// Compositor output buffer backed by shared memory (100% safe - uses ShmBuffer)
struct CompositorOutput {
    buffer: ShmBuffer,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
}

impl CompositorOutput {
    /// Allocate compositor output buffer
    fn new(fb: &UserFbInfo) -> Option<Self> {
        let pitch = fb.pitch as usize;
        let size = pitch * fb.height as usize;
        let bytes_pp = (fb.bpp / 8) as u8;

        if size == 0 || bytes_pp < 3 {
            return None;
        }

        // Allocate shared memory buffer using safe wrapper
        let buffer = ShmBuffer::create(size).ok()?;

        Some(Self {
            buffer,
            width: fb.width,
            height: fb.height,
            pitch,
            bytes_pp,
        })
    }

    /// Get a DrawBuffer for this output (100% safe - no raw pointers)
    fn draw_buffer(&mut self) -> Option<DrawBuffer<'_>> {
        // ShmBuffer::as_mut_slice() is safe - bounds checked at creation
        let slice = self.buffer.as_mut_slice();
        DrawBuffer::new(slice, self.width, self.height, self.pitch, self.bytes_pp)
    }

    /// Present the output buffer to the framebuffer
    fn present(&self) -> bool {
        sys_fb_flip(self.buffer.token()) == 0
    }
}

struct WindowManager {
    windows: [UserWindowInfo; MAX_WINDOWS],
    window_count: u32,
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
    prev_taskbar_state: TaskbarState,
    taskbar_needs_redraw: bool,
    // Force full redraw flag
    needs_full_redraw: bool,
    // Client surface cache for shared memory mappings
    surface_cache: ClientSurfaceCache,
    // Output buffer info for compositing
    output_bytes_pp: u8,
    output_pitch: usize,
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
            taskbar_needs_redraw: true,
            needs_full_redraw: true,
            surface_cache: ClientSurfaceCache::new(),
            output_bytes_pp: 4,
            output_pitch: 0,
        }
    }

    fn set_output_info(&mut self, bytes_pp: u8, pitch: usize) {
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    /// Update mouse state from kernel
    fn update_mouse(&mut self) {
        let mut event = UserMouseEvent::default();
        if sys_mouse_read(&mut event) > 0 {
            self.mouse_buttons_prev = self.mouse_buttons;
            self.prev_mouse_x = self.mouse_x;
            self.prev_mouse_y = self.mouse_y;
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

    /// Refresh window list from kernel
    fn refresh_windows(&mut self) {
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;
        self.window_count = sys_enumerate_windows(&mut self.windows) as u32;

        // Clean up stale surface mappings
        self.surface_cache.cleanup_stale(&self.windows, self.window_count);

        // Check if taskbar state changed
        let new_state =
            TaskbarState::from_windows(&self.windows, self.window_count, self.focused_task);
        if new_state != self.prev_taskbar_state {
            self.taskbar_needs_redraw = true;
            self.prev_taskbar_state = new_state;
        }

        // Check for window position/state changes that require redraw
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            // Find in previous frame
            for j in 0..self.prev_window_count as usize {
                let prev = &self.prev_windows[j];
                if prev.task_id == window.task_id {
                    if prev.x != window.x
                        || prev.y != window.y
                        || prev.state != window.state
                        || window.is_dirty()
                    {
                        self.needs_full_redraw = true;
                    }
                    break;
                }
            }
        }
    }

    /// Handle all mouse events
    fn handle_mouse_events(&mut self, fb_height: i32) {
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
            if self.mouse_y >= fb_height - TASKBAR_HEIGHT {
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
                    if self.hit_test_close_button(&window) {
                        self.close_window(window.task_id);
                        return;
                    }

                    if self.hit_test_minimize_button(&window) {
                        sys_set_window_state(window.task_id, WINDOW_STATE_MINIMIZED);
                        return;
                    }

                    self.start_drag(&window);
                    sys_raise_window(window.task_id);
                    sys_tty_set_focus(window.task_id);
                    self.focused_task = window.task_id;
                    return;
                }
            }
        }
    }

    fn hit_test_title_bar(&self, window: &UserWindowInfo) -> bool {
        let title_y = window.y - TITLE_BAR_HEIGHT;
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= title_y
            && self.mouse_y < window.y
    }

    fn hit_test_close_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING;
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
    }

    fn hit_test_minimize_button(&self, window: &UserWindowInfo) -> bool {
        let button_x = window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2);
        let button_y = window.y - TITLE_BAR_HEIGHT + BUTTON_PADDING;
        self.mouse_x >= button_x
            && self.mouse_x < button_x + BUTTON_SIZE
            && self.mouse_y >= button_y
            && self.mouse_y < button_y + BUTTON_SIZE
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
        sys_set_window_position(self.drag_task, new_x, new_y);
        self.needs_full_redraw = true;
    }

    fn close_window(&mut self, task_id: u32) {
        sys_set_window_state(task_id, WINDOW_STATE_MINIMIZED);
        self.needs_full_redraw = true;
    }

    fn handle_taskbar_click(&mut self) {
        let mut x = TASKBAR_BUTTON_PADDING;
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            let button_width = TASKBAR_BUTTON_WIDTH;

            if self.mouse_x >= x && self.mouse_x < x + button_width {
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
                self.needs_full_redraw = true;
                return;
            }

            x += button_width + TASKBAR_BUTTON_PADDING;
        }
    }

    /// Draw window title bar to the output buffer
    fn draw_title_bar(&self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        let focused = window.task_id == self.focused_task;
        let color = if focused {
            COLOR_TITLE_BAR_FOCUSED
        } else {
            COLOR_TITLE_BAR
        };

        let title_y = window.y - TITLE_BAR_HEIGHT;

        // Title bar background
        gfx::fill_rect(buf, window.x, title_y, window.width as i32, TITLE_BAR_HEIGHT, color);

        // Window title text
        let title = title_to_str(&window.title);
        gfx::font::draw_string(buf, window.x + 8, title_y + 4, title, COLOR_TEXT, color);

        // Close button (X)
        self.draw_button(
            buf,
            window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING,
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "X",
            self.hit_test_close_button(window),
            true,
        );

        // Minimize button (_)
        self.draw_button(
            buf,
            window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2),
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "_",
            self.hit_test_minimize_button(window),
            false,
        );
    }

    /// Draw a button to the output buffer
    fn draw_button(
        &self,
        buf: &mut DrawBuffer,
        x: i32,
        y: i32,
        size: i32,
        label: &str,
        hover: bool,
        is_close: bool,
    ) {
        let color = if hover && is_close {
            COLOR_BUTTON_CLOSE_HOVER
        } else if hover {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        gfx::fill_rect(buf, x, y, size, size, color);
        gfx::font::draw_string(buf, x + size / 4, y + size / 4, label, COLOR_TEXT, color);
    }

    /// Draw taskbar to the output buffer
    fn draw_taskbar(&self, buf: &mut DrawBuffer) {
        let taskbar_y = buf.height() as i32 - TASKBAR_HEIGHT;

        // Taskbar background
        gfx::fill_rect(
            buf,
            0,
            taskbar_y,
            buf.width() as i32,
            TASKBAR_HEIGHT,
            COLOR_TASKBAR,
        );

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

            let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
            let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);

            gfx::fill_rect(buf, x, btn_y, TASKBAR_BUTTON_WIDTH, btn_height, btn_color);

            // Button text (truncated to fit)
            let title = title_to_str(&window.title);
            let max_chars = (TASKBAR_BUTTON_WIDTH / 8 - 1) as usize;
            let truncated: &str = if title.len() > max_chars {
                &title[..max_chars]
            } else {
                title
            };
            gfx::font::draw_string(buf, x + 4, btn_y + 4, truncated, COLOR_TEXT, btn_color);

            x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }
    }

    /// Draw mouse cursor to the output buffer
    fn draw_cursor(&self, buf: &mut DrawBuffer) {
        // Simple crosshair cursor
        let mx = self.mouse_x;
        let my = self.mouse_y;

        // Horizontal line
        gfx::fill_rect(buf, mx - 4, my, CURSOR_SIZE, 1, COLOR_CURSOR);

        // Vertical line
        gfx::fill_rect(buf, mx, my - 4, 1, CURSOR_SIZE, COLOR_CURSOR);
    }

    /// Draw window content from client's shared memory surface (100% safe)
    fn draw_window_content(&mut self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        // Calculate buffer size for this surface
        let bytes_pp = self.output_bytes_pp as usize;
        let src_pitch = (window.width as usize) * bytes_pp;
        let buffer_size = src_pitch * (window.height as usize);

        // Try to get or create a cached mapping for the client's surface
        let cache_index = match self.surface_cache.get_or_create_index(
            window.task_id,
            window.shm_token,
            buffer_size,
        ) {
            Some(idx) => idx,
            None => {
                // No shared memory surface - draw placeholder
                self.draw_window_placeholder(buf, window);
                return;
            }
        };

        // Get the cached buffer slice (100% safe - bounds checked)
        let src_data = match self.surface_cache.get_slice(cache_index) {
            Some(slice) => slice,
            None => {
                self.draw_window_placeholder(buf, window);
                return;
            }
        };

        let dst_pitch = self.output_pitch;
        let buf_width = buf.width() as i32;
        let buf_height = buf.height() as i32;

        // Clip to buffer bounds
        let x0 = window.x.max(0);
        let y0 = window.y.max(0);
        let x1 = (window.x + window.width as i32).min(buf_width);
        let y1 = (window.y + window.height as i32).min(buf_height);

        if x0 >= x1 || y0 >= y1 {
            return;
        }

        // Calculate offsets into source buffer
        let src_start_x = (x0 - window.x) as usize;
        let src_start_y = (y0 - window.y) as usize;

        // Get destination buffer data
        let dst_data = buf.data_mut();

        // Copy each row from client surface to output buffer (100% safe - slice ops only)
        for row in 0..(y1 - y0) as usize {
            let src_row = src_start_y + row;
            let dst_row = (y0 as usize) + row;

            let src_off = src_row * src_pitch + src_start_x * bytes_pp;
            let dst_off = dst_row * dst_pitch + (x0 as usize) * bytes_pp;
            let copy_width = ((x1 - x0) as usize) * bytes_pp;

            // Safe slice operations with bounds checking
            let src_end = src_off + copy_width;
            let dst_end = dst_off + copy_width;

            if src_end <= src_data.len() && dst_end <= dst_data.len() {
                dst_data[dst_off..dst_end].copy_from_slice(&src_data[src_off..src_end]);
            }
        }
    }

    /// Draw placeholder when client hasn't migrated to shared memory yet
    fn draw_window_placeholder(&self, buf: &mut DrawBuffer, window: &UserWindowInfo) {
        // Draw a colored rectangle as placeholder for window content
        gfx::fill_rect(
            buf,
            window.x,
            window.y,
            window.width as i32,
            window.height as i32,
            COLOR_WINDOW_PLACEHOLDER,
        );

        // Draw a border to show window bounds
        gfx::draw_rect(
            buf,
            window.x,
            window.y,
            window.width as i32,
            window.height as i32,
            COLOR_TITLE_BAR,
        );

        // Draw placeholder text
        let text = "Window content pending migration";
        let text_x = window.x + 10;
        let text_y = window.y + window.height as i32 / 2 - 8;
        gfx::font::draw_string(
            buf,
            text_x,
            text_y,
            text,
            COLOR_TEXT,
            COLOR_WINDOW_PLACEHOLDER,
        );
    }

    /// Full compositor render pass
    fn render(&mut self, buf: &mut DrawBuffer) {
        // 1. Clear background
        buf.clear(COLOR_BACKGROUND);

        // 2. Draw windows (bottom to top for proper z-ordering)
        let window_count = self.window_count as usize;
        for i in 0..window_count {
            let window = self.windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Draw window content from client's shared memory surface
            self.draw_window_content(buf, &window);

            // Draw title bar
            self.draw_title_bar(buf, &window);
        }

        // 3. Draw taskbar (on top of windows)
        self.draw_taskbar(buf);

        // 4. Draw cursor (on top of everything)
        self.draw_cursor(buf);

        // Reset redraw flags
        self.needs_full_redraw = false;
        self.first_frame = false;
        self.taskbar_needs_redraw = false;
    }

    /// Check if any redraw is needed
    fn needs_redraw(&self) -> bool {
        self.first_frame
            || self.needs_full_redraw
            || self.taskbar_needs_redraw
            || self.mouse_moved()
            || self.any_window_dirty()
    }

    fn mouse_moved(&self) -> bool {
        self.mouse_x != self.prev_mouse_x || self.mouse_y != self.prev_mouse_y
    }

    fn any_window_dirty(&self) -> bool {
        for i in 0..self.window_count as usize {
            if self.windows[i].is_dirty() {
                return true;
            }
        }
        false
    }
}

/// Convert c_char title array to &str
///
/// Note: Contains one minimal unsafe block for FFI boundary reinterpretation.
/// c_char is i8 on Linux, but we need &[u8] for UTF-8 validation. This is
/// unavoidable without changing the kernel API.
fn title_to_str(title: &[c_char; 32]) -> &str {
    // Find the null terminator
    let mut len = 0usize;
    for &ch in title.iter() {
        if ch == 0 {
            break;
        }
        len += 1;
    }

    if len == 0 {
        return "";
    }

    // SAFETY: FFI boundary - reinterpret c_char bytes as u8 bytes.
    // c_char and u8 are both 1 byte with same alignment. len <= 32.
    // This is the only unsafe in compositor.rs - required for kernel FFI.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(title.as_ptr() as *const u8, len) };

    // Validate UTF-8 and return "<invalid>" for non-UTF8 strings
    core::str::from_utf8(bytes).unwrap_or("<invalid>")
}

#[unsafe(link_section = ".user_text")]
pub fn compositor_user_main(_arg: *mut c_void) {
    let mut wm = WindowManager::new();
    let mut fb_info = UserFbInfo::default();

    // Get framebuffer info
    if sys_fb_info(&mut fb_info) < 0 {
        loop {
            sys_yield();
        }
    }

    // Allocate compositor output buffer
    let mut output = match CompositorOutput::new(&fb_info) {
        Some(out) => out,
        None => {
            // Failed to allocate output buffer - yield forever
            loop {
                sys_yield();
            }
        }
    };

    // Set output info on window manager for compositing
    wm.set_output_info(output.bytes_pp, output.pitch);

    // Set pixel format based on framebuffer info
    let pixel_format = if fb_info.pixel_format == 2 || fb_info.pixel_format == 4 {
        PixelFormat::Bgra
    } else {
        PixelFormat::Rgba
    };

    // 60Hz fixed refresh rate compositor loop
    const TARGET_FRAME_MS: u64 = 16;

    loop {
        let frame_start_ms = sys_get_time_ms();

        // === QUEUE DRAIN PHASE ===
        // Process all pending client operations (commits, registers, unregisters)
        // Must be called before enumerate_windows to ensure consistent state
        sys_drain_queue();

        // === INPUT PHASE ===
        wm.update_mouse();
        wm.refresh_windows();
        wm.handle_mouse_events(fb_info.height as i32);

        // === RENDERING PHASE ===
        if wm.needs_redraw() {
            if let Some(mut buf) = output.draw_buffer() {
                buf.set_pixel_format(pixel_format);
                wm.render(&mut buf);
            }

            // Present to framebuffer
            output.present();
        }

        // === FRAME PACING ===
        let frame_end_ms = sys_get_time_ms();
        let frame_time = frame_end_ms.saturating_sub(frame_start_ms);
        if frame_time < TARGET_FRAME_MS {
            sys_sleep_ms((TARGET_FRAME_MS - frame_time) as u32);
        }

        sys_yield();
    }
}
