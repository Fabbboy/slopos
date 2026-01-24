# PreemptGuard::drop() Race Condition Analysis

**Date**: 2026-01-24  
**Analyzer**: Atlas (Orchestrator)  
**Context**: VirtIO-GPU Performance Optimization - Task 5

---

## Executive Summary

**Race Condition**: REAL and EXPLOITABLE  
**Severity**: Medium (can cause missed reschedules, not memory corruption)  
**Recommended Fix**: Use per-CPU preemption counter with proper atomic ordering

---

## Current Implementation Analysis

### Code Under Review

```rust
// lib/src/preempt.rs:64-76
impl Drop for PreemptGuard {
    #[inline]
    fn drop(&mut self) {
        let prev = PREEMPT_COUNT.fetch_sub(1, Ordering::SeqCst);  // ← STEP 1
        debug_assert!(prev > 0, "preempt_count underflow");

        if prev == 1 && RESCHEDULE_PENDING.swap(0, Ordering::SeqCst) != 0 {  // ← STEP 2
            if let Some(callback) = unsafe { RESCHEDULE_CALLBACK } {
                callback();  // ← STEP 3: deferred_reschedule_callback()
            }
        }
    }
}
```

### The Race Window

**Timeline of Events**:

```
T0: Thread drops PreemptGuard
T1: fetch_sub(1) executes → prev=1, PREEMPT_COUNT now 0
    ┌─────────────────────────────────────────────────┐
    │ RACE WINDOW: Interrupts can fire here!         │
    │ - PREEMPT_COUNT is 0 (preemption re-enabled)   │
    │ - RESCHEDULE_PENDING not yet checked           │
    │ - Timer IRQ can set RESCHEDULE_PENDING=1       │
    └─────────────────────────────────────────────────┘
T2: swap(0) executes → reads RESCHEDULE_PENDING
T3: Callback invoked (if flag was set)
```

**Scenario 1: Race Occurs**
1. Thread drops guard, `fetch_sub(1)` sets `PREEMPT_COUNT=0`
2. **Timer IRQ fires** before `swap(0)` executes
3. Timer handler calls `set_reschedule_pending()` → `RESCHEDULE_PENDING=1`
4. Timer handler returns (does NOT reschedule because `PREEMPT_COUNT=0` now)
5. Thread continues, `swap(0)` reads and clears `RESCHEDULE_PENDING`
6. Callback invoked → reschedule happens

**Result**: Reschedule happens, but delayed by one race window. **NOT LOST**.

**Scenario 2: Permanent Loss (Theoretical)**
1. Thread drops guard, `fetch_sub(1)` sets `PREEMPT_COUNT=0`
2. Timer IRQ fires, sets `RESCHEDULE_PENDING=1`
3. **Another interrupt fires** before `swap(0)`, also sets `RESCHEDULE_PENDING=1`
4. Thread continues, `swap(0)` clears flag
5. Callback invoked once
6. **Second reschedule request is lost** (flag was already 1, no increment)

**Result**: If multiple reschedule requests occur in race window, only ONE is serviced.

---

## Question-by-Question Analysis

### Q1: Can an interrupt fire between `fetch_sub` and `swap`?

**Answer**: YES

**Evidence**:
- `PreemptGuard::new()` does NOT disable interrupts (only increments counter)
- Only `IrqPreemptGuard` disables interrupts via `save_flags_cli()`
- Timer IRQ is enabled and fires periodically (100 Hz, see `drivers/src/pit.rs`)
- The race window is ~10-50 CPU cycles (fetch_sub + conditional check)

**Conclusion**: Interrupts are ENABLED during the race window.

---

### Q2: If yes, can it set `RESCHEDULE_PENDING` and have it missed?

**Answer**: NO (single request), YES (multiple requests)

**Single Request Case**:
- Timer IRQ sets `RESCHEDULE_PENDING=1`
- `swap(0, SeqCst)` atomically reads and clears it
- Callback is invoked
- **Reschedule happens** (delayed, but not lost)

