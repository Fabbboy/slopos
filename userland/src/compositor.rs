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

use core::ffi::c_void;

use crate::gfx::{self, rgb, DamageRect, DamageTracker, DrawBuffer, PixelFormat};
use crate::syscall::{
    sys_drain_queue, sys_enumerate_windows, sys_fb_flip, sys_fb_info, sys_get_time_ms,
    sys_input_get_button_state, sys_input_get_pointer_pos, sys_input_set_pointer_focus_with_offset,
    sys_mark_frames_done, sys_raise_window, sys_set_window_position, sys_set_window_state,
    sys_shm_unmap, sys_sleep_ms, sys_spawn_task, sys_tty_set_focus, sys_yield,
    CachedShmMapping, ShmBuffer, UserFbInfo, UserWindowInfo,
};
use crate::ui_utils;

use crate::theme::*;


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

    /// Unmaps and clears cached client surface mappings for entries whose windows no longer exist.
    ///
    /// Iterates the cache and for each entry with a nonzero task id checks whether that task id
    /// is present in the provided window list (first `window_count` entries of `windows`).
    /// If the task id is not found, the entry's shared-memory mapping (if any) is unmapped and
    /// the entry is reset to an empty state.
    ///
    /// # Parameters
    ///
    /// - `windows`: slice containing the current set of windows (length `MAX_WINDOWS`).
    /// - `window_count`: number of active windows to consider from `windows`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Call cleanup_stale to ensure mappings for removed windows are released.
    /// let mut cache = ClientSurfaceCache::new();
    /// // Create an all-zero windows array (no active windows).
    /// let windows: [UserWindowInfo; MAX_WINDOWS] = unsafe { std::mem::zeroed() };
    /// cache.cleanup_stale(&windows, 0);
    /// ```
    fn cleanup_stale(&mut self, windows: &[UserWindowInfo; MAX_WINDOWS], window_count: u32) {
        for entry in &mut self.entries {
            if entry.task_id == 0 {
                continue;
            }

            let still_exists = (0..window_count as usize)
                .any(|i| windows[i].task_id == entry.task_id);

            if !still_exists {
                // Window no longer exists - unmap the shared memory and clear the entry
                if let Some(ref mapping) = entry.mapping {
                    unsafe { sys_shm_unmap(mapping.vaddr()); }
                }
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

/// Bounds of a window (for damage tracking)
#[derive(Copy, Clone, Default)]
struct WindowBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    visible: bool,
}

impl WindowBounds {
    fn from_window(w: &UserWindowInfo) -> Self {
        Self {
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            visible: w.state != WINDOW_STATE_MINIMIZED,
        }
    }

    /// Get the full window rect including title bar
    fn to_damage_rect(&self) -> DamageRect {
        if !self.visible {
            return DamageRect::invalid();
        }
        DamageRect {
            x0: self.x,
            y0: self.y - TITLE_BAR_HEIGHT,
            x1: self.x + self.width as i32 - 1,
            y1: self.y + self.height as i32 - 1,
        }
    }
}

/// Maximum cursor positions to track per frame (for damage)
const MAX_CURSOR_TRAIL: usize = 16;

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
    // Output damage accumulator for partial redraw
    output_damage: DamageTracker,
    // Previous frame's window bounds (for expose damage calculation)
    prev_window_bounds: [WindowBounds; MAX_WINDOWS],
    // Cursor positions visited this frame (for trail-free damage)
    cursor_trail: [(i32, i32); MAX_CURSOR_TRAIL],
    cursor_trail_count: usize,
}

impl WindowManager {
    /// Constructs a new WindowManager initialized to its default, empty runtime state.
    ///
    /// The returned manager has empty window lists and previous-window buffers, default input
    /// and dragging state, an initialized ClientSurfaceCache, default output damage tracker,
    /// and flags set to require an initial full redraw.
    ///
    /// # Examples
    ///
    /// ```
    /// let wm = WindowManager::new();
    /// assert_eq!(wm.window_count, 0);
    /// assert!(wm.first_frame);
    /// assert_eq!(wm.surface_cache.get_slice(0), None);
    /// ```
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
            first_frame: true,
            prev_taskbar_state: TaskbarState::empty(),
            taskbar_needs_redraw: true,
            needs_full_redraw: true,
            surface_cache: ClientSurfaceCache::new(),
            output_bytes_pp: 4,
            output_pitch: 0,
            output_damage: DamageTracker::new(),
            prev_window_bounds: [WindowBounds::default(); MAX_WINDOWS],
            cursor_trail: [(0, 0); MAX_CURSOR_TRAIL],
            cursor_trail_count: 0,
        }
    }

    fn set_output_info(&mut self, bytes_pp: u8, pitch: usize) {
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    /// Update mouse state from kernel.
    /// Queries global position and button state directly (works even when focus is on another task).
    fn update_mouse(&mut self) {
        // Reset cursor trail for this frame
        self.cursor_trail_count = 0;

        // Record previous position for damage tracking
        let old_x = self.mouse_x;
        let old_y = self.mouse_y;

        // Always query global pointer position (works even when focus is on another task)
        let (new_x, new_y) = sys_input_get_pointer_pos();
        if new_x != self.mouse_x || new_y != self.mouse_y {
            // Record trail for damage tracking
            if self.cursor_trail_count < MAX_CURSOR_TRAIL {
                self.cursor_trail[self.cursor_trail_count] = (old_x, old_y);
                self.cursor_trail_count += 1;
            }
            self.mouse_x = new_x;
            self.mouse_y = new_y;
        }

        // Always query global button state (works even when focus is on another task)
        self.mouse_buttons_prev = self.mouse_buttons;
        self.mouse_buttons = sys_input_get_button_state();
    }

    /// Check if mouse was just clicked (press event)
    fn mouse_clicked(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0 && (self.mouse_buttons_prev & 0x01) == 0
    }

    /// Check if mouse is currently pressed
    fn mouse_pressed(&self) -> bool {
        (self.mouse_buttons & 0x01) != 0
    }

    /// Refresh window list from kernel and accumulate damage
    fn refresh_windows(&mut self) {
        // Save previous state
        self.prev_windows = self.windows;
        self.prev_window_count = self.window_count;

        // Get current windows
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

        // Clear output damage for this frame (accumulate fresh)
        self.output_damage.clear();

        // Accumulate damage from all sources
        for i in 0..self.window_count as usize {
            // Copy window data to avoid borrow conflicts
            let window = self.windows[i];
            let curr_bounds = WindowBounds::from_window(&window);

            // Find previous bounds for this window
            let prev_bounds = self.find_prev_bounds(window.task_id);

            // Check for window movement or visibility change - add both old and new positions as damage
            if let Some(old) = prev_bounds {
                if old.x != curr_bounds.x || old.y != curr_bounds.y
                    || old.width != curr_bounds.width || old.height != curr_bounds.height
                    || old.visible != curr_bounds.visible
                {
                    // Old position needs redraw (expose damage)
                    // Note: add_bounds_damage handles invisible bounds by returning early
                    self.add_bounds_damage(&old);
                    // New position needs redraw (if visible)
                    self.add_bounds_damage(&curr_bounds);
                }
            }

            // Store current bounds for next frame (even for minimized windows)
            self.prev_window_bounds[i] = curr_bounds;

            // Skip content damage for minimized windows (they're not drawn)
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            // Add window's content damage (from client's sys_surface_damage calls)
            if window.is_dirty() {
                self.add_window_damage(&window);
            }
        }

        // Handle removed windows (expose damage)
        for i in 0..self.prev_window_count as usize {
            let prev = &self.prev_windows[i];
            if !self.window_exists(prev.task_id) {
                let old_bounds = self.prev_window_bounds[i];
                self.add_bounds_damage(&old_bounds);
            }
        }
    }

    /// Find previous bounds for a window by task_id
    fn find_prev_bounds(&self, task_id: u32) -> Option<WindowBounds> {
        for i in 0..self.prev_window_count as usize {
            if self.prev_windows[i].task_id == task_id {
                return Some(self.prev_window_bounds[i]);
            }
        }
        None
    }

    /// Check if a window with given task_id exists in current frame
    fn window_exists(&self, task_id: u32) -> bool {
        (0..self.window_count as usize).any(|i| self.windows[i].task_id == task_id)
    }

    /// Find a window by its title and return its task_id
    fn find_window_by_title(&self, title: &[u8]) -> Option<u32> {
        for i in 0..self.window_count as usize {
            let window = &self.windows[i];
            // Compare title bytes (null-terminated)
            let title_len = title.iter().position(|&b| b == 0).unwrap_or(title.len());
            let win_title_len = window.title.iter().position(|&b| b == 0).unwrap_or(32);
            if title_len == win_title_len && &window.title[..win_title_len] == &title[..title_len] {
                return Some(window.task_id);
            }
        }
        None
    }

    /// Add window bounds (including title bar) to output damage
    fn add_bounds_damage(&mut self, bounds: &WindowBounds) {
        let rect = bounds.to_damage_rect();
        if rect.is_valid() {
            self.output_damage.add(rect.x0, rect.y0, rect.x1, rect.y1);
        }
    }

    /// Add window's content damage (transformed to screen coords) to output damage
    fn add_window_damage(&mut self, window: &UserWindowInfo) {
        // If full damage (damage_count == u8::MAX), add entire window bounds
        if window.damage_count == u8::MAX {
            let bounds = WindowBounds::from_window(window);
            self.add_bounds_damage(&bounds);
            return;
        }

        // Transform each damage region from surface-local to screen coordinates
        for i in 0..window.damage_count as usize {
            let region = &window.damage_regions[i];
            if region.is_valid() {
                // Transform to screen coordinates (add window position)
                self.output_damage.add(
                    window.x + region.x0,
                    window.y + region.y0,
                    window.x + region.x1,
                    window.y + region.y1,
                );
            }
        }
    }

    /// Process queued mouse input and perform high-level interactions (window dragging,
    /// title-bar actions, and taskbar clicks).
    ///
    /// This consumes click/drag events in priority order:
    /// - Ongoing window drags are continued or stopped based on current button state.
    /// - New clicks are tested against the taskbar, then window title bars (front-to-back),
    ///   handling close, minimize, raise/focus, and initiating drags.
    ///
    /// # Parameters
    ///
    /// - `fb_height`: Height of the framebuffer in pixels; used to detect clicks on the taskbar
    ///   area at the bottom of the screen.
    ///
    /// # Examples
    ///
    /// ```
    /// // Basic example: update internal mouse state first, then dispatch events.
    /// let mut wm = WindowManager::new();
    /// // ... populate wm.mouse_x/mouse_y/mouse_buttons as needed ...
    /// wm.handle_mouse_events(480);
    /// ```
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

            // Check window title bar and content area clicks (front to back)
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

                // Check content area click - set pointer focus so client receives events
                if self.hit_test_content_area(&window) {
                    sys_raise_window(window.task_id);
                    sys_tty_set_focus(window.task_id);
                    // Set pointer focus with window offset for coordinate translation
                    sys_input_set_pointer_focus_with_offset(window.task_id, window.x, window.y);
                    self.focused_task = window.task_id;
                    return;
                }
            }
        }
    }

    fn hit_test_content_area(&self, window: &UserWindowInfo) -> bool {
        self.mouse_x >= window.x
            && self.mouse_x < window.x + window.width as i32
            && self.mouse_y >= window.y
            && self.mouse_y < window.y + window.height as i32
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

    /// Minimizes the window identified by `task_id` and marks the compositor for a full redraw.
    ///
    /// This sets the window state to `WINDOW_STATE_MINIMIZED` and ensures the next frame repaints the
    /// output to reflect the change.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given a mutable WindowManager instance `wm`
    /// wm.close_window(42);
    /// ```
    fn close_window(&mut self, task_id: u32) {
        sys_set_window_state(task_id, WINDOW_STATE_MINIMIZED);
        self.needs_full_redraw = true;
    }

    /// Handle a mouse click on the taskbar, spawning the file manager or minimizing/restoring a window.
    ///
    /// If the click is inside the Files button, this spawns the file manager task if not already
    /// running, or raises the existing file manager window. If the click hits a per-window taskbar
    /// button, this toggles that window between minimized and normal state via `sys_set_window_state`;
    /// when restoring, it raises the window, sets TTY focus, updates `focused_task`, and marks a
    /// full redraw. Returns after handling the first matching button.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assume `wm` is a prepared WindowManager with valid taskbar layout and mouse_x/mouse_y set.
    /// let mut wm = WindowManager::new();
    /// // position mouse over the Files button and simulate click handling
    /// wm.mouse_x = 4; // within TASKBAR_BUTTON_PADDING
    /// wm.handle_taskbar_click();
    /// assert!(wm.needs_full_redraw);
    /// ```
    fn handle_taskbar_click(&mut self) {
        let files_btn_x = TASKBAR_BUTTON_PADDING;
        // Check Files button click - spawn file manager or raise existing window
        if self.mouse_x >= files_btn_x && self.mouse_x < files_btn_x + FM_BUTTON_WIDTH {
            if let Some(task_id) = self.find_window_by_title(b"Files") {
                // File manager already running - raise it
                sys_raise_window(task_id);
                sys_tty_set_focus(task_id);
                self.focused_task = task_id;
            } else {
                // Spawn new file manager task
                sys_spawn_task(b"file_manager\0");
            }
            self.needs_full_redraw = true;
            return;
        }

        let mut x = TASKBAR_BUTTON_PADDING + FM_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
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
        ui_utils::draw_button(
            buf,
            window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING,
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "X",
            self.hit_test_close_button(window),
            true,
        );

        // Minimize button (_)
        ui_utils::draw_button(
            buf,
            window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2),
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "_",
            self.hit_test_minimize_button(window),
            false,
        );
    }


    /// Renders the taskbar into the provided draw buffer, including the Files button and one button per tracked window.
    ///
    /// The taskbar is drawn at the bottom of the buffer; the Files button reflects the File Manager's visible/hover state,
    /// and each window gets a fixed-width button that indicates focus and shows a truncated title.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// let wm = WindowManager::new();
    /// // Obtain a DrawBuffer from a CompositorOutput in real usage:
    /// // let mut buf = output.draw_buffer();
    /// // For this example we assume `buf` is a valid DrawBuffer:
    /// let mut buf: DrawBuffer = unimplemented!();
    /// wm.draw_taskbar(&mut buf);
    /// ```
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
        
        // Draw Files button
        let files_btn_x = TASKBAR_BUTTON_PADDING;
        let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
        let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);
        
        let files_hover = self.mouse_x >= files_btn_x && self.mouse_x < files_btn_x + FM_BUTTON_WIDTH
                       && self.mouse_y >= btn_y && self.mouse_y < btn_y + btn_height;

        // Highlight if file manager window exists or button is hovered
        let file_manager_running = self.find_window_by_title(b"Files").is_some();
        let files_color = if file_manager_running || files_hover {
             COLOR_BUTTON_HOVER
        } else {
             COLOR_BUTTON
        };
        
        gfx::fill_rect(buf, files_btn_x, btn_y, FM_BUTTON_WIDTH, btn_height, files_color);
        gfx::font::draw_string(buf, files_btn_x + 4, btn_y + 4, "Files", COLOR_TEXT, files_color);

        // Draw app buttons
        let mut x = TASKBAR_BUTTON_PADDING + FM_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
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

    /// Render a simple placeholder for a window's content area when the client's surface is not available.
    ///
    /// This draws a filled rectangle using COLOR_WINDOW_PLACEHOLDER, an outline using COLOR_TITLE_BAR,
    /// and a short informational text centered vertically inside the window bounds.
    ///
    /// # Parameters
    ///
    /// - `buf`: the draw target where the placeholder will be rendered.
    /// - `window`: the window geometry and position; `x`, `y`, `width`, and `height` determine the placeholder area.
    ///
    /// # Examples
    ///
    /// ```
    /// // Construct minimal buffer and window info (types shown here are from the compositor crate).
    /// let mut buf = DrawBuffer::default();
    /// let window = UserWindowInfo { x: 20, y: 16, width: 200, height: 120, ..Default::default() };
    /// let wm = WindowManager::new();
    /// // Render a placeholder for the window into the buffer.
    /// wm.draw_window_placeholder(&mut buf, &window);
    /// ```
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

    /// Perform a full compositor render pass into the given draw buffer.
    ///
    /// Clears the output, draws all visible windows (content then title bar) in back-to-front
    /// order, renders the taskbar, draws the cursor on top, and resets internal redraw flags.
    /// This implementation always performs a full redraw when invoked; partial-redraw
    /// optimizations are deferred.
    ///
    /// # Parameters
    ///
    /// - `buf`: The output draw buffer to render into.
    ///
    /// # Examples
    ///
    /// ```
    /// // Prepare compositor state and an output buffer (types and construction depend on context).
    /// let mut wm = WindowManager::new();
    /// let mut draw_buf = /* obtain a DrawBuffer for the output framebuffer */ unimplemented!();
    /// // Render the current scene into the output buffer.
    /// wm.render(&mut draw_buf);
    /// ```
    fn render(&mut self, buf: &mut DrawBuffer) {
        // Always clear the entire buffer
        buf.clear(COLOR_BACKGROUND);

        // Draw all visible windows (bottom to top for proper z-ordering)
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

        // Draw taskbar
        self.draw_taskbar(buf);

        // Draw cursor (on top of everything)
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
            || self.output_damage.is_dirty()
            || self.any_window_dirty()
    }

    fn mouse_moved(&self) -> bool {
        // Mouse moved if we recorded any trail positions this frame
        self.cursor_trail_count > 0
    }

    /// Check if any window has pending damage (fallback for damage tracking)
    fn any_window_dirty(&self) -> bool {
        for i in 0..self.window_count as usize {
            if self.windows[i].is_dirty() {
                return true;
            }
        }
        false
    }
}

/// Convert UTF-8 title array to &str (now 100% safe - no unsafe needed)
///
/// With the ABI change to use `[u8; 32]` instead of `[c_char; 32]`,
/// this function is now completely safe Rust.
fn title_to_str(title: &[u8; 32]) -> &str {
    // Find the null terminator
    let len = title.iter().position(|&b| b == 0).unwrap_or(32);

    if len == 0 {
        return "";
    }

    // Direct UTF-8 validation - no unsafe needed!
    core::str::from_utf8(&title[..len]).unwrap_or("<invalid>")
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

            // Notify clients that their frames have been presented (Wayland wl_surface.frame)
            let present_time = sys_get_time_ms();
            sys_mark_frames_done(present_time);
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