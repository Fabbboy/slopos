# Framebuffer Type Consolidation Plan

**Status**: Proposed  
**Author**: AI Analysis  
**Date**: 2026-01-11  
**Priority**: High (DRY violation with architectural implications)

---

## Problem Statement

SlopOS has accumulated **4+ different framebuffer/display info structures** that represent essentially the same data with inconsistent field types. This is not just a DRY violation - it reflects a missing architectural decision about type layering between hardware abstraction, kernel internals, and userland ABI.

### Current Type Proliferation

| Type | Location | Fields | Purpose |
|------|----------|--------|---------|
| `FramebufferInfo` | `lib/src/lib.rs:206` | `address: *mut u8, width: u64, height: u64, pitch: u64, bpp: u16` | Boot/Limine data |
| `FramebufferInfoC` | `abi/src/video_traits.rs:5` | `initialized: u8, width: u32, height: u32, pitch: u32, bpp: u32, pixel_format: u32` | Syscall export |
| `FbInfo` | `abi/src/window.rs:133` | `width: u32, height: u32, pitch: u32, bpp: u8, pixel_format: u8` | Window module |
| `UserFbInfo` | `abi/src/syscall.rs:115` | `width: u32, height: u32, pitch: u32, bpp: u8, pixel_format: u8` | Userland syscall |
| `FbState` | `video/src/framebuffer.rs:23` | `base: *mut u8, width: u32, height: u32, pitch: u32, bpp: u8, pixel_format: u8` | Internal video state |

### Type Width Chaos

```
Field      | lib      | abi/video_traits | abi/window | abi/syscall | video
-----------|----------|------------------|------------|-------------|-------
width      | u64      | u32              | u32        | u32         | u32
height     | u64      | u32              | u32        | u32         | u32
pitch      | u64      | u32              | u32        | u32         | u32
bpp        | u16      | u32              | u8         | u8          | u8
pixel_fmt  | -        | u32              | u8         | u8          | u8
```

---

## Root Cause Analysis

The proliferation stems from **three missing architectural decisions**:

### 1. No Clear Layer Boundaries

SlopOS lacks explicit separation between:
- **Hardware abstraction layer** (what bootloader provides)
- **Kernel-internal state** (what subsystems need)
- **ABI contract** (what userland sees)

### 2. No Type Ownership Policy

No single crate "owns" the canonical display property definition:
- `lib` defines one type
- `abi` defines three similar types
- `video` has its own internal type

### 3. Underutilized Rust Type System

The codebase already has `PhysAddr`/`VirtAddr` newtypes in `abi/src/addr.rs` and `PixelFormat` enum in `abi/src/pixel.rs`, but framebuffer code doesn't leverage them.

---

## Reference Architectures

### Linux Kernel (UAPI Pattern)

Linux separates concerns with strict layering:

```c
// include/uapi/linux/fb.h - ABI-STABLE, never changes
struct fb_var_screeninfo {
    __u32 xres;
    __u32 yres;
    __u32 bits_per_pixel;
    // ...
};

struct fb_fix_screeninfo {
    unsigned long smem_start;  // Physical address
    __u32 line_length;         // Pitch
    // ...
};

// include/linux/fb.h - Kernel-only, EMBEDS UAPI types
struct fb_info {
    struct fb_var_screeninfo var;  // Embed, don't duplicate
    struct fb_fix_screeninfo fix;
    void __iomem *screen_base;     // Kernel-only field
    // ...
};
```

**Key principles:**
1. UAPI types are sacred - field types never change
2. Kernel types embed UAPI types, don't duplicate
3. Fixed-width types (`__u32`) for ABI stability

### Redox OS (Rust IPC Pattern)

Redox uses `#[repr(C, packed)]` IPC structures:

```rust
// Shared IPC definitions
#[repr(C, packed)]
pub struct DisplaySize {
    pub display_id: usize,
    pub width: u32,
    pub height: u32,
}

// Kernel side - internal state
pub struct Display {
    width: u32,
    height: u32,
    active_resource: Option<ResourceId>,
}
```

**Key principles:**
1. IPC types define the contract
2. Kernel types may differ internally
3. No pointer sharing across boundaries

---

## Proposed Architecture

### The ABI-Centric Model

```
                    abi/src/display.rs (NEW)
                   THE ONE CANONICAL SOURCE
                            |
          +-----------------+-----------------+
          |                 |                 |
          v                 v                 v
    boot layer        video layer        userland
    (converts from    (uses directly)    (uses directly)
     Limine data)
```

### Canonical Type Definition

```rust
// abi/src/display.rs

/// Display information - the canonical type for all layers.
///
/// This is the single source of truth for display properties shared
/// between kernel subsystems and userland. All other display-related
/// types should either use this directly or implement `From` conversions.
///
/// # ABI Stability
///
/// This type is `#[repr(C)]` and forms part of the kernel-userland ABI.
/// Field types and order must not change without careful consideration
/// of backward compatibility.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Display width in pixels
    pub width: u32,
    /// Display height in pixels
    pub height: u32,
    /// Bytes per scanline (may be > width * bytes_per_pixel due to alignment)
    pub pitch: u32,
    /// Pixel format (determines bytes per pixel and channel layout)
    pub format: PixelFormat,
}

