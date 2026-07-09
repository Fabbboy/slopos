# Task lifetime: rip out `*mut Task` + open-coded refcount, own tasks with `KArc<Task>`

## Intent

Make `KArc<Task>` the single owning handle for every kernel task, with
individually allocated tasks that are freed by their final strong drop — the
ownership shape Linux (`get/put_task_struct` + `finish_task_switch`),
Rust-for-Linux (`ARef<Task>` + `ListArc`), Redox (weak-in-runqueue contexts),
and Asterinas (`Arc<Task>` + per-CPU deferred previous-task drop) all converge
on. Asterinas — the framekernel SlopOS's ostd discipline is modeled on — is the
line-for-line precedent. This is a pre-alpha rip-and-replace: the permanent
task pool, its generation-based identity, and the hand-rolled refcount are
deleted, not wrapped.

## Why

Today the kernel has exactly one type left with an open-coded refcount:
`Task::refcnt: AtomicU32` (`slopos-ostd/src/task/kernel_task.rs:371`). Every
other shared kernel object is already `KArc`-managed (`OpenFile`, socket
backings, `VmSpace`, `Session`/`ProcessGroup`). The task side costs us:

- **Signatures lie.** `*mut Task` carries no lifetime; the contract ("caller
  bumped refcnt") is enforced by review, and every one of the 150+ raw-pointer
  signatures across `sched/`, `core/`, and the ~128-function accessor layer
  (`slopos-ostd/src/task/accessors.rs`) must be audited by hand.
- **Send/Sync are asserted, not derived.** Raw task pointers cross CPUs via
  blanket `unsafe impl Send/Sync` on `KernelSync<T>` and `AtomicPtr<Task>`
  fields, with no type-level connection to a held reference.
- **The refcount is not what it looks like.** `refcnt` never frees anything —
  it gates *slot recycling* (`reset_in_place`) in a permanent 8192-slot pool
  (`sched/src/task/task_table.rs:21`) whose `KBox<Task>` backings live until
  shutdown. Identity is a parallel generation scheme. Two hand-rolled lifetime
  mechanisms, both outside the type system, in the subsystem with the
  kernel's worst historical bug class (lost wakes, use-after-free ordering).

`KArc<Task>` collapses this to one audited primitive: clone is the bump, drop
is the decrement, weak-upgrade is the only stale-handle check, and dead-task
memory actually returns to the allocator.

## Baseline being replaced (verified paths)

- Pool + recycling: `sched/src/task/task_table.rs` — `TASK_POOL_CAPACITY`
  (`:21`), `reap_zombies` (`:173`), `reserve_task_slot` (`:617`), and the
  lock-free scanners that depend on never-freed slots: `task_find_by_id`
  (`:436`), `task_resolve_handle` (`:481`), `task_find_by_cr3` (`:504`).
- Refcount + accessors: `kernel_task.rs:371` (`refcnt`), `accessors.rs:415/428`
  (`task_inc_ref`/`task_dec_ref`), `TaskRefGuard`.
- Scheduler ownership: ReadyQueue links/unlinks pair with inc/dec
  (`sched/src/per_cpu.rs`), remote-wake Treiber inbox increfs on push, the
  dispatcher takes an "on CPU" reference before `execute_task` and releases it
  post-switch on the idle stack (`sched/src/scheduler.rs:1248`), `WAIT_REFS`
  holds `KernelSync<*mut Task>` values, and `sched_placement: AtomicU8`
  (`kernel_task.rs:370`) hand-enforces cross-list exclusivity.
- Teardown: `task_terminate` under a whole-sequence `PreemptGuard`
  (`sched/src/task/task_lifecycle.rs:682`), `on_cpu`-deferred stack free via
  `cleanup_current_task_after_switch` (`:896`).

Two properties of the baseline are load-bearing and must be re-provided, not
dropped: (1) type-stable memory is what makes the three scanners safe — once
tasks free for real, *every* lookup must go through a liveness-checked path;
(2) the post-switch release on the idle stack is a half-built
`finish_task_switch` — the migration completes it rather than inventing it.

## Target architecture

**Ownership.** One task = one `KArc<Task>`, allocated per spawn with
`KArc::try_init(Task::init_…())` (in-place; the ~8 KiB `Task` rvalue never
touches a stack frame — `check_stack_sizes.sh` stays green). Strong refs are
held by: the containers a task is placed in (runqueue / remote inbox / wait
maps), the dispatch reference while on CPU, and the parent (zombie ownership).
Everything else — registry, TTY/session back-edges, parent links from child,
signal targets, poll registrations — holds `KWeak<Task>`.

**Placement token (the ListArc analog).** Keep the intrusive links (O(1), no
allocation in wake paths) but make `sched_placement` the formal *ownership*
state machine: a successful CAS `None → ReadyQueue/Inbox/OnCpu` is the moment
one owning `KArc<Task>` is moved *into* that container (stored through the
link, via ostd-internal `into_raw`/`from_raw` that only the placement
primitives may call); the reverse transition moves it back out. Linked ⇒ that
container provably holds the owning ref. Double-insert is impossible by
construction, exactly as RfL's `ListArc` guarantees, and it is today's CAS
discipline with ownership attached rather than a new mechanism.

**Current task.** The PCR `current_task: AtomicPtr<()>` at frozen offset 40
(`slopos-ostd/src/cpu/x86_64/pcr.rs:93,247` — SafeStack naked asm reads it;
ABI cannot change) becomes a *borrow projection* of the dispatch-held strong
ref. The public accessor returns a `!Send` borrow guard valid only in the
current context; storing a task requires an explicit `KArc` clone. Never an
owned handle on the fast path (RfL/Asterinas rule).

**Deferred final drop.** A per-CPU `previous_task` slot (Asterinas
`PREVIOUS_TASK_PTR`): the context switch stashes the outgoing owning ref
raw; the *successor* drains and drops it after the switch, with IRQs enabled,
on its own stack. An exiting task's last owning ref leaves through this slot,
so stacks are always freed from someone else's context. Before dispatching a
task picked cross-CPU, spin until its `on_cpu` clears (Asterinas
`switch_to_task` guard) — no CPU ever runs or frees a stack another CPU is
still leaving.

**Registry and lookup.** The pool is deleted. A `TaskRegistry` (locked
`KBTreeMap`, integrated with the canonical ostd `Handle`/`HandleTable`
machinery) maps id → `KWeak<Task>`. Lookup = weak upgrade (`inc_not_zero`
semantics): dead task ⇒ `None`, never a fabricated strong ref from a raw
pointer. `task_find_by_cr3` becomes a registry walk under the same lock —
its callers must be audited for IRQ context; the lock is a cli-spinlock and
the upgrade path allocates nothing. Task ids become monotonic and non-reused
(widen internal id to `u64`; the userland-facing handle encoding keeps its
current width), which deletes generation-based identity outright. A plain
live-task counter preserves the pool's old DoS bound (spawn fails fallibly at
the cap).

