# SlopOS Known Issues

Last updated: 2026-07-30

---

## The 256th process created since boot cannot run

**Status**: Open
**Severity**: High (trivially reachable; the machine stops being able to start programs)
**Component**: `mm/src/process_vm.rs`, `sched/src/scheduler.rs`

### Description

Process *slots* are recycled; process *ids* are not. `create_process_vm` finds a free
slot in `PROCESS_VMS[MAX_PROCESSES]` and then draws `process_id` from
`VmSlotAlloc::next_process_id`, a strictly monotonic counter that is never bounded and
never reused (`mm/src/process_vm.rs:1541-1542`). Only `init_process_vm()` resets it.

`execute_task` refuses to dispatch any task whose `process_id >= MAX_PROCESSES` and
terminates it instead (`sched/src/scheduler.rs:1071-1084`):

```
SCHED: refusing to dispatch task N with invalid pid 256
```

So the 256th process created since boot is built successfully — slot allocation,
address space, ELF load all succeed — and is then killed the first time the scheduler
looks at it. Every process created after it meets the same fate. The system
permanently loses the ability to start a program, until reboot.

Reaching it takes 255 ordinary process creations: a shell session running commands
gets there without trying.

The same shape appears once more with a different ceiling, again from an unbounded
monotonic id used as a direct array index:

- `PROCESS_TLB_INFO` is `[ProcessTlbInfo; MAX_PROCESSES]` indexed by `process_id`
  (`mm/src/tlb.rs:400-412`), so `flush_all_for_process` silently no-ops past 256. The
  dispatch guard above is the only reason this is not a stale-TLB correctness bug: an
  address space with `pid >= 256` is never activated on any CPU.

### Why the test suite does not catch it

The suite runs 2716 tests in ~36 s and creates far fewer than 255 processes. Nothing
asserts that process creation still works after an arbitrary number of prior creations,
and the failure is a klog line plus a terminated task rather than a test failure.

### Fix

Bound and recycle the id, or stop using it as an index. `VmSlotAlloc::alloc_generation`
(`mm/src/process_vm.rs:119-124`) already supplies the never-reused generation values a
recycling allocator needs to stay ABA-safe, and `slopos-ostd`'s `Handle`/`HandleTable`
already implements exactly that pattern. The structural version — a `Process` object
that owns its vm_space, fd table, tlb info and session, resolved once per syscall
instead of re-derived by scan in four subsystems — is in `plans/process-identity.md`.

Whatever lands must include a regression test that creates more than `MAX_PROCESSES`
processes sequentially and asserts the last one runs.

### Related Files

- `mm/src/process_vm.rs:1520-1546` — slot recycling next to unbounded id allocation
- `sched/src/scheduler.rs:1071-1084` — the dispatch guard that turns it into a hard stop
- `mm/src/tlb.rs:400-412` — pid-indexed TLB info, same ceiling
- `mm/src/memory_layout_defs.rs:370` — `MAX_PROCESSES = 256`

---

## Two writers mutate the kernel master PML4 with incompatible synchronisation

**Status**: Open
**Severity**: Low (no production instance of the race; the proof's stated premise
is what is wrong)
**Component**: `mm/src/paging/tables.rs`, `slopos-ostd/src/mm/vm_space.rs`,
`verification/proofs/vm_space_cursor.rs`

The kernel master PML4 (from `read_cr3()`) is wrapped as a `VmSpace`
(`boot/src/boot_memory.rs:87`) and *also* recorded as
`KERNEL_PML4_PHYS: AtomicU64` (`mm/src/paging/tables.rs:40,447`). The latter has a
complete second walker with its own `PageTableLevel`, `PageTableEntry`, `PageFlags`
and huge-page split, writing PTEs with `Ordering::Relaxed`
(`page_table_defs.rs:245-259`).

`verification/proofs/vm_space_cursor.rs:27-46` states its whole-system premise as:

> SlopOS uses the **Rust borrow checker** — `CursorMut<'a>` holds `&'a mut VmSpace`,
> so at most one mutator exists per address space at any time, statically, with no
> SMT obligation.

For the kernel half that premise does not hold: `sched/src/task_stack.rs:149,208`
calls the raw `map_page_4kb`/`unmap_page` on every task create and exit,
concurrently on any CPU.

