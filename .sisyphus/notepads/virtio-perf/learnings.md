# Learnings - VirtIO-GPU Performance Optimization

## Conventions & Patterns

### Phase 1a: Measurement Infrastructure (COMPLETE)

#### Counter Placement Strategy
- **VIRTIO_FENCE_COUNT**: Incremented BEFORE `fence(Ordering::SeqCst)` in `poll_used()` loop
  - Tracks every memory barrier operation
  - Baseline metric for synchronization overhead
  
- **VIRTIO_SPIN_COUNT**: Incremented when timeout exceeded (no progress detected)
  - Indicates polling inefficiency
  - Signals when queue is not responding within expected time
  
- **VIRTIO_COMPLETION_COUNT**: Incremented when `used_idx` changes
  - Tracks successful queue completions
  - Ratio to SPIN_COUNT reveals completion rate

#### Atomic Operations Pattern
- Use `AtomicU64::fetch_add(1, Ordering::Relaxed)` for hot-path increments
  - Relaxed ordering sufficient (no cross-thread synchronization needed)
  - Minimal performance impact on hot path
  
- Use `swap(0, Ordering::Relaxed)` for atomic read-and-reset in logging
  - Ensures consistent snapshot of counters
  - Prevents counter overflow in long-running sessions

#### Logging Integration Pattern
- Add instrumentation at function START (before GPU commands)
- Format: `klog_info!("[VIRTIO PERF] fences={} spins={} completions={}", ...)`
- Captures per-frame metrics before command submission
- Counters reset after logging to prevent accumulation

#### Build & Verification
- `make build` compiles cleanly with instrumentation
- LSP diagnostics remain clean on modified files
- No compilation warnings or errors

### Phase 1b: Barrier Optimization (COMPLETE)

#### poll_used() Barrier Placement Optimization
- **Before**: `fence(Ordering::SeqCst)` executed on EVERY spin iteration (~2M/frame)
- **After**: `fence(Ordering::Acquire)` executed ONLY when progress detected (~2/frame)
- **Expected reduction**: >99% fewer memory barriers per frame

#### VirtIO Spec 2.7.13 Compliance
- Spec says: "The driver MUST perform a suitable memory barrier before processing the descriptor chain"
- "suitable memory barrier" = `Acquire` barrier (not `SeqCst`)
- "before processing" = AFTER detecting change, BEFORE reading descriptor data
- Linux reference: `drivers/virtio/virtio_ring.c:810-840` (`virtqueue_get_buf_ctx_split()`)

#### Why This Is Safe
- Volatile read of `used_idx` detects device updates (no barrier needed for detection)
- Acquire barrier ensures device writes to descriptors are visible BEFORE we read them
- Only pay barrier cost when device actually responded

#### Counter Placement After Optimization
- `VIRTIO_FENCE_COUNT`: Now incremented INSIDE progress-detected branch (before fence)
- `VIRTIO_COMPLETION_COUNT`: Stays in progress-detected branch (after fence)
- `VIRTIO_SPIN_COUNT`: Stays in timeout branch (unchanged)


### Phase 1c: submit() Barrier Upgrade (COMPLETE)

#### submit() Barrier Upgrade to Real Fence
- **Before**: `compiler_fence(Ordering::Release)` in `submit()` function
- **After**: `fence(Ordering::Release)` in `submit()` function
- **Location**: `drivers/src/virtio/queue.rs:157`

#### VirtIO Spec 2.7.7 Compliance
- Spec requires: "A suitable memory barrier MUST be issued by the driver before updating the available ring idx"
- "suitable memory barrier" = `Release` barrier (ensures descriptor writes visible before index update)
- `compiler_fence` insufficient on ARM: only prevents compiler reordering, not CPU reordering
- `fence(Ordering::Release)` provides real hardware barrier on all architectures

#### Why This Is Safe
- Release barrier ensures descriptor writes complete before index update
- Device reads index first, then reads descriptors
- Without real barrier, device might see stale descriptor data on ARM
- Matches Linux reference: `virtio_wmb()` maps to real barriers on all architectures

#### Build & Test Results
- `make build`: ✅ Compiles cleanly
- `make test`: ✅ All test suites pass (VirtIO-blk exercises submit path)
- LSP diagnostics: ✅ Clean (no errors/warnings)
- Import cleanup: Removed unused `compiler_fence` import

#### Pattern Summary
- **Read path** (poll_used): `Acquire` barrier after detecting device response
- **Write path** (submit): `Release` barrier before signaling new work to device
- Both are real hardware barriers, not compiler fences
- Ensures ARM portability and spec compliance


