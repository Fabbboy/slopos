# SlopOS Known Issues

Last updated: 2026-07-11

---

## Multi-CPU userland blocked by a buddy/slab cross-CPU TLB-shootdown deadlock

**Status**: Open (thread-per-core *scheduler* half resolved; the *allocator* half is the blocker)  
**Severity**: Low (no production consumer; the test suite runs userland co-located)  
**Component**: `mm/src/slab/`, `mm/src/buddy*` (the SMP alloc / reuse-drain path)

### Description

The thread-per-core **scheduler** path is complete. A `ring_enter`-parked reactor woken cross-core is
now re-dispatched on an affinity-permitted CPU (the remote-inbox flush before every dispatch pick +
affinity-honoring wake selection), a runnable thread whose affinity no longer permits its current CPU
is repatriated (`task_apply_affinity` + the switch-out-tail migration), `select_target_cpu` falls back
to an affinity-permitted online CPU instead of misplacing, and `set_cpu_affinity` re-places. Verified:
with strict per-worker pins, `percore_reactor_test`'s N reactors run on N distinct cores and the
cross-core round-trip completes (each worker on its pinned CPU, `multi_cpu && each_on_pinned_cpu`).

The remaining blocker is in the **allocator**, exposed (not caused) by the distribution the scheduler
now produces: under sustained concurrent allocation on multiple CPUs, a `SlabAllocator::alloc_one`
running under a per-CPU `CpuLocal::get_pinned_mut` pin (interrupts off) can reach the buddy reuse
path, whose synchronous cross-CPU TLB shootdown spins with interrupts off — stopping that CPU's timer
tick until the NMI watchdog fires (`NMI WATCHDOG: CPU N not responding`). This is the latent
"heap-alloc under a cli-lock / LUF reuse-drain is a hidden cross-CPU wait" hazard. It is intermittent
(~2/12 with 4 strictly-pinned workers) and does **not** occur co-located, so `percore_reactor_test`
ships with the co-located `(1 << idx) | 1` mask and the full suite is reliably green.

### Fix (allocator SMP hardening)

The buddy reuse-path cross-CPU TLB shootdown must not spin interrupts-off under a slab per-CPU pin.
Candidates: defer the shootdown out of the pinned/IRQ-off region; make the shootdown wait
tick-pumping / interruptible; or refill slab magazines without holding the pin across a buddy reuse
drain. See the related latent notes on synchronous cross-CPU shootdown on the buddy reuse path.

### Related Files

- `mm/src/slab/allocator.rs`, `mm/src/slab/magazine.rs` — `alloc_one`, `get_pinned_mut`
- `mm/src/buddy*` — the reuse-drain / cross-CPU TLB shootdown
- `userland/src/bin/tests/percore_reactor_test.rs` — flip the worker mask to strict `1 << idx` and
  promote the `multi_cpu` / `each_on_pinned_cpu` assertions once the allocator is hardened; the
  scheduler already passes that variant except for this deadlock

---

## Performance: Compositor Frame Rate During Task Termination

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `sched/`

### Description

When a task terminates, `pause_all_aps()` is called which blocks all AP scheduler loops. While this is necessary for safe task cleanup, it can cause brief stalls in compositor frame rendering if the compositor happens to be scheduled on an AP.

### Current Behavior

1. Task calls `task_terminate()`
2. `pause_all_aps()` sets `AP_PAUSED = true` and waits for APs to stop executing
3. `release_task_dependents()` unblocks waiting tasks
4. `resume_all_aps()` sets `AP_PAUSED = false` and sends wake IPIs

During steps 2-3, any task on an AP (including compositor) is paused.

### Impact

- Brief frame drops (1-2 frames) during task termination
- More noticeable with frequent task spawning/termination

### Potential Optimizations

1. **Fine-grained locking**: Instead of pausing all APs, use per-task locks
2. **RCU-style cleanup**: Defer task cleanup to a dedicated kernel thread
3. **Lock-free dependent release**: Use atomic operations instead of global pause

### Related Files

- `sched/src/task/task_lifecycle.rs` - task teardown invoking the pause
- `sched/src/per_cpu.rs` - `pause_all_aps()`, `resume_all_aps()`

---

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `slopos-ostd/src/cpu/x86_64/pcr.rs` for architecture details.
