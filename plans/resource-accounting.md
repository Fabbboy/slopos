# Per-process resource accounting — spike

There is no rlimit, quota, ucount or racct machinery anywhere in the kernel. A
case-insensitive grep for the whole family returns one unrelated errno variant.
**Zero per-process counters exist for any kernel object.** Every object table is
a fixed global array, and one unprivileged process can exhaust any of them.

This is a spike, not a work plan, because the framework question is genuinely
blocked on a decision the code cannot answer: *who is the principal?* SlopOS has
no uid, no credential and no namespace. A quota written today would be enforced
against an unauthenticated principal: the spawn boundary refuses to mint
`TASK_FLAG_SYSTEM`, but it remains a per-task bit that fork copies wholesale and
exec never drops, so it identifies no principal a quota could bill.

What follows is the inventory, the parts that can be fixed without answering
that question, and the questions the design must answer.

## The inventory

Every fixed global table a userland process can consume entries in:

| Resource | Capacity | Per-process bound | Reclaim on exit |
|---|---:|---|---|
| fd table | 32 per process | **the only one that exists** | yes |
| TCP established PCBs | 16 shards × 4 = 64 | none | no — protocol state only |
| TCP listeners | 16 | none | yes |
| SlopRing registry | 256 | none (`owner_pid` is an access check, never counted) | yes |
| AF_UNIX sockets / pairs | 32 / 16 | none | yes |
| AF_INET sockets | 64 (event-bus key space) | none | yes |
| input event queues | 32, via a 16384-entry task-id map | none | yes |
| futex waiters | 64 buckets × 16 slots | none | n/a |
| memfd | 64 | none | yes |
| signalfd | 256 | none | yes |
| pipes | 64 | none | yes |
| open vnodes | 256 | none | yes |
| TTY / PTY | 32 | none | yes |
| VFS mounts | 16 | none | n/a |
| parked spawns | 64 | none | yes |
| tasks | 8192 global | **none** — one process can create every task | yes |
| pinned memory | 1 GiB per registration × 1024 registrations per ring | none | yes |

Two structural notes.

**Exhaustion is graceful everywhere.** Every table returns `None`,
`HandleError::Full`, `TcpError::TableFull` or a typed errno; the two exceptions
(input events, `install_accepted_child`) silently drop. Nothing panics. So the
failure mode is permanent silent denial rather than a crash — which is why none
of this shows up in testing.

**Reclaim already works, and there is exactly one pattern that works.** The
resource's release lives in a `FileBacking::drop`, the backing is owned by a
`KArc<OpenFile>` in the fd table, and the fd table is destroyed exactly once on
process exit. Ten `FileBacking` impls follow it — TTY, pipes, vnodes, memfd,
AF_INET, AF_UNIX, rings, signalfd. The exit path is exactly-once even under
cross-CPU kill: `cleanup_task_process_resources` latches on a per-task atomic bit
and runs either on the killer's CPU or on the victim's own CPU after the register
switch.

That gives the one non-negotiable design rule for whatever framework lands:

> **The charge lives inside the charged object, beside `FileBacking`, and is
> released by the same `Drop` that releases the registry slot.**

It is the only reclaim path in this kernel proven correct under cross-CPU kill,
and it is the only placement that does not re-create invariant I8 on a syscall
stack frame.

## Fix these first — they need no framework

The worst items in the inventory are not accounting failures. They are **reclaim
failures**, and two of them have fixes already sitting unused in the tree.

1. **Wire the SYN queue that exists.** `TcpListenState::on_syn`
   (`net/src/tcp/listener.rs:257`) is a complete, tested, bounded SYN queue with
   `SYN_QUEUE_MAX`, `SYN_RETRIES_MAX` and real retransmit timers. It has **zero
   production callers**; the live path
   (`net/src/tcp/pcb/listen.rs:98-105`) installs a child straight into the
   64-slot shard table and discards the `Result`. Wiring it removes the
   remote-unauthenticated permanent denial of the whole TCP stack.

2. **Give input-event queues a slot allocator.** The 16384-entry task-id map
   means every task created after id 16383 silently receives no input, forever.
   `AtomicBitmap` (`slopos-ostd/src/atomic_bitmap.rs:23`) is the lock-free slot
   allocator that removes the ceiling, and it is already used elsewhere.

3. **Release established children when their listener closes.** Closing a
   listening socket clears its accept queue but never releases the child PCBs
   already installed in the shard table; those slots come back only via RST, FIN
   or TIME_WAIT expiry.

4. **Make `pick_pid_slot_locked`'s fallback a hard error.** When no per-process
   fd-table slot is free it silently installs a userland process's descriptors
   into the **kernel's own fd table** (`fs/src/fileio/mod.rs:383`), shared with
   every other process that also fell back. That is a cross-process isolation
   break independent of quotas, and three fd-installing call sites reach it.

Items 1–3 remove the two permanent unrecoverable denials in the inventory. Item 4
removes an isolation bug. None requires deciding what a principal is.