**Multiple Request Case**:
- IRQ1 sets `RESCHEDULE_PENDING=1`
- IRQ2 sets `RESCHEDULE_PENDING=1` (no change, already 1)
- `swap(0)` clears flag
- Callback invoked ONCE
- **Second reschedule request is lost** (counter semantics, not flag semantics)

**Current Implementation Flaw**:
- `RESCHEDULE_PENDING` is a FLAG (0 or 1), not a COUNTER
- Multiple reschedule requests collapse into one
- This is acceptable for timer-based preemption (next tick will retry)
- This is NOT acceptable for IPI-based cross-CPU reschedules (future work)

---

### Q3: Is the callback guaranteed to be called eventually?

**Answer**: YES (for timer-based reschedules), MAYBE (for future IPI-based)

**Timer-Based Reschedule** (current implementation):
- Timer fires every 10ms (100 Hz)
- If reschedule is missed, next timer tick will retry
- Worst-case delay: 10ms (one timer period)
- **Guarantee**: Eventually called within one timer period

**IPI-Based Reschedule** (future work):
- Cross-CPU reschedule sends IPI to target CPU
- If IPI arrives during race window and is lost, NO RETRY
- Target CPU may never reschedule
- **Guarantee**: NONE (permanent loss possible)

**Conclusion**: Current implementation is SAFE for timer-based preemption, UNSAFE for future IPI-based cross-CPU reschedules.

---

### Q4: What ordering does the callback mechanism require?

**Answer**: `Release` on decrement, `Acquire` on check

**Memory Ordering Requirements**:

1. **Decrement Operation** (`fetch_sub`):
   - Must use `Release` ordering
   - Ensures all memory operations in preempt-disabled section complete BEFORE counter decrements
   - Prevents reordering of critical section operations past the guard drop

2. **Flag Check** (`swap`):
   - Must use `Acquire` ordering (or stronger)
   - Ensures flag observation happens-before callback invocation
   - Current `SeqCst` is overkill but correct

3. **Flag Set** (`store` in `set_reschedule_pending`):
   - Must use `Release` ordering
   - Ensures timer handler's decision to reschedule is visible to callback

**Current Implementation**:
- Uses `SeqCst` everywhere (correct but slower than necessary)
- `SeqCst` provides both `Release` and `Acquire` semantics
- Overkill for single-CPU case, necessary for multi-CPU

**Recommended Ordering** (per-CPU implementation):
```rust
// In PreemptGuard::drop()
let prev = percpu.preempt_count.fetch_sub(1, Ordering::Release);  // ← Release
if prev == 1 && RESCHEDULE_PENDING.swap(0, Ordering::Acquire) != 0 {  // ← Acquire
    callback();
}

// In set_reschedule_pending()
RESCHEDULE_PENDING.store(1, Ordering::Release);  // ← Release
```

**Why this is safe**:
- `Release` on decrement ensures critical section completes before preemption re-enables
- `Acquire` on flag check ensures flag observation happens-before callback
- Per-CPU counter eliminates cross-CPU synchronization (no `SeqCst` needed)

---

## Per-CPU Implementation Recommendations

### Recommended Changes

1. **Use Per-CPU Counter** (already exists in `PerCpuData.preempt_count`):
   ```rust
   impl PreemptGuard {
       pub fn new() -> Self {
           let percpu = get_percpu_data();
           percpu.preempt_count.fetch_add(1, Ordering::Relaxed);  // ← Relaxed (per-CPU)
           Self { _marker: PhantomData }
       }
   }
   
   impl Drop for PreemptGuard {
       fn drop(&mut self) {
           let percpu = get_percpu_data();
           let prev = percpu.preempt_count.fetch_sub(1, Ordering::Release);  // ← Release
           
           if prev == 1 && RESCHEDULE_PENDING.swap(0, Ordering::Acquire) != 0 {  // ← Acquire
               if let Some(callback) = unsafe { RESCHEDULE_CALLBACK } {
                   callback();
               }
           }
       }
   }
   ```

2. **Keep `RESCHEDULE_PENDING` Global** (for now):
   - Timer IRQ is routed to BSP only (see `drivers/src/irq.rs:57-64`)
   - Cross-CPU reschedules use IPIs, not this flag
   - Future work: per-CPU reschedule pending when multi-CPU timer handling is added

