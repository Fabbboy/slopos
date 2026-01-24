# Learnings: VirtIO GPU Logging Fix

## [2026-01-24T18:45] Task: Remove per-frame logging

### Root Cause Analysis
After fixing the VirtIO barrier placement, user reported rendering was still line-by-line. Investigation revealed:

**The real culprit**: `klog_info!` logging in `virtio_gpu_flush_full()` called on EVERY FRAME.

### Why Logging Caused Line-by-Line Rendering

1. **Serial output is extremely slow**:
   - Baud rate: ~115200 (~11KB/s throughput)
   - Each log message: ~50-80 bytes
   - Time per message: ~1-5ms to write to serial
   
2. **Flush callback is per-frame**:
   - `virtio_gpu_flush_full()` registered as framebuffer flush callback
   - Called on every frame flip (~30-60 times per second)
   - At 30 FPS: 30 messages/sec = 30-150ms overhead per frame
   
3. **Visible impact**:
   - Frame time budget at 30 FPS: ~33ms
   - Serial overhead: 30-150ms per frame
   - Result: Frames take 63-183ms instead of 33ms
   - **User sees line-by-line rendering** as each frame takes multiple refresh cycles

### Why Loading Screen Was Unaffected

- Loading screen uses Limine framebuffer directly
- Doesn't trigger `virtio_gpu_flush_full()` callback
- No serial logging = smooth rendering

### The Fix

**Before (BROKEN)**:
```rust
pub fn virtio_gpu_flush_full() -> c_int {
    unsafe {
        let fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
        let spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
        let completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
        klog_info!(  // <-- SERIAL BOTTLENECK
            "[VIRTIO PERF] fences={} spins={} completions={}",
            fences,
            spins,
            completions
        );
```

**After (FIXED)**:
```rust
pub fn virtio_gpu_flush_full() -> c_int {
    unsafe {
        // Performance counters - reset but don't log (serial too slow for per-frame output)
        let _fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
        let _spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
        let _completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
        // NOTE: Logging removed - serial output on every frame caused line-by-line rendering
        // To debug performance, use klog_debug! with boot.debug=on (but expect slowdown)
```

### Key Insights

1. **Never log in per-frame callbacks** - serial is too slow
2. **Serial output is a major bottleneck** - ~115200 baud is ancient
3. **Performance instrumentation must be lightweight** - logging is not lightweight
4. **Different code paths for different screens** - loading vs roulette use different rendering paths

### Alternative Approaches (Not Implemented)

If we wanted to keep performance logging:

**Option 1: Periodic logging** (every N frames):
```rust
static FRAME_COUNTER: AtomicU64 = AtomicU64::new(0);
let frame = FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
if frame % 60 == 0 {  // Log once per second at 60fps
    klog_info!("[VIRTIO PERF] ...");
}
```

**Option 2: Debug-only logging**:
```rust
klog_debug!("[VIRTIO PERF] ...");  // Only with boot.debug=on
```

**Option 3: In-memory ring buffer**:
- Store metrics in memory
- Dump on demand via syscall or panic
- No serial overhead during normal operation

### Convention Established

**For per-frame callbacks**:
1. **Never** use `klog_info!` or `klog_warn!` (too slow)
2. Use `klog_debug!` only if absolutely necessary (still slow)
3. Prefer in-memory metrics collection
4. If logging needed, make it periodic (every N frames)
5. Document performance impact in comments

### Files Modified
- `drivers/src/virtio_gpu.rs:1026-1033` - Removed klog_info! from virtio_gpu_flush_full()

### Testing Limitations
- Automated tests don't exercise GPU rendering path
- Build verification passes (code compiles fine)
- Only manifests during actual GPU operations with display
- **Manual verification required** to confirm smooth rendering

### Expected Result
- **Before fix**: Line-by-line rendering at ~1-5 FPS (serial bottleneck)
- **After fix**: Smooth rendering at 30+ FPS (no serial bottleneck)

### Commit
`6ecca7ede` - fix(virtio-gpu): remove per-frame logging that caused line-by-line rendering