## Prerequisites for the framework

Both must land before quota work starts, not alongside it:

- **`plans/privilege-model.md`** — a quota needs a principal to bill and an
  exemption that survives fork and exec. `TASK_FLAG_SYSTEM` is neither.
- **`plans/process-identity.md` phases 1–3** — a pid-keyed accounting row would
  inherit the monotonic-counter bug wholesale and stop resolving at process 256.

## Questions the spike must answer

**1. What is the principal?** The candidates in the tree are the pid (poisoned
until process-identity lands), the `Session`/`ProcessGroup` KArc DAG, or a new
object. The `ProcessGroup` option is interesting because it already exists, its
lifetime is already exactly "at least one member alive", it is O(1)-reachable
from `ctx.task()`, both its `Drop`s are documented safe under any lock, and it is
immune to the pid bug. It is also coarser than a process and empty for kernel
tasks. This choice determines everything else.

**2. What are the numbers?** Every current cap is unmeasured. 256 rings across
256 processes is one each. 32 AF_UNIX slots is fewer than the compositor's own 32
client slots. Choosing a per-process quota requires knowing the real peak usage
of a running desktop session, which nothing records today. Does the plan get to
raise the global caps — and note `EventBus`'s static grows with
`MAX_SOCKETS`/`MAX_PIPES`/`MAX_TTYS`/`MAX_UNIX_SOCKETS`, so raising them is not
free.

**3. Should `FILEIO_MAX_OPEN_FILES = 32` become a declared `RLIMIT_NOFILE`?** It
is the only per-process limit in the kernel, it is implicit rather than declared,
and it is very low. Raising it multiplies every downstream table's per-process
reach.

**4. How are remote-driven consumptions charged?** A TCP shard slot taken by an
unsolicited SYN has no local process. Per-listener accounting, a separate remote
budget, SYN cookies, or just the bounded queue from item 1 — these are different
designs with different behaviour under a real flood.

**5. Where does the counter storage live, given the crate DAG?** net, ring, fs,
mm, sched and drivers all need to charge, so it must sit at or below `ostd`. But
`ostd` is the trusted domain and adding policy there moves the `tcb_ratio`
denominator that `scripts/tcb_ratio.sh --max 1.0` gates. Is a policy-only,
`unsafe`-free module in `ostd` acceptable, or does this need a new crate below
everything?

**6. Fail-the-syscall or deny-the-object-class?** Linux rlimits fail the syscall
with a typed errno, which is what every call site currently expects. Fuchsia's
`ZX_POL_NEW_*` job policy denies an object class with a DENY/EXCEPTION action,
which composes better with a future job hierarchy and gives a debuggable failure.

**7. Does memory itself get accounted?** The per-process page counter was deleted
in commit `7feff87e` for having 21 writes and zero readers. Its charge points —
the demand-fault resolver and the range-unmap helpers — are therefore known-good
and freshly excised. Reintroducing a page charge means re-adding what was just
removed, which needs an explicit decision rather than a silent revert. Note that
pinned memory (1 GiB × 1024 registrations per ring, bounded only by physical RAM)
is the largest genuinely unbounded consumption in the inventory.

**8. Is a per-process task cap in scope?** One process can create all 8192 tasks,
each with a 32 KiB kernel stack plus a data stack. This is arguably the single
largest denial available, and it is not table-shaped — so it is only addressed if
`allocate_task` becomes a charge site.

**9. Does the compositor's 32-client / 32-window limit belong here?** It is
userland, outside the framekernel discipline, and already bounded by the kernel's
32 AF_UNIX slots — but it is a real denial surface with its own reclaim story.

## Prior art worth reading before deciding

- **Linux rlimits + `struct ucounts`** — per-user-namespace counters added
  precisely because rlimits alone could not bound a fork bomb across a namespace.
  The retrofit cost is the lesson.
- **FreeBSD `racct`/`rctl`** — a general resource-accounting framework with
  pluggable rules, and login classes as the principal. Closer to what a kernel
  without namespaces wants.
- **Fuchsia job hierarchy** — resources are bounded per *job*, jobs nest, and
  policy is inherited. This is the best structural fit for SlopOS: it needs no
  uid, it gives the compositor a natural place in the hierarchy, and it answers
  question 4 (a listener's job is charged) and question 6 together.
- **seL4 untyped memory** — the extreme: every resource comes from a
  caller-supplied capability, so accounting is structural and no quota exists.
  Too far for a Linux-ABI kernel, but it is the argument for making the charge
  part of the object rather than a side table.

## Spike deliverable

A design note answering questions 1–9, a proposed charge/uncharge placement for
three representative resources (an fd-backed one, the TCP table, and tasks), and
a migration order. That note replaces this document.

Meanwhile the four items under "fix these first" should land regardless of what
the spike concludes, and each needs an exhaustion test — the 2716-test suite
contains no exhaustion or cross-process-denial test for any global registry.
