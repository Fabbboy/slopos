# SlopOS Desktop UI — Design & Implementation Plan

## 1. Vision

Transform SlopOS from its current 1990s-era Windows-style compositor into a modern, macOS-inspired desktop environment with clean typography, translucent compositing, and a polished interaction model. The current Windows-style taskbar and start menu are replaced entirely with a macOS-like top menu bar and bottom dock.

**Design pillars** (drawn from macOS):
- **Clean typography**: Anti-aliased TrueType fonts everywhere — no bitmap text in the GUI
- **Depth through transparency**: Window shadows, translucent panels, alpha-blended overlays
- **Minimal chrome**: Thin title bars, traffic-light buttons (close/minimize/maximize), centered titles
- **Spatial consistency**: Global menu bar (top), dock (bottom), notification area (top-right)
- **Smooth motion**: Eased animations for window open/close/minimize, dock magnification

**Target environment**: QEMU first (PS/2 input, Limine framebuffer), real hardware on the roadmap.

**Widget toolkit depth**: Moderate — buttons, labels, text fields, scroll views, lists, tabs, dropdown menus. Enough for a file manager, settings app, and system monitor.

---

## 2. Current State Assessment

### 2.1 What Exists (Summary from Codebase Audit)

| Subsystem | Status | Key Files |
|-----------|--------|-----------|
| **Compositor** | ✅ Working, Wayland-inspired | `userland/src/apps/compositor/` — damage tracking, 60fps, SHM surfaces |
| **Window chrome** | ⚠️ Windows-style | `compositor/renderer.rs` — taskbar, start menu, square close/minimize buttons |
| **Drawing primitives** | ✅ Full set | `gfx/src/canvas_ops.rs` — rect, circle, triangle, line, fill (all aliased) |
| **Font rendering** | ❌ Bitmap only | `abi/src/font.rs` — 8×16 fixed-width, 95 ASCII glyphs |
| **Alpha blending** | ❌ None | Compositor copies pixels verbatim, no per-pixel alpha compositing |
| **Anti-aliasing** | ❌ None | All drawing primitives use Bresenham (aliased) |
| **Input system** | ✅ Mature | `drivers/src/input_event.rs` — per-task ring buffers, focus routing, clipboard |
| **Window surfaces** | ✅ SHM-backed | `userland/src/appkit/surface.rs` — DrawBuffer, present_full/present_region |
| **Event loop** | ✅ Working | `userland/src/appkit/run.rs` — WindowedApp trait, poll→dispatch→redraw→yield |
| **PS/2 mouse** | ⚠️ No scroll | `drivers/src/ps2/mouse.rs` — 3-byte packets, no IntelliMouse scroll |
| **Window resize** | ❌ None | No resize protocol, no drag handles, no resize negotiation |
| **Window move** | ❌ None | No move/drag protocol |
| **Cursor shapes** | ⚠️ Limited | Arrow + text beam only — no resize cursors, no grab hand |
| **Theme system** | ⚠️ Minimal | `userland/src/theme.rs` — color and dimension constants |
| **Pixel formats** | ✅ 6 formats | ARGB8888, XRGB8888, RGB888, BGR888, RGBA8888, BGRA8888 |
| **Shared memory** | ✅ Wayland-style | `mm/src/shared_memory.rs` — 64 buffers, acquire/release/refcount |
| **Scheduler** | ⚠️ No GUI boost | 4 static priority levels, no interactive promotion on I/O wakeup |
| **Memory** | ✅ Mature | Buddy + slab, mmap, COW, demand paging, ASLR |
| **Filesystem** | ✅ ext2 R/W | Can load fonts and assets from disk |
| **Syscalls** | ✅ 148 defined | Surface, SHM, input, window management, process, memory |

### 2.2 Files to Delete (Rip-and-Replace)

The following Windows-style compositor code will be **completely replaced**:

| File | Contains | Replacement |
|------|----------|-------------|
| `userland/src/apps/compositor/renderer.rs` | Taskbar, start menu, window decoration renderer | macOS-style renderer (menu bar, dock, traffic lights) |

**Files to heavily modify** (keeping the core, replacing the chrome):

