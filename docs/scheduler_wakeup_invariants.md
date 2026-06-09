# Scheduler wakeup and runqueue invariants

This note records the design rule SlopOS follows for lost-wakeup prevention. It was written after investigating the `SCHED: rescuing stranded READY task` issue.

## World-class patterns

The common pattern across mature kernels is that **"runnable" is not just a task state**. It is a state plus a scheduler placement/ownership fact protected by a small set of scheduler locks or typed queue-membership fields.

- **Linux CFS/RT/DL** (`kernel/sched/core.c`): `try_to_wake_up()` serializes state changes with `p->pi_lock`; runqueue placement is `p->on_rq` under `rq->lock`; physical execution is `p->on_cpu`, cleared with a release store after `finish_task_switch()`. Remote wakeups may use `wake_entry.llist`, but the task is first moved to `TASK_WAKING`, and the remote CPU activates it under its runqueue lock.
- **FreeBSD ULE** (`sys/kern/sched_ule.c`, `sys/kern/subr_sleepqueue.c`): sleep queues hold the sleepqueue chain lock and thread lock while moving a thread out of sleep; `setrunnable()`/`sched_add()` performs the runnable transition and runqueue insertion under the thread/TDQ lock discipline. `TD_ON_RUNQ` is a first-class scheduler fact.
- **XNU/Mach** (`osfmk/kern/sched_prim.c`, `osfmk/kern/waitq.c`): waitq wakeups select and lock threads, remove waitq state, then `thread_go()`/`thread_setrun()` move them to `TH_RUN` and choose a processor while scheduler/thread locks are held. XNU explicitly handles the "waking a thread still on core" case.
- **Zircon/Fuchsia** (`zircon/kernel/kernel/scheduler.cc`, `owned_wait_queue.cc`): wait queues produce a locked `UnblockList`; `Scheduler::UnblockCommon()` runs with the thread lock and target scheduler queue lock, then `thread->set_ready()` and `Insert()` are performed in one critical path. Thread state mutators are annotated so only scheduler/waitqueue code can set ready/running/blocked.
- **seL4** (`src/object/tcb.c`, `src/kernel/thread.c`): the scheduler queue bit (`tcbQueued`) lives in the verified thread state and is changed only by `tcbSchedEnqueue()`/`tcbSchedDequeue()`. The proof obligation is essentially: a schedulable TCB is either current or queued.
- **Zephyr** (`kernel/sched.c`): a single scheduler spinlock protects ready, pend, unpend, and swap operations. `ready_thread()` checks the queued bit and ready state together; `z_pend_curr()` swaps caller locks for the scheduler lock so blocking and scheduling are one transaction.

## SlopOS rule

SlopOS keeps the same invariant, adapted to the framekernel split:

> A non-idle task with `TaskStatus::Ready` must have a non-`None` scheduler placement: exactly one ready queue, exactly one remote wake inbox, the current/on-CPU switch-out owner, a short `Waking` publisher reservation, or a migration handoff.

The READY bit alone is never a scheduling proof. `SchedPlacement` is the scheduler-owned proof.

## Rust/framekernel encoding

The kernel crates remain `#![forbid(unsafe_code)]` outside the documented exemptions (`kernel/src/main.rs` and `hermetic/src/macros.rs`); the trusted OSTD crate owns the unsafe intrusive-list machinery. We use that to encode membership roles:

- `ready_link: Link<Task, ReadyQueueRole>` — ready-queue membership.
- `remote_inbox_link: Link<Task, RemoteWakeRole>` — remote-wake inbox membership.
- `zombie_link: Link<Task, ZombieListRole>` — zombie-list membership.

The role type is part of the link type, so using the ready link as a remote-wake link is a compile-time type mismatch. The link slot itself owns the single-membership bit, so duplicate pushes fail at the primitive instead of relying on out-of-band booleans.

Wake paths follow a Linux-style `try_to_wake_up()` contract:

1. If no scheduler owner exists, OSTD's task-state primitive reserves `SchedPlacement::Waking` before publishing `TaskStatus::Ready`. Scheduler publish then transfers that reservation to one ready queue or one remote inbox.
2. If the task is still `OnCpu`, a wake may perform `Blocked -> Ready` but must re-check placement after the CAS. If the switch-out owner already released `OnCpu`, the wake publishes from `None` itself. This closes the stale-`on_cpu` lost-wakeup race without producer-side spinning.
3. If normal CPU selection or remote-inbox publication loses a placement race, the publisher falls back to a local ready-queue publish; a successful wake CAS is not allowed to return with `Ready + None`.
4. If a ready queue, remote inbox, migration handoff, or another `Waking` publisher already owns the task, duplicate wakes are success/no-op. If the task is `Blocked` with stale ownership (legacy fixture/direct mutator), the wake still performs the `Blocked -> Ready` CAS so the existing ownership becomes runnable again.

Block paths funnel through `commit_blocked_deschedule()` / `consume_ready_wake_for_current()`, which consume a racing wake by restoring the current task to `Running + OnCpu` instead of descheduling a task that has already become Ready.

The rescue sweep is therefore a diagnostic backstop, not normal control flow. If it fires, one of the invariants above was violated and the producing path must be fixed rather than papered over.