3. **Ordering Justification**:
   - `Relaxed` on increment: Per-CPU counter, no cross-CPU sync needed
   - `Release` on decrement: Ensures critical section completes before re-enabling preemption
   - `Acquire` on flag check: Ensures flag observation happens-before callback

---

## Interrupt Disable Analysis

### Should `PreemptGuard::new()` Disable Interrupts?

**Answer**: NO (current design is correct)

**Rationale**:
1. **Separation of Concerns**:
   - `PreemptGuard`: Prevents scheduler from preempting (voluntary reschedule only)
   - `IrqPreemptGuard`: Prevents interrupts AND scheduler preemption
   - Two distinct use cases, two distinct guards

2. **Performance**:
   - Disabling interrupts is expensive (CLI/STI instructions)
   - Many critical sections only need preemption disabled, not IRQ disabled
   - Example: Incrementing a per-CPU counter (no IRQ disable needed)

3. **Correctness**:
   - Per-CPU counter with atomic operations is safe even with interrupts enabled
   - Interrupt on same CPU sees consistent counter value (sequential execution)
   - No data race (atomic operations provide necessary synchronization)

4. **Existing Pattern**:
   - Linux kernel has same separation: `preempt_disable()` vs `local_irq_save()`
   - SlopOS follows this pattern with `PreemptGuard` vs `IrqPreemptGuard`

**Conclusion**: Current design is correct. Do NOT add interrupt disable to `PreemptGuard::new()`.

---

## Race Condition Severity Assessment

### Is This a Critical Bug?

**Answer**: NO (medium severity, acceptable for current implementation)

**Impact Analysis**:

1. **Current System** (timer-based preemption only):
   - Worst-case: Reschedule delayed by one timer period (10ms)
   - No permanent loss (next timer tick retries)
   - No memory corruption or data races
   - **Severity**: LOW

2. **Future System** (IPI-based cross-CPU reschedules):
   - Worst-case: Reschedule request permanently lost
   - Target CPU may never reschedule
   - Potential deadlock or starvation
   - **Severity**: HIGH

**Recommendation**: Fix now (as part of per-CPU migration) to prevent future issues.

---

## Conclusion

### Summary of Findings

1. **Race is REAL**: Interrupt can fire between `fetch_sub` and `swap`
2. **Current impact is LOW**: Timer-based preemption retries on next tick
3. **Future impact is HIGH**: IPI-based reschedules could be permanently lost
4. **Fix is SIMPLE**: Use per-CPU counter with `Release`/`Acquire` ordering
5. **No IRQ disable needed**: Atomic operations on per-CPU data are sufficient

### Recommended Action Plan

1. **Task 6**: Migrate to per-CPU preemption counter (already exists in `PerCpuData`)
2. **Task 7**: Use `Release` ordering on decrement, `Acquire` on flag check
3. **Task 8**: Keep `RESCHEDULE_PENDING` global (timer IRQ routed to BSP only)
4. **Future work**: Per-CPU reschedule pending when multi-CPU timer handling is added

### Ordering Summary

| Operation | Current | Recommended | Justification |
|-----------|---------|-------------|---------------|
| `new()` increment | `SeqCst` | `Relaxed` | Per-CPU, no cross-CPU sync |
| `drop()` decrement | `SeqCst` | `Release` | Ensures critical section completes |
| Flag check | `SeqCst` | `Acquire` | Ensures flag observation before callback |
| Flag set | `SeqCst` | `Release` | Ensures decision visible to callback |

**Performance gain**: `SeqCst` → `Release`/`Acquire` eliminates unnecessary memory barriers on x86 (MFENCE → compiler barrier only).

---

## References

- `lib/src/preempt.rs:64-76` - Current `drop()` implementation
- `lib/src/percpu.rs:46` - Per-CPU `preempt_count` field (already exists)
- `core/src/scheduler/scheduler.rs:705` - Reschedule callback registration
- `drivers/src/irq.rs:57-64` - Timer IRQ routing (BSP only)
- Linux kernel: `include/linux/preempt.h` - Preemption disable patterns
- Rust Atomics and Locks (Mara Bos) - Memory ordering semantics