**Teardown.** `task_terminate` keeps its status protocol but reclamation
becomes ownership-driven: a zombie is simply a dead task whose parent still
holds the strong ref; `waitpid` reads `exit_info` out of the `Task` and drops
that ref (off-lock); reparenting moves the ref. No global zombie scan, no
recycle gate, and nothing in per-task teardown needs a stop-the-world pause.
`Task`'s fields are already RAII (kernel stack, FPU area, VM space, fd table,
job-control `KArc`s), so the destructor is field drops — heavy, which is
exactly why invariant I3 below exists.

## Invariants (the plan's contract)

- **I1** Every owning task reference is a `KArc<Task>`. Raw task pointers
  exist only inside ostd placement/link primitives and the PCR slot, each
  derived from an owning ref whose holder is documented at the site.
- **I2** Linked ⇒ owned: a task on any queue/inbox/wait-map has its owning
  ref held *by that container*, moved in and out only through the placement
  state machine.
- **I3** The final drop never runs on the dying task's own stack, never with
  IRQs disabled, and never under any lock. `Task::drop` debug-asserts IRQs-on
  and an empty held-lock set (the per-CPU lock bookkeeping that already feeds
  the NMI watchdog dump). Rationale: the destructor frees to the buddy
  allocator, whose LUF reuse path performs synchronous cross-CPU TLB drains —
  a drop under a cli-lock is the known slab/LUF deadlock, not a latency blip.
  Every detach-under-lock site collects refs and drops them after release
  (generalize the ring's `pending_reap` discipline, `ring/src/enter.rs`).
- **I4** Wake and enqueue paths allocate nothing; `KArc` clone is one atomic.
- **I5** `current` is a borrow; PCR offset 40 stays raw and ABI-frozen.
- **I6** Lookup is weak-upgrade only; upgrading a terminated-but-referenced
  task is legal (status-checked by callers), fabricating strong refs is not.
- **I7** `KArc` is fallible everywhere and saturates on refcount overflow.

## Workstreams

**W0 — In-house `KArc` (complete 2026-07-09).** `KArc`/`KWeak` are now an
in-house ostd primitive backed by one tail-allocated `KArcInner<T: ?Sized>`;
the `alloc::sync::Arc`/`Weak` backing was removed outright. Strong and weak
counts saturate permanently at `isize::MAX`, releases use Release CAS plus an
Acquire fence before destruction/deallocation, and weak upgrade is an Acquire
`inc_not_zero` loop. `get_mut` locks the implicit weak count while proving
uniqueness. `try_new`, in-place `try_init`, and `try_new_cyclic` are fallible;
cyclic construction publishes the strong count only after `T` is initialized.
DST coercion works for both handles, including all existing
`KArc<dyn FileBacking>` users, and sized ostd-internal `into_raw`/`from_raw`
move one strong reference into and out of future placement/PCR slots.

W0 verification:

- Kernel `stest!` coverage exercises weak lifetime, downgrade/upgrade,
  fallible cyclic construction, weak-count accounting, and strong-count
  saturation.
- Host and Miri coverage exercises 10,000 native upgrade-vs-final-drop races,
  cyclic publication, strong and weak DST coercion, empty weak handles,
  uniqueness, and raw ownership round trips with exactly-once destruction.
- `just build`, `just check-framekernel`, and all 82 Verus obligations pass;
  the TCB ratio is 0.528%, below both the Phase-1 and Phase-2 limits.
- Full QEMU testing passes: 2,646 kernel tests plus 23 userland tests, with no
  failures or skips.
- Security triage raw findings: none. The refcount ordering, saturation races,
  allocation layouts, destructor/deallocator split, cyclic publication,
  raw ownership boundary, and the syscall/mm/fs/driver consumers were
  re-reviewed. No confidence-scored or CVSS-eligible finding was produced.

**W1 — Drop-context infrastructure (complete 2026-07-10).** The PCR now owns
one deferred previous-task slot per CPU. The IRQ-off switch tail moves its
dispatch reference into that slot after publishing `on_cpu = false`; the idle
dispatcher takes and releases it exactly once on its own stack, with IRQs
enabled and no tracked lock held. Cross-CPU dispatch now waits for the prior
CPU's Acquire/Release `on_cpu` handoff instead of requeueing a still-switching
task. The reusable off-lock context gate and `Task::drop` assertions enforce
IRQs-on/empty-held-lock destruction. The slot currently carries the existing
dispatch refcount ownership; W3 changes that payload to the moved `KArc<Task>`
without changing this switch boundary.

W1 verification:

- A kernel `stest!` moves one task reference through the PCR slot, proves the
  first drain releases it, and proves a second drain is empty.
- Host and Miri coverage proves `Task::drop` rejects IRQs-off and held-lock
  contexts and that the off-lock helper runs destructors with IRQs enabled.
- `just build`, `just check-framekernel`, all 82 Verus obligations, and the
  complete OSTD Miri suite pass. The TCB ratio is 0.529%, below both targets.
- Full QEMU testing passes: 2,647 kernel tests plus 23 userland tests, with no
  failures or skips.
- Security triage raw findings: none. The PCR ownership transfer, exact-once
  drain, `on_cpu` publication ordering, destructor context gate, and the
  syscall/mm/fs/driver boundaries were re-reviewed. No confidence-scored or
  CVSS-eligible finding was produced; `CVSS.md` is unchanged.

**W2 — Registry.** `TaskRegistry` with `KWeak` values, monotonic `u64` ids,
`Handle` resolution via upgrade, live-task cap. Port `task_find_by_id`,
`task_resolve_handle`, `task_find_by_cr3` and audit every caller's execution
context. The pool, `reserve_task_slot`, `reap_zombies`, and `reset_in_place`
are deleted in this workstream.

**W3 — Scheduler ownership flip.** Placement state machine carries the owning
ref (I2): ReadyQueue, remote-wake inbox (Treiber push moves a strong ref;
drain moves it out), dispatch reference, `WAIT_REFS` →
`KBTreeMap<u32, KArc<Task>>` with off-lock drops. Migrate the raw-pointer
signatures across `scheduler.rs`, `per_cpu.rs`, `futex.rs`, signals, IPC —
`KArc<Task>`/`&Task` per site — and shrink the accessor layer to methods on
`Task`.

**W4 — Teardown rebuild.** Ownership-driven zombie/waitpid/reparent per the
target architecture; exit paths route the final ref through the deferred
slot; TTY hangup and futex cleanup keep their ordering but hold typed refs.

**W5 — Excision + acceptance.** Delete `refcnt`, `inc_ref`/`dec_ref`,
`task_inc_ref`/`task_dec_ref`, `TaskRefGuard`, every `KernelSync<*mut Task>`,
and generation identity. Acceptance greps below must return zero hits.

W1 is complete. W2 remains independent of the ownership flip and lands next
with its own tests; W3–W5 are the rip-and-replace and land as one coherent
series.

## Acceptance

- `grep -rn "refcnt\|task_inc_ref\|task_dec_ref\|inc_ref\|dec_ref"` over
  kernel crates: zero hits (page-table refcount in `mm/paging` excepted —
  different concept, out of scope).
- No `*mut Task` in any signature outside ostd placement/link primitives and
  the PCR slot; no `KernelSync<*mut Task>` anywhere.
- No `unsafe impl Send`/`Sync` justified by "caller holds a task refcount".
- All framekernel gates green (`check_unsafe_outside_ostd.sh`,
  `check_stack_sizes.sh`, `check_kernel_softfloat.sh`); full `just test`
  green **under KVM** on this host.
- Wall-time of `just test` and `just boot-fast` within noise of the
  pre-migration baseline (measure before/after; the async-migration
  regression taught us to check, not assume).

## Testing

- W0 coverage is complete (`KArc` saturation and cyclic init as `stest!`s;
  upgrade-vs-drop races, DST coercion, raw round trips, and uniqueness under
  host tests and Miri). W1 coverage is complete (the deferred slot drains
  exactly once; host/Miri fault injection exercises the lock/IRQ assertions).
  Add coverage for W2 (upgrade returns `None` after death; id non-reuse; cap
  behavior) and W3 (placement transitions move ownership exactly once;
  double-insert CAS-rejected).
- Torture: existing spawn/kill stress (`mm_stress_test`, `ctrlc_flood_test`,
  strand sweep) plus a dedicated spawn-exit-waitpid churn test asserting
  memory returns (allocator watermark) — the property the pool never had.
- The lost-wake regression suite must stay green: wake paths change ownership
  plumbing but not the ttwu-style totality loop semantics.

## Out of scope

- Thread-per-core placement/re-dispatch fixes (`plans/KNOWN_ISSUES.md`) —
  sequenced after this migration precisely so scheduler surgery isn't done
  twice.
- RCU/epoch lockless lookup. `KArc` buys existence, not lockless traversal;
  registry lookups stay lock-guarded until a real RCU design exists.
- Intrusive list mechanics beyond ownership (queues stay intrusive).
- Page-table page refcounts (`mm/src/paging/tables.rs`) — frame-level
  bookkeeping, not object sharing.

## Prior art

Linux two-refcount `task_struct` (`usage` vs stack refcount; final put in
`finish_task_switch`; PREEMPT_RT forbids the final put in atomic context —
our I3); RfL `ARef<Task>`/`current`-as-borrow and `ListArc` (list consumes
the membership token — our placement state machine); Asterinas
`ostd/src/task/processor.rs` deferred previous-task drop + run-queue
`Arc<Task>` (our W1); Redox weak-refs-in-runqueue (our registry/backedge
discipline). In-tree templates: `OpenFile` (strong-owns/weak-observes,
`fs/src/fileio/mod.rs`), `Session`/`ProcessGroup` (trivial-Drop KArc DAG),
ring `pending_reap` (detach-under-lock, drop-off-lock).
