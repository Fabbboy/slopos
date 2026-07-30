# Process identity: a bounded id and a handle that detects reuse

`create_process_vm` recycles the 256 VM slots but draws `process_id` from a
strictly monotonic counter that is never bounded and never reused
(`mm/src/process_vm.rs:1526-1543`, duplicated verbatim in the fork path at
`:2854-2879`). `execute_task` refuses to dispatch any task whose `process_id`
is at or above `MAX_PROCESSES` and terminates it instead
(`sched/src/scheduler.rs:1069-1084`). The 256th process created since boot is
built successfully and killed at first dispatch, and so is every process after
it, until reboot. A shell session running commands reaches that in an afternoon.

The bug is one line. The reason it is worth a plan is that the obvious fix is a
trap, and that the machinery to fix it properly is already in the tree, written
and tested, with zero callers.

## The trap

`targeted_flush_request` returns `Ok(())` — success — when the pid is out of
range (`mm/src/tlb.rs:876-884`), because `process_tlb_info` returns `None` and
the caller treats that as "nothing to do". `PROCESS_TLB_INFO` is the **only**
array in the tree indexed numerically by `process_id`; everything else that
looks pid-keyed (fs's `PROCESS_TABLES`, mm's `PROCESS_VMS`) is slot-indexed with
a linear scan and is correct at any pid value.

So the dispatch guard in `execute_task` is not redundant defensive coding. It is
the only reason a silently-skipped shootdown is currently harmless: an address
space with `pid >= 256` is never loaded on any CPU, so there is nothing to flush.
Delete the guard to "fix" the availability cliff and the result is a silent
stale-TLB condition on SMP — an availability bug traded for memory corruption.

**Nothing may relax that guard except a commit that also re-keys or relocates the
cpumask, and makes the missing-entry path fail loudly instead of returning `Ok`.**

## What is already built

- **`process_vm_handle` / `process_vm_with_handle`** (`mm/src/process_vm.rs:286-325`)
  mint and resolve a generation-checked `Handle<ProcessVm>`: slot bound checked,
  `NoEntry` and `Stale` distinguished, staleness a typed error rather than an
  aliasing read. Its regression test `test_process_vm_handle_stale_after_reuse`
  (`mm/src/tests/tests.rs:620-665`) passes today. **Production callers: zero.**
- **`VmSlotAlloc::alloc_generation`** (`:104-124`) already stamps a
  globally-monotonic generation on every slot binding. The uniqueness-over-time
  half of a recycling id exists and is simply never published.
- **`slopos_ostd::handle::HandleTable<T>`** with `with_fixed_capacity` — a
  recycling slot table used in production by five subsystems already
  (`mm/src/memfd.rs:82`, `fs/src/pipe.rs:177`, `net/src/unix_socket/mod.rs:69`,
  `drivers/src/virtio_blk.rs:781`, `fs/src/vfs_file_ops.rs:26`).
- **`job_control.rs`'s KArc/KWeak model** already states this project's posture
  in its own header: *"a reused pid can never be mistaken for the old group"*.

Every design must answer "what stops a recycled id from aliasing its predecessor".
This tree answered it twice and wired neither answer into process identity.

## Two facts that make this cheaper than it looks

**Nothing POSIX-visible is keyed on `process_id`.** `getpid` returns the *task*
id (`core/src/syscall/process_handlers.rs:487-492`), `waitpid` takes a `Tid`,
`kill` and the pgrp calls all resolve through `task_find_by_id`, and pidfd is
task-keyed. `process_id` reaches userland only as a read-only display field in
`UserTaskEntry` (`abi/src/syscall/types.rs:51-65`), rendered by sysmon. There is
no ABI to break.

**`process_id` already means "address space + fd table", not "thread".** The
clone path gives a thread its parent's `process_id` verbatim
(`sched/src/task/task_lifecycle.rs:1445-1448`). It is already a process identity.

## Design

Land the recycling allocator and the handle together. Neither is safe alone: a
recycling id without a staleness check detonates the aliasing hazards below on
the very next process creation, and a handle without a bounded id leaves the
cliff in place.

### The allocator

Replace `VmSlotAlloc::next_process_id` with a free list over `1..=MAX_PROCESSES`
held inside `VmSlotAlloc`. Both allocation sites collapse into one
`alloc_pid_and_slot()`; `destroy_process_vm` returns the id at the very end of
teardown. The free list is a fixed array or bitmap in `.bss` — nothing may
allocate under `VM_SLOT_ALLOC`, which is a cli-lock, because the buddy reuse path
performs a synchronous cross-CPU shootdown.

Reuse must be **delayed, not immediate**. Return ids to the tail of the free list
so the space cycles rather than handing back the just-freed id. This is
`idr_alloc_cyclic`'s reason for existing in Linux, and `randompid`'s in FreeBSD:
immediate reuse is what makes stale-id bugs reachable.

Do **not** take the tempting shortcut of `process_id = slot + 1`. It is less code
and removes the scan, but it makes reuse maximally aggressive and bakes the 256
cap into the identity itself.

### The identity token

Add `Handle<ProcessVm>` to `TaskInner` beside `process_id`, stamped where
`task_ref.process_id` is set today (`sched/src/task/task_lifecycle.rs:633`).
Convert mm's pid-keyed accessors to handle-keyed forms built on the existing
`process_vm_with_handle`, keeping thin pid-keyed shims for the display and
diagnostic callers.

This is what removes the 256-slot `find_slot_for_pid` scan
(`mm/src/process_vm.rs:274-284`) from the page-fault path and from
`process_vm_activate`, which runs on **every context switch into a user task**.
The performance win is incidental; the point is that a stale reference becomes a
typed `HandleError` instead of a silent wrong answer.