impl DisplayInfo {
    /// Create a new DisplayInfo with the given parameters.
    #[inline]
    pub const fn new(width: u32, height: u32, pitch: u32, format: PixelFormat) -> Self {
        Self { width, height, pitch, format }
    }

    /// Returns bytes per pixel for this display's format.
    #[inline]
    pub fn bytes_per_pixel(&self) -> u8 {
        self.format.bytes_per_pixel()
    }

    /// Returns the total buffer size in bytes.
    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.pitch as usize * self.height as usize
    }

    /// Check if dimensions are valid (non-zero, reasonable bounds).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.width > 0 
            && self.height > 0 
            && self.width <= 8192 
            && self.height <= 8192
            && self.pitch >= self.width * self.bytes_per_pixel() as u32
    }
}
```

### Kernel-Internal State (Separate from ABI)

```rust
// video/src/framebuffer.rs

/// Internal framebuffer state - kernel-only, not part of ABI.
///
/// This wraps `DisplayInfo` and adds kernel-specific fields like
/// the mapped virtual address and flush callbacks.
struct FramebufferState {
    /// Display properties (uses canonical ABI type)
    info: DisplayInfo,
    /// Virtual address of mapped framebuffer (kernel-only)
    base: VirtAddr,
    /// Optional flush callback for hardware that needs it
    flush_callback: Option<fn() -> i32>,
}
```

### Boot Layer Conversion

```rust
// boot/src/limine_protocol.rs

impl From<&limine::Framebuffer> for DisplayInfo {
    fn from(fb: &limine::Framebuffer) -> Self {
        let format = PixelFormat::from_bpp_and_masks(
            fb.bpp as u8,
            fb.red_mask_shift,
            fb.blue_mask_shift,
        );
        
        DisplayInfo {
            width: fb.width as u32,
            height: fb.height as u32,
            pitch: fb.pitch as u32,
            format,
        }
    }
}
```

---

## Implementation Plan

### Phase 1: Define Canonical Type (Low Risk)

**Goal**: Establish the single source of truth without breaking anything.

**Tasks**:
1. Create `abi/src/display.rs` with `DisplayInfo` struct
2. Add helper methods (`bytes_per_pixel`, `buffer_size`, `is_valid`)
3. Re-export from `abi/src/lib.rs`
4. Add `From<PixelFormat>` helper for format detection from bpp

**Files to create**:
- `abi/src/display.rs`

**Files to modify**:
- `abi/src/lib.rs` (add module and re-export)

**Risk**: None - purely additive change.

**Verification**: `cargo build` succeeds, no functional changes.

---

### Phase 2: Migrate Boot Layer (Low Risk)

**Goal**: Convert Limine data to canonical type at the source.

**Tasks**:
1. Add `impl From<&limine::Framebuffer> for DisplayInfo` in boot
2. Update `BootInfo` to use `DisplayInfo` instead of `FramebufferInfo`
3. Update `init_limine_protocol()` to perform conversion once

**Files to modify**:
- `boot/src/limine_protocol.rs`

**Risk**: Low - boot code is well-isolated.

**Verification**: Kernel boots successfully, framebuffer displays correctly.

---

### Phase 3: Migrate Video Layer (Medium Risk)

**Goal**: Use canonical type internally, leverage `VirtAddr`.

**Tasks**:
1. Update `FbState` to use `DisplayInfo` + `VirtAddr`
2. Update `init_with_info()` to accept `DisplayInfo`
3. Remove internal type conversions
4. Update all framebuffer operations to use new structure

**Files to modify**:
- `video/src/framebuffer.rs`
- `video/src/lib.rs`
- `video/src/graphics.rs`

**Risk**: Medium - core video functionality, needs careful testing.

**Verification**: 
- Boot splash displays correctly
- Roulette wheel renders
- Compositor functions properly

---

### Phase 4: Unify Syscall Types (Low-Medium Risk)

**Goal**: Single type for kernel-userland communication.

**Tasks**:
1. Update `syscall_fb_info` to return `DisplayInfo` directly
2. Remove `FramebufferInfoC` type
3. Remove `UserFbInfo` type (use `DisplayInfo`)
4. Remove `FbInfo` from `window.rs` (use `DisplayInfo`)
5. Update userland syscall wrappers

**Files to modify**:
- `abi/src/video_traits.rs` (remove `FramebufferInfoC`)
- `abi/src/window.rs` (remove `FbInfo`)
- `abi/src/syscall.rs` (remove `UserFbInfo`)
- `core/src/syscall/handlers.rs`
- `core/src/syscall_services/video.rs`
- `userland/src/syscall.rs`

**Risk**: Low-Medium - ABI change requires userland updates.

**Verification**:
- Shell displays framebuffer info correctly
- Compositor receives correct display dimensions
- All userland apps function normally

---

### Phase 5: Remove Legacy Types (Low Risk)

**Goal**: Clean up dead code.

**Tasks**:
1. Remove `slopos_lib::FramebufferInfo`
2. Remove any remaining type aliases
3. Update all imports across codebase
4. Update documentation

**Files to modify**:
- `lib/src/lib.rs`
- Various files with stale imports

**Risk**: Low - just cleanup after migration.

**Verification**: `cargo build` succeeds, `cargo clippy` clean.

---

## Type Mapping Reference

### Before -> After

| Old Type | New Type | Notes |
|----------|----------|-------|
| `slopos_lib::FramebufferInfo` | `DisplayInfo` | Remove after Phase 5 |
| `abi::video_traits::FramebufferInfoC` | `DisplayInfo` | Remove after Phase 4 |
| `abi::window::FbInfo` | `DisplayInfo` | Remove after Phase 4 |
| `abi::syscall::UserFbInfo` | `DisplayInfo` | Remove after Phase 4 |
| `video::FbState` | `FramebufferState` (internal) | Wraps `DisplayInfo` |

### Field Mapping

| Old Fields | New Field | Type |
|------------|-----------|------|
| `width` (various) | `width` | `u32` |
| `height` (various) | `height` | `u32` |
| `pitch` / `line_length` | `pitch` | `u32` |
| `bpp` + `pixel_format` | `format` | `PixelFormat` enum |
| `address` / `base` | (separate in internal state) | `VirtAddr` |
| `initialized` | (method on state) | `fn is_initialized() -> bool` |

---

## Success Criteria

1. **Single definition**: Only `DisplayInfo` exists for display properties
2. **Type consistency**: All fields use consistent types (`u32`, `PixelFormat`)
3. **Clear separation**: ABI type vs kernel-internal state clearly distinguished
4. **Rust idioms**: Uses `From` traits, newtypes where appropriate
5. **No functional regression**: All existing features work identically
6. **Reduced code**: Net reduction in lines of type definitions

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| ABI breakage | Medium | High | Phase 4 updates userland atomically |
| Boot regression | Low | Critical | Phase 2 tested in isolation |
| Video corruption | Low | High | Phase 3 includes visual verification |
| Missed conversions | Medium | Low | Compiler will catch type mismatches |

---

## Future Considerations

### Multi-Monitor Support

The `DisplayInfo` type is per-display. Future multi-monitor support would use:

```rust
pub struct DisplayManager {
    displays: [Option<DisplayInfo>; MAX_DISPLAYS],
    primary: usize,
}
```

### Hardware Acceleration

If GPU acceleration is added, extend with capability flags:

```rust
pub struct DisplayInfo {
    // ... existing fields ...
    pub capabilities: DisplayCapabilities,
}

