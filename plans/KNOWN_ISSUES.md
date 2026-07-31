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

The same shape appears twice more with different ceilings, both from an unbounded
monotonic id used as a direct array index:

- `PROCESS_TLB_INFO` is `[ProcessTlbInfo; MAX_PROCESSES]` indexed by `process_id`
  (`mm/src/tlb.rs:400-412`), so `flush_all_for_process` silently no-ops past 256. The
  dispatch guard above is the only reason this is not a stale-TLB correctness bug: an
  address space with `pid >= 256` is never activated on any CPU.
- The input-event map is a direct array indexed by the never-recycled task id with
  `TASK_MAP_SIZE = 16384`, so once a boot session has allocated that many task ids no
  process created afterwards can receive keyboard or pointer input.

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

## Five comments assert a close-on-fork ring-fd policy that does not exist

**Status**: Open
**Severity**: Low (documentation drift; no authorization boundary is crossed)
**Component**: `ring/`, `fs/src/fileio/`, `mm/src/process_vm.rs`

Five in-tree comments state that SlopRing fds are close-on-fork, citing
"SLOPRING § 14": `mm/src/process_vm.rs:2700-2702` and `:2934-2937`,
`ring/src/registry.rs:9-11`, `ring/src/enter.rs:222`, and `ring/src/file_ops.rs:3-6`
(which additionally claims exec teardown).

No such mechanism exists. `fileio_clone_table_for_process`
(`fs/src/fileio/fdtable.rs:152-155`) duplicates every valid descriptor with no
`FileKind` filter, and `fileio_open_fd_with_ops` (`fs/src/fileio/fdops.rs:846-860`)
passes `OpenMode::READ | OpenMode::WRITE`, so the `O_CLOEXEC` test at `fdops.rs:56`
is false for every ring fd. Ring fds are inherited by fork and survive exec.

This is contained: `registry::owner_is` gates `ring_enter` and all four register
ops, the child gets no ring VMA (`process_vm.rs:2939`), and read/write/poll on an
inherited ring fd return EINVAL/POLLNVAL. The consequence is ordinary inherited-fd
resource retention, not an authorization hole.

Fix: either make the claim true — a `close_on_fork` `FdEntry` bit honoured by
`fileio_clone_table_for_process`, plus `O_CLOEXEC` on the ring fd to match
io_uring — or delete the five comments and document `owner_pid` as the primary,
load-bearing check. Do not leave a documented mechanism the code does not
implement.

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
