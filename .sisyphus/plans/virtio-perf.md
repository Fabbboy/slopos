# VirtIO-GPU Performance Optimization for SMP

## Context

### Original Request
Fix the ~1 FPS roulette wheel performance issue caused by excessive memory barriers in the VirtIO-GPU frame flush path. Research how RedoxOS and Linux handle this, implement best practices leveraging Rust's type safety, and properly implement per-CPU preemption handling for SMP.

### Interview Summary
**Key Discussions**:
- Frame flush path has `fence(Ordering::SeqCst)` in tight polling loop (up to 2M fences/frame)
- Preemption counter uses `SeqCst` for all operations, called on every interrupt
- Linux kernel uses `virtio_wmb/rmb` (Release/Acquire), not SeqCst
- VirtIO spec only requires Release before `avail_idx` update, Acquire after `used_idx` change
- Oracle confirmed: move fence to "only when progress detected", use Acquire not SeqCst
- Oracle warned: `compiler_fence(Release)` too weak for ARM - need real barrier
- User requested Phase 1 (quick wins) + Phase 2 (proper SMP architecture)

**Research Findings**:
- Linux `virtio_ring.c`: `virtio_wmb()` before `avail_idx` update
- VirtIO spec sections 2.7.7 and 2.7.13 define exact barrier requirements
- Oracle: SeqCst per-iteration is pathological - ~millions of fences per frame is root cause

### Metis Review
**Identified Gaps** (addressed):
- Missing baseline measurement methodology → Added Phase 1a for instrumentation
- Concurrent queue access undefined behavior → Added guardrail to not change locking
- `PreemptGuard::drop()` race condition with `RESCHEDULE_PENDING` → Added analysis step before changing ordering
- VirtIO-blk has same pattern → Explicit exclusion, separate PR
- Missing acceptance criteria → Added performance and correctness criteria

---

## Work Objectives

### Core Objective
Optimize memory barrier usage in VirtIO-GPU driver and preemption subsystem to achieve acceptable frame rates (target: 30+ FPS, minimum: 10 FPS) while maintaining correctness on x86-64 with ARM-portable patterns.

### Concrete Deliverables
- Optimized `poll_used()` in `drivers/src/virtio/queue.rs`
- Optimized `submit()` barrier in `drivers/src/virtio/queue.rs`
- `virtio_wmb()`/`virtio_rmb()` abstraction in `drivers/src/virtio/mod.rs`
- Per-CPU preemption storage in `lib/src/preempt.rs` and `lib/src/percpu.rs`
- Baseline and post-optimization measurements in test logs

### Definition of Done
- [ ] `VIRGL=1 VIDEO=1 make boot` shows roulette wheel at visually smooth frame rate
- [x] `make test` passes without regressions (verifies VirtIO-blk not broken)
- [ ] No visual artifacts or corruption in framebuffer
- [ ] **Fences per frame reduced by >90%** (measured via Task 1 instrumentation)
  - Baseline: ~2M fences/frame (SeqCst per spin iteration)
  - Target: ~2 fences/frame (one Acquire per completion)
- [x] No `virtio-blk: request timeout` messages in serial log (from `drivers/src/virtio_blk.rs:176`)
  - Note: VirtIO-GPU does not log timeouts today; failure is silent (`virtio_gpu_flush_full()` returns `-1`)
  - GPU success is implied by visible roulette animation + absence of hangs
- [x] Code compiles without new warnings

### Must Have
- Barrier placement that satisfies VirtIO spec 2.7.7 and 2.7.13
- Release barrier before publishing `avail_idx`
- Acquire barrier after observing `used_idx` change
- Per-CPU preemption counter (not global atomic)

### Must NOT Have (Guardrails)
- Do NOT remove fences entirely - move or weaken, never delete
- Do NOT change `spin_loop()` hint - it's correct and separate
- Do NOT touch descriptor chain logic - only barrier placement
- Do NOT optimize VirtIO-blk driver code (`drivers/src/virtio_blk.rs`) - separate concern
  - **Note**: `drivers/src/virtio/queue.rs` is shared code used by both GPU and blk drivers
  - Changes to `queue.rs` affect blk implicitly; this is expected and acceptable
  - The `make test` verification covers VirtIO-blk correctness via itest harness
