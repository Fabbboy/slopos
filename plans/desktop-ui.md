# SlopOS Desktop UI — Design & Implementation Plan

## 0. Progress Summary

> **Last updated**: 2026-03-29 (Phase 5 resize + cursors verified)

| Phase | Status | Completion | Notes |
|-------|--------|------------|-------|
| **Phase 1** — TTF Font Rasterizer | ✅ **Complete** | 100% | Full TTF parser + rasterizer + cache. Compositor title bars use TTF. Kernel has bitmap→coverage upgrade path via `SYS_FONT_SET`. Bonus: `GlyphAtlas` + `bitmap.rs` beyond original plan. |
| **Phase 2** — Alpha Blending | ✅ **Complete** | 100% | Blend math in `gfx/src/blend.rs` + compositor wiring in commit `51c3c7a`. Drop shadows (12px spread, quadratic falloff) and semi-transparent title bars (0xD0 alpha tint) active. Damage tracking includes shadow bounds. |
| **Phase 3** — AA Primitives | ✅ **Complete** | 100% | `line_aa`, `circle_aa`, `rounded_rect`, `rounded_rect_filled` all landed in `gfx/src/canvas_ops.rs`. Aliased primitives preserved. |
| **Phase 4** — macOS Chrome | ✅ **Complete** | 100% | Full rip-and-replace in commit `27b29e7`. Menu bar (24px, clock, active app name), dock (magnification, running dots, pinned/running separator), traffic-light window decorations (12px circles, hover glyphs, 8px corner radius). Old taskbar/start menu deleted. App ID system (`SYSCALL_SURFACE_SET_APP_ID`) added. |
| **Phase 5** — Window Interactions | 🟡 **Partial** | ~75% | Window move ✅, resize ✅ (wlroots SSD model, 8-edge detection, Wayland content model, throttled Configure events), 8 directional resize cursors ✅, Super+LMB move ✅. Remaining: scroll wheel, grab-hand cursor. |
| **Phase 6** — Widget Toolkit | ❌ **Not started** | 0% | No `widgets/` directory exists. |

**Next milestone**: Phase 5 remaining (scroll wheel via PS/2 IntelliMouse protocol).

---

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
| **Drawing primitives** | ✅ Full set + AA | `gfx/src/canvas_ops.rs` — rect, circle, triangle, line, fill (aliased) + `line_aa`, `circle_aa`, `rounded_rect`, `rounded_rect_filled` |
| **Font rendering** | ✅ TTF + bitmap | `font/` crate — full TTF parser, coverage rasterizer, LRU cache, `GlyphAtlas`, VGA bitmap fallback. Compositor title bars use TTF; kernel console uses bitmap→coverage atlas with RCU hot-swap via `SYS_FONT_SET`. |
| **Alpha blending** | ✅ Complete | `gfx/src/blend.rs` — `alpha_blend`, `blend_coverage`, `put_pixel_blended`, `fill_rect_blended`, `fill_rect_blended_clipped` + 10 unit tests. Wired into compositor: drop shadows + semi-transparent title bars. |
| **Anti-aliasing** | ✅ Done | `gfx/src/canvas_ops.rs` — Xiaolin Wu line, AA circle, AA rounded rect (outline + filled). Blending backend in `gfx/src/blend.rs`. |
| **Input system** | ✅ Mature | `drivers/src/input_event.rs` — per-task ring buffers, focus routing, clipboard |
| **Window surfaces** | ✅ SHM-backed | `userland/src/appkit/surface.rs` — DrawBuffer, present_full/present_region |
| **Event loop** | ✅ Working | `userland/src/appkit/run.rs` — WindowedApp trait, poll→dispatch→redraw→yield |
| **PS/2 mouse** | ⚠️ No scroll | `drivers/src/ps2/mouse.rs` — 3-byte packets, no IntelliMouse scroll |
| **Window resize** | ✅ Working | wlroots SSD model: `start_resize`/`update_resize`/`stop_resize` in `compositor/input.rs`, edge detection in `decorations.rs`, Configure events via `SYSCALL_SEND_CONFIGURE` (151), frame/buffer separation in `compositor_context.rs`, Wayland content model in `renderer.rs`, deferred SHM destruction in `mm/shared_memory.rs`, min size 200×150. Shell handles resize with realloc + unconditional redraw. |
| **Window move** | ✅ Working | Title-bar drag via `start_drag`/`update_drag`/`stop_drag` + Super+LMB on content area (wlroots/Sway pattern, commit `28e4260`) |
| **Cursor shapes** | ✅ 11 shapes | Default, Text, Pointer, N/S/E/W/NW/NE/SW/SE resize — defined in `abi/src/window.rs`, rendered as pixel-art bitmaps in `renderer.rs` |
| **Theme system** | ⚠️ Windows-style | `userland/src/theme.rs` — "Dark Roulette Theme", Windows 10/11 dark palette |
| **Pixel formats** | ✅ 6 formats | ARGB8888, XRGB8888, RGB888, BGR888, RGBA8888, BGRA8888 |
| **Shared memory** | ✅ Wayland-style | `mm/src/shared_memory.rs` — 64 buffers, acquire/release/refcount |
| **Font assets** | ✅ Bundled | `assets/fonts/Inter-Regular.ttf` + `JetBrainsMono-Regular.ttf`; `build_fs_image.sh` copies to `/usr/share/fonts/` |
| **Scheduler** | ⚠️ No GUI boost | 4 static priority levels, no interactive promotion on I/O wakeup |
| **Memory** | ✅ Mature | Buddy + slab, mmap, COW, demand paging, ASLR |
| **Filesystem** | ✅ ext2 R/W | Can load fonts and assets from disk |
| **Syscalls** | ✅ 148+ defined | Surface, SHM, input, window management, process, memory, `SYS_FONT_SET` |

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

