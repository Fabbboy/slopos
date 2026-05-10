# SlopOS Wait/Wake/Block Protocol

> **Audience.** Future kernel hackers (or AI agents) adding a new "block until X"
> primitive, debugging a lost-wakeup, or porting the scheduler onto an async
> runtime. This is a maintainer's reference, not a tutorial.
>
> **Scope.** The synchronous, preemptive wait/wake/block protocol introduced by
> the harmonic-cascade refactor (Phases 1–6, branch `sched/fix-self-wakeup-deadlock`).
> The async successor lives in `plans/FRAMEKERNEL_PLAN.md` Phase 3 — see §8.

## 1. Background — the race that motivated the redesign

Pre-Phase-1, `task_wait_for(child)` published two atomics:

```text
status:     AtomicU8   // Running | WillBlock | Blocked | Ready | Terminated
waiting_on: AtomicU32  // child task id, or INVALID
```

`release_task_dependents` (the producer side, called from `mark_task_terminated`)
read both atomics to find waiters and wake them. Even with `SeqCst` fences in
the right places, the *pair* is not observable atomically: the waker could
read `(waiting_on=child, status=Running-stale)`, decide there was no sleeper,
and skip the wake. The waiter would then complete its `WillBlock → Blocked`
CAS and sleep forever.

Compounding the bug, the dispatcher *coerced* unexpected states to `Running`
instead of rejecting them, so a task caught half-way through the wait protocol
could be dispatched with an inconsistent kernel-mode context — the smoking gun
for the `cr2 = 0xdfdedddcdbdad9d8`-shape page faults in CI.

Manifestations:

| Environment | Symptom |
|---|---|
| KVM | `task_wait_for` deadlock at ~5%, hang at `fork_test: pipeline repro start`. |
| TCG / CI | ~30% flake; `dispatch: unexpected state 3 for task 4`, then page fault. |

Every other blocking subsystem in the kernel (pipe, TTY, socket, futex) was
already correct, because they all used `slopos_ostd::sync::WaitQueue`, which
serialises wake against block under its own SpinLock. The bug existed *only*
where `task_wait_for` did its own ad-hoc CAS dance.

The harmonic-cascade fix synthesises the Linux + Theseus + Redox design:

1. **Durable exit value** — publish `ExitInfo` once, before fanout. Late
   waiters see it on their first re-check.
2. **Per-task `WaitQueue`** — drained under SpinLock during exit. The
   lock-pair is the bidirectional full barrier.
3. **Fused `TaskState`** — `AtomicU64` packing of `(status, reason, epoch)`,
   so the two-atomic-pair race is structurally unrepresentable. `WillBlock`
   ceases to exist.

The retired `plans/SAFE_BY_DESIGN.md` and the active
`plans/ok-lets-fully-implement-harmonic-cascade.md` are the authoritative
historical record.

## 2. The three primitives

### 2.1 Durable exit value — `AtomicCell<ExitInfo>`

`slopos-ostd/src/sync/atomic_cell.rs` defines the single-publisher,
multi-observer cell. Each `Task` holds an `AtomicCell<ExitInfo>` (see
`core/src/scheduler/exit_info.rs` for the payload type). Publish happens
*exactly once*, in `mark_task_terminated`, *before* the wake fanout
(`core/src/scheduler/task/task_lifecycle.rs:763-774`):

```rust
// Publish exit_info BEFORE the wake fanout.
let info = ExitInfo { exit_code, exit_reason, fault_reason, signal: 0, exit_time_ms };
let _ = task.exit_info.try_set(info);   // Release CAS (AcqRel)
release_task_dependents(resolved_id);   // wake_all under SpinLock
```

Memory ordering (see the module preamble in `atomic_cell.rs`):

| Op | Ordering | Used by |
|---|---|---|
| `try_set` | `AcqRel` CAS, success-only publish | `mark_task_terminated` (one alloc per termination) |
| `try_get` / `is_set` | `Acquire` load | `task_wait_for`'s condition closure |
| `take` | `AcqRel` swap-to-null | `wait4`-style consumers |
| `reset` | `Release` swap; `unsafe` (caller serialises) | `Task::reset_in_place` slot recycle |

Storage is heap-backed (`AtomicPtr<T>` over a leaked `KBox<T>`), so once
published the value remains addressable for the cell's lifetime. The cell is
the *durable source of truth* for child-wait. `mgr.exit_records` (the
`TaskExitRecord` cache) is now strictly diagnostic.

### 2.2 Per-task `WaitQueue`

