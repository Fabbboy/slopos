# VirtIO GPU Logging Fix - URGENT

## Context

### Original Problem
Line-by-line rendering visible on roulette screen (loading screen unaffected).

### Root Cause FOUND
**The `klog_info!` logging we added to `virtio_gpu_flush_full()` is called on EVERY FRAME!**

Serial output is extremely slow (~115200 baud = ~11KB/s). Writing log messages on every frame (~30-60 times per second) causes massive delays, resulting in visible line-by-line rendering.

### Evidence
- Loading screen is unaffected (doesn't use virtio_gpu_flush_full callback)
- Roulette screen uses framebuffer flush callback which calls our logging
- `klog_info!` writes to serial on every frame = bottleneck

---

## Work Objectives

### Core Objective
Remove the per-frame logging from `virtio_gpu_flush_full()` to fix line-by-line rendering.

### Definition of Done
- [x] `make build` succeeds
- [ ] `VIDEO=1 make boot` shows smooth rendering (not line-by-line) - REQUIRES HUMAN VERIFICATION

---

## TODOs

- [x] 1. Remove per-frame logging from virtio_gpu_flush_full()

  **What to do**:
  
  Edit `drivers/src/virtio_gpu.rs` lines 1026-1037.

  **FIND THIS:**
  ```rust
  pub fn virtio_gpu_flush_full() -> c_int {
      unsafe {
          let fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
          let spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
          let completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
          klog_info!(
              "[VIRTIO PERF] fences={} spins={} completions={}",
              fences,
              spins,
              completions
          );

          if VIRTIO_GPU_DEVICE.fb_ready == 0 {
  ```

  **REPLACE WITH:**
  ```rust
  pub fn virtio_gpu_flush_full() -> c_int {
      unsafe {
          // Performance counters - reset but don't log (serial too slow for per-frame output)
          let _fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
          let _spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
          let _completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
          // NOTE: Logging removed - serial output on every frame caused line-by-line rendering
          // To debug performance, use klog_debug! with boot.debug=on (but expect slowdown)

          if VIRTIO_GPU_DEVICE.fb_ready == 0 {
  ```

  **Why this fixes it**:
  - Serial output is ~115200 baud (~11KB/s)
  - At 30 FPS, we were trying to log ~30 messages per second
  - Each log message takes ~1-5ms to write to serial
  - This adds 30-150ms delay PER FRAME = line-by-line visible rendering

  **Acceptance Criteria**:
  - [x] `make build` succeeds with no warnings
  - [ ] `VIDEO=1 make boot` shows smooth rendering (REQUIRES HUMAN VERIFICATION)
  - [ ] Roulette wheel animates fluidly (not line-by-line) (REQUIRES HUMAN VERIFICATION)

  **Commit**: YES
  - Message: `fix(virtio-gpu): remove per-frame logging that caused line-by-line rendering`
  - Files: `drivers/src/virtio_gpu.rs`

---

## Alternative: Keep Logging But Make It Periodic

If you want to KEEP the performance logging but fix the issue, use this instead:

```rust
pub fn virtio_gpu_flush_full() -> c_int {
    unsafe {
        static FRAME_COUNTER: AtomicU64 = AtomicU64::new(0);
        
        let fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
        let spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
        let completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
        
        // Only log every 60 frames (~1 second at 60fps) to avoid serial bottleneck
        let frame = FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        if frame % 60 == 0 {
            klog_info!(
                "[VIRTIO PERF] fences={} spins={} completions={} (frame {})",
                fences, spins, completions, frame
            );
        }

        if VIRTIO_GPU_DEVICE.fb_ready == 0 {
```

This logs once per second instead of 30-60 times per second.

---

## Success Criteria

```bash
make build           # Should succeed
VIDEO=1 make boot    # Should show smooth rendering (not line-by-line)
```