- Do NOT add interrupt-driven completion - out of scope
- Do NOT change `RESCHEDULE_PENDING` ordering without race analysis
- Do NOT touch scheduler fences (`scheduler.rs`, `per_cpu.rs` AP pause)

---

## Verification Strategy (MANDATORY)

### Test Decision
- **Infrastructure exists**: YES (make test with QEMU itests)
- **User wants tests**: Manual verification (QA-focused)
- **Framework**: Make test harness + manual QEMU verification

### Manual Execution Verification

Each TODO includes:
1. Specific QEMU boot commands
2. Expected visual/log output
3. Measurement collection methodology

**Evidence Required:**
- Serial output showing spin counts (instrumentation tasks)
- Visual confirmation of roulette animation smoothness
- `make test` output showing pass/fail

---

## Task Flow

```
Phase 1a: Instrumentation (measurement baseline)
     ↓
Phase 1b: VirtIO Barriers (poll_used + submit + abstraction)
     ↓
Phase 2: Preemption (race analysis + per-CPU storage)
```

## Parallelization

| Group | Tasks | Reason |
|-------|-------|--------|
| A | 1 | Baseline measurement must complete first |
| B | 2, 3 | Can parallelize poll_used and submit fixes |
| C | 4 | Depends on B completing |
| D | 5, 6 | Preemption requires analysis before implementation |

| Task | Depends On | Reason |
|------|------------|--------|
| 2, 3 | 1 | Need baseline before optimization |
| 4 | 2, 3 | Abstraction after both patterns verified |
| 5 | None | Analysis can start in parallel |
| 6 | 5 | Implementation requires race analysis results |

---

## TODOs

### Phase 1a: Measurement Infrastructure