Each `Task` carries a `waiters: WaitQueue` field. `task_wait_for` enqueues on
it; `release_task_dependents` (`core/src/scheduler/task/task_session.rs:47-63`)
calls `waiters.wake_all()` on it. The internal SpinLock is the lock side of
the lock-pair full-barrier (§3).

`WaitQueue` is the same primitive used by every other blocking subsystem in
the kernel — pipes, TTY, sockets, the per-bucket futex queue, and the
task-exit waiters above. There is no second wait primitive in the kernel.

### 2.3 Fused `TaskState`

`core/src/scheduler/task_state.rs` packs three fields into a single
`AtomicU64`:

```text
bits  0..4   TaskStatus    (4 bits, 5 variants — Invalid|Ready|Running|Blocked|Terminated)
bits  4..12  BlockReason   (8 bits, 8 variants)
bits 12..16  reserved
bits 16..32  cpu_hint      (16 bits — reserved for affinity-aware wakeup)
bits 32..64  epoch         (32 bits, ABA defence — bumped on every transition)
```

Phase 5 collapsed `WillBlock` out of the FSM. The state machine is now:

```text
Invalid → Ready ⇄ Running → Terminated
                      ↘  Blocked  ↗
                         (waited)
```

Transitions go through `try_transition`, `try_transition_keep_reason`, or
`force_set`. Each is a 64-bit CAS; the epoch is bumped on success. Readers
use `snapshot()` (Acquire load + unpack) to get a consistent
`(status, reason, epoch)` view in one atomic operation. The two-atomic-pair
race is not just hard to hit — it is unrepresentable.

The `epoch` is currently unused by the scheduler's correctness arguments; it
is reserved for the bounded work-stealer that lands in FRAMEKERNEL Phase 7.

## 3. The race-freedom proof

The protocol is the McKenney symmetric-full-barrier formulation. Consumer
(`task_wait_for`) and producer (`mark_task_terminated`) are mirror images
across the queue's SpinLock:

```text
Producer (mark_task_terminated):                Consumer (task_wait_for):
                                                 [enter wait_event(condition)]
                                                 if condition() { return }    # fast path
  exit_info.try_set(info)         # Release      [take WaitQueue.lock()]
  waiters.wake_all():                            if condition() { drop lock; return }   # Acquire
    [take WaitQueue.lock()]                      waiters.push_node(self)
    pop nodes, unblock_task each                 mark_current_blocked()        # Running→Blocked CAS, AcqRel
    [drop WaitQueue.lock()]                      [drop WaitQueue.lock()]
                                                  yield_blocked_task()         # schedule()
```

`condition()` for `task_wait_for` is `exit_cell.is_set() || task_is_terminated(target)`
(`core/src/scheduler/scheduler.rs:1074`).

The SpinLock pair gives a bidirectional full barrier — identical to Linux's
`wq_head->lock` in `prepare_to_wait_event` / `wake_up`. The proof obligation
is to show that **no interleaving loses a wakeup or deadlocks**. Argue both
directions.

### Direction A — consumer-first (producer arrives after consumer enqueues)

1. Consumer takes `WaitQueue.lock`.
2. Consumer's re-check evaluates `is_set()` → `false` (publish hasn't happened).
3. Consumer pushes its node onto the list.
4. Consumer calls `mark_current_blocked` — `Running → Blocked` CAS succeeds.
5. Consumer drops the lock and yields.
6. Producer publishes `exit_info.try_set(info)` (Release).
7. Producer takes `WaitQueue.lock`.
8. Producer pops the consumer's node, calls `unblock_task(consumer)`.
9. `unblock_task` does `Blocked → Ready` CAS — succeeds, because the consumer
   committed `Blocked` under the lock at step 4.
10. Consumer is rescheduled, re-checks `is_set()` → `true`, returns.

The Acquire-on-lock at step 7 makes the publish at step 6 visible to anything
the producer does after the lock; the Release-on-unlock at step 5 makes
everything the consumer did before the unlock (including the push at step 3)
visible to the producer at step 7. **No lost wakeup.**

### Direction B — producer-first (publish + fanout finish before consumer locks)

1. Producer publishes `exit_info.try_set(info)` (Release).
2. Producer takes `WaitQueue.lock`, observes empty list, drops lock.
3. Consumer takes `WaitQueue.lock`.
4. Consumer's re-check evaluates `is_set()` → `true`.
5. Consumer drops the lock and returns. **No block, no yield, no wake needed.**

The Acquire-on-lock at step 3 sees everything the producer published before
its own unlock at step 2, including the cell store from step 1. **No deadlock.**