No production race exists today — the only live kernel-half `CursorMut` is
`mark_kernel_global`, which runs at boot priority 55 on the BSP before SMP
bring-up and before any task exists, and the runtime raw writers only store leaf
PTEs into sentinel-preallocated tables. The defect is that a machine-checked proof
states a premise the tree does not satisfy, which is how a future change silently
invalidates the proof.

Fix: route `sched::task_stack` through OSTD's `KERNEL_VM_SPACE` cursor —
`mm/src/kernel_mappings.rs::kernel_map_4kb` already exists for this and is dead
code — then delete the mutating half of `mm/src/paging/tables.rs` with its
duplicate walker. Until that lands, amend the proof header to scope its exclusivity
claim to user address spaces.

---

## slibc keeps no open-stream list, so `exit()` loses buffered writes

**Status**: Open
**Severity**: Low (nothing in the tree calls `fopen`)
**Component**: `slibc/src/stdio/`

`fflush(NULL)` explicitly flushes only `stdout` and `stderr` and returns
(`slibc/src/stdio/file.rs:287-300`); there is no registry of open streams to walk.
`fopen` mallocs a `FILE` and never links it into a list (`:40-66`), and writable
streams default to `BufferMode::Full`, holding up to 4096 bytes each. `exit()`
calls `fflush(NULL)` (`slibc/src/process/mod.rs:147-153`), so every `fopen`'d write
stream loses its buffer at process exit. This violates C11 §7.22.4.4.

Latent today: nothing in the repository calls `fopen`/`fdopen`/`fclose`, slibc
ships rlib-only with no C consumers, and the Rust userland's file, stdout and exit
paths all bypass C stdio.

Related and in the same module: stdio is completely unlocked while slibc ships
`pthread_create` and backs Rust's `std::thread`, and a `FILE` shares one buffer
between read and write cursors with no direction state, so an `r+`/`w+` direction
switch loses writes or flushes read-ahead into the file.

Fix: an intrusive open-stream list linked on `fopen`/`fdopen`, unlinked on
`fclose`, walked by `fflush(NULL)` and by a new `__stdio_exit` called from `exit`.
It needs the same lock as per-stream thread safety, so land the two together, and
add the direction-state field while the file is open.

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

## A task on an AP can stall behind three unbounded or O(CPUs) scheduler waits

**Status**: Open
**Severity**: Low (latency only; no correctness consequence)
**Component**: `sched/src/scheduler.rs`, `sched/src/task/task_reclaim.rs`

Task termination itself does not stall an AP — `task_terminate` serialises with a
local `PreemptGuard` (`sched/src/task/task_lifecycle.rs:836`), and the AP pause is
reached only from `task_shutdown_all` (`:1177`), whose sole production caller is
`kernel_shutdown` (`boot/src/shutdown.rs:165`). Three other waits on the ordinary
scheduling path can, and a latency-sensitive task such as the compositor is where
they would be seen.

**The `on_cpu` handover spin** (`scheduler.rs:1240-1242`) is the one genuinely
unbounded wait a dispatching AP can hit. Having dequeued a task whose prior CPU
has not finished its switch-out tail, the AP spins on that CPU's Release store of
`on_cpu` with no bound and no fallback. The window is short by construction — it
is the tail of a context switch — but a prior CPU that takes an interrupt inside
it extends the spin by exactly that handler's duration.

**`unschedule_task`'s per-CPU sweep** (`scheduler.rs:1017-1026`) takes every
CPU's scheduler in turn on every termination, to find the one queue the task
might be on. O(CPUs) lock acquisitions per termination, on a path a
spawn-heavy workload runs constantly.

**The task graveyard drains only from the idle dispatcher**
(`task_reclaim.rs:174-208`), so under sustained load dead tasks' kernel stacks and
address spaces accumulate until some CPU goes idle. Already tracked as item 2 of
`plans/deferred-work.md`; recorded here only because it shares this shape.

Fixes for the first two: bound the `on_cpu` spin, re-enqueueing rather than
spinning past a threshold; and record the owning CPU on the task so
`unschedule_task` takes one lock instead of `n`. Neither is scheduled work.

---

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `slopos-ostd/src/cpu/x86_64/pcr.rs` for architecture details.