| File | What Changes |
|------|-------------|
| `userland/src/apps/compositor/mod.rs` | Main loop stays, but window management logic changes for menu bar + dock |
| `userland/src/apps/compositor/input.rs` | Hit-testing changes for dock icons, menu bar, traffic-light buttons |
| `userland/src/appkit/window.rs` | Window creation needs title bar style enum (macOS vs headless) |
| `userland/src/theme.rs` | Complete visual overhaul — macOS colors, spacing, corner radii |

---

## 3. Architecture

### 3.1 Component Stack

```
┌──────────────────────────────────────────────────────┐
│                  USER APPLICATIONS                    │
│   File Manager  │  System Monitor  │  Settings  │ ... │
├──────────────────────────────────────────────────────┤
│                   WIDGET TOOLKIT                      │
│  Button │ Label │ TextField │ ScrollView │ ListView   │
│  VStack │ HStack │ TabBar │ Menu │ Popover           │
├──────────────────────────────────────────────────────┤
│                     APPKIT                            │
│  Window │ Surface │ Event │ WindowedApp │ run()       │
├──────────────────────────────────────────────────────┤
│                   FONT LIBRARY                        │
│  TTF parser │ Glyph rasterizer │ Glyph cache          │
│  draw_text() │ measure_text() │ Font loading          │
├──────────────────────────────────────────────────────┤
│                  GFX PRIMITIVES                       │
│  Canvas trait │ DrawBuffer │ DamageTracker             │
│  fill_rect │ line_aa │ circle_aa │ alpha_blend         │
├──────────────────────────────────────────────────────┤
│                   COMPOSITOR                          │
│  Menu Bar │ Dock │ Window Manager │ Alpha Compositor   │
│  Surface registry │ Damage tracking │ Frame pacing     │
├──────────────────────────────────────────────────────┤
│              KERNEL (syscalls + SHM)                  │
│  Surface ops │ SHM │ Input events │ framebuffer flip   │
└──────────────────────────────────────────────────────┘
```

### 3.2 Font Library Architecture

**Location**: New crate `font/` (workspace member, `no_std` compatible)

```
font/
├── Cargo.toml
└── src/
    ├── lib.rs           # Public API: Font, GlyphCache, draw_text()
    ├── ttf_parser.rs    # TrueType table parsing (cmap, glyf, head, hhea, hmtx, loca, maxp)
    ├── outline.rs       # Quadratic Bézier evaluation, contour processing
    ├── rasterizer.rs    # Coverage-based anti-aliased rasterization
    ├── cache.rs         # LRU glyph bitmap cache (size+codepoint → rendered bitmap)
    └── metrics.rs       # Font metrics, text measurement, line layout
```

**Key design decisions**:
- Parse TTF from a `&[u8]` slice (loaded from ext2 via VFS)
- Rasterize glyphs on-demand into an LRU cache
- Output: `&[u8]` coverage bitmap (0–255 per pixel) — caller alpha-blends with foreground color
- No hinting (too complex for v1) — rely on supersampling for quality
- No complex text layout (no RTL, no ligatures, no shaping) — simple left-to-right for v1
- Support multiple font sizes via cache key `(codepoint, size_px)`

### 3.3 Alpha Blending Architecture

**Location**: Extend `abi/src/draw.rs` and `gfx/src/canvas_ops.rs`

```rust
/// Alpha-blend src over dst using standard Porter-Duff "over" operator.
/// src and dst are both ARGB8888 (0xAARRGGBB).
/// Returns the composited pixel.
pub fn alpha_blend(src: u32, dst: u32) -> u32;

/// Blend a coverage value (0–255) with a foreground color onto a destination pixel.
/// Used by the font rasterizer to draw anti-aliased glyphs.
pub fn blend_coverage(coverage: u8, fg: Color32, dst: u32) -> u32;
```

The compositor renders back-to-front:
1. Desktop background
2. Window shadows (pre-rendered, alpha-blended)
3. Window content (from client SHM buffers)
4. Window decorations (title bar, traffic lights)
5. Menu bar (semi-transparent background)
6. Dock (semi-transparent background, icon magnification)
7. Cursor

### 3.4 macOS Chrome Architecture

**Menu Bar** (top of screen, full width):
- Compositor-owned — apps don't render it
- Left side: Apple-equivalent logo/icon + active app name
- Right side: Clock, battery(?), network status icons
- App menus: apps register menu items via syscall, compositor renders them
- Height: ~24px at 1×

