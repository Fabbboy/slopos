# One exit path

SlopOS tears a blocked task down from another CPU without unwinding its stack.
Every resource class that can be held across a blocking call therefore needs a
hand-written release keyed on the dying task's id, and that count grows linearly
with the kernel. Eight such spines exist.

The invariant to reach:

> **A task only ever exits from its own context.**

Kill becomes a flag. Every blocking primitive checks it and returns. Frames
unwind by *returning*, so destructors run on the task's own stack at a point the
task chose. Remote teardown disappears, and with it invariant I8 and the eight
spines.

Panic strategy is a **separate axis** and is not part of this work. See
[Panic strategy](#panic-strategy-separate-axis).

## What forces it

The abandon model has already produced a use-after-free inside `slopos-ostd`
(`SLOPOS-2026-0044`, 7.8 HIGH).

`WaitQueue::wait_event_until` pins its node on the caller's kernel stack
(`slopos-ostd/src/sync/wait_queue.rs:607`). Nothing unlinks it at teardown:
`mark_task_terminated` (`sched/src/task/task_lifecycle.rs:985`) cancels sleep,
strips futex waiters and releases wait refs, but touches no wait queue, and
`WaitQueue::remove_task` declines by contract — *"stack-pinned `wait_event` nodes
manage their own lifecycle and are left alone here"* (`:1035-1037`). The
mechanism that would unlink is `Drop for WaitNode`, which is exactly what an
abandoned stack never runs.

The stack is then recycled, not quarantined: `TaskStack::drop`
(`sched/src/task_stack.rs:222-232`) deliberately leaves the mapping in place so
the next allocation reuses the slot, and `allocate` (`:128-136`) zeroes a
`was_backed()` slot and hands it to a new task. A later `wake_one` pops the stale
node and writes through it (`wait_queue.rs:885`, `has_woken_swap_true()`).

The `unsafe` is justified by a premise this model falsifies. Verbatim at
`wait_queue.rs:880-884`:

> SAFETY: `nn` was just popped from the list and is alive: stack waiters are
> blocked / will block before freeing their frame

An abandoned waiter's frame is freed *while* it is blocked and still linked. The
push side states an equally unfulfillable contract at `:1110-1117`.

Reachability is ordinary: `kill -9` a process blocked in a pipe read, spawn
anything, then write to the pipe.

## The substrate

Interruptibility is currently a convention. `WaitOutcome` is
`Ready | Timeout | NoRuntime` (`slopos-ostd/src/sync/wait_queue.rs:168-179`) and
its doc at `:161-166` explicitly declines a signal variant, so `wait_event`
returns `bool` and `wait_event_until` returns `Option<R>` — neither is
`#[must_use]`, and neither gives a caller anything to handle.

Replace it with a two-tier, must-use result:

```rust
#[must_use]
pub enum WaitAbort { Interrupted, Killed }   // constructible only inside slopos-ostd
pub type WaitResult<R> = Result<R, WaitAbort>;
```

`core::result::Result` is `#[must_use]`; `bool` and `Option` are not. Changing
the signature turns 24 of 27 production `.wait_event*` call sites into compile
errors — the migration becomes a compiler-enumerated list rather than an audit.

Close the `let _ =` hole with a grep gate carrying a `--self-test`, modelled on
`scripts/check_wait_predicate_purity.sh` (wired at `justfile:396,411`): forbid
`let _ =`, `.ok()` and `.unwrap_or` on the interruption type at any wait site.
That converts *"every blocking call handles interruption"* from an obligation
that decays into a CI invariant that does not. The tree already contains the
motivating instance — `sched/src/scheduler.rs:1883` is literally
`let _ = BUS…wait_event(…)`, at `waitpid`.

The three-position probe to lift into the primitive already exists and works:
`net/src/socket.rs:994-1029` (`wait_socket_event` / `SockWait`) probes before the
wait, inside the predicate, and after the wake.

**Killable before interruptible.** The killable tier needs neither the
partial-completion rule nor a restart-code decision — its caller error path is
unconditionally *"return, you are dying."* Doing killable first at all eleven
sites is a strictly smaller and lower-risk step.

## Steps

Each step leaves the tree green on its own.

**0. Close the UAF.** Either a by-id teardown unlink (a wait-node registry plus a
hook in `mark_task_terminated`, ~40 lines) or accept it as the forcing function
for step 5 and ship the repro first. It is a live memory-safety hole in the TCB
with an unsound `SAFETY` comment; it does not wait on the rest of this plan.

**1. Bound kill latency.** `task_terminate` aims no IPI at the victim's CPU, and
`sched/src/scheduler.rs:2394-2397` declines to preempt when
`scheduler_ready_count(cpu_id) == 0`. Nothing on any return-to-user path checks
`is_exited()`. A killed task alone on a CPU therefore keeps running in userland,
as a Zombie, indefinitely. Add the `is_exited()` escape to that arm and a check
on the return-to-user path.

**2. Make `set_status` transition-validated.** Close the three resurrection
doors — `wait_queue.rs:664-675`, `:651-653`, `scheduler.rs:1739-1745` — which
route to the unvalidated `force_set` at `slopos-ostd/src/task/state.rs:188-207`.
A remotely-killed running task that then enters any wait queue is restored
Zombie→Running, after which `task_lifecycle.rs:1113` makes deferred cleanup a
**permanent** no-op: fd table, process VM and `task_reap` never run while the
task lives on with published `exit_info` and reparented children.

**3. Build the fatal tier.** `slopos-ostd/src/task/kernel_task.rs:597` is
`pub signal_pending: AtomicU64` with ≤32 signals used, so bits 32-63 are free. A
killed bit in that word makes the fatal probe free inside any predicate that
already loads it, and avoids touching `abi/src/task.rs:316`'s flag word. Add a
single **fused** set-and-wake in ostd with no public way to do one without the
other; both halves exist separately today (`slopos-ostd/src/task/ops.rs:118`
`fetch_or`, and `unblock_task`).

Do not delete SIGKILL's `task_terminate` yet.

**4. Land the substrate.** `WaitResult<R>` as the only wait API, plus the
`let _ =` gate.

**5. Convert all eleven sites to killable.** Add the short-count branch wherever
a transfer exists — `handle_erestartsys` cannot carry partial progress, since
`core/src/syscall/dispatch.rs:119-120` rewinds `rip` and reloads `rax`, restarting
from argument zero.

| Site | Location |
|---|---|
| pipe read | `fs/src/pipe_file_ops.rs:219` |
| pipe write | `fs/src/pipe_file_ops.rs:329`, `:436` |
| waitpid | `sched/src/scheduler.rs:1887` |
| futex | `sched/src/futex.rs:106` |
| AF_UNIX accept | `net/src/unix_socket/mod.rs:206` |
| AF_UNIX recv | `net/src/unix_socket/mod.rs:385`, `:465` |
| AF_UNIX sendmsg | `net/src/unix_socket/mod.rs:646` |
| UDP/raw recvfrom | `net/src/socket.rs:2607` |
| `slopos_ostd::sync::Mutex::lock` | `slopos-ostd/src/sync/mutex.rs:59-76` |

`Mutex::lock` is the one the spike never named and the worst of the set:
unbounded, uninterruptible, and guarding `CACHED_EXT2`, `virtio_blk`'s `io_lock`,
the GPU ctrl/cursor rings and the io_uring registry.

**6. Add a restart-block mechanism, then promote to interruptible.** SlopOS has
only `ERESTARTSYS` (`abi/src/errno.rs:162`). Without `ERESTART_RESTARTBLOCK` — or
absolute-deadline recomputation, which has only three consumers (sleep, poll,
futex) — every timeout-bearing interruptible wait restarts with its *original*
timeout and livelocks under signal pressure.

**7. Kthread stop protocol and the `Mutex` answer.** Kernel tasks are
structurally excluded from signals (`core/src/syscall/signal.rs:505-507` returns
`Done` for `!TASK_FLAG_USER_MODE`), so the four kthread parks (napi
`net/src/napi_waker.rs:85`, timer, touchpad `drivers/src/touchpad/mod.rs:176`,
ext2 flusher) need a non-signal flag. `FLUSH_SHUTDOWN`
(`fs/src/ext2_vfs.rs:389`) is the template. For `Mutex`, choose owner-tracked
kill-time release or a fallible `lock()`; the latter grows an error return at
every `CACHED_EXT2.lock()` site (`fs/src/ext2_vfs.rs:85,294,321`).

This step also dissolves the standing wedge: `drivers/src/virtio_blk.rs:494`
takes the sleeping `io_lock` and `:442` parks for up to 5 s, while
`fs/src/ext2_vfs.rs:81-99` holds `CACHED_EXT2` across the closure that reaches
that park. `MutexGuard::drop` is the sole release and never runs on an abandoned
stack, and `assert_switch_preempt_safe` counts only SpinLock/PreemptMutex/
PreemptGuard, so it cannot see it. A kill in that window wedges the filesystem.

**8. Delete remote teardown.** SIGKILL's `task_terminate` special case
(`core/src/syscall/signal.rs:238-246`), the eight spines, invariant I8 from
`CLAUDE.md`, and the three PreemptGuard-as-exemption sites.

Only now is this safe. Deleting it before step 5 manufactures unkillable
processes at eleven sites — SIGKILL currently works there *because* it bypasses
the flag.

The spines that go:

| Spine | Location |
|---|---|
| poll registrations | `boot/src/boot_services.rs:54`, `fs/src/fileio/poll.rs:188` |
| SCM_RIGHTS in-flight | `boot/src/boot_services.rs:59-61`, `net/src/unix_socket/mod.rs:731` |
| input | `boot/src/boot_drivers.rs:80` |
| compositor | `video/src/lib.rs:137` |
| futex | `sched/src/task/task_lifecycle.rs:991` |
| test-report ring | `sched/src/task/task_lifecycle.rs:913-925` |
| waitpid wait refs | `sched/src/scheduler.rs:1893-1943` |
| pending spawn | `sched/src/task/pending_spawn.rs`, `core/src/exec/mod.rs:284-290` |

Three of them allocate under a cli-lock (`scheduler.rs:1919`,
`unix_socket/mod.rs:751`, `fs/src/fileio/poll.rs:202`) — the buddy/LUF deadlock
shape recorded in `KNOWN_ISSUES.md:24-31`. `SpawnGuard::park` additionally
converts a memory-safety rule into a 64-slot spawn concurrency limit
(`core/src/exec/mod.rs:291-293`).

## What this costs

The unkillable-D-state class, introduced at eleven sites at once — a class SlopOS
does not have today. Linux took from 1991 to 2.6.25 to claw it back and needed a
new task state to do it, converting NFS first. That is what the killable-first
ordering in steps 5-6 is for.

Sites that will not observe a flag promptly are dominated by one primitive:
`SpinLock::lock` (`slopos-ostd/src/sync/spin.rs:266-291`) is a `PreemptGuard`
plus `save_flags_cli` plus an unbounded ticket spin, and `PreemptMutex::lock`
(`:398-412`) and `IrqRwLock` (`:583`, `:652`) share the shape. A task spinning
there becomes genuinely unstoppable rather than merely slow. Offsetting this:
`assert_switch_preempt_safe` (`sched/src/scheduler.rs:1676-1684`, sole call site
`:1555`, live in release) guarantees every blocking point is reached with no lock
held, so a flag check at any blocking primitive always runs clean.

Teardown will always run on the dying task's own stack — the context
`destroy_context_is_safe` (`sched/src/task/task_reclaim.rs:114-130`) forbids the
destructor from running in. The graveyard may have to absorb more, giving back
part of the ~305 shipped lines the spines return. Sequence against
`deferred-work.md` phase 5, which migrates the graveyard onto a per-CPU work-list
substrate.

## Panic strategy (separate axis)

Not part of this work, and cheaper than it looks. Recorded here so it is not
re-litigated mid-migration.

- Kill-as-flag buys nothing for exception-context panics; the unwinder buys
  nothing for the kill path. `AbortOnUnwind` already declares nine regions where
  unwinding is unsurvivable.
- 100% of panics originating in exception or interrupt context are already
  fatal — `IrqNestHold::enter()` is the second statement of
  `common_exception_handler_impl` (`boot/src/idt.rs:471`), before the frame
  pointer is even validated. That covers demand paging, every driver ISR, the
  timer tick, all four IPI handlers and the NMI watchdog.
- The shipped cost is small: `kernel-release.elf` carries `.eh_frame` 177,180 B
  and `.gcc_except_table` 95,056 B against a 2.22 MB `.text`. The 1.5 MB figure
  belongs to `kernel-tests.elf`.
- There is no Inv. 5' problem to fix: `scripts/gates/stack/release.txt` has zero
  function entries, and `scripts/check_stack_sizes.sh:38` is 2048. The unwinder
  exemptions exist only in the dev build.
- Diagnostics do not depend on it. The oops record, the message and the
  symbolized backtrace all run at `boot/src/panic.rs:207-273`, *before*
  `begin_panic` at `:275`, and the backtrace is an rbp walk with kallsyms, not
  DWARF.
- Abort semantics already ships as a boot mode: `panic.on_oops=on` makes
  `production_recovery_enabled()` false.
- The real asset is test isolation across 2743 registered tests
  (`ktesting/src/runner.rs:19`, baseline at `scripts/check_test_count.sh:24`).
  Per-variant strategy — `unwind` for tests, `abort` for dev/release — keeps it.
  `-Cpanic` overrides a target spec in both directions, so this needs a cargo
  profile or a `KERNEL_RUSTFLAGS` token, not a second target JSON.

Revisit after step 8, when the choice is genuinely free.

## Fix regardless

Each is independently valuable and none depends on this plan.

- `sched/src/futex.rs:106` takes `_timeout_ms` and discards it, so every timed
  futex wait blocks forever — while `abi/src/syscall/numbers.rs:550,553` document
  the timeout and `-ETIMEDOUT`, and
  `core/src/syscall/process_handlers.rs:787` passes the user value through.
- `slopos-ostd/src/sync/wait_queue.rs:161-166` names `PipeReadOps::read` as the
  exemplar of a signal-checking discipline that file does not implement.
- `drivers/src/tty/termios.rs:134-136` documents drain as uninterruptible and
  names a `wait_event_interruptible` that does not exist.
- `boot/src/idt.rs:391-443` is a second, divergent copy of the syscall recovery
  policy on an `int 0x80` vector no userland source issues. Delete it.
- Three comments describe `catch_panic!` as a longjmp that skips `Drop`
  (`hermetic/src/boot_ctx.rs:228-230`, `slopos-ostd/src/klog.rs:241-243`,
  `sched/src/sched_tests.rs:5687-5690`). Destructors do run —
  `boot/src/tests/panic_recovery_tests.rs:30` asserts it and passes. The last of
  the three uses the false belief to justify *not* writing a panic-mid-wait
  WaitNode-unlink test, so a real coverage gap rests on it.
- `verification/STATUS.md:198` says 16 safe `pub fn`s carry a `# Safety` section;
  the gate prints 0.
- `CLAUDE.md`'s "Inv. 5'" and "I8" collide with the framekernel paper's actual
  Inv. 5 (*sensitive memory cannot be tampered with by user programs*) and Inv. 8
  (*a Task runs on at most one CPU at any given time*). Renumber or rename.

## Open experiments

1. **Reproduce the UAF.** Park a task in `wait_event` on a queue with another
   live waiter, SIGKILL it, drain the graveyard so the stack slot recycles, then
   wake the queue. Decides whether `SLOPOS-2026-0044` is scored or downgraded.
2. **Reproduce the `virtio_blk` / `CACHED_EXT2` wedge.** SIGKILL a task inside a
   slow disk read, then attempt any file I/O.
3. **Measure kill-to-exit latency today.** Tick-stamp `task_terminate` against
   `cleanup_current_task_after_switch`. Establishes the baseline step 1 must beat
   and confirms the unbounded single-runnable-task case empirically.
4. **Are the four kthread parks meant to be killable at all?** If not, step 7
   shrinks to the `Mutex` question alone.
5. **Does `-Zemit-stack-sizes` report safe or safe+unsafe stack under
   `-Z sanitizer=safestack`?** Nine exact arithmetic matches say summed, but
   neither `scripts/check_stack_sizes.sh` nor `CLAUDE.md` says so, and both speak
   of "the guard page" singular where there are two
   (`mm/src/memory_layout_defs.rs:87`, `:121`).
