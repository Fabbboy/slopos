# Compositor Refactor: Rust-Safe Surface Management

## Overview

This plan outlines the migration from the current unsafe raw-pointer-based surface system to a fully Rust-safe design using `Arc` for lifetime management and per-surface locking for fine-grained concurrency.

## Current State (Problematic)

### Architecture

```
static SURFACES: Spinlock<[SurfaceSlot; MAX_TASKS]>
```

- Single global array with fixed slots
- Raw pointers (`*mut u8`) for buffers
- Pointers escape lock scope → use-after-free
- Compositor copies entire array → stale pointers

### Files Involved

| File | Lines | Role |
|------|-------|------|
| `video/src/surface.rs` | ~1300 | Main surface/compositor implementation |
| `video/src/lib.rs` | ~50 | Module exports |
| `drivers/src/video_bridge.rs` | ~200 | Syscall bridge to video subsystem |
| `drivers/src/syscall_handlers.rs` | ~800 | Syscall dispatch |

---

## Target State (Rust-Safe)

### Architecture

```
static SURFACES: Spinlock<BTreeMap<u32, SurfaceRef>>

SurfaceRef = Arc<Surface>
```

- Dynamic map instead of fixed array
- `Arc` for automatic lifetime management
- `Box<[u8]>` for owned buffers
- Per-surface `Mutex` for fine-grained locking
- No raw pointers escape any scope

---

## Phase 1: Foundation Types

### 1.1 Create `OwnedBuffer` struct

**File:** `video/src/surface.rs`

```rust
use alloc::boxed::Box;

/// A buffer with owned memory - no raw pointers
pub struct OwnedBuffer {
    /// Owned pixel data - automatically freed on drop
    data: Box<[u8]>,
    /// Buffer dimensions (cached for fast access)
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    /// Damage tracking for this buffer
    damage: DamageRegion,
}

impl OwnedBuffer {
    pub fn new(width: u32, height: u32, bpp: u8) -> Result<Self, VideoError> {
        let bytes_pp = (bpp / 8) as usize;
        let pitch = (width as usize) * bytes_pp;
        let size = pitch * (height as usize);

        // Allocate zeroed buffer
        let data = vec![0u8; size].into_boxed_slice();

        Ok(Self {
            data,
            width,
            height,
            pitch,
            bytes_pp: bytes_pp as u8,
            damage: DamageRegion::empty(),
        })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    #[inline]
    pub fn pixel_offset(&self, x: u32, y: u32) -> usize {
        (y as usize) * self.pitch + (x as usize) * (self.bytes_pp as usize)
    }
}
```

### 1.2 Create `DoubleBuffer` struct

```rust
/// Double buffer pair for tear-free rendering
pub struct DoubleBuffer {
    /// Front buffer - compositor reads from this
    front: OwnedBuffer,
    /// Back buffer - client draws to this
    back: OwnedBuffer,
}

impl DoubleBuffer {
    pub fn new(width: u32, height: u32, bpp: u8) -> Result<Self, VideoError> {
        Ok(Self {
            front: OwnedBuffer::new(width, height, bpp)?,
            back: OwnedBuffer::new(width, height, bpp)?,
        })
    }

    /// Swap front and back buffers (called on commit)
    pub fn swap(&mut self) {
        core::mem::swap(&mut self.front, &mut self.back);
    }
}
```

### 1.3 Create new `Surface` struct

```rust
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

/// Thread-safe surface with interior mutability
pub struct Surface {
    // === Immutable after creation ===
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    pub pixel_format: PixelFormat,
    pub task_id: u32,

    // === Per-surface lock for buffer access ===
    buffers: Mutex<DoubleBuffer>,

    // === Atomic state (lock-free) ===
    dirty: AtomicBool,
    window_x: AtomicI32,
    window_y: AtomicI32,
    z_order: AtomicU32,
    visible: AtomicBool,
}

/// Reference-counted surface handle
pub type SurfaceRef = Arc<Surface>;
```

---

## Phase 2: Registry Migration

### 2.1 Replace global array with BTreeMap

**File:** `video/src/surface.rs`

**Before:**
```rust
const MAX_TASKS: usize = 64;

struct SurfaceSlot {
    active: bool,
    task_id: u32,
    surface: Surface,
    // ...
}

static SURFACES: Spinlock<[SurfaceSlot; MAX_TASKS]> = ...;
```