**Dock** (bottom-center, floating):
- Compositor-owned
- App icons with labels on hover
- Separator between running apps and pinned
- Magnification effect on hover (icon grows as cursor approaches)
- Bounce animation on app launch (stretch goal)

**Window Decorations**:
- Thin title bar (~28px) with frosted/translucent background
- Traffic-light buttons (🔴🟡🟢) at top-left, 12px circles with 8px spacing
- Close (red), Minimize (yellow), Maximize/Fullscreen (green)
- Window title centered in title bar
- 8px corner radius on window frame
- Drop shadow (gaussian blur or pre-rendered shadow texture)

---

## 4. Implementation Phases

### Phase 1: TTF Font Rasterizer (Visual Foundation)
**Goal**: Replace the 8×16 bitmap font with anti-aliased TrueType text rendering.

**Acceptance criteria**:
- [ ] New `font/` crate parses a standard TTF file (e.g., Inter, Noto Sans)
- [ ] Rasterizes glyphs at arbitrary pixel sizes with coverage-based anti-aliasing
- [ ] LRU glyph cache avoids re-rasterizing frequently used glyphs
- [ ] `draw_text()` function renders a string onto any `Canvas` target
- [ ] `measure_text()` returns width and height for layout purposes
- [ ] Compositor title bars, menu bar, and dock use TTF text
- [ ] Shell text remains bitmap (kernel/TTY doesn't need TTF)
- [ ] A `.ttf` font file is loaded from the ext2 filesystem at compositor startup
- [ ] `just boot` shows the new font in window title bars

**Files to create**:
- `font/Cargo.toml` — new workspace crate
- `font/src/lib.rs` — public API
- `font/src/ttf_parser.rs` — TrueType table parser
- `font/src/outline.rs` — Bézier evaluation
- `font/src/rasterizer.rs` — coverage rasterizer
- `font/src/cache.rs` — LRU glyph cache
- `font/src/metrics.rs` — text measurement

**Files to modify**:
- `Cargo.toml` (workspace) — add `font` member
- `userland/Cargo.toml` — add `font` dependency
- `userland/src/apps/compositor/renderer.rs` — use `font::draw_text()` for title bars
- `userland/src/appkit/surface.rs` or `userland/src/gfx/font.rs` — wire TTF drawing

**Font file to bundle**:
- Download Inter or Noto Sans (OFL license) `.ttf` into `assets/fonts/`
- Modify `scripts/build_fs_image.sh` to create `/usr/share/fonts/` in the ext2 image and copy the `.ttf` file into it (the script currently only creates `/bin` and `/sbin`)

**Estimated effort**: 1500–2500 lines of Rust for the TTF parser + rasterizer.

**QA scenario**:
1. Run `just build` — must compile with zero errors (the new `font/` crate is a workspace member).
2. Run `VIDEO=1 just boot` — wait for the compositor to start.
3. Open a GUI app (e.g., `sysmon` or file manager) so a window with a title bar is visible.
4. **PASS condition**: The window title bar text is visibly smoother than the old 8×16 bitmap font — characters have anti-aliased (gray-shaded) edges rather than hard black/white pixel boundaries. The text must be readable at the default window title size (~14–16px). No garbled/missing glyphs for ASCII A–Z, a–z, 0–9.
5. **FAIL condition**: Title bar text is still the old chunky bitmap font, OR text is garbled/missing, OR the compositor panics on startup.
6. Verify the shell/TTY still uses the old bitmap font (TTF is userland-only).
7. Run `just test` — all existing kernel tests must still pass (no regressions).

---

### Phase 2: Alpha Blending & Compositing
**Goal**: Enable transparency, shadows, and layered compositing in the window compositor.

**Acceptance criteria**:
- [ ] `alpha_blend(src, dst) -> u32` function in `abi/src/draw.rs`
- [ ] `blend_coverage(coverage, fg, dst) -> u32` for font anti-aliasing
- [ ] Compositor renders windows back-to-front with per-pixel alpha
- [ ] Window shadows visible (pre-rendered shadow texture or computed)
- [ ] Semi-transparent title bars (frosted glass effect — even a simple tinted overlay)
- [ ] `just boot` shows windows with visible drop shadows

**Files to create**:
- `gfx/src/blend.rs` — alpha blending functions

**Files to modify**:
- `abi/src/draw.rs` — add `alpha_blend()`, `blend_coverage()` to the public API
- `gfx/src/lib.rs` — export `blend` module
- `userland/src/apps/compositor/renderer.rs` — composite windows with alpha instead of direct copy
- `userland/src/apps/compositor/output.rs` — compositor output buffer must use ARGB8888 (alpha channel)

**Note**: Alpha compositing has a performance cost (~2× per-pixel). The damage tracking system already limits redraws to dirty regions, which mitigates this. If performance is unacceptable, we can use a "dirty region only" alpha blend path.

**Estimated effort**: 300–500 lines.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — wait for the compositor to start with 2+ windows open.
3. **PASS condition — shadows**: Each window has a visible drop shadow. The shadow must be a soft gradient (not a hard black border) visible on at least 2 sides of the window. Overlapping windows show the shadow of the front window over the content of the back window.
4. **PASS condition — transparency**: The window title bar area has a semi-transparent tint — the background or window content behind it is partially visible through it, confirming alpha blending is active.
5. **FAIL condition**: Windows have no shadows, OR windows are rendered as fully opaque rectangles with no transparency, OR the compositor frame rate drops below 30fps (check serial output for late-frame warnings).
6. Run `just test` — all existing kernel tests must still pass.

---

### Phase 3: Anti-Aliased Drawing Primitives
**Goal**: Replace aliased Bresenham primitives with smooth anti-aliased versions.

**Acceptance criteria**:
- [ ] `line_aa(canvas, x0, y0, x1, y1, color)` — Xiaolin Wu's line algorithm
- [ ] `circle_aa(canvas, cx, cy, radius, color)` — anti-aliased circle
- [ ] `rounded_rect(canvas, x, y, w, h, radius, color)` — rounded rectangle (for window corners, buttons)
- [ ] `rounded_rect_filled(canvas, x, y, w, h, radius, color)` — filled variant
- [ ] Existing aliased primitives remain available (for performance-critical paths)
- [ ] Compositor window frames use `rounded_rect` for corner radius

**Files to modify**:
- `gfx/src/canvas_ops.rs` — add `line_aa`, `circle_aa`, `rounded_rect`, `rounded_rect_filled`
- `abi/src/draw.rs` — may need `put_pixel_blend(x, y, color, coverage)` on Canvas trait

**Estimated effort**: 200–400 lines.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — open a GUI app window.
3. **PASS condition — rounded rect**: Window corners are visibly rounded (not sharp 90° corners). The corner radius is smooth, not stairstepped.
4. **PASS condition — AA lines**: Draw a diagonal line (if any app renders one, or temporarily add a test draw in the compositor). The line must have gray intermediate pixels along its edges rather than a jagged pixel staircase.
5. **FAIL condition**: Window corners are still sharp rectangles, OR the `rounded_rect` / `rounded_rect_filled` functions don't exist in `gfx/src/canvas_ops.rs`, OR existing aliased primitives (`fill_rect`, `line`) no longer work.
6. Run `just test` — all existing kernel tests must still pass.

---

### Phase 4: macOS Chrome — Menu Bar & Dock
**Goal**: Replace the Windows-style taskbar + start menu with a macOS-inspired menu bar and dock.

**Acceptance criteria**:
- [ ] **Top menu bar** rendered by compositor:
  - Left: SlopOS icon + active app name
  - Center: (empty for now — app menus come later)
  - Right: Uptime clock (HH:MM:SS since boot via HPET — no RTC exists yet), CPU indicator
  - Semi-transparent background (using Phase 2 alpha blending)
  - Height: 24px
- [ ] **Bottom dock** rendered by compositor:
  - Centered row of app icons (Shell, File Manager, System Monitor)
  - Icons are 48×48 with 8px padding
  - Label appears on hover below icon
  - Separator dot between pinned and running apps
  - Semi-transparent rounded-rect background
  - Magnification effect on hover (icon scales up as cursor approaches)
- [ ] **Window decorations**:
  - Thin title bar (28px) with centered title (TTF font from Phase 1)
  - Traffic-light buttons at top-left (12px circles: red/yellow/green)
  - Close (red), Minimize (yellow), Maximize (green — stretch)
  - 8px corner radius on window frame (using Phase 3 rounded_rect)
- [ ] **Old taskbar and start menu code deleted**
- [ ] `just boot` shows the new macOS-style desktop

**Files to delete/gut**:
- Remove taskbar rendering from `compositor/renderer.rs`
- Remove start menu rendering and hit-testing

**Files to create**:
- `userland/src/apps/compositor/menu_bar.rs` — menu bar state and rendering
- `userland/src/apps/compositor/dock.rs` — dock state, icon layout, magnification
- `userland/src/apps/compositor/decorations.rs` — traffic-light buttons, title bar

**Files to modify**:
- `userland/src/apps/compositor/mod.rs` — integrate menu bar + dock into main loop
- `userland/src/apps/compositor/input.rs` — hit-testing for dock icons, menu bar, traffic lights
- `userland/src/apps/compositor/renderer.rs` — complete rendering overhaul
- `userland/src/theme.rs` — macOS-inspired color palette, dimensions, corner radii

**Estimated effort**: 800–1200 lines.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — wait for the compositor to start.
3. **PASS condition — menu bar**: A horizontal bar spans the full width of the screen at the top (24px tall). Left side shows a SlopOS icon and the active app name. Right side shows an uptime counter (HH:MM:SS format, updating each second). The bar has a semi-transparent background.
4. **PASS condition — dock**: A centered row of icons sits at the bottom of the screen with a semi-transparent rounded-rect background. At least 3 app icons are visible (Shell, File Manager, System Monitor). Hovering the cursor over a dock icon shows a label below it. Icons near the cursor grow larger (magnification effect).
5. **PASS condition — traffic lights**: Each window has 3 small colored circles (red, yellow, green) at the top-left of the title bar. Clicking the red circle closes the window. Clicking yellow minimizes it. Window title is centered in the title bar using the TTF font (Phase 1).
6. **PASS condition — old chrome removed**: No taskbar at the bottom. No start menu. Grep for `start_menu` and `taskbar` in `userland/src/apps/compositor/` — should return zero hits in active code (comments are OK).
7. **FAIL condition**: Old Windows-style taskbar is still visible, OR menu bar/dock are missing, OR traffic-light buttons are absent, OR clicking close/minimize does nothing.
8. Run `just test` — all existing kernel tests must still pass.

---

### Phase 5: Window Interactions
**Goal**: Make windows movable, resizable, and scrollable.

**Acceptance criteria**:
- [ ] **Window move**: Drag title bar to reposition window
  - Compositor tracks drag state (offset from click to window origin)
  - Window position updates in real-time during drag
  - Damage tracking handles the moving window's old and new positions
- [ ] **Window resize**: Drag window edges or corners to resize
  - Compositor defines 8px resize grab zones at window edges
  - Cursor changes to resize arrows when hovering grab zones
  - Resize is negotiated: compositor sends new size to app, app re-renders at new size
  - Needs new syscall or surface protocol for resize events
  - Minimum window size enforced (200×150 or configurable)
- [ ] **Scroll wheel**: PS/2 IntelliMouse 4-byte protocol
  - Mouse driver sends scroll events via `input_route_pointer_button()` or new scroll event type
  - `InputEventType::Scroll` added to ABI
  - Compositor forwards scroll events to focused window
- [ ] **Cursor shapes**: At minimum 6 shapes
  - Default (arrow), Text (I-beam), Resize-NS, Resize-EW, Resize-NWSE, Resize-NESW
  - Cursor bitmaps embedded in compositor
  - `set_cursor_shape()` syscall already exists — extend with new shapes

**Files to modify**:
- `drivers/src/ps2/mouse.rs` — 4-byte IntelliMouse packet support, scroll events
- `abi/src/input.rs` — add `Scroll` event type (or reuse pointer button codes 4/5)
- `userland/src/apps/compositor/input.rs` — resize/move drag handling, edge detection
- `userland/src/apps/compositor/renderer.rs` — resize grab zone visualization (optional)
- `userland/src/apps/compositor/mod.rs` — resize negotiation protocol
- `userland/src/appkit/window.rs` — resize event handling, surface reallocation
- `userland/src/appkit/event.rs` — add resize/scroll event variants
- `video/src/compositor_context.rs` — possibly extend surface protocol for resize

**Estimated effort**: 600–900 lines.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — open a GUI app window.
3. **PASS condition — window move**: Click and hold on the window title bar, drag the mouse. The window follows the cursor in real-time. Release the mouse — the window stays at the new position. No ghost artifacts at the old position (damage tracking handles cleanup).
4. **PASS condition — window resize**: Move the cursor to the right edge of a window — cursor changes to a horizontal resize arrow. Click and drag — the window width changes. The app content re-renders at the new size. Repeat for bottom edge (vertical resize) and bottom-right corner (diagonal resize). Minimum size (200×150) is enforced — dragging smaller snaps to minimum.
5. **PASS condition — scroll wheel**: In any scrollable content area, roll the mouse scroll wheel. Content scrolls up/down. Serial log shows scroll events being delivered (check for `Scroll` or button 4/5 in debug output).
6. **PASS condition — cursor shapes**: Cursor changes to resize arrows when hovering window edges. Cursor is the normal arrow when hovering over window content or the desktop background. Cursor is a text I-beam when hovering a text input area (if one exists).
7. **FAIL condition**: Windows cannot be moved by dragging title bar, OR resize does nothing, OR scroll wheel has no effect, OR cursor never changes shape.
8. Run `just test` — all existing kernel tests must still pass.

---

### Phase 6: Widget Toolkit
**Goal**: Build a moderate widget toolkit for building desktop applications.

**Acceptance criteria**:
- [ ] **Core widgets**: Label, Button, TextField, ScrollView, Separator
- [ ] **Container layouts**: VStack (vertical), HStack (horizontal), ZStack (overlay)
- [ ] **List widgets**: ListView with item recycling, selectable rows
- [ ] **Navigation**: TabBar with tab switching
- [ ] **Menus**: Dropdown menu, context menu (right-click)
- [ ] **Event model**: Hit-test tree → event bubble up → handler callback
- [ ] **Styling**: Consistent with macOS theme (Phase 4 `theme.rs`)
- [ ] **Text rendering**: All widgets use TTF fonts (Phase 1)
- [ ] **At least one demo app** built with the toolkit (e.g., enhanced file manager)

**Files to create**:
```
userland/src/widgets/
├── mod.rs           # Widget trait, WidgetTree, event dispatch
├── label.rs         # Text label (single/multi-line)
├── button.rs        # Clickable button with label, hover/press states
├── text_field.rs    # Single-line text input with cursor, selection
├── scroll_view.rs   # Scrollable container with scrollbar
├── list_view.rs     # Virtualized list with item recycling
├── tab_bar.rs       # Tab switching header
├── menu.rs          # Dropdown and context menus
├── separator.rs     # Visual divider
├── layout.rs        # VStack, HStack, ZStack layout engine
└── style.rs         # Widget styling (colors, fonts, spacing from theme)
```

**Files to modify**:
- `userland/src/lib.rs` — export `widgets` module
- `userland/src/appkit/run.rs` — integrate widget tree into event dispatch
- `userland/src/appkit/event.rs` — widget-level event types (click, hover, focus, text input)

**Estimated effort**: 2000–3000 lines.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — launch the demo app (enhanced file manager or a dedicated widget-gallery app).
3. **PASS condition — Label**: Text labels render using TTF fonts (Phase 1) with correct alignment and wrapping.
4. **PASS condition — Button**: A clickable button is visible. It has a hover state (color change on mouse-over) and a pressed state (visual feedback on click). Clicking it triggers a visible action (e.g., navigates a directory, opens a dialog).
5. **PASS condition — TextField**: A single-line text input is visible. Clicking it gives it focus (cursor blinks). Typing inserts characters. Backspace deletes. Arrow keys move the cursor.
6. **PASS condition — ScrollView**: A scrollable area with content taller than its viewport. Scroll wheel scrolls the content. A scrollbar thumb is visible and reflects the viewport position.
7. **PASS condition — ListView**: A list of items is visible. Clicking an item selects it (highlight changes). Scrolling reveals more items. The list handles 50+ items without visible lag.
8. **PASS condition — TabBar**: Two or more tabs are visible. Clicking a tab switches the content below it. The active tab is visually distinguished from inactive tabs.
9. **PASS condition — Menu**: Right-clicking in the app shows a context menu with at least 2 items. Clicking a menu item dismisses the menu and triggers an action. The menu disappears when clicking outside it.
10. **PASS condition — Layout**: Widgets are arranged in vertical and horizontal stacks. Resizing the window causes widgets to reflow according to the layout rules.
11. **FAIL condition**: Any of the core widgets (Label, Button, TextField, ScrollView, ListView, TabBar, Menu) is missing or non-functional, OR the demo app crashes on startup.
12. Run `just test` — all existing kernel tests must still pass.

---

## 5. Stretch Goals (Post-Phase 6)

These are not planned for the initial implementation but should be kept in mind architecturally:

| Goal | Dependency | Complexity |
|------|-----------|------------|
| **App-specific menus in menu bar** | Phase 4 menu bar + new IPC protocol | Medium |
| **Window snap/split** (half-screen) | Phase 5 resize | Low |
| **Dock bounce animation** on app launch | Phase 4 dock + animation system | Medium |
| **Blur-behind / vibrancy** | Phase 2 alpha + gaussian blur | High (perf-heavy) |
| **Notification center** | Widget toolkit + overlay surface | Medium |
| **Spotlight-like search** | Widget toolkit + filesystem indexing | High |
| **System Preferences app** | Widget toolkit | Medium |
| **Drag-and-drop** between windows | New compositor protocol | High |
| **Multiple keyboard layouts** | Keyboard driver + layout tables | Medium |
| **Animations / easing** | Timer-based interpolation system | Medium |
| **Desktop wallpaper** | Image decoder (PNG/JPEG) + background surface | Medium |
| **Multi-monitor** | Compositor + output enumeration | High |

---

## 6. Risk Assessment

| Risk | Mitigation |
|------|-----------|
| TTF parser complexity (tables, edge cases) | Start with a single font (Inter Regular). Support only the tables needed for simple ASCII+Latin text. Skip hinting, ligatures, complex shaping. |
| Alpha blending performance | Damage tracking already limits redraws. Benchmark early — if >16ms per frame, add SIMD blending (`u64` pair writes already exist). |
| Compositor architecture debt | The rip-and-replace of taskbar/start menu is a one-time cost. The underlying SHM + damage + surface architecture is sound and stays. |
| Font file loading from ext2 | Already proven — ext2 VFS can read arbitrary files. Just need the `.ttf` in the rootfs image. |
| Window resize complexity | Resize negotiation between compositor and app is the hardest protocol. Design the ABI carefully — it must handle: app-requested size, compositor-enforced size, minimum size, aspect ratio. |
| Glyph cache memory | LRU with size cap (e.g., 256KB). At 16px, a glyph is ~256 bytes (16×16 coverage). 256KB ≈ 1000 cached glyphs — more than enough for Latin text. |

---

## 7. Testing Strategy

**Each phase has boot-level verification**:
- `just boot` with `VIDEO=1` — visual inspection of rendered output
- Screenshot comparison (manual) for regressions
- `just test` must continue to pass (non-GUI kernel tests)

**Font rendering validation**:
- Render the alphabet at 12px, 16px, 24px, 32px — inspect for artifacts
- Compare glyph metrics against reference (fonttools or online TTF inspector)

**Alpha blending validation**:
- Render overlapping colored rectangles with known alpha values
- Verify pixel values match expected Porter-Duff "over" results

**Compositor validation**:
- Open 3+ windows, verify z-order compositing
- Move/resize windows, verify damage tracking correctness
- Check that dock and menu bar render at correct positions

---

## 8. Dependencies and Ordering

```
Phase 1 (Fonts) ──────────────────────────┐
                                           │
Phase 2 (Alpha Blending) ─────────────────┤
                                           ├──▶ Phase 4 (macOS Chrome)
Phase 3 (AA Primitives) ──────────────────┘         │
                                                     │
                           Phase 5 (Interactions) ◀──┘
                                   │
                                   ▼
                           Phase 6 (Widgets)
```

Phases 1, 2, and 3 can be developed **in parallel** — they are independent. Phase 4 depends on all three. Phase 5 depends on Phase 4 (needs the new chrome to add resize handles to). Phase 6 depends on everything.

---

## 9. Non-Goals (Explicitly Out of Scope)

- **GPU-accelerated rendering** — software compositing only for now
- **3D graphics / OpenGL / Vulkan** — not needed for desktop UI
- **USB HID drivers** — QEMU PS/2 is sufficient; USB is a separate project
- **Audio** — no audio subsystem needed for visual UI work
- **Network-dependent features** — no web browser, no remote desktop
- **Accessibility** — important but deferred to post-Phase 6
- **Internationalization / i18n** — Latin-1 charset only for v1; CJK/RTL deferred
- **Dynamic linking** — all apps statically linked (existing pattern)