**Location**: `font/` crate (workspace member, `no_std` compatible) — ✅ **IMPLEMENTED**

```
font/
├── Cargo.toml          # slopos-font, deps: slopos-abi, slopos-gfx, libm; feature "kernel" for RCU atlas
└── src/
    ├── lib.rs           # Public API: FontSource, FontRenderer, draw_text(), measure_text() (569 lines)
    ├── ttf_parser.rs    # TrueType table parsing: head, maxp, cmap (Fmt 4), hhea, hmtx, loca, glyf (534 lines)
    ├── outline.rs       # Y-flip, font-unit→pixel scaling, implied on-curve midpoints, Bézier subdivision (159 lines)
    ├── rasterizer.rs    # Non-zero winding, 8× vertical supersampling, coverage bitmap output (135 lines)
    ├── cache.rs         # LRU glyph cache, 512 entries, keyed on (codepoint, size_px) (95 lines)
    ├── metrics.rs       # measure_text() — single-line width/height from hmtx advances (31 lines)
    ├── bitmap.rs        # VGA 8×16 bitmap fallback (256 glyphs), bitmap_to_coverage() (412 lines)
    └── atlas.rs         # GlyphAtlas: pre-rasterized fixed-width grid for console/terminal. RCU global
                         #   singleton (kernel feature), draw_char/draw_str, blend helpers (705 lines)
```

**Key design decisions** (all implemented as planned):
- Parse TTF from a `&[u8]` slice (loaded from ext2 via VFS)
- Rasterize glyphs on-demand into an LRU cache
- Output: `&[u8]` coverage bitmap (0–255 per pixel) — caller alpha-blends with foreground color
- No hinting (too complex for v1) — rely on supersampling for quality
- No complex text layout (no RTL, no ligatures, no shaping) — simple left-to-right for v1
- Support multiple font sizes via cache key `(codepoint, size_px)`

**Beyond-plan additions**:
- `bitmap.rs` — embedded VGA 8×16 font with 1-bit→coverage conversion for kernel boot
- `atlas.rs` — `GlyphAtlas` pre-rasterized monospace grid for terminal rendering; RCU-protected global singleton with atomic hot-swap via `replace_global()` + generation counter
- `FontSource` enum — `Embedded`, `Filesystem`, `Syscall`, `BitmapFallback` for provenance tracking
- Two rendering paths: variable-width `FontRenderer::draw_text()` and fixed-width `GlyphAtlas::draw_char()`
- Kernel font upgrade: boot starts with VGA bitmap atlas → userspace can hot-swap via `SYS_FONT_SET` syscall

### 3.3 Alpha Blending Architecture

**Location**: `gfx/src/blend.rs` — ✅ **COMPLETE** (math + compositor wiring)

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

### Phase 1: TTF Font Rasterizer (Visual Foundation) — ✅ COMPLETE
**Goal**: Replace the 8×16 bitmap font with anti-aliased TrueType text rendering.

