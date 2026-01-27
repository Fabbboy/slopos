# Remove VirtIO GPU / VirGL Support

## Overview

Remove all VirtIO GPU and VirGL code from SlopOS. This technology is QEMU-only virtualization overhead with no benefit on real hardware.

## Rationale

| Technology | Purpose | Real Hardware? | Verdict |
|------------|---------|----------------|---------|
| VirtIO GPU | Paravirtualized GPU for VMs | No | **Remove** |
| VirGL | OpenGL passthrough to host | No | **Remove** |
| VGA/Limine framebuffer | Standard UEFI GOP | Yes | **Keep** |
| Intel Xe driver | Native Intel GPU | Yes | **Keep** |

**Why remove:**
- VirGL only accelerates QEMU, useless on bare metal
- Current implementation has severe performance issues (1-5 FPS due to polling)
- Adds ~1000 lines of dead code for real hardware deployments
- Complicates video subsystem with unnecessary backend switching
- VGA framebuffer works identically in QEMU and on real hardware

## Files to Delete

| File | Lines | Purpose |
|------|-------|---------|
| `drivers/src/virtio_gpu.rs` | ~1050 | VirtIO GPU driver |

## Files to Modify

### `drivers/src/lib.rs`
- Remove `pub mod virtio_gpu;` declaration

### `boot/src/boot_drivers.rs`
- Remove `virtio_gpu::virtio_gpu_register_driver()` call
- Remove `video=virgl` cmdline parsing
- Remove `VideoBackend::Virgl` handling
- Simplify `boot_video_backend()` to only handle Framebuffer and Xe

### `video/src/lib.rs`
- Remove `VideoBackend::Virgl` variant
- Remove `try_init_virgl_backend()` function
- Remove `virtio_gpu` import
- Remove VirGL flush callback registration

### `Makefile`
- Remove `VIRGL` variable and all `QEMU_VIRGL` logic
- Remove `-device virtio-gpu-*` flags
- Remove `gl=on` display options
- Simplify to always use `-vga std`

### `TODO.md`
- Remove virgl TODO item (line 7)

### `plans/KNOWN_ISSUES.md`
- Remove "Performance: Synchronous VirtIO-GPU Frame Flush" section (lines 121-209)

### `README.md`
- Remove `VIRGL=1` from advanced options
- Remove GPU acceleration mentions

### `abi/src/arch/x86_64/pci.rs` (if applicable)
- Remove VirtIO GPU device ID constants if defined here

## Migration Steps

### Phase 1: Remove Driver
1. Delete `drivers/src/virtio_gpu.rs`
2. Remove module declaration from `drivers/src/lib.rs`
3. Run `cargo check` in drivers crate to find all broken imports

### Phase 2: Update Boot Sequence
1. Edit `boot/src/boot_drivers.rs`:
   - Remove `use slopos_drivers::virtio_gpu`
   - Remove `virtio_gpu_register_driver()` call
   - Simplify `boot_video_backend()`:
     ```rust
     fn boot_video_backend() -> video::VideoBackend {
         let cmdline = boot_get_cmdline();
         if cmdline_contains(cmdline, "video=xe") {
             video::VideoBackend::Xe
         } else {
             video::VideoBackend::Framebuffer
         }
     }
     ```

### Phase 3: Simplify Video Subsystem
1. Edit `video/src/lib.rs`:
   - Remove `VideoBackend::Virgl` from enum
   - Remove `try_init_virgl_backend()` function
   - Remove virgl-specific init path in `init()`
   - Remove `use slopos_drivers::virtio_gpu`

### Phase 4: Clean Up Build System
1. Edit `Makefile`:
   - Remove lines 281, 311-328, 365, 395-411 (all VIRGL handling)
   - Simplify display args to always use `-vga std`

### Phase 5: Documentation Cleanup
1. Update `TODO.md` - remove virgl item
2. Update `plans/KNOWN_ISSUES.md` - remove VirtIO-GPU performance section
3. Update `README.md` - remove VIRGL mentions

### Phase 6: Verify
1. `make build` - ensure compilation succeeds
2. `make test` - ensure test harness passes
3. `make boot VIDEO=1` - verify display works
4. `make boot` - verify headless boot works

## What Remains

After cleanup, SlopOS will have two video backends:

1. **Framebuffer (default)** - Works everywhere (QEMU + real hardware)
2. **Intel Xe** - Native driver for Intel GPUs on real hardware

Both are production-relevant. No QEMU-only code paths remain in the video subsystem.

## Estimated Effort

| Phase | Time |
|-------|------|
| Remove driver | 5 min |
| Update boot | 10 min |
| Simplify video | 10 min |
| Clean Makefile | 10 min |
| Documentation | 10 min |
| Verify | 10 min |
| **Total** | **~1 hour** |

## Risk

**Low** - VirGL is not used by default. All default boot paths use VGA framebuffer. Removal is purely subtractive with no architectural changes.