### Three latent bugs that recycling turns live

Each must land in or before the commit that enables reuse:

1. **`unregister_process_tlb` has zero callers** (`mm/src/tlb.rs:421-426`).
   `destroy_process_vm` never calls it, so a dead process's shootdown cpumask
   survives until the next `register_process_tlb` happens to clear it. Under
   recycling, the new occupant inherits the old occupant's CPU set and sends
   shootdown IPIs to CPUs that never mapped it — or, worse, misses ones that did.

2. **`fileio_create_table_for_process` returns success without creating anything**
   when a slot already carries that pid (`fs/src/fileio/fdtable.rs:71-77`). Under
   a monotonic pid this is unreachable. Under recycling it means a fresh process
   silently inherits a dead one's open descriptors. Its sibling
   `fileio_create_empty_table_for_process` already rejects; make the two agree.

3. **`init_process_vm` destroys every process VM and resets the counter but leaves
   fs's `PROCESS_TABLES` slots bound** (`mm/src/process_vm.rs:1849-1868`). Ten
   test fixtures call it mid-suite. Under recycling that is a live fd-inheritance
   bug inside the test harness itself. Either give it a matching fs teardown or
   restrict it to boot and give tests a narrower fixture.

## Phases

Each lands independently and leaves the tree green.

| # | Work | Done when |
|---|---|---|
| 1 | The three latent bugs above, in isolation: give `unregister_process_tlb` its caller in `destroy_process_vm`; make `fileio_create_table_for_process` reject a bound slot; fix `init_process_vm`'s fs teardown | Each has a test that fails before and passes after; no behaviour change under the monotonic allocator |
| 2 | Extract `execute_task`'s `pid_ok` expression into `pub(crate) fn dispatch_pid_ok(pid) -> bool`, and add the churn test that builds and abandons `MAX_PROCESSES + 64` user tasks asserting every one is dispatchable | The test fails at iteration 256 — this is the bug, reproduced in-harness |
| 3 | Free-list allocator with delayed reuse; both allocation sites collapse to one helper; id returned at the end of `destroy_process_vm` | The phase-2 test passes; `create_process_vm` never returns a pid at or above `MAX_PROCESSES` |
| 4 | Re-key the shootdown cpumask off `process_id` — either index by slot or move it into `ProcessVm`. Make the missing-entry path in `targeted_flush_request` a hard failure, not `Ok(())` | A pid the mask has no entry for fails loudly; the dispatch guard can be demoted to a debug assert |
| 5 | `Handle<ProcessVm>` in `TaskInner`; convert mm's fault-path and context-switch accessors to handle-keyed | `find_slot_for_pid` is off the page-fault and `process_vm_activate` paths; the stale-handle test is upgraded to prove pid reuse is detected |

Phases 1–3 close the user-visible bug. Phases 4–5 are what make it stay closed.

## Tests

The suite is blind here by construction: no existing test creates more than 8
processes (`mm/src/tests/tests_oom.rs:222-252`), and every mm process fixture
calls `init_process_vm()` first, which resets the counter. **A change can be
entirely wrong and entirely green.** The churn test is mandatory and must not
call `init_process_vm`.

- **sched `stest!`** — build and abandon `MAX_PROCESSES + 64` user tasks via the
  existing `task_build`/`task_abandon` fixture (`sched/src/sched_tests.rs:977-1020`),
  asserting `dispatch_pid_ok` for each. The one test that would have caught this.
- **mm `stest!`** — extend `test_process_vm_creation_pressure` from 8 to
  `MAX_PROCESSES + 64` create/destroy cycles, asserting each pid stays in range.
- **mm `stest!`** — upgrade `test_process_vm_handle_stale_after_reuse` from
  "slots recycle" to "pids recycle": capture a handle, destroy, recreate until the
  pid repeats, and assert the old handle resolves `Stale` rather than to the new
  occupant.
- **cross-crate `stest!`** — destroy only the VM, recycle the pid, assert
  `fileio_create_table_for_process` rejects and the new process sees an empty
  descriptor table.
- **mm `stest!`** — set cpumask bits via `notify_mm_switch`, destroy the process,
  assert the mask is clear before the id is reissued.

New test names are crate-prefixed (`slopos_mm::`, `slopos_sched::`), and
`scripts/check_test_count.sh`'s baseline must be re-measured with
`TEST_COUNT_BASELINE=0 scripts/check_test_count.sh` once these land.

## Where this ends

A `KArc<Process>` owning the address space, fd table, cpumask, job-control
membership and — eventually — resource counters, with `Task` holding it in an
`RcuArcSlot<Process>` exactly as it already holds `process_group`. That removes
every pid scan at once, turns `process_has_other_live_tasks`'s O(live) registry
sweep (`sched/src/task/task_lifecycle.rs:257-273`) into a refcount, and gives
per-process resource accounting somewhere to live.

It is not the first step. It moves fd-table and address-space ownership into the
crate that holds all `unsafe`, its `drop` sits on the buddy shootdown path, and a
`KArc<Process>` in a descheduling frame has the same abandon-leak shape that
invariant I8 exists to prevent. The handle conversion above is what turns it into
a re-pointing of already-handle-keyed call sites rather than a simultaneous
rewrite of identity and ownership.

Note that Linux deliberately does *not* have a single process struct:
`task_struct` holds independently refcounted `mm_struct`, `files_struct`,
`signal_struct` and `struct pid`, and that decomposition is what makes the
`CLONE_*` combinations expressible. A SlopOS `Process` should be a struct of
owning handles to separately refcounted parts, not a fused object.