**After:**
```rust
use alloc::collections::BTreeMap;

/// Global surface registry
/// Lock held briefly only to insert/remove/lookup Arc
static SURFACES: Spinlock<BTreeMap<u32, SurfaceRef>> = Spinlock::new(BTreeMap::new());
```

### 2.2 Implement registry operations

```rust
/// Create a new surface for a task
pub fn surface_create(task_id: u32, width: u32, height: u32, bpp: u8) -> Result<(), VideoError> {
    let surface = Arc::new(Surface::new(task_id, width, height, bpp)?);

    let mut registry = SURFACES.lock();
    if registry.contains_key(&task_id) {
        return Err(VideoError::AlreadyExists);
    }
    registry.insert(task_id, surface);
    Ok(())
}

/// Destroy a task's surface
pub fn surface_destroy(task_id: u32) {
    let mut registry = SURFACES.lock();
    registry.remove(&task_id);
    // Arc refcount drops - surface freed when last user done
}

/// Get a reference to a task's surface (brief lock)
fn get_surface(task_id: u32) -> Result<SurfaceRef, VideoError> {
    let registry = SURFACES.lock();
    registry.get(&task_id).cloned().ok_or(VideoError::NotFound)
}

/// Get all surfaces (for compositor)
fn get_all_surfaces() -> Vec<SurfaceRef> {
    let registry = SURFACES.lock();
    registry.values().cloned().collect()
}
```

---

## Phase 3: Drawing API Migration

### 3.1 Replace `with_surface_mut` pattern

**Before (unsafe):**
```rust
fn with_surface_mut(task_id: u32, f: impl FnOnce(&mut Surface) -> VideoResult) -> VideoResult {
    let slot = get_or_create_surface(task_id)?;
    let mut slots = SURFACES.lock();
    f(&mut slots[slot].surface)
}
```

**After (safe):**
```rust
/// Execute a drawing operation on a surface's back buffer
fn with_back_buffer<F, R>(task_id: u32, f: F) -> Result<R, VideoError>
where
    F: FnOnce(&mut OwnedBuffer) -> R,
{
    // Brief global lock to get Arc clone
    let surface = get_surface(task_id)?;

    // Per-surface lock - other surfaces unaffected
    let mut buffers = surface.buffers.lock();
    Ok(f(&mut buffers.back))
}

/// Read-only access to surface metadata
fn with_surface<F, R>(task_id: u32, f: F) -> Result<R, VideoError>
where
    F: FnOnce(&Surface) -> R,
{
    let surface = get_surface(task_id)?;
    Ok(f(&surface))
}
```

### 3.2 Update all drawing functions

Each drawing function changes from raw pointer manipulation to slice-based:

**Before:**
```rust
pub fn surface_draw_rect_filled_fast(task_id: u32, x: i32, y: i32, w: i32, h: i32, color: u32) -> VideoResult {
    let result = with_surface_mut(task_id, |surface| {
        if surface.back_buffer.is_null() {
            return Err(VideoError::Invalid);
        }
        // ... raw pointer math ...
        unsafe { *pixel_ptr = color; }
    });
    result
}
```

**After:**
```rust
pub fn surface_draw_rect_filled_fast(task_id: u32, x: i32, y: i32, w: i32, h: i32, color: u32) -> VideoResult {
    with_back_buffer(task_id, |buffer| {
        let color_bytes = color.to_ne_bytes();
        let bytes_pp = buffer.bytes_pp as usize;
        let data = buffer.as_mut_slice();

        for row in y..(y + h) {
            let row_start = buffer.pixel_offset(x as u32, row as u32);
            let row_end = row_start + (w as usize) * bytes_pp;
            let row_slice = &mut data[row_start..row_end];

            // Fill entire row at once using chunks
            for chunk in row_slice.chunks_exact_mut(bytes_pp) {
                chunk.copy_from_slice(&color_bytes[..bytes_pp]);
            }
        }

        buffer.damage.add_rect(x, y, w, h);
        Ok(())
    })?
}
```

### 3.3 Functions to update

