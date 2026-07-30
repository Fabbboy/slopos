# A deferred-work substrate

Three places in the kernel need to run work later, and each solved it separately
or not at all:

- **RCU callbacks** run on **CPU 0's idle loop only**
  (`slopos-ostd/src/sync/rcu.rs:517`, sole caller `sched/src/runtime.rs:246`
  gated on `cpu_id == 0`). If CPU 0 is running any task, no RCU callback is
  invoked on any CPU. The timer tick only sets a flag; nothing consumes it.
- **The task graveyard** drains only from the idle dispatcher
  (`sched/src/task/task_reclaim.rs:174`), so dead tasks' stacks accumulate under
  load.
- **`luf::drain_if_high_watermark`** is documented "Safe to call from a timer-tick
  bottom half" and has **zero callers** (`mm/src/mmu/luf.rs:681`).

The rest of the inventory is five permanent singleton kthreads, one global
idle-callback list with four slots and exactly one registrant, and a per-TTY
stack-local accumulator.

## First, the doctrine

`slopos-ostd/src/irq/mod.rs:4-12` states the current position explicitly:

> **No bottom halves.** SlopOS deliberately does not ship the softirq / tasklet /
> work-queue family. Drivers that need to defer work out of an IRQ-context
> callback spawn an ordinary `Task` and signal it from inside the handler. This
> keeps the trusted core smaller (one scheduling primitive instead of three) and
> matches the Asterinas framekernel design.

That reasoning is sound and this plan does not overturn it. What it proposes is
narrower than a bottom-half family: **an allocation-free per-CPU work list with
no execution context of its own**, drained from three sites that already run.
It adds no task, no thread pool, no new scheduling primitive and no `.bss`. The
doctrine's "one scheduling primitive" property is preserved.

The doctrine (`slopos-ostd/src/irq/mod.rs:4-12`) should be amended to say so,
rather than left to contradict the code.

## Why `sched/src/kthread.rs` has no callers

72 lines, a full spawn/join/exit/yield API, zero callers anywhere. This is the
tell, and the reasons it went unused are the design constraints for anything new:

1. **Layering.** Every crate that actually defers work — `drivers`, `fs`, `net`,
   `mm`, `ring` — does not depend on `slopos-sched`. `sched::kthread` is not even
   nameable from them.
2. **ABI.** Its entry type is `extern "C" fn(*mut c_void)` with a `*const c_char`
   name. Every consumer crate is `#![forbid(unsafe_code)]`, so a consumer's entry
   function cannot dereference the payload it is handed. It returns a sentinel
   rather than a `Result`.

The replacement that *did* get adopted is the inversion:
`slopos_ostd::task::spawn` behind a boot-registered `KernelThreadSpawner` trait
object (`slopos-ostd/src/task/spawner.rs:20-137`, impl at
`sched/src/runtime.rs:120-155`), with a safe `fn()` entry, no sched dependency,
and a typed `SpawnError`.

**The new primitive must live in `slopos-ostd` and use the same inversion.**
Anything defined in `sched` repeats the mistake.

`luf::drain_if_high_watermark` is the same tell a second time: the deferrable
half was written and the place to run it never existed.

## Design: a per-CPU work list, no workers

Three properties, and nothing else:

- **Allocation-free enqueue**, legal from hard IRQ context and from under a
  cli-spinlock. This is non-negotiable: heap allocation under a cli-lock reaches
  the buddy reuse path, which performs a synchronous cross-CPU TLB shootdown.
- **Per-CPU list**, so enqueue never contends across CPUs.
- **Drain from sites that already run under load**, not only at idle.

The work item is a caller-owned intrusive node embedded in the structure that
needs the work, exactly as a wait node is. No allocation, no fixed slot ring, no
silent drop.

### The enqueue machinery already exists

`sched/src/per_cpu.rs:553-696` already implements precisely this: a Treiber-stack
push, `Link::try_mark_linked` for idempotent enqueue (a node already queued is
not queued twice — free coalescing), `reverse_detached_chain` to restore FIFO
order, and a swap-detach drain. It is written, hardened and in production for the
remote task inbox.

**The work is generalising that out of `Task`, not inventing it.** Lift it into
`slopos-ostd` over a `WorkNode` with a `fn(&WorkNode)` callback, and let the
existing task-inbox use be one instantiation.

### The drain sites already exist

- the timer tick's process-context tail, next to `drain_remote_inbox`
- every CPU's idle loop
- the syscall-return path, if latency demands it

None is new. The `cpu_id == 0` gate on RCU processing at
`sched/src/runtime.rs:246` is deleted as part of this.

### What this does *not* build

**No worker pool.** One blocking worker per CPU is head-of-line blocking, and
fixing that is cmwq's `wq_worker_sleeping`/`wq_worker_running` scheduler hooks —
new `unsafe`-adjacent surface inside the TCB, raising the ratio
`scripts/tcb_ratio.sh --max 1.0` gates. There is also no forced consumer: the
five `spawn_kernel_io!` threads are each individually correct today.

