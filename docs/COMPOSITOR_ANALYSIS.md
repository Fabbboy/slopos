# SlopOS Compositor & Surface System Analysis

**Date**: 2024-12-24
**Branch**: `claude/analyze-slopos-compositor-xatad`
**Status**: WIP (Recent commits: a424687, 6735c6b, 3b3841a)

---

## Executive Summary

SlopOS implements a **Wayland-inspired compositor architecture** with kernel-side surface management and userland window composition. The current implementation has the foundational pieces in place but requires refinement in several areas to achieve Wayland-like efficiency and correctness.

**Key Findings:**
- Damage tracking is implemented but has edge cases with partial region handling
- Buffer management is single-buffered (no double/triple buffering)
- No explicit synchronization primitives between clients and compositor
- Window management is task-based rather than surface-based
- No atomic commit mechanism for surface state changes

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Current Implementation Analysis](#2-current-implementation-analysis)
3. [Wayland Comparison](#3-wayland-comparison)
4. [Identified Gaps](#4-identified-gaps)
5. [Checkpoints / Work Items](#5-checkpoints--work-items)

---

## 1. Architecture Overview

### 1.1 Layer Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                     USERLAND COMPOSITOR                          │
│                  (userland/src/compositor.rs)                    │
│  - Window enumeration & tracking                                 │
│  - Mouse/keyboard input handling                                 │
│  - Damage region calculation (content, move, cursor)             │
│  - Title bar & taskbar rendering                                 │
│  - Z-order management                                            │
└──────────────────────────┬──────────────────────────────────────┘
                           │ Syscalls (27, 30-36)
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                      KERNEL VIDEO BRIDGE                         │
│                  (drivers/src/video_bridge.rs)                   │
│  - Callback registration                                         │
│  - DamageRegion struct                                           │
│  - WindowInfo struct                                             │
└──────────────────────────┬──────────────────────────────────────┘
                           │ Function callbacks
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                    KERNEL SURFACE MANAGER                        │
│                    (video/src/surface.rs)                        │
│  - Per-task surface allocation                                   │
│  - Dirty rectangle tracking                                      │
│  - compositor_present() / compositor_present_with_damage()       │
│  - Window state (position, z-order, minimize)                    │
└──────────────────────────┬──────────────────────────────────────┘
                           │ Direct memory access
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                       FRAMEBUFFER                                │
│                 (video/src/framebuffer.rs)                       │
│  - Limine-provided physical address                              │
│  - HHDM-mapped for kernel access                                 │
│  - Pixel format conversion (RGB/BGR)                             │
└─────────────────────────────────────────────────────────────────┘
```

### 1.2 Key Data Structures

| Structure | Location | Purpose |
|-----------|----------|---------|
| `Surface` | `video/src/surface.rs:31` | Per-task rendering buffer with dirty tracking |
| `SurfaceSlot` | `video/src/surface.rs:73` | Wrapper adding task ID, z-order, window state |
| `DamageRegion` | `drivers/src/video_bridge.rs:36` | Screen-space rectangle for compositor |
| `WindowInfo` | `video/src/surface.rs:1123` | Exported window metadata for userland |
| `WindowManager` | `userland/src/compositor.rs:71` | Userland state machine for window management |

### 1.3 Syscall Interface

| Syscall # | Name | Purpose |
|-----------|------|---------|
| 27 | `COMPOSITOR_PRESENT` | Full framebuffer composition (legacy) |
| 30 | `ENUMERATE_WINDOWS` | Get window list with dirty state |
| 31 | `SET_WINDOW_POSITION` | Move window (compositor only) |
| 32 | `SET_WINDOW_STATE` | Minimize/restore window |
| 33 | `RAISE_WINDOW` | Bring window to front (z-order) |
| 34 | `FB_FILL_RECT` | Direct framebuffer draw (compositor) |
| 35 | `FB_FONT_DRAW` | Direct framebuffer text (compositor) |
| 36 | `COMPOSITOR_PRESENT_DAMAGE` | Wayland-style damage composition |

---

## 2. Current Implementation Analysis

### 2.1 Surface Allocation (`surface.rs:166-269`)

**Current Behavior:**
- Surfaces are allocated on-demand when a task first draws
- Allocation tries framebuffer dimensions, then falls back to 800x600, 640x480, 320x240
- Physical pages allocated via buddy allocator, mapped via HHDM
- Cascading window position: `(100 + (active_count * 32) % 200)`
- Z-order assigned via atomic counter (monotonically increasing)

**Observations:**
- [x] Lazy allocation reduces memory pressure
- [ ] No way to request specific surface dimensions
- [ ] Surface size locked at creation time (no resize)
- [ ] No surface destruction API (only task termination)

### 2.2 Dirty Rectangle Tracking (`surface.rs:131-164`)

**Current Behavior:**
```rust
fn mark_dirty(surface: &mut Surface, x0, y0, x1, y1) {
    // Clips to surface bounds
    // First mark: initialize bounds
    // Subsequent marks: expand bounding box
    surface.dirty = true;
    surface.dirty_x0 = min(existing, new);
    // ...etc
}
```

**Observations:**
- [x] Efficient single bounding box per surface
- [x] Clips to surface bounds correctly
- [ ] Bounding box can grow excessively with scattered draws
- [ ] No per-region list (Wayland uses region lists)
- [ ] No way to query dirty state without enumeration

### 2.3 Compositor Present with Damage (`surface.rs:798-976`)

**Current Behavior:**
1. For each damage region (max 64):
   - For each window (sorted by z-order, back-to-front):
     - Check rectangle overlap
     - Calculate intersection
     - Copy pixels from surface buffer to framebuffer
2. Clear dirty flags for all composited windows

**Observations:**
- [x] Correct back-to-front ordering
- [x] Intersection calculation is correct
- [ ] Clears ALL dirty flags, even if only partial region was composited
- [ ] No occlusion culling (draws fully obscured windows)
- [ ] Bubble sort for z-order is O(n²) - acceptable for small N

### 2.4 Userland Compositor Loop (`compositor.rs:672-793`)

**Current Behavior:**
```
1. Update mouse state
2. Refresh window list (detects taskbar state changes)
3. Handle mouse events (drag, click, minimize)
4. Calculate content damage (dirty windows + taskbar)
5. Calculate move damage (position changes)
6. Get cursor damage (old position)
7. Clear content + move damage regions
8. Composite all damage via syscall
9. Redraw title bars that overlap damage
10. Redraw taskbar if overlaps damage
11. Draw cursor
12. Yield to scheduler
```

**Observations:**
- [x] Three-tier damage system (content, move, cursor)
- [x] Position changes don't mark surface dirty (a424687)
- [x] Taskbar redraws only on state change
- [ ] No frame timing (runs as fast as possible)
- [ ] No VSync integration
- [ ] Clears then redraws (could blit existing pixels for moves)

### 2.5 Window Position Updates (`surface.rs:1081-1092`)

**Current Behavior:**
```rust
pub fn surface_set_window_position(task_id, x, y) {
    slots[slot_idx].surface.x = x;
    slots[slot_idx].surface.y = y;
    // NOTE: Don't mark dirty - userland tracks position changes
}
```

**Observations:**
- [x] Correct: position change != content change
- [x] Compositor detects position delta and adds screen damage
- [ ] No bounds checking (window can be positioned off-screen)
- [ ] No event notification to client about position change

---

## 3. Wayland Comparison

### 3.1 Wayland Core Concepts vs SlopOS

| Wayland Concept | SlopOS Equivalent | Gap |
|-----------------|-------------------|-----|
| `wl_surface` | `Surface` struct | No attach/commit cycle |
| `wl_buffer` | Inline in Surface | No separate buffer objects |
| `wl_region` (damage) | `DamageRegion` | Single rect vs region list |
| `wl_callback` (frame) | None | No frame timing |
| `wl_compositor` | Userland + kernel split | Correct architecture |
| Double buffering | None | Single buffer only |
| Atomic commit | None | Changes apply immediately |
| Subsurfaces | None | No surface hierarchy |

### 3.2 Wayland Damage Flow

```
Wayland Client:
  1. wl_surface_attach(buffer)      // Attach new buffer
  2. wl_surface_damage(x, y, w, h)  // Mark damaged regions
  3. wl_surface_commit()            // Atomic commit
  4. wl_callback_done()             // Wait for frame callback

Wayland Compositor:
  1. Receives commit
  2. Swaps buffer atomically
  3. Collects damage from all surfaces
  4. Composites only damaged regions
  5. Sends frame callbacks
```

### 3.3 SlopOS Damage Flow (Current)

```
SlopOS Client:
  1. surface_draw_*()               // Draw directly to buffer
  2. (implicit dirty marking)       // Kernel tracks dirty region

SlopOS Compositor:
  1. enumerate_windows()            // Get dirty flags
  2. Calculate damage regions
  3. Clear damage regions
  4. compositor_present_damage()    // Composite
  5. (kernel clears dirty flags)
```

**Key Differences:**
1. No explicit attach/commit cycle
2. No double buffering (clients draw to live buffer)
3. Damage calculated by userland, not accumulated by kernel
4. No frame callbacks for client pacing

### 3.4 Performance Implications

| Aspect | Wayland | SlopOS Current | Impact |
|--------|---------|----------------|--------|
| Buffer swap | Atomic, O(1) | N/A | Tearing possible |
| Damage tracking | Per-surface regions | Single bounding box | Over-composition |
| Client sync | Frame callbacks | None | No pacing |
| Compositor sync | VSync | None | Tearing, CPU spinning |

---

## 4. Identified Gaps

### 4.1 Critical (Correctness)

| ID | Gap | Current Behavior | Impact |
|----|-----|------------------|--------|
| C1 | Dirty flag cleared for partial composite | `compositor_present_with_damage` clears all dirty flags even if only partial region was composited | Lost updates if damage region doesn't cover entire dirty area |
| C2 | No input region tracking | Clicks go to any window based on position | Transparent areas should pass through |
| C3 | Surface buffer race | Client draws while compositor copies | Potential tearing/artifacts |

### 4.2 High (Performance)

| ID | Gap | Current Behavior | Impact |
|----|-----|------------------|--------|
| H1 | No occlusion culling | Draws fully obscured windows | Wasted bandwidth |
| H2 | Bounding box expansion | Single rect grows with scattered draws | Over-composition |
| H3 | Clear + redraw for moves | Clears old position, recomposites | Could blit existing pixels |
| H4 | No VSync | Runs as fast as possible | Tearing, CPU waste |
| H5 | Bubble sort for z-order | O(n²) each frame | Scales poorly |

### 4.3 Medium (Features)

| ID | Gap | Current Behavior | Impact |
|----|-----|------------------|--------|
| M1 | No double buffering | Single buffer | Client must coordinate |
| M2 | No surface resize | Fixed at creation | Can't resize windows |
| M3 | No subsurfaces | Flat hierarchy | No popups/tooltips |
| M4 | No frame callbacks | No client pacing | Overdraw, no latency control |
| M5 | No opacity/alpha | Fully opaque | No transparency |
| M6 | No transforms | Identity only | No rotation/scale |

### 4.4 Low (Polish)

| ID | Gap | Current Behavior | Impact |
|----|-----|------------------|--------|
| L1 | No window constraints | Can position anywhere | Windows can go off-screen |
| L2 | No focus change events | Client doesn't know | UI can't react |
| L3 | No cursor shapes | Fixed crosshair | No resize/text cursors |
| L4 | Title bar height hardcoded | 24px constant | No DPI awareness |

---

## 5. Checkpoints / Work Items

### Phase 1: Correctness Fixes

#### [x] Checkpoint 1.1: Fix Partial Damage Dirty Flag Handling ✅ **COMPLETED** (2025-12-24)
**Location**: `video/src/surface.rs:959-973`

**Problem**: Currently clears ALL dirty flags after any composition, even if only a partial region was composited.

**Solution**:
- Track which portions of dirty region were composited
- Only clear dirty flag if entire dirty region was covered
- Or: Don't clear dirty, let compositor enumerate again

**Files to modify**:
- `video/src/surface.rs` - Modify `compositor_present_with_damage()`
- Add intersection tracking per-surface

**Verification**:
- Create window, draw in corner A
- Trigger damage for corner B only
- Verify corner A still dirty on next enumerate

---

#### [x] Checkpoint 1.2: Add Basic Synchronization ✅ **COMPLETED** (2025-12-24)
**Location**: `video/src/surface.rs` (new)

**Problem**: Client can draw while compositor is copying, causing tearing.

**Solution** (minimal):
- Add `compositing: bool` flag to Surface
- Set during `compositor_present_with_damage`
- Drawing functions wait/skip if compositing

**Files to modify**:
- `video/src/surface.rs` - Add flag, check in draw functions
- Consider using atomic for lock-free check

**Verification**:
- Rapid drawing during composition doesn't show artifacts

---

### Phase 2: Performance Improvements

#### [x] Checkpoint 2.1: Implement Damage Region List ✅ **COMPLETED** (2025-12-24)
**Location**: `video/src/surface.rs:24-128`

**Problem**: Single bounding box grows with scattered draws.

**Solution**:
Implemented fixed-size array of 8 damage regions per surface with smart merging.
No backward compatibility fields - clean implementation:
```rust
const MAX_DAMAGE_REGIONS: usize = 8;

struct DamageRect { x0: i32, y0: i32, x1: i32, y1: i32 }

struct Surface {
    damage_regions: [DamageRect; MAX_DAMAGE_REGIONS],
    damage_count: u8,
    // ... other fields (no dirty/dirty_x0/y0/x1/y1)
}

impl Surface {
    fn is_dirty(&self) -> bool { self.damage_count > 0 }
    fn damage_union(&self) -> DamageRect { /* computes union on-the-fly */ }
}
```

When region array is full, `merge_smallest_pair()` combines the two regions
with smallest combined area, making room for new damage.

**Files modified**:
- `video/src/surface.rs` - DamageRect struct, Surface with damage_regions array, is_dirty()/damage_union() methods, add_damage_region(), merge_smallest_pair(), clear_damage_regions(), WindowInfo with damage_regions
- `userland/src/syscall.rs` - UserWindowDamageRect, UserWindowInfo with damage_regions and is_dirty() method
- `userland/src/compositor.rs` - get_content_damage_regions() iterates per-window regions

**Verification**:
- Build succeeds with `make build`
- Tests pass with `make test`
- Compositor uses per-window damage regions instead of single bounding box

---

#### [x] Checkpoint 2.2: Add Occlusion Culling ✅ **COMPLETED** (2025-12-24)
**Location**: `video/src/surface.rs:1231-1407`

**Problem**: Draws pixels that will be overwritten by higher windows.

**Solution**:
Implemented front-to-back rendering with `VisibleRegion` tracking:
- Added `VisibleRegion` struct with 16 rectangle slots
- Iterate windows highest z-order first (front-to-back)
- For each window, composite only visible (non-occluded) portions
- Subtract window bounds from visible region after compositing
- Early exit when damage region is fully occluded
- Merge smallest pair when region array overflows

**Files modified**:
- `video/src/surface.rs`:
  - Added `DamageRect::intersect()` method
  - Added `VisibleRegion` struct with `subtract()` and `merge_smallest_pair_static()`
  - Refactored `compositor_present_with_damage()` to use front-to-back iteration
  - Added debug logging (gated by `boot.debug=on`) for culling statistics

**Verification**:
- Build succeeds with `make build`
- Tests pass with `make test`
- Debug logging shows "compositor: occlusion culling: X composited, Y culled, Z early exits"

---

#### [ ] Checkpoint 2.3: Optimize Window Move (Blit Instead of Recomposite)
**Location**: `userland/src/compositor.rs:703-724`

**Problem**: Clears old position and recomposites everything.

**Solution**:
- For simple moves (no overlapping windows changed):
  - Use `framebuffer_blit()` to copy existing pixels
  - Only recomposite exposed regions

**Files to modify**:
- `userland/src/compositor.rs` - Add blit-based move path
- May need new syscall for safe framebuffer-to-framebuffer blit

**Verification**:
- Drag window smoothly
- Measure CPU usage before/after

---

#### [ ] Checkpoint 2.4: Replace Bubble Sort with Better Algorithm
**Location**: `video/src/surface.rs:679-688`

**Problem**: O(n²) sort every frame.

**Solution**:
- Keep surfaces sorted by z-order in the array
- Update on raise/create only
- Or: Use insertion sort (nearly sorted = O(n))

**Files to modify**:
- `video/src/surface.rs` - Maintain sorted order

**Verification**:
- Create 32 windows
- Verify composition time doesn't degrade

---

### Phase 3: Double Buffering

#### [ ] Checkpoint 3.1: Add Front/Back Buffer to Surface
**Location**: `video/src/surface.rs:31-46`

**Problem**: Client draws directly to compositor-visible buffer.

**Solution**:
```rust
struct Surface {
    front_buffer: *mut u8,  // Compositor reads from here
    back_buffer: *mut u8,   // Client draws here
    // ...
}
```

**Files to modify**:
- `video/src/surface.rs` - Allocate two buffers
- Add swap/commit function

---

#### [ ] Checkpoint 3.2: Add Commit Syscall
**Location**: New syscall

**Problem**: No way to atomically commit changes.

**Solution**:
- New syscall: `surface_commit(task_id)`
- Swaps front/back buffer pointers
- Damage applied atomically

**Files to modify**:
- `drivers/src/syscall_handlers.rs` - New handler
- `userland/src/syscall.rs` - New wrapper
- `video/src/surface.rs` - Commit logic

**Verification**:
- Draw complex scene in back buffer
- Commit
- Verify no partial state visible

---

### Phase 4: Frame Pacing

#### [ ] Checkpoint 4.1: Add Frame Callback Mechanism
**Location**: New

**Problem**: Clients don't know when to draw next frame.

**Solution**:
- Add frame callback request queue per surface
- After composition, signal waiting clients
- Clients call syscall to wait for callback

**Files to modify**:
- `video/src/surface.rs` - Callback queue
- `drivers/src/syscall_handlers.rs` - Wait syscall
- `sched/src/task.rs` - Task blocking for callback

---

#### [ ] Checkpoint 4.2: Add VSync Support
**Location**: Compositor loop

**Problem**: Compositor runs unbounded.

**Solution**:
- If VirtIO GPU: Use fence/sync objects
- Fallback: Timer-based 60Hz cap
- Integrate with frame callbacks

**Files to modify**:
- `drivers/src/virtio_gpu.rs` - VSync primitives
- `userland/src/compositor.rs` - Timing loop

---

### Phase 5: Advanced Features

#### [ ] Checkpoint 5.1: Surface Resize Support
**Location**: `video/src/surface.rs`

**Problem**: Surfaces fixed at creation size.

**Solution**:
- Add `surface_resize(task_id, w, h)` syscall
- Reallocate buffer (or use larger buffer with size tracking)
- Client redraws after resize

---

#### [ ] Checkpoint 5.2: Alpha Blending
**Location**: `video/src/surface.rs:926-937`

**Problem**: Surfaces fully opaque.

**Solution**:
- Add per-surface alpha
- Blend during composition: `dst = src*alpha + dst*(1-alpha)`
- SIMD optimization for blending

---

#### [ ] Checkpoint 5.3: Input Regions
**Location**: New

**Problem**: Transparent areas still receive input.

**Solution**:
- Per-surface input region (rectangle list)
- Hit test against input region, not surface bounds
- Pass-through to windows below

---

## Appendix A: File Reference

| File | Lines | Purpose |
|------|-------|---------|
| `video/src/surface.rs` | ~1664 | Core compositor, surface management, occlusion culling |
| `video/src/framebuffer.rs` | 381 | Framebuffer state, pixel operations |
| `video/src/graphics.rs` | 397 | Drawing primitives |
| `video/src/font.rs` | 841 | Text rendering |
| `userland/src/compositor.rs` | 794 | Userland window manager |
| `drivers/src/video_bridge.rs` | 345 | Kernel-userland video API |
| `drivers/src/syscall_handlers.rs` | 1000+ | Syscall dispatch |

## Appendix B: Recent Commits Analysis

### a424687: "WIP: compositor windowing position-based damage tracking"
- Changed `surface_set_window_position()` to NOT mark dirty
- Compositor now detects position changes and adds screen damage
- **Impact**: Window dragging doesn't trigger client redraw

### 6735c6b: "WIP: compositor windowing damage tracking improvements"
- Refinements to damage calculation
- Move damage separate from content damage

### 3b3841a: "compositor: implement Wayland-style damage tracking"
- Initial damage-based composition
- `compositor_present_with_damage()` added
- Three damage types: content, move, cursor

## Appendix C: Wayland Protocol References

For detailed Wayland comparison:
- `wl_surface.attach`: https://wayland.app/protocols/wayland#wl_surface:request:attach
- `wl_surface.damage`: https://wayland.app/protocols/wayland#wl_surface:request:damage
- `wl_surface.commit`: https://wayland.app/protocols/wayland#wl_surface:request:commit
- `wl_callback`: https://wayland.app/protocols/wayland#wl_callback

---

*This document is a living analysis. Update as checkpoints are completed.*