bitflags! {
    pub struct DisplayCapabilities: u32 {
        const HARDWARE_CURSOR = 0x01;
        const VSYNC = 0x02;
        const DOUBLE_BUFFER = 0x04;
    }
}
```

### HDR/Wide Color

Future pixel formats can be added to `PixelFormat` enum without breaking ABI:

```rust
pub enum PixelFormat {
    // Existing...
    Argb8888 = 0,
    // Future additions...
    Argb2101010 = 10,  // 10-bit HDR
    Fp16Rgba = 20,     // 16-bit float
}
```

---

## References

- Linux kernel `include/uapi/linux/fb.h` - UAPI framebuffer structures
- Linux kernel `include/linux/fb.h` - Internal fb_info structure
- Redox OS `graphics-ipc` crate - Rust IPC patterns
- SlopOS `abi/src/addr.rs` - Existing newtype pattern (PhysAddr/VirtAddr)
- SlopOS `abi/src/pixel.rs` - Existing PixelFormat enum

---

## Appendix: Current Data Flow

```
Limine Bootloader
       |
       | (raw hardware data)
       v
boot/limine_protocol.rs
       |
       | FramebufferInfo { address, width:u64, height:u64, pitch:u64, bpp:u16 }
       v
video/lib.rs::init()
       |
       | converts to FbState { base, width:u32, height:u32, pitch:u32, bpp:u8 }
       v
video/framebuffer.rs
       |
       | on syscall: converts to FramebufferInfoC { initialized, width:u32, ... bpp:u32 }
       v
core/syscall/handlers.rs
       |
       | converts to UserFbInfo { width:u32, height:u32, pitch:u32, bpp:u8 }
       v
Userland
```

### After Consolidation

```
Limine Bootloader
       |
       | (raw hardware data)
       v
boot/limine_protocol.rs
       |
       | From<Limine> -> DisplayInfo { width:u32, height:u32, pitch:u32, format:PixelFormat }
       v
video/lib.rs::init()
       |
       | FramebufferState { info: DisplayInfo, base: VirtAddr }
       v
video/framebuffer.rs
       |
       | syscall returns &DisplayInfo directly (no conversion)
       v
core/syscall/handlers.rs
       |
       | copies DisplayInfo to userland (no conversion)
       v
Userland (uses DisplayInfo directly)
```