| Function | Line | Changes |
|----------|------|---------|
| `surface_clear` | ~615 | Use `OwnedBuffer::as_mut_slice()` |
| `surface_draw_rect_filled_fast` | ~682 | Use slice indexing |
| `surface_draw_line` | ~737 | Use slice indexing |
| `surface_draw_circle` | ~781 | Use slice indexing |
| `surface_draw_circle_filled` | ~823 | Use slice indexing |
| `surface_font_draw_string` | ~864 | Use slice indexing |
| `surface_blit` | ~934 | Use slice-to-slice copy |

---

## Phase 4: Compositor Migration

### 4.1 Replace snapshot approach

**Before (stale pointers):**
```rust
pub fn compositor_present() -> i32 {
    let slots_snapshot = {
        let slots = SURFACES.lock();
        *slots  // Copies array with raw pointers!
    };

    for slot in slots_snapshot.iter() {
        if slot.active {
            // Uses potentially stale pointers
            let surface = &slot.surface;
            if surface.front_buffer.is_null() { continue; }
            // ...
        }
    }
}
```

**After (Arc-safe):**
```rust
pub fn compositor_present() -> i32 {
    let fb = match framebuffer::snapshot() {
        Some(fb) => fb,
        None => return -1,
    };

    // Collect Arc clones - surfaces stay alive for entire render
    let surfaces: Vec<SurfaceRef> = get_all_surfaces();

    // Sort by z-order
    let mut sorted: Vec<&SurfaceRef> = surfaces.iter().collect();
    sorted.sort_by_key(|s| s.z_order.load(Ordering::Relaxed));

    // Render each surface
    for surface in sorted {
        if !surface.visible.load(Ordering::Relaxed) {
            continue;
        }

        // Per-surface lock to read front buffer
        let buffers = surface.buffers.lock();

        if !surface.dirty.load(Ordering::Acquire) {
            continue;
        }

        let wx = surface.window_x.load(Ordering::Relaxed);
        let wy = surface.window_y.load(Ordering::Relaxed);

        blit_buffer_to_framebuffer(
            &fb,
            &buffers.front,
            wx,
            wy,
        );

        surface.dirty.store(false, Ordering::Release);
    }

    0
}
```

### 4.2 Implement safe blit

```rust
fn blit_buffer_to_framebuffer(
    fb: &FramebufferInfo,
    buffer: &OwnedBuffer,
    dst_x: i32,
    dst_y: i32,
) {
    let src = buffer.as_slice();
    let dst = unsafe {
        core::slice::from_raw_parts_mut(fb.address as *mut u8, fb.pitch * fb.height)
    };

    let bytes_pp = buffer.bytes_pp as usize;

    for row in 0..buffer.height as usize {
        let src_row_start = row * buffer.pitch;
        let dst_row = (dst_y as usize + row) * fb.pitch as usize;
        let dst_col_start = (dst_x as usize) * bytes_pp;

        let src_row = &src[src_row_start..src_row_start + (buffer.width as usize * bytes_pp)];
        let dst_row = &mut dst[dst_row + dst_col_start..dst_row + dst_col_start + src_row.len()];

        dst_row.copy_from_slice(src_row);
    }
}
```

---

## Phase 5: Commit and Window Operations

### 5.1 Safe commit

```rust
pub fn surface_commit(task_id: u32) -> VideoResult {
    let surface = get_surface(task_id)?;

    {
        let mut buffers = surface.buffers.lock();
        buffers.swap();
        // Transfer damage from old back (now front) for compositor
    }

    surface.dirty.store(true, Ordering::Release);
    Ok(())
}
```

### 5.2 Atomic window operations

```rust
pub fn surface_set_position(task_id: u32, x: i32, y: i32) -> VideoResult {
    let surface = get_surface(task_id)?;
    surface.window_x.store(x, Ordering::Relaxed);
    surface.window_y.store(y, Ordering::Relaxed);
    surface.dirty.store(true, Ordering::Release);
    Ok(())
}

pub fn surface_set_z_order(task_id: u32, z: u32) -> VideoResult {
    let surface = get_surface(task_id)?;
    surface.z_order.store(z, Ordering::Relaxed);
    surface.dirty.store(true, Ordering::Release);
    Ok(())
}

pub fn surface_set_visible(task_id: u32, visible: bool) -> VideoResult {
    let surface = get_surface(task_id)?;
    surface.visible.store(visible, Ordering::Relaxed);
    surface.dirty.store(true, Ordering::Release);
    Ok(())
}
```

---

## Phase 6: Cleanup and Removal

### 6.1 Remove deprecated code