**Acceptance criteria**:
- [x] New `font/` crate parses a standard TTF file (e.g., Inter, Noto Sans)
- [x] Rasterizes glyphs at arbitrary pixel sizes with coverage-based anti-aliasing
- [x] LRU glyph cache avoids re-rasterizing frequently used glyphs
- [x] `draw_text()` function renders a string onto any `Canvas` target
- [x] `measure_text()` returns width and height for layout purposes
- [x] Compositor title bars use TTF text (with bitmap fallback)
- [ ] ~~Menu bar and dock use TTF text~~ → deferred to Phase 4 (menu bar/dock don't exist yet)
- [x] Shell text remains bitmap (kernel/TTY doesn't need TTF)
- [x] A `.ttf` font file is loaded from the ext2 filesystem at compositor startup
- [x] `just boot` shows the new font in window title bars

**What was built** (2640+ lines across 8 files):
- `font/Cargo.toml` — workspace crate with `kernel` feature for RCU atlas
- `font/src/lib.rs` (569 lines) — `FontSource`, `FontRenderer`, `draw_text()`, `rasterize_glyph()`
- `font/src/ttf_parser.rs` (534 lines) — head, maxp, cmap (Format 4), hhea, hmtx, loca, glyf; simple + compound glyphs
- `font/src/outline.rs` (159 lines) — Y-flip, scaling, implied midpoints, Bézier subdivision → Edge list
- `font/src/rasterizer.rs` (135 lines) — non-zero winding, 8× vertical supersampling
- `font/src/cache.rs` (95 lines) — LRU, 512 entries, `(codepoint, size_px)` key
- `font/src/metrics.rs` (31 lines) — `measure_text()` via hmtx advances
- `font/src/bitmap.rs` (412 lines) — **bonus**: VGA 8×16 embedded font, `bitmap_to_coverage()`
- `font/src/atlas.rs` (705 lines) — **bonus**: `GlyphAtlas` pre-rasterized grid, RCU global, kernel console upgrade path

**Wiring completed**:
- `Cargo.toml` (workspace) — `font` member added
- `userland/Cargo.toml` — `slopos-font` dependency added
- `userland/src/apps/compositor/renderer.rs` — `FontRenderer` used for title bar text at 14px; bitmap fallback on failure
- `userland/src/apps/init_process.rs` — uses `GlyphAtlas` to upgrade kernel console font
- `assets/fonts/Inter-Regular.ttf` + `JetBrainsMono-Regular.ttf` bundled
- `scripts/build_fs_image.sh` — copies `*.ttf` to `/usr/share/fonts/` in ext2 image
- Kernel boot: `video::kernel_font::init()` → `init_global_bitmap()` → VGA coverage atlas
- Runtime upgrade: `SYS_FONT_SET` syscall accepts bitmap or pre-rasterized coverage data

**Remaining gap** (minor, not blocking):
- Taskbar buttons, start menu items, and close/minimize button labels still use bitmap `gfx::draw_str_clipped()`. This will be resolved when Phase 4 replaces the Windows-style chrome entirely.

---

### Phase 2: Alpha Blending & Compositing — ✅ COMPLETE
**Goal**: Enable transparency, shadows, and layered compositing in the window compositor.

**Acceptance criteria**:
- [x] `alpha_blend(src, dst) -> u32` function — implemented in `gfx/src/blend.rs:20` (Porter-Duff source-over, straight alpha)
- [x] `blend_coverage(coverage, fg, dst) -> u32` — implemented in `gfx/src/blend.rs:67`
- [x] Compositor renders windows back-to-front with per-pixel alpha
- [x] Window shadows visible — `draw_window_shadow()` in `renderer.rs:557` with 12px spread, 4px Y-offset, quadratic alpha falloff
- [x] Semi-transparent title bars — `draw_title_bar()` uses `fill_rect_blended_clipped()` with 0xD0 alpha tint
- [x] `just boot` shows windows with visible drop shadows

**What was built**:
- `gfx/src/blend.rs` — ✅ **COMPLETE** (318+ lines, 10 unit tests)
  - `alpha_blend(src, dst) -> u32` — Porter-Duff source-over
  - `blend_coverage(coverage, fg, dst) -> u32` — font AA blending
  - `put_pixel_blended(canvas, x, y, color)` — single-pixel RMW blend
  - `put_pixel_coverage(canvas, x, y, color, coverage)` — coverage-weighted blend
  - `fill_rect_blended(canvas, x, y, w, h, color)` — rect with opaque fast-path
  - `fill_rect_blended_clipped(canvas, x, y, w, h, color, clip)` — clipped rect fill (added in `51c3c7a`)
- `abi/src/draw.rs` — `Color32::alpha()` accessor + `Canvas::read_encoded_at()` for RMW reads

**Compositor wiring** (completed in commit `51c3c7a`, 2026-03-24):
- `userland/src/apps/compositor/renderer.rs` — `draw_window_shadow()` renders concentric 1px frames with quadratic alpha falloff via `fill_rect_blended_clipped()`; `draw_title_bar()` uses blended fill for semi-transparent tint (focused: `0x2D2D30D0`, unfocused: `0x1E1E1ED0`)
- `userland/src/apps/compositor/output.rs` — damage rect calculation expanded by `SHADOW_SPREAD` to include shadow bounds
- `userland/src/theme.rs` — added `SHADOW_SPREAD`, `SHADOW_OFFSET_Y`, `SHADOW_MAX_ALPHA`, `COLOR_TITLE_BAR_TINT`, `COLOR_TITLE_BAR_FOCUSED_TINT`
- Shadows and title bars rendered in both full-render and partial/damage-tracked paths

**Design note**: The plan originally called for `alpha_blend`/`blend_coverage` in `abi/src/draw.rs`, but they were placed in `gfx/src/blend.rs` instead — this is architecturally cleaner since ABI is the kernel-userland boundary and blending is a userland-only concern.

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — wait for the compositor to start with 2+ windows open.
3. **PASS condition — shadows**: Each window has a visible drop shadow. The shadow must be a soft gradient (not a hard black border) visible on at least 2 sides of the window. Overlapping windows show the shadow of the front window over the content of the back window.
4. **PASS condition — transparency**: The window title bar area has a semi-transparent tint — the background or window content behind it is partially visible through it, confirming alpha blending is active.
5. **FAIL condition**: Windows have no shadows, OR windows are rendered as fully opaque rectangles with no transparency, OR the compositor frame rate drops below 30fps (check serial output for late-frame warnings).
6. Run `just test` — all existing kernel tests must still pass.

---

### Phase 3: Anti-Aliased Drawing Primitives — ✅ COMPLETE
**Goal**: Replace aliased Bresenham primitives with smooth anti-aliased versions.

**Acceptance criteria**:
- [x] `line_aa(canvas, x0, y0, x1, y1, color)` — Xiaolin Wu's algorithm (`canvas_ops.rs:390`), fixed-point 8.8 gradient
- [x] `circle_aa(canvas, cx, cy, radius, color)` — integer-only distance + coverage (`canvas_ops.rs:488`)
- [x] `rounded_rect(canvas, x, y, w, h, radius, color)` — AA corners, integer-only (`canvas_ops.rs:567`)
- [x] `rounded_rect_filled(canvas, x, y, w, h, radius, color)` — filled variant (`canvas_ops.rs:666`)
- [x] Existing aliased primitives remain available (for performance-critical paths)
- [x] Compositor window frames use `rounded_rect` for corner radius → deferred to Phase 4 (compositor still renders Windows-style sharp rectangles)

**What was built**:
- All 4 AA primitives in `gfx/src/canvas_ops.rs`
- Supporting helpers: `isqrt()` (Newton's method), `circle_coverage()`, `circle_coverage_inner()`, `circle_should_step_y()`
- All AA primitives use `put_pixel_coverage()` from `gfx/src/blend.rs` for sub-pixel blending

**Not in scope** (noted for future):
- `circle_filled_aa` — no AA filled circle yet (only outline)
- `triangle_filled_aa` — no AA filled triangle
- `ellipse_aa` / `arc_aa` — no ellipse/arc AA
- Thick AA lines — `line_aa` is 1px only

---

### Phase 4: macOS Chrome — Menu Bar & Dock — ✅ COMPLETE
**Goal**: Replace the Windows-style taskbar + start menu with a macOS-inspired menu bar and dock.

**Acceptance criteria**:
- [x] **Top menu bar** rendered by compositor:
  - Left: SlopOS icon (green circle) + active app name (TTF 13px, bitmap fallback)
  - Center: (empty for now — app menus come later)
  - Right: Uptime clock (HH:MM:SS since boot, auto-updating each second)
  - Semi-transparent background (`PANEL_BG` 0x1A1A1C @ 0xCC alpha) with 1px bottom border
  - Height: 24px (`SYSTEM_BAR_HEIGHT`)
- [x] **Bottom dock** rendered by compositor:
  - Centered row of app icons (Shell, File Manager, System Monitor)
  - Icons are 48×48 base size with 10px corner radius
  - Label appears on hover above icon (11px font in semi-transparent rounded-rect pill)
  - 2px vertical separator between pinned and running apps
  - Semi-transparent rounded-rect background (`SHELF_BG` 0x1A1A1C @ 0xB0 alpha, 12px radius)
  - Magnification effect on hover (quadratic scaling, 48→64px, 120px/80px proximity zones)
  - Running indicator dots (4px diameter, light gray) below each running app
- [x] **Window decorations**:
  - Thin title bar (28px) with centered title (TTF 14px, bitmap fallback, ellipsis for long titles)
  - Traffic-light buttons at top-left (12px circles: red #FF5F57, yellow #FFBD2E, green #28C840)
  - Close (red), Minimize (yellow), Maximize (green — no-op, stretch goal)
  - Hover glyphs: X (close), − (minimize), + (maximize) rendered with `line_aa()`
  - Focused/unfocused states: colored vs gray (0x3E3E42), glyphs only on focused + hovered
  - 8px corner radius on window frame with AA arc boundaries
- [x] **Old taskbar and start menu code deleted** — `taskbar.rs` removed, zero references remain
- [x] `just boot` shows the new macOS-style desktop
- [x] **Bonus: App ID system** — `SYSCALL_SURFACE_SET_APP_ID` (149) allows apps to declare identity for reliable dock matching (`org.slopos.shell`, `org.slopos.files`, `org.slopos.sysmon`)

**What was built** (commit `27b29e7`, 2272 insertions / 828 deletions across 22 files):

Files created (3):
- `userland/src/apps/compositor/menu_bar.rs` (286 lines) — system bar state, rendering, clock, active app name, hit-testing
- `userland/src/apps/compositor/dock.rs` (709 lines) — shelf state, icon layout, magnification math, pinned/running entries, labels, hit-testing
- `userland/src/apps/compositor/decorations.rs` (585 lines) — title bar, traffic-light buttons, AA corner arcs, hover glyphs, hit-testing

Files substantially modified:
- `userland/src/apps/compositor/mod.rs` — main loop integration for menu bar + dock + decorations
- `userland/src/apps/compositor/input.rs` — hit-testing priority chain: system bar → shelf → signal buttons → title bar → content → desktop
- `userland/src/apps/compositor/renderer.rs` — full rendering pipeline overhaul (background → shadows → content → decorations → shelf → system bar → cursor)
- `userland/src/theme.rs` (+246 lines) — complete macOS-inspired palette: panel colors/alpha, signal button colors/positions, shelf dimensions, magnification constants, title bar focused/unfocused tints

Files deleted:
- `userland/src/apps/compositor/taskbar.rs` (129 lines removed)

Supporting changes:
- `abi/src/window.rs` — `AppId` newtype (32-byte)
- `abi/src/syscall/numbers.rs` — `SYSCALL_SURFACE_SET_APP_ID` (149)
- `core/src/syscall/ui_handlers.rs` — app_id storage in surface
- `userland/src/appkit/window.rs` — `Window::set_app_id()` wrapper
- `userland/src/apps/compositor/hover.rs` (7 lines) — signal group hover tracking

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

### Phase 5: Window Interactions — 🟡 PARTIAL (~75%)
**Goal**: Make windows movable, resizable, and scrollable.

**Acceptance criteria**:
- [x] **Window move**: Drag title bar to reposition window
  - ✅ Compositor tracks drag state via `start_drag()`/`update_drag()`/`stop_drag()` in `compositor/input.rs`
  - ✅ Window position updates in real-time via `window::set_window_position()`
  - ✅ Damage tracking handles the moving window's old and new positions
- [x] **Super+LMB interactive move**: Hold Super + left-click on window content to move (commit `28e4260`)
  - ✅ wlroots/Sway modifier-based interaction pattern
  - ✅ Reuses existing drag state machine — no new syscalls needed
- [x] **Window resize**: Drag window edges or corners to resize
  - ✅ wlroots SSD model: `start_resize()`/`update_resize()`/`stop_resize()` state machine in `compositor/input.rs:507-602`
  - ✅ Edge detection via labwc `ssd_get_resizing_type()` algorithm in `decorations.rs:111-198` — 12px shadow grab zone, corner range = `WINDOW_CORNER_RADIUS × 3`, corners take priority
  - ✅ Two new syscalls: `SYSCALL_SET_WINDOW_SIZE` (150) sets frame dimensions, `SYSCALL_SEND_CONFIGURE` (151) notifies client of new size
  - ✅ Frame/buffer separation: `WindowInfo` has both `width/height` (committed buffer) and `frame_width/frame_height` (resize target) in `abi/src/window.rs:54-130`
  - ✅ Wayland content model in `renderer.rs:685-766` — always blit last committed buffer clipped to `min(buffer, frame)`, fill gap with placeholder color during grow
  - ✅ Throttled Configure events (~100ms during drag, final on release) in `input.rs:565`
  - ✅ Minimum window size enforced: 200×150 (`theme.rs:31-32`)
  - ✅ Client-side handling: `appkit/surface.rs:108-137` allocates new SHM buffer + re-attaches; old buffer uses deferred destruction (`mm/shared_memory.rs:516-600`) if compositor still has a read-only mapping
  - ✅ Shell resize: `shell/display.rs:796-845` recalculates cols/rows, unconditionally redraws (fixes black screen on cell-count-unchanged resize), clamps cursor, adjusts view_top (commit `2c8dceb`)
- [x] **Cursor shapes**: 11 shapes (exceeds the planned 6)
  - ✅ Default (arrow), Text (I-beam), Pointer (hand), N/S/E/W/NW/NE/SW/SE resize — `abi/src/window.rs:9-19`
  - ✅ 8 directional resize cursor bitmaps (pixel-art arrows) embedded in `renderer.rs:388-482`
  - ✅ Hover feedback: `update_resize_cursor()` in `input.rs:606-649` updates cursor on z-order hit-test each frame
- [ ] **Scroll wheel**: PS/2 IntelliMouse 4-byte protocol
  - Mouse driver sends scroll events via `input_route_pointer_button()` or new scroll event type
  - `InputEventType::Scroll` added to ABI
  - Compositor forwards scroll events to focused window

**What was built** (commits `29c3fcd`, `e5556e8`, `2c8dceb`, `28e4260`):

Files substantially modified:
- `userland/src/apps/compositor/input.rs` — `ResizeEdge` bitfield (lines 19-73), `InputHandler` resize state (lines 75-124), resize state machine (lines 507-602), hover cursor update (lines 606-649), Super+LMB move (line 419)
- `userland/src/apps/compositor/decorations.rs` — `hit_test_resize_edge()` (lines 111-198) — labwc algorithm with shadow grab zone + adaptive corner ranges
- `userland/src/apps/compositor/renderer.rs` — Wayland content model (lines 685-766), 8 directional resize cursor bitmaps (lines 388-482), placeholder gap fill for frame > buffer
- `userland/src/apps/shell/display.rs` — `shell_console_resize()` (lines 796-845) — unconditional redraw, view_top adjustment
- `userland/src/appkit/surface.rs` — `resize()` (lines 108-137) — new SHM buffer alloc + deferred destruction of old
- `abi/src/window.rs` — `WindowInfo` frame_width/frame_height fields, `effective_width()`/`effective_height()` methods, 8 new cursor shape constants
- `abi/src/input.rs` — `InputEvent::configure()` constructor (lines 194-205), `InputEventType::Configure` (value 8)
- `abi/src/syscall/numbers.rs` — `SYSCALL_SET_WINDOW_SIZE` (150), `SYSCALL_SEND_CONFIGURE` (151)
- `core/src/syscall/ui_handlers.rs` — `syscall_set_window_size()`, `syscall_send_configure()` handlers
- `video/src/compositor_context.rs` — `set_window_size()` (lines 570-588), frame dims in `SurfaceState`, exported in `surface_enumerate_windows()`
- `mm/src/shared_memory.rs` — `shm_destroy()` deferred destruction (lines 516-600) — owner mappings removed, compositor read-only mapping keeps pages alive

**Remaining work**:
- `drivers/src/ps2/mouse.rs` — 4-byte IntelliMouse packet support, scroll events
- `abi/src/input.rs` — add `Scroll` event type (or reuse pointer button codes 4/5)
- Compositor scroll event forwarding to focused window

**Estimated remaining effort**: ~200-300 lines (scroll wheel only).

**QA scenario**:
1. Run `just build` — must compile with zero errors.
2. Run `VIDEO=1 just boot` — open a GUI app window.
3. **PASS condition — window move**: Click and hold on the window title bar, drag the mouse. The window follows the cursor in real-time. Release the mouse — the window stays at the new position. No ghost artifacts at the old position (damage tracking handles cleanup).
4. **PASS condition — Super+LMB move**: Hold Super key and left-click anywhere on window content, drag. The window follows the cursor. This works even when the cursor is not on the title bar.
5. **PASS condition — window resize**: Move the cursor to the right edge of a window — cursor changes to a horizontal resize arrow. Click and drag — the window width changes. The app content re-renders at the new size. Repeat for bottom edge (vertical resize) and bottom-right corner (diagonal resize). Minimum size (200×150) is enforced — dragging smaller snaps to minimum. During resize growth, a placeholder gap is visible until the client re-renders. During resize shrink, content is cropped at the frame edge.
6. **PASS condition — resize cursor feedback**: Hovering each window edge/corner shows the appropriate directional resize cursor (8 directions). Cursor reverts to default arrow when leaving the resize zone. Maximized or minimized windows do not show resize cursors.
7. **PASS condition — shell resize**: Resize a shell/terminal window. The terminal recalculates columns/rows and redraws with the new dimensions. No black screen or stale content.
8. **PASS condition — scroll wheel**: *(Not yet testable — PS/2 IntelliMouse not implemented.)*
9. **FAIL condition**: Windows cannot be moved by dragging title bar, OR resize does nothing, OR cursor never changes shape, OR shell shows black screen after resize.
10. Run `just test` — all existing kernel tests must still pass.

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

| Risk | Status | Mitigation |
|------|--------|------------|
| TTF parser complexity (tables, edge cases) | ✅ **Resolved** | Implemented with Inter Regular + JetBrains Mono. Supports cmap Format 4, simple + compound glyphs. No hinting, no ligatures, no complex shaping — exactly as planned. |
| Alpha blending performance | ✅ **Resolved** | Wired into compositor (`51c3c7a`). Damage tracking includes shadow bounds. No frame-rate regressions reported. Fast-path for fully opaque pixels. |
| Compositor architecture debt | ✅ **Resolved** | macOS chrome rip-and-replace complete (Phase 4). Underlying SHM + damage + surface architecture extended cleanly for resize (frame/buffer separation, deferred destruction). |
| Font file loading from ext2 | ✅ **Resolved** | Proven working — compositor loads `Inter-Regular.ttf` from `/usr/share/fonts/` at startup. Build script automates deployment. |
| Window resize complexity | ✅ **Resolved** | Full wlroots SSD model landed across 4 commits (`29c3fcd`, `e5556e8`, `2c8dceb`, `28e4260`). Frame/buffer separation, Configure events, deferred SHM destruction, Wayland content model, 8-edge detection, throttled events, min-size enforcement. Shell handles resize cleanly. |
| Glyph cache memory | ✅ **Resolved** | LRU with 512 entries. At 16px, ~256 bytes/glyph = ~128KB max. Well within budget. |

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
Phase 1 (Fonts) ───────── ✅ COMPLETE ─────────┐
                                                  │
Phase 2 (Alpha Blending) ─ ✅ COMPLETE ───────────┤
                                                  ├──▶ Phase 4 (macOS Chrome) ✅ COMPLETE
Phase 3 (AA Primitives) ── ✅ COMPLETE ───────────┘         │
                                                            │
                           Phase 5 (Interactions) 🟡 ◀──────┘
                                   │       (move ✅, resize ✅, cursors ✅, scroll ❌)
                                   ▼
                           Phase 6 (Widgets) ❌
```

Phases 1–4 are **done** — all rendering foundations and macOS chrome are in place. Phase 5 is ~75% complete: move, resize, and cursor shapes are fully implemented. Only scroll wheel remains (PS/2 IntelliMouse 4-byte protocol). Phase 6 (widget toolkit) is fully unblocked but not started.

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