Work items therefore **must not block**. That is a real constraint and it must be
stated at the API, not discovered: a callback that needs to block still spawns a
task. This is the same split Linux draws between `irq_work` (atomic, no sleeping)
and workqueues (process context, may sleep), and SlopOS is building only the
first half.

**No self-IPI vector yet.** The hardirq→process-context hop is served by the next
timer tick. If a consumer appears that cannot wait a tick, add the vector then —
the IPI machinery exists.

## Consumers, in order

1. **RCU callbacks.** Split `PENDING_HEAD` into `CpuLocal<...>`, drain from the
   tick and from every CPU's idle loop, delete the CPU 0 gate. This is the one
   *correctness* fix in the list: reclaim currently stops entirely whenever CPU 0
   is busy. It is also the migration that proves the primitive.
2. **The task graveyard.** Drains under load instead of only at idle, so dead
   tasks stop accumulating 48 KiB of stacks each.
3. **`luf::drain_if_high_watermark`.** Acquires the caller its documentation
   already assumes.

Not in scope: the five kthreads stay. `NapiWaker` and `TouchpadWaker` are already
byte-identical copies and deserve unification, but that is a separate cleanup and
neither needs to become a work item to get it.

## SlopRing is a different problem

The audit records "SlopRing has no kernel-side asynchronous execution" as a
consequence of this gap. It is not. A would-block ring op needs to be re-probed
**from the waker's context** when the underlying file becomes ready — register
the ring on the file's poll wake — not handed to a work queue. That fix is
independent of this plan and should not wait for it.

## Phases

| # | Work | Done when |
|---|---|---|
| 1 | Amend the no-bottom-halves doctrine in `irq/mod.rs` to describe what is and is not shipped | The doc and the code agree |
| 2 | Lift the Treiber/`try_mark_linked`/reverse-chain machinery out of `per_cpu.rs` into a generic `WorkNode` in `slopos-ostd`; re-express the task inbox on it | The suite is green with the inbox on the generic path — no behaviour change |
| 3 | Per-CPU work lists + drain from the tick tail and every idle loop | A work item enqueued from hard IRQ on CPU N runs on CPU N within one tick |
| 4 | RCU callbacks migrate; the `cpu_id == 0` gate is deleted | Callbacks are invoked on every CPU with CPU 0 permanently busy |
| 5 | Task graveyard migrates | Dead-task stacks are reclaimed under sustained load, not only at idle |
| 6 | `luf::drain_if_high_watermark` gets its caller | The high-watermark drain runs without an idle CPU |

## Tests

- `stest!` — enqueue from a hard-IRQ callback and assert the item runs within one
  tick, on the enqueuing CPU. This is the primitive's contract.
- `stest!` — enqueue the same node twice before a drain and assert it runs once.
  `try_mark_linked`'s coalescing is load-bearing and easy to lose.
- `stest!` — the RCU fix: pin a busy loop to CPU 0, issue `call_rcu` from another
  CPU, assert the callback fires. **Fails today**, which is the point.
- `stest!` — drain order is FIFO after `reverse_detached_chain`.
- `stest!` — enqueue from under a cli-held spinlock and assert no allocation
  occurs. The existing heap-statistics hooks can assert this directly.

## Risks

- **Lifetime of a caller-owned node.** A node freed while queued is a
  use-after-free. The `Link` discipline and `check_drop_panic_free.sh` cover the
  Drop side, but the plan needs an explicit rule: a `WorkNode` may only live in
  storage that outlives the drain, never in a stack frame. This is invariant I8's
  shape applied to work items, and it should be written down the same way.
- **A blocking callback is a wedged CPU.** There is no worker to yield to. Assert
  it: run the drain under a guard that panics on deschedule, the way
  `assert_switch_preempt_safe` already does for the switch path.
- **Phase 2 touches the remote task inbox**, which is on the dispatch hot path and
  has a documented lost-wake history. It must land as a pure refactor with no
  behaviour change, verified green before phase 3 builds on it.

## Prior art

Linux's cmwq (Tejun Heo, 2.6.36) replaced the dedicated-worker-per-workqueue
model because systems "saturated the default 32k PID space just booting up" and
because one execution context per CPU per workqueue made pools deadlock-prone. It
introduced shared per-CPU `worker_pool`s, dynamic worker creation, the
`wq_worker_sleeping`/`wq_worker_running` hooks, and `WQ_MEM_RECLAIM` rescuer
threads for forward progress under memory pressure. That is the destination if
SlopOS ever needs blocking deferred work — and its complexity is the argument for
not starting there.

`irq_work` is the atomic-context sibling and is what this plan actually builds.
`kthread_worker`/`kthread_work` is Linux's deliberately-simple option for cases
needing a dedicated, priority-controllable, CPU-affinable thread — which is what
the five `spawn_kernel_io!` threads already are, and why they should stay.

FreeBSD's `taskqueue(9)` with `taskqgroup_attach` (iflib's per-CPU RX/TX gtasks)
is the same per-CPU-pinned shape.