### Phase 1d: Memory Barrier Abstraction (COMPLETE)

#### virtio_wmb() and virtio_rmb() Abstraction Functions
- **Location**: `drivers/src/virtio/mod.rs` (lines 176-195)
- **Purpose**: Portable abstraction layer for VirtIO memory barriers
- **Implementation**:
  - `virtio_wmb()`: `fence(Ordering::Release)` - write barrier before index update
  - `virtio_rmb()`: `fence(Ordering::Acquire)` - read barrier after detecting device response
  - Both marked `#[inline(always)]` for zero-overhead abstraction

#### Linux Kernel Pattern Adoption
- Follows `include/linux/virtio_ring.h:37-45` pattern
- Single point of change for future optimizations (e.g., weak barriers on ARM)
- Self-documenting code: function names reveal intent
- VirtIO spec references in docstrings aid maintainability

#### Integration in queue.rs
- **Import**: Added `virtio_rmb, virtio_wmb` to super imports
- **submit() path** (line ~157): Replaced `fence(Ordering::Release)` with `virtio_wmb()`
- **poll_used() path** (line ~170): Replaced `fence(Ordering::Acquire)` with `virtio_rmb()`
- **Cleanup**: Removed unused `fence` import from `core::sync::atomic`

#### VirtIO Spec Compliance
- `virtio_wmb()`: Per spec 2.7.7 - "A write memory barrier before updating avail idx"
- `virtio_rmb()`: Per spec 2.7.13 - "A read memory barrier before reading used buffers"
- Both barriers are real hardware fences (not compiler fences)
- Ensures ARM portability and spec compliance

#### Build & Test Results
- `make build`: ✅ Compiles cleanly
- `make test`: ✅ All 363 tests pass (VirtIO-blk exercises both paths)
- LSP diagnostics: ✅ Clean on both mod.rs and queue.rs
- No compilation warnings or errors

#### Pattern Summary - Complete Optimization Chain
1. **Phase 1a**: Added instrumentation counters (VIRTIO_FENCE_COUNT, VIRTIO_COMPLETION_COUNT, VIRTIO_SPIN_COUNT)
2. **Phase 1b**: Optimized poll_used() barrier placement (Acquire only on progress)
3. **Phase 1c**: Upgraded submit() barrier to real fence (Release instead of compiler_fence)
4. **Phase 1d**: Abstracted barriers into named functions (virtio_wmb/rmb)

Result: Portable, maintainable, spec-compliant VirtIO synchronization with zero-overhead abstractions.


## Per-CPU Preemption Counter Migration (Task 6)

**Date**: 2026-01-24

### Changes Made
- Migrated `PreemptGuard` from global `PREEMPT_COUNT` to per-CPU `PerCpuData.preempt_count`
- Removed global `PREEMPT_COUNT` static
- Added import: `use crate::percpu::get_percpu_data;`

### Atomic Ordering Used
| Operation | Ordering | Justification |
|-----------|----------|---------------|
| `new()` increment | `Relaxed` | Per-CPU, no cross-CPU sync needed |
| `drop()` decrement | `Release` | Ensures critical section completes before preemption re-enables |
| `is_active()` read | `Relaxed` | Per-CPU read, no sync needed |
| `count()` read | `Relaxed` | Per-CPU read, no sync needed |

### Key Decisions
1. **RESCHEDULE_PENDING stays global with SeqCst**: Timer IRQ is routed to BSP only, cross-CPU reschedules use IPIs
2. **No IRQ disable in PreemptGuard::new()**: Atomic ops on per-CPU data are safe even with interrupts enabled
3. **Release ordering on decrement**: Ensures all critical section operations complete before counter decrements

### Performance Impact
- Eliminated cross-CPU cache line bouncing on preempt counter
- Reduced memory barriers: SeqCst → Relaxed/Release (no MFENCE on x86)
- Per-CPU access pattern is cache-friendly

### Verification
- `make build`: PASSED
- `make test`: PASSED (all suites)
- `lsp_diagnostics`: Clean


## [2026-01-24] Verified: No VirtIO-blk Timeout Messages

Ran comprehensive testing to verify VirtIO-blk still works after queue.rs changes:
- make test: 363/363 suites passed, no timeout messages
- make boot-log: VirtIO-blk initialized successfully, no timeouts
- grep for 'timeout' in test output: No matches

This confirms that the barrier optimizations in queue.rs (shared by VirtIO-blk and VirtIO-GPU) 
do not break VirtIO-blk functionality.

Acceptance criterion verified: ✅ No virtio-blk timeout messages