### Direction C — interleaved (producer locks between consumer's pre-check and consumer's lock)

1. Consumer's outer-loop pre-check: `is_set()` → `false`. (Outside the lock.)
2. Producer publishes `exit_info.try_set(info)` (Release).
3. Producer takes the lock, observes empty list, drops the lock.
4. Consumer takes the lock.
5. Consumer's *under-lock* re-check: `is_set()` → `true`. Drop lock, return.

The under-lock re-check (step 5, see `wait_queue.rs:326-330`) is what closes
this window. Without it, the consumer would push, block, and sleep forever
because the producer's wake fanout already drained an empty list. **No lost
wakeup.**

### Why the `Running → Blocked` CAS belongs *under* the lock

This is the load-bearing detail. Pre-Phase-5 the CAS was outside the lock,
which created a fourth interleaving:

1. Consumer pushes its node under the lock.
2. Consumer drops the lock.
3. Producer takes the lock, pops the consumer's node, calls `unblock_task`.
4. `unblock_task` does `Blocked → Ready` CAS — **fails**, because the
   consumer hasn't called `mark_current_blocked` yet (still `Running`).
5. Consumer calls `mark_current_blocked` — `Running → Blocked` succeeds.
6. Consumer yields. Lost wakeup.

By doing the `Running → Blocked` CAS under the same lock the wake side will
take (`wait_queue.rs:333-335`), we guarantee that any `wake_*` that observes
our node also observes us as `Blocked`, and its `Blocked → Ready` CAS
succeeds. This is exactly Linux's `prepare_to_wait_event` discipline.

## 4. WaitQueue intrusive-list internals

Phase 3 refactored the queue from a fixed-capacity ring buffer of task
pointers into an unbounded intrusive linked list of `WaitNode`s. Source:
`slopos-ostd/src/sync/wait_queue.rs` and `slopos-ostd/src/sync/wait_node.rs`.

### Two node lifecycles in the same list

| Flavour | Constructor | Owner | Reclaim path |
|---|---|---|---|
| **Stack-pinned** | `WaitNode::new()` via `core::pin::pin!` in `wait_event` / `wait_event_timeout` / `wait_once` | the waiter's stack frame | the waiter unlinks itself before its frame returns; queue never frees |
| **Heap-owned** | `WaitNode::new_heap()` via `KBox::try_new` in `enqueue_current` | the queue (via `KBox::into_raw`) | whoever dequeues — `wake_one` / `wake_all` / `remove_current` — calls `KBox::from_raw` and drops |

The discriminator is the `heap_owned: AtomicBool` field, set once at
construction and read `Relaxed` on dequeue (the SpinLock supplies ordering).
The wake path inspects it under the queue's lock and decides whether to
`KBox::from_raw` the popped `NonNull<WaitNode>`. **Exactly-one-reclaim** is
preserved by the lock: the same SpinLock that protects the list also
protects the `is_heap_owned` decision against double-frees.

### Address stability for stack-pinned nodes

The waiter's invariant — its node's address must stay stable while linked —
is guaranteed by:

1. `core::pin::pin!` projects the local `WaitNode` so it cannot move.
2. The kernel's task stack does not move while the task is blocked.
3. `wait_event` always unlinks under the queue lock before returning (every
   exit path of `wait_event` / `wait_event_timeout` / `wait_once` calls
   `unlink_if_linked`).

This is the Rust analogue of Linux's `DEFINE_WAIT(wait); prepare_to_wait(...)`
idiom, where `wait` is a stack-resident `wait_queue_entry_t` and the
`finish_wait()` epilogue is the unlink step.

### `WaitQueueBackend` — runtime hookup