- [x] 1. Add fence and spin count instrumentation to `poll_used()`

  **What to do**:
  - Add `pub static` atomic counters in `drivers/src/virtio/queue.rs`:
    - `VIRTIO_FENCE_COUNT: AtomicU64` - tracks fence executions
    - `VIRTIO_SPIN_COUNT: AtomicU64` - tracks spin iterations
    - `VIRTIO_COMPLETION_COUNT: AtomicU64` - tracks completions
  - Increment fence counter INSIDE the current fence location (before barrier change)
  - Log summary at START of `virtio_gpu_flush_full()` using `klog_info!` macro
  - Reset counters using `swap(0, Relaxed)` to get-and-reset atomically
  - Run `VIRGL=1 VIDEO=1 make boot` and observe log output
  - Record baseline: **fences-per-frame** (this is the key metric for Task 2 acceptance)

  **Counter placement (before optimization)**:
  ```rust
  // In poll_used(), BEFORE the barrier change:
  pub fn poll_used(&mut self, timeout_spins: u32) -> bool {
      let mut spins = 0u32;
      loop {
          VIRTIO_FENCE_COUNT.fetch_add(1, Ordering::Relaxed);  // Count fence executions
          fence(Ordering::SeqCst);
          let used_idx = self.read_used_idx();
          if used_idx != self.last_used_idx {
              VIRTIO_COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
              self.last_used_idx = used_idx;
              return true;
          }
          VIRTIO_SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
          spins += 1;
          if spins > timeout_spins { return false; }
          core::hint::spin_loop();
      }
  }
  ```

  **Must NOT do**:
  - Do NOT change polling loop structure
  - Do NOT change existing barrier placement yet
  - Do NOT make counters per-CPU (keep simple global for measurement)

  **Parallelizable**: NO (must complete before B group)

  **References**:

  **Pattern References** (existing code to follow):
  - `drivers/src/virtio/queue.rs:157-172` - Current `poll_used()` implementation
  - `drivers/src/virtio_gpu.rs:1022-1051` - `virtio_gpu_flush_full()` where to add logging
  - `drivers/src/virtio_gpu.rs:7` - Shows how to import `klog_debug!` from `slopos_lib`
  - `drivers/src/virtio_gpu.rs:547-581` - Example usage of `klog_info!` for logging

  **API/Type References**:
  - `core::sync::atomic::AtomicU64` - For spin counters
  - `core::sync::atomic::Ordering::Relaxed` - For counter increments (measurement only)
  - `slopos_lib::klog_debug!` - Kernel logging macro for debug output to serial

  **WHY Each Reference Matters**:
  - `poll_used()` is where we add the counter increment
  - `virtio_gpu_flush_full()` is called once per frame - good place to log/reset counters
  - `klog_debug!` is the standard kernel logging mechanism (NOT `serial_print!` which doesn't exist)
  - Counting fences directly allows Task 2 acceptance to measure "fences per frame" reduction

  **Acceptance Criteria**:

  **CRITICAL: Boot Configuration for VirtIO-GPU**:
  - VirtIO-GPU flush path is only active when Virgl backend is used
  - Must use `VIRGL=1 VIDEO=1` to enable VirtIO-GPU (not just `VIDEO=1`)
  - Confirm VirtIO-GPU is active by checking for BOTH conditions:
    1. Serial output contains: `"PCI: virtio-gpu driver probe succeeded"` (from `drivers/src/virtio_gpu.rs:837`)
    2. Serial output does NOT contain: `"virgl backend unavailable; falling back to framebuffer"` (from `video/src/lib.rs:97`)
  - If fallback message appears → VirtIO-GPU flush path is NOT active, fix QEMU config
  
  **Why per-frame counters work**:
  - `virtio_gpu_flush_full()` is registered as the flush callback at `video/src/lib.rs:99`
  - Compositor calls `fb_flip_from_shm()` → `framebuffer_flush()` → callback once per frame
  - See `video/src/framebuffer.rs:366` for callback invocation point

  **CRITICAL: Logging Visibility**:
  - Use `klog_info!` for instrumentation output (NOT `klog_debug!`)
  - `klog_debug!` is filtered by default; `klog_info!` always visible
  - Log line format: `klog_info!("[VIRTIO PERF] fences={} spins={} completions={}", ...)`

  **CRITICAL: Counter Access Across Modules**:
  - Counters are declared in `drivers/src/virtio/queue.rs` as `pub static`:
    ```rust
    pub static VIRTIO_FENCE_COUNT: AtomicU64 = AtomicU64::new(0);
    pub static VIRTIO_SPIN_COUNT: AtomicU64 = AtomicU64::new(0);
    pub static VIRTIO_COMPLETION_COUNT: AtomicU64 = AtomicU64::new(0);
    ```
  - Reset and log in `drivers/src/virtio_gpu.rs` via:
    ```rust
    use crate::virtio::queue::{VIRTIO_FENCE_COUNT, VIRTIO_SPIN_COUNT, VIRTIO_COMPLETION_COUNT};
    
    // At start of virtio_gpu_flush_full():
    let fences = VIRTIO_FENCE_COUNT.swap(0, Ordering::Relaxed);
    let spins = VIRTIO_SPIN_COUNT.swap(0, Ordering::Relaxed);
    let completions = VIRTIO_COMPLETION_COUNT.swap(0, Ordering::Relaxed);
    klog_info!("[VIRTIO PERF] fences={} spins={} completions={}", fences, spins, completions);
    ```

  **Manual Execution Verification:**
  - [ ] Using QEMU with VirtIO-GPU enabled:
    - Command: `VIRGL=1 VIDEO=1 make boot 2>&1 | tee boot.log`
    - Verify: Serial output contains `"PCI: virtio-gpu driver probe succeeded"`
    - Verify: Serial output does NOT contain `"virgl backend unavailable"`
    - Let roulette run for 10 seconds
    - Expected output contains: `[VIRTIO PERF] fences=N spins=N completions=N` per frame
  - [ ] Record baseline metrics in this format:
    - Fences per frame: ______ (expected: ~2M with current SeqCst per-spin)
    - Spins per frame: ______
    - Completions per frame: ______ (expected: 2 - one transfer + one flush)

  **Commit**: YES
  - Message: `drivers(virtio): add fence/spin count instrumentation for performance analysis`
  - Files: `drivers/src/virtio/queue.rs`, `drivers/src/virtio_gpu.rs`
  - Pre-commit: `cargo build --target x86_64-slos`

---

### Phase 1b: VirtIO Barrier Optimization

- [x] 2. Optimize `poll_used()` barrier placement

  **What to do**:
  - Move `fence(Ordering::SeqCst)` from before the read to after detecting change
  - Change ordering from `SeqCst` to `Acquire` (per VirtIO spec 2.7.13)
  - Keep `read_volatile()` for the used_idx read
  - Test that device responses are still correctly detected

  **Implementation pattern (pseudo-code)**:
  ```rust
  pub fn poll_used(&mut self, timeout_spins: u32) -> bool {
      let mut spins = 0u32;
      loop {
          // NO FENCE HERE - only volatile read
          let used_idx = self.read_used_idx();
          if used_idx != self.last_used_idx {
              // Acquire barrier ONLY when progress detected
              // IMPORTANT: Keep fence counter here for instrumentation
              VIRTIO_FENCE_COUNT.fetch_add(1, Ordering::Relaxed);
              fence(Ordering::Acquire);
              VIRTIO_COMPLETION_COUNT.fetch_add(1, Ordering::Relaxed);
              self.last_used_idx = used_idx;
              return true;
          }
          VIRTIO_SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
          spins += 1;
          if spins > timeout_spins { return false; }
          core::hint::spin_loop();
      }
  }
  ```
  
  **CRITICAL: Instrumentation after barrier move**:
  - Fence counter must be incremented at NEW fence location (inside progress-detected branch)
  - This ensures "fences per frame" metric remains valid after optimization
  - Expected result: ~2 fences/frame (one per GPU command completion) vs ~2M baseline

  **Must NOT do**:
  - Do NOT change loop termination logic
  - Do NOT change `last_used_idx` semantics
  - Do NOT remove `spin_loop()` hint
  - Do NOT add any new functionality

  **Parallelizable**: YES (with task 3)

  **References**:

  **Pattern References** (existing code to follow):
  - `drivers/src/virtio/queue.rs:157-172` - Current implementation to modify
  - Linux `drivers/virtio/virtio_ring.c:810-840` - `virtqueue_get_buf_ctx_split()` pattern

  **Documentation References**:
  - VirtIO spec v1.2, "Split Virtqueues" → "Receiving Used Buffers From The Device"
  - PDF: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.pdf (page ~40)
  - TeX source: https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/tex/split-ring.tex (search `"Processing Used Ring Entries"`)
  - **Exact normative text** (from split-ring.tex):
    > "The driver MUST perform a suitable memory barrier before processing the descriptor chain, so that it sees the most recent data."
  - Mapping: "suitable memory barrier" = Acquire barrier (ensures device's writes to used ring/descriptors are visible before driver reads them)
  - Linux reference: https://github.com/torvalds/linux/blob/v6.7/drivers/virtio/virtio_ring.c#L810-L840

  **WHY Each Reference Matters**:
  - Current impl shows exact code to change
  - Linux shows how production kernels place the barrier AFTER detecting change
  - Spec confirms Acquire is sufficient (not SeqCst)

  **Acceptance Criteria**:

  **Manual Execution Verification:**
  - [ ] Using QEMU with VirtIO-GPU enabled:
    - Command: `VIRGL=1 VIDEO=1 make boot 2>&1 | tee boot_optimized.log`
    - Let roulette run for 10 seconds
    - Expected: Visually smoother animation than baseline
  - [ ] Compare fence counts from instrumentation (from Task 1 counters):
    - **Fences per frame**: ~2 (one per command completion)
    - **Reduction**: >90% fewer fences than baseline (~2M → ~2)
    - Spin count may remain similar (spins are cheap without fences)
  - [ ] Verify no corruption or missed completions:
    - Roulette wheel renders correctly
    - No visual artifacts or garbage pixels
    - No `virtio-blk: request timeout` messages in serial output
    - No GPU command timeout warnings
  - [ ] Note: Changes to `queue.rs` affect VirtIO-blk implicitly (shared code)
    - Verify `make test` passes (exercises VirtIO-blk)

  **Commit**: YES
  - Message: `drivers(virtio): optimize poll_used barrier placement per VirtIO spec`
  - Files: `drivers/src/virtio/queue.rs`
  - Pre-commit: `cargo build --target x86_64-slos && make test`

---

- [x] 3. Upgrade `submit()` barrier for ARM portability

  **What to do**:
  - Change `compiler_fence(Ordering::Release)` to `fence(Ordering::Release)`
  - This ensures a real hardware barrier, not just compiler reordering prevention
  - Verify descriptors are still correctly visible to device

  **Implementation change**:
  ```rust
  // Before:
  compiler_fence(Ordering::Release);
  
  // After:
  fence(Ordering::Release);
  ```

  **Must NOT do**:
  - Do NOT change descriptor write order
  - Do NOT change `avail_ring_ptr` logic
  - Do NOT add any new parameters

  **Parallelizable**: YES (with task 2)

  **References**:

  **Pattern References**:
  - `drivers/src/virtio/queue.rs:144-155` - Current `submit()` implementation
  - Linux `drivers/virtio/virtio_ring.c:677` - Uses `virtio_wmb()` (real barrier)

  **Documentation References**:
  - VirtIO spec v1.2, "Split Virtqueues" → "Making Buffers Available"
  - PDF: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.pdf (page ~38)
  - TeX source: https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/tex/split-ring.tex (search `"Updating Available Ring"`)
  - **Exact normative text** (from split-ring.tex):
    > "A suitable memory barrier MUST be issued by the driver before updating the available ring idx, so that the device sees the most recent data."
  - Mapping: "suitable memory barrier" = Release barrier (ensures descriptor writes are visible to device before index update signals new work)
  - Linux reference: https://github.com/torvalds/linux/blob/v6.7/drivers/virtio/virtio_ring.c#L677
  - Oracle consultation: "compiler_fence is too weak for ARM - need real fence"

  **WHY Each Reference Matters**:
  - Current impl shows exact line to change
  - Linux shows production kernels use real barriers, not compiler fences
  - Spec confirms write barrier requirement
  - Oracle warned this is an ARM portability issue

  **Acceptance Criteria**:

  **Manual Execution Verification:**
  - [ ] Build verification:
    - Command: `cargo build --target x86_64-slos`
    - Expected: Compiles without errors
  - [ ] Functional verification (exercises VirtIO queue submission):
    - Command: `make test`
    - Expected: All tests pass (VirtIO-blk tests exercise `submit()` path)
    - Verify: No `virtio-blk: request timeout` in test output (from `drivers/src/virtio_blk.rs:176`)
  - [ ] Visual verification:
    - Command: `VIRGL=1 VIDEO=1 make boot`
    - Expected: Roulette displays correctly (no corruption)
    - Expected: No hangs during GPU command submission

  **Commit**: YES
  - Message: `drivers(virtio): upgrade submit barrier to real fence for ARM portability`
  - Files: `drivers/src/virtio/queue.rs`
  - Pre-commit: `cargo build --target x86_64-slos && make test`

---

- [x] 4. Create `virtio_wmb()`/`virtio_rmb()` abstraction

  **What to do**:
  - Create inline functions in `drivers/src/virtio/mod.rs`
  - `virtio_wmb()` = `fence(Ordering::Release)` (write memory barrier)
  - `virtio_rmb()` = `fence(Ordering::Acquire)` (read memory barrier)
  - Replace raw fence calls in queue.rs with these abstractions
  - Add documentation referencing VirtIO spec sections

  **Implementation**:
  ```rust
  // drivers/src/virtio/mod.rs
  
  /// VirtIO write memory barrier.
  /// 
  /// Per VirtIO spec 2.7.7: "A write memory barrier before updating avail idx"
  /// Ensures descriptor writes are visible before publishing availability.
  #[inline(always)]
  pub fn virtio_wmb() {
      core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
  }
  
  /// VirtIO read memory barrier.
  /// 
  /// Per VirtIO spec 2.7.13: "A read memory barrier before reading used buffers"
  /// Ensures used_idx observation happens-before reading completion data.
  #[inline(always)]
  pub fn virtio_rmb() {
      core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
  }
  ```

  **Must NOT do**:
  - Do NOT add weak_barriers parameter yet (future ARM optimization)
  - Do NOT make these conditional (keep simple)
  - Do NOT add cfg attributes for different architectures

  **Parallelizable**: NO (depends on tasks 2 and 3)

  **References**:

  **Pattern References**:
  - Linux `include/linux/virtio_ring.h:37-45` - `virtio_wmb()`/`virtio_rmb()` definitions
  - `drivers/src/virtio/mod.rs` - Where to add the abstractions

  **Documentation References**:
  - VirtIO spec v1.2, "Split Virtqueues" → barrier requirements for making buffers available and processing used ring
  - PDF: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.pdf
  - TeX source: https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/tex/split-ring.tex
  - Linux kernel reference: https://github.com/torvalds/linux/blob/v6.7/include/linux/virtio_ring.h#L37-L45

  **WHY Each Reference Matters**:
  - Linux shows the exact abstraction we're creating
  - mod.rs is the natural location for shared virtio utilities
  - Spec provides text for documentation comments

  **Acceptance Criteria**:

  **Manual Execution Verification:**
  - [ ] Code review verification:
    - `virtio_wmb()` and `virtio_rmb()` exist in `drivers/src/virtio/mod.rs`
    - Both functions have documentation referencing VirtIO spec
    - `queue.rs` uses these functions instead of raw `fence()` calls
  - [ ] Build verification:
    - Command: `cargo build --target x86_64-slos`
    - Expected: Compiles without errors or warnings

  **Commit**: YES
  - Message: `drivers(virtio): add virtio_wmb/rmb abstraction for portable barriers`
  - Files: `drivers/src/virtio/mod.rs`, `drivers/src/virtio/queue.rs`
  - Pre-commit: `cargo build --target x86_64-slos`

---

### Phase 2: Preemption Subsystem

- [x] 5. Analyze `PreemptGuard::drop()` race condition

  **What to do**:
  - Study the interaction between `PREEMPT_COUNT` and `RESCHEDULE_PENDING`
  - Document the race window in `drop()` implementation
  - Determine if `Relaxed` ordering is safe or if we need `Release/Acquire` pairs
  - Consider: interrupt sets `RESCHEDULE_PENDING` between `fetch_sub` and `swap`

  **Analysis questions to answer**:
  1. Can an interrupt fire between `fetch_sub` and `swap`?
  2. If yes, can it set `RESCHEDULE_PENDING` and have it missed?
  3. Is the callback guaranteed to be called eventually, or can it be permanently lost?
  4. What ordering does the callback mechanism require?

  **Must NOT do**:
  - Do NOT change code yet - analysis only

  **Parallelizable**: YES (can start while Phase 1b executes)

  **References**:

  **Pattern References**:
  - `lib/src/preempt.rs:64-76` - Current `drop()` implementation with race window
  - `core/src/scheduler/scheduler.rs` - Where `RESCHEDULE_CALLBACK` is registered

  **WHY Each Reference Matters**:
  - preempt.rs shows the exact race window to analyze
  - scheduler.rs shows what the callback does and its correctness requirements

  **Acceptance Criteria**:

  **Analysis Deliverable:**
  - [ ] Create directory: `mkdir -p .sisyphus/analysis`
  - [ ] Create analysis document: `.sisyphus/analysis/preempt-drop-race.md`
  - [ ] Document:
    - Is the race real or theoretical?
    - What ordering is required for correctness?
    - Recommended ordering for per-CPU implementation
    - Whether `PreemptGuard::new()` needs to disable interrupts

  **Commit**: YES (analysis document only)
  - Message: `docs: add preemption drop race condition analysis`
  - Files: `.sisyphus/analysis/preempt-drop-race.md`
  - Pre-commit: None (documentation only)

---

- [x] 6. Implement per-CPU preemption counter

  **What to do**:
  - Modify `PreemptGuard` to use existing `PerCpuData.preempt_count` instead of global `PREEMPT_COUNT`
  - NOTE: `PerCpuData` already has `preempt_count: AtomicU32` field (line 46 of percpu.rs)
  - Use `get_percpu_data()` to access per-CPU data (NOT `get_current_cpu_data()` which doesn't exist)
  - Use atomic operations (`fetch_add`/`fetch_sub`) since interrupts are NOT disabled in `PreemptGuard::new()`

  **CRITICAL: Concurrency Model**:
  - `PreemptGuard::new()` does NOT disable interrupts (only `IrqPreemptGuard` does)
  - Therefore, an interrupt can fire while incrementing/decrementing the counter
  - Must use `AtomicU32` operations (existing field type) to avoid data races
  - Ordering: `Relaxed` is sufficient since counter is per-CPU (no cross-CPU sync needed)
  - An interrupt on same CPU sees consistent counter value (same CPU = sequential execution)

  **ALL FUNCTIONS TO MODIFY** (complete list):

  ```rust
  // lib/src/preempt.rs - COMPLETE CHANGES REQUIRED
  use crate::percpu::get_percpu_data;
  
  impl PreemptGuard {
      // MODIFY: Use per-CPU counter
      pub fn new() -> Self {
          let percpu = get_percpu_data();
          percpu.preempt_count.fetch_add(1, Ordering::Relaxed);
          Self { _marker: PhantomData }
      }
  
      // MODIFY: Read per-CPU counter
      pub fn is_active() -> bool {
          get_percpu_data().preempt_count.load(Ordering::Relaxed) > 0
      }
  
      // MODIFY: Read per-CPU counter
      pub fn count() -> u32 {
          get_percpu_data().preempt_count.load(Ordering::Relaxed)
      }
  
      // KEEP UNCHANGED: RESCHEDULE_PENDING stays global (see below)
      pub fn set_reschedule_pending() { ... }
      pub fn is_reschedule_pending() -> bool { ... }
      pub fn clear_reschedule_pending() { ... }
  }
  
  impl Drop for PreemptGuard {
      fn drop(&mut self) {
          let percpu = get_percpu_data();
          let prev = percpu.preempt_count.fetch_sub(1, Ordering::Relaxed);
          debug_assert!(prev > 0, "preempt_count underflow");
          
          // KEEP: RESCHEDULE_PENDING check unchanged (global is correct)
          if prev == 1 && RESCHEDULE_PENDING.swap(0, Ordering::SeqCst) != 0 {
              if let Some(callback) = unsafe { RESCHEDULE_CALLBACK } {
                  callback();
              }
          }
      }
  }
  ```

  **RESCHEDULE_PENDING Decision**:
  - **Stays GLOBAL** for this task (not per-CPU)
  - **Why this is correct**: 
    - Timer IRQ is routed to BSP only during init (see `drivers/src/irq.rs:57-64` - routes to `apic::get_id()`)
    - Routes are programmed on BSP during init and NOT reprogrammed on APs
    - `set_reschedule_pending()` is called from timer handler, which runs on BSP only
    - The check in `drop()` is also on the same CPU (BSP for most cases currently)
    - Cross-CPU rescheduling uses IPIs, not this flag
  - **Invariant** (must be maintained): Timer IRQ route stays fixed to BSP; if multi-CPU timer handling is added later, RESCHEDULE_PENDING must become per-CPU
  - **SeqCst ordering kept**: The `swap(0, SeqCst)` atomically clears and reads, preventing race where interrupt sets flag between check and clear.
  - **Future work**: Per-CPU reschedule pending when multi-CPU timer handling is added

  **Callers of PreemptGuard functions** (verified in scheduler):
  - `core/src/scheduler/scheduler.rs:674` - `if !PreemptGuard::is_active()`
  - `core/src/scheduler/scheduler.rs:877` - `if PreemptGuard::is_active()`  
  - `core/src/scheduler/scheduler.rs:921` - `if should_set == Some(true) && !PreemptGuard::is_active()`
  - `core/src/scheduler/scheduler.rs:927` - `if PreemptGuard::is_active()`
  
  **All these call sites are checking LOCAL CPU's preemption state**, so per-CPU counter is semantically correct.

  **Must NOT do**:
  - Do NOT change the API surface (PreemptGuard::new(), is_active(), count())
  - Do NOT break IrqPreemptGuard (it uses PreemptGuard internally)
  - Do NOT remove the global PREEMPT_COUNT completely - keep it with `#[allow(dead_code)]` attribute
    - Rationale: Allows rollback if issues found, can remove in future cleanup PR
    - Alternative: Keep it updated in parallel with per-CPU counter for verification
  - Do NOT change RESCHEDULE_PENDING to per-CPU in this task
  - Do NOT assume interrupts are disabled - they are NOT in PreemptGuard::new()

  **Parallelizable**: NO (depends on task 5)

  **References**:

  **Pattern References**:
  - `lib/src/percpu.rs:46` - Existing `preempt_count: AtomicU32` field in PerCpuData
  - `lib/src/percpu.rs:231-235` - `get_percpu_data()` accessor function (returns `&'static PerCpuData`)
  - `lib/src/preempt.rs:23-56` - Current PreemptGuard implementation (ALL functions to modify)
  - `lib/src/preempt.rs:64-76` - Current Drop impl with RESCHEDULE_PENDING logic
  - `lib/src/preempt.rs:82-95` - IrqPreemptGuard (must keep working)
  - `core/src/scheduler/scheduler.rs:674,877,921,927` - Callers of `is_active()`

  **API References**:
  - `lib/src/percpu.rs:231` - `get_percpu_data() -> &'static PerCpuData`
  - `lib/src/percpu.rs:183` - `get_current_cpu() -> usize`

  **External References**:
  - Linux `include/linux/preempt.h` - Per-CPU preempt_count pattern
  - URL: https://github.com/torvalds/linux/blob/v6.7/include/linux/preempt.h

  **WHY Each Reference Matters**:
  - percpu.rs:46 shows field already exists - we USE it, not create it
  - percpu.rs:231-235 shows the correct accessor
  - preempt.rs shows ALL functions that need modification
  - scheduler.rs callers verify semantic correctness of per-CPU change

  **Acceptance Criteria**:

  **Manual Execution Verification:**
  - [ ] Build verification:
    - Command: `cargo build --target x86_64-slos`
    - Expected: Compiles without errors
  - [ ] Test verification:
    - Command: `make test`
    - Expected: All tests pass (scheduler tests exercise preemption)
  - [ ] SMP verification:
    - Command: `make boot VIDEO=1` (multi-CPU QEMU)
    - Let roulette run for 30 seconds
    - Expected: No hangs, no scheduler anomalies

  **Commit**: YES
  - Message: `lib(preempt): implement per-CPU preemption counter for proper SMP support`
  - Files: `lib/src/preempt.rs`, `lib/src/percpu.rs`
  - Pre-commit: `cargo build --target x86_64-slos && make test`

---

## Commit Strategy

| After Task | Message | Files | Verification |
|------------|---------|-------|--------------|
| 1 | `drivers(virtio): add fence/spin count instrumentation for performance analysis` | queue.rs, virtio_gpu.rs | VIRGL=1 VIDEO=1 make boot |
| 2 | `drivers(virtio): optimize poll_used barrier placement` | queue.rs | make boot VIDEO=1, make test |
| 3 | `drivers(virtio): upgrade submit barrier to real fence` | queue.rs | make boot VIDEO=1 |
| 4 | `drivers(virtio): add virtio_wmb/rmb abstraction` | mod.rs, queue.rs | cargo build |
| 5 | `docs: add preemption drop race condition analysis` | .sisyphus/analysis/preempt-drop-race.md | N/A (docs) |
| 6 | `lib(preempt): implement per-CPU preemption counter` | preempt.rs | make test |

**Notes**:
- Task 5 creates a new file in `.sisyphus/analysis/`
- Task 6 only modifies `preempt.rs` - `percpu.rs` already has the `preempt_count` field at line 46

---

## Success Criteria

### Verification Commands
```bash
# Build verification
cargo build --target x86_64-slos  # Expected: success, no warnings

# Test verification  
make test  # Expected: PASS

# Performance verification (manual, with VirtIO-GPU)
VIRGL=1 VIDEO=1 make boot  # Expected: Visually smooth roulette animation
```

### Final Checklist
- [x] All "Must Have" present:
  - [x] Barrier placement satisfies VirtIO spec
  - [x] Release before `avail_idx`, Acquire after `used_idx`
  - [x] Per-CPU preemption counter implemented
- [x] All "Must NOT Have" absent:
  - [x] No fences removed entirely
  - [x] No VirtIO-blk changes
  - [x] No interrupt-driven completion
  - [x] No scheduler fence changes
- [x] All tests pass (`make test`)
- [ ] Roulette runs at acceptable frame rate (minimum 10 FPS, target 30+ FPS)
- [ ] No visual corruption in framebuffer
- [ ] Boot with `VIRGL=1 VIDEO=1 make boot` succeeds