- [ ] Remove `SurfaceSlot` struct
- [ ] Remove `MAX_TASKS` constant (dynamic now)
- [ ] Remove all `*mut u8` buffer pointers from Surface
- [ ] Remove `get_or_create_surface()` slot-based function
- [ ] Remove `find_slot()` and related helpers
- [ ] Remove old `with_surface_mut()` implementation
- [ ] Remove `slots_snapshot` pattern from compositor

### 6.2 Remove unsafe blocks

- [ ] Remove unsafe from drawing functions (now safe slice access)
- [ ] Keep unsafe only for framebuffer MMIO access (unavoidable)

---

## Testing Plan

### Unit Tests (if added)

```rust
#[test]
fn test_owned_buffer_allocation() {
    let buf = OwnedBuffer::new(100, 100, 32).unwrap();
    assert_eq!(buf.as_slice().len(), 100 * 100 * 4);
}

#[test]
fn test_double_buffer_swap() {
    let mut db = DoubleBuffer::new(10, 10, 32).unwrap();
    db.back.as_mut_slice()[0] = 0xFF;
    db.swap();
    assert_eq!(db.front.as_slice()[0], 0xFF);
}
```

### Integration Tests

1. `make build` - Verify compilation
2. `make boot VIDEO=1` - Interactive testing:
   - Type in shell (triggers `surface_font_draw_string`)
   - Move mouse (triggers compositor updates)
   - Move window (triggers `surface_set_position`)
   - Run for 30+ seconds without crash
3. `make test` - Automated harness

---

## Migration Checklist

### Phase 1: Foundation
- [ ] Add `OwnedBuffer` struct
- [ ] Add `DoubleBuffer` struct
- [ ] Add new `Surface` struct with atomics
- [ ] Add `SurfaceRef` type alias
- [ ] Verify `alloc::sync::Arc` available in kernel

### Phase 2: Registry
- [ ] Add `BTreeMap` import
- [ ] Replace `SURFACES` static
- [ ] Implement `surface_create()`
- [ ] Implement `surface_destroy()`
- [ ] Implement `get_surface()`
- [ ] Implement `get_all_surfaces()`

### Phase 3: Drawing
- [ ] Implement `with_back_buffer()`
- [ ] Migrate `surface_clear()`
- [ ] Migrate `surface_draw_rect_filled_fast()`
- [ ] Migrate `surface_draw_line()`
- [ ] Migrate `surface_draw_circle()`
- [ ] Migrate `surface_draw_circle_filled()`
- [ ] Migrate `surface_font_draw_string()`
- [ ] Migrate `surface_blit()`

### Phase 4: Compositor
- [ ] Implement `blit_buffer_to_framebuffer()`
- [ ] Rewrite `compositor_present()`
- [ ] Remove snapshot pattern

### Phase 5: Operations
- [ ] Implement `surface_commit()`
- [ ] Implement `surface_set_position()`
- [ ] Implement `surface_set_z_order()`
- [ ] Implement `surface_set_visible()`

### Phase 6: Cleanup
- [ ] Remove old structs
- [ ] Remove deprecated functions
- [ ] Remove unnecessary unsafe blocks
- [ ] Update video_bridge.rs if needed
- [ ] Update syscall_handlers.rs if needed

### Testing
- [ ] Build passes
- [ ] Boot without crash
- [ ] Shell input works
- [ ] Mouse movement works
- [ ] Window operations work
- [ ] No memory leaks (check with info command)

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Arc not available in no_std | Low | High | Already using `alloc` crate |
| Performance regression | Medium | Low | Per-surface locks reduce contention |
| API breakage | High | Medium | Keep function signatures compatible |
| Subtle memory bugs | Low | High | Rust ownership prevents this |

---

## Estimated Scope

| Phase | New Lines | Modified Lines | Deleted Lines |
|-------|-----------|----------------|---------------|
| Phase 1 | ~150 | 0 | 0 |
| Phase 2 | ~50 | ~30 | ~50 |
| Phase 3 | ~100 | ~200 | ~150 |
| Phase 4 | ~80 | ~120 | ~100 |
| Phase 5 | ~40 | ~20 | ~30 |
| Phase 6 | 0 | 0 | ~100 |
| **Total** | **~420** | **~370** | **~430** |

Net change: approximately same LOC, but dramatically safer.