`WaitQueue` lives in `slopos-ostd`, which has no dependency on the scheduler
crate. It calls into the runtime through a `WaitQueueBackend` trait object
installed once at boot via `register_wait_queue_backend`. Until installed,
all blocking methods short-circuit with `false` (treated as "runtime not
initialised", lets early-boot callers no-op cleanly).

## 5. `WaitQueueBackend` trait contract

The trait surface (`wait_queue.rs:107-163`) is:

| Method | Returns | Contract |
|---|---|---|
| `is_runtime_initialised()` | bool | True once the kernel task runtime is up. |
| `current_task_handle()` | opaque ptr | Current task on this CPU, or null. |
| `block_current_task()` | – | `Running → Blocked` CAS + `schedule()`. Legacy single-shot path; new code uses the split below. |
| `mark_current_blocked()` | bool | `Running → Blocked` CAS only; no yield. **Must be called under the WaitQueue's SpinLock.** |
| `yield_blocked_task()` | – | Remove from runqueue + `schedule()`. **Must be called outside any SpinLock** (`schedule()` is not reentrant-safe under our locks). |
| `yield_blocked_task_with_timeout(ms)` | – | Same as above, plus arms a sleep-queue entry that fires `unblock_task` on the deadline. |
| `unblock_task(handle)` | i32 | `Blocked → Ready` CAS + re-enqueue on a runqueue. Tolerant of stale handles. |
| `get_time_ms()` | u64 | Monotonic time. |

The Phase-5 split (`mark_current_blocked` + `yield_blocked_task`) exists so
the consumer's CAS and the lock release can both happen *under* the queue's
SpinLock, with the actual `schedule()` deferred to *after* the unlock. The
old single-shot `block_current_task` is preserved for legacy callers but
must not be used by new wait-protocol code.

`wait_event` is the canonical consumer:

```rust
loop {
    if condition() { return true; }                        // fast path
    let blocked = {
        let inner = self.inner.lock();                     // acquire WQ lock
        if condition() { drop(inner); return true; }       // under-lock re-check
        Self::push_node(&inner, node.as_ref());            // enqueue under lock
        bk.mark_current_blocked()                          // CAS under lock
    };                                                     // release WQ lock
    if blocked { bk.yield_blocked_task(); }                // schedule outside lock
    // Loop back: pre-iteration cleanup unlinks if still linked.
}
```

## 6. Cookbook — adding a new wait/wake subsystem

If you need a new "block until X" primitive, you inherit the AUDIT 2C
correctness contract for free as long as you follow this template.

### 6.1 The producer side

```rust
// 1. Publish your condition (any data store + appropriate ordering).
SHARED_FLAG.store(true, Ordering::Release);
// 2. Wake. Do NOT add a fence between the store and the wake.
WAITERS.wake_all();
```

That is the entire contract. The internal SpinLock that `wake_all` takes
**is the producer's release-half of the lock-pair barrier**. A
`compiler_fence(SeqCst)` or `core::sync::atomic::fence(SeqCst)` between the
two is **dead code** — the audit (Phase 2) deleted dozens of them and broke
nothing.

### 6.2 The consumer side

```rust
WAITERS.wait_event(|| SHARED_FLAG.load(Ordering::Acquire));
```

The closure must:
- Be cheap (it runs at least once outside the lock and once under it).
- Re-evaluate from primary state — never cache.
- Use `Acquire` (or stronger) on its loads, mirroring the producer's `Release`.
- Be idempotent (it can be called arbitrarily many times before `true`).

### 6.3 What NOT to do

| Anti-pattern | Why it's wrong |
|---|---|
| `compiler_fence(SeqCst); WAITERS.wake_all();` | The SpinLock already supplies the barrier. Dead code, deleted in Phase 2. |
| Reading the condition outside the closure and capturing `bool` | Caches the pre-lock value; defeats the under-lock re-check. |
| Skipping `wake_all` when "the queue looks empty" | The empty observation must be made *under the lock* (e.g. via `has_waiters()`). Outside the lock, "empty" can race with an in-flight enqueue. |
| Adding a second atomic alongside the condition (`is_signalled` + `value`) and reading them in the wake path | Recreates the original two-atomic race. Use a single source of truth. |
| Calling `block_current_task()` directly from your own ad-hoc CAS path | Bypasses the lock-pair. Use `wait_event` / `wait_event_timeout` / `wait_once`. |

### 6.4 Worked example — pipes

`fs/src/pipe_file_ops.rs:168` is the current pattern in tree:

```rust
pipe::reader_wq(h).wait_event(|| match pipe::lock_slot(h) {
    Some(slot) => slot.len > 0 || slot.writers == 0,
    None => true,
});
```

The producer side (`pipe_release_writer` at `:60-83`) does
`pipe::reader_wq(h).wake_all()` after decrementing `writers` under the slot
lock. Two locks involved (slot lock + WaitQueue SpinLock); the slot lock
serialises the data update, the WaitQueue lock serialises wake against block.
No fence is necessary or desirable.

## 7. The dispatcher invariant

`core/src/scheduler/scheduler.rs:dispatch` (line 188) hard-rejects unexpected
states. Pre-Phase-4, the same site logged a warning and *coerced* the task
to `Running` regardless of its observed state — that's how a `WillBlock`
half-state task could end up dispatched with a corrupted user-mode RIP, the
`cr2 = 0xdfdedddcdbdad9d8` page-fault smoking gun.

The current rule is simple:

> A task entering `dispatch` MUST be `Ready` or `Running`. Anything else is
> an invariant violation.

Enforcement (`scheduler.rs:223-244`):

```rust
debug_assert!(
    matches!(current_status, Some(TaskStatus::Ready) | Some(TaskStatus::Running)),
    "dispatch: invariant broken — task {} in unexpected state {:?}",
    task_id_of(task).unwrap_or(INVALID_TASK_ID),
    current_status,
);
if !matches!(current_status, Some(TaskStatus::Ready) | Some(TaskStatus::Running)) {
    return; // production fallback: skip dispatch, let caller pick another task.
}
```

In debug builds the assertion fires immediately and surfaces the bug. In
release builds the dispatch is silently skipped — the caller picks another
task on its next pass. The "log + coerce" path is gone for good.

`Blocked` reaching the dispatcher would mean a wake path enqueued without
first running the `Blocked → Ready` CAS, or a state transition raced the
runqueue insert. Both are bugs the assertion exists to surface.

## 8. Migration to async (FRAMEKERNEL Phase 3)

`plans/FRAMEKERNEL_PLAN.md` Phase 3 (§7, lines 1269–1392) replaces tasks
with `Pin<Box<dyn Future>>` and the synchronous executor with a cooperative
async runtime. The harmonic-cascade primitives map directly onto async
equivalents:

| Sync (today) | Async (Phase 3) |
|---|---|
| `WaitQueue` | `AsyncWaitQueue` / `Notify` (Tokio-style; `wait()` returns a `WaitFuture<'_>`) |
| `AtomicCell<Option<ExitInfo>>` | `OnceCell<ExitInfo>` future; `.await` resolves on first publish |
| Fused `TaskState` | The executor's `Wake` trait state; the same `(status, reason, epoch)` semantics survive but the dispatcher disappears in favour of the executor |
| `mark_current_blocked` + `yield_blocked_task` | `Future::poll` returning `Poll::Pending` after registering a `Waker`; the waker is the `unblock_task` analogue |
| `wait_event(closure)` | `loop { if cond { break } else { notify.notified().await } }` |

The current sync/preemptive design is the synchronous predecessor; flipping
to async is a primitive-substitution exercise, not a redesign. The
race-freedom argument transfers verbatim: Tokio's `Notify` uses the same
intrusive-list-under-Mutex discipline as our `WaitQueue` (the Mutex plays
the role of the SpinLock), and `OnceCell` is the durable-publish analogue of
`AtomicCell`.

## 9. Out-of-scope / known limits

- **TCG environment** is broken at the boot stage by an unrelated bug. KVM is
  the canonical validation environment for the wait protocol. Once the TCG
  boot bug is fixed, the existing race-stress tests should run unmodified.
- **1-hour soak run** on KVM is not yet performed. Phase 7C deferred this to
  Phase 8 ("hardening and verification").
- **Verus formalisation** of the lock-pair barrier proof (FRAMEKERNEL
  Phase 4) is the ultimate race-freedom proof. The argument in §3 is a
  textual proof; a machine-checked one is future work.
- The `cpu_hint` field of `TaskState` (bits 16..32) is reserved but unused.
  It is the affinity-aware-wakeup hook for the bounded work-stealer that
  lands in FRAMEKERNEL Phase 7.
- `BlockReason` is informational only — it does not gate any CAS. The
  scheduler's correctness arguments are status-only. Tracers and the future
  signal-aware unblock path consume it.

## Cross-references

- Source-of-truth files: `slopos-ostd/src/sync/wait_queue.rs`,
  `slopos-ostd/src/sync/atomic_cell.rs`,
  `slopos-ostd/src/sync/wait_node.rs`,
  `core/src/scheduler/task_state.rs`,
  `core/src/scheduler/exit_info.rs`,
  `core/src/scheduler/scheduler.rs`,
  `core/src/scheduler/task/task_lifecycle.rs`,
  `core/src/scheduler/task/task_session.rs`,
  `core/src/scheduler/futex.rs`.
- Race-stress tests: `core/src/scheduler/sched_tests.rs::test_task_wait_exit_race_1000`
  and `::test_task_wait_exit_race_with_work`.
- Historical record: `plans/ok-lets-fully-implement-harmonic-cascade.md`,
  retired `plans/SAFE_BY_DESIGN.md`.
- Async successor: `plans/FRAMEKERNEL_PLAN.md` §7 (Phase 3).
