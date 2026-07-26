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

**W2 — Registry (complete 2026-07-10).** The permanent pool is gone: tasks
are individually `KArc::try_init`-allocated and the pool, `reserve_task_slot`,
`reap_zombies`, `reset_in_place`, generation identity, `slot_index`, and the
`ZombieList`/`zombie_link` are deleted. `TaskRegistry` is a pre-reserved
`MAX_TASKS` slot spine (allocated once outside the manager lock, never
grown) whose slots hold `RegistryEntry { id, KWeak, KArc }`: the weak handle
serves lookups, the strong handle is the transitional scheduler-lifetime
owner W3 moves into placement containers. Every mutation under the
cli-spinlock is a plain slot write — registration and retirement allocate
nothing under the lock, and removed handles drop off-lock — so the buddy's
LUF reuse drain can never deadlock against the registry lock. Ids are
monotonic non-reused `u64` (public width stays `u32`; exhaustion is a
permanent failure, not a wrap); `Handle` generation is permanently zero and
resolution is the same weak upgrade as id lookup. `task_find_by_id`,
`task_resolve_handle`, and `task_find_by_cr3` all return a liveness-checked
`TaskRef` guard; lookup is weak-upgrade only (I6). A dead task's spine slot
is reclaimed by the last releaser's `TaskRef`/`task_release_ref` drop; when
that release lands in a context that cannot run the allocator-heavy `Task`
destructor (IRQs off or a tracked lock held), a one-shot latch defers the
retirement to the idle dispatcher's off-lock drain, so no terminated task
strands. Both the concurrent-task cap and the id space preserve the pool's
old DoS bound (spawn fails fallibly at either limit). Every migrated
lookup caller holds its `TaskRef` across the raw-pointer window
(`scheduler.rs`, `runtime.rs`, `kthread.rs`, `exec`, syscall handlers, the
user-fault path).

W2 verification:

- New/rewritten kernel `stest!`s: weak upgrade returns `None` after death, a
  held guard pins a terminated task until its final drop, ids advance and
  never reuse across a 1000-task stampede, the concurrent-task cap rejects
  without consuming an id or a spine slot, and the handle path never aliases
  a later id. 110 `slopos_sched` tests, 16 context tests, 127 core-syscall
  tests, and 45 process/signal/exec tests pass.
- `just build`, the tests-enabled kernel build, all framekernel gates
  (`check_unsafe_outside_ostd`, `check_stack_sizes`, `check_kernel_softfloat`,
  `check_alloc_dep`, `check_drop_panic_free`, `check_wait_predicate_purity`),
  all 82 Verus obligations, the OSTD host suite, and the OSTD Miri suite pass.
  The TCB ratio is 0.527%, below both targets.
- End-to-end: the production kernel boots to userland and spawns `/sbin/init`
  plus the shell through the registry with no fault.
- The full 2648-test kernel phase cannot complete on this host because of a
  pre-existing panic-recovery/unwinder NMI-watchdog interaction (deliberate
  panic in `test_run_recoverable_cleanup` runs the DWARF unwinder long enough
  to trip the cross-CPU watchdog). An A/B run of the same filter against the
  pre-W2 baseline reproduces the identical panic, confirming it is not a W2
  regression.
- Security triage raw findings: `task_find_by_cr3` guard drops under the
  registry lock, the `task_release_ref` pre-decrement status/id capture, the
  deferred-reclaim latch, id monotonicity/exhaustion, and the spine
  circular-scan bound were all reviewed. No finding reached the confidence-80
  threshold; `CVSS.md` is unchanged.

**W3 — Scheduler ownership flip (complete 2026-07-10).** Every scheduler
ownership transfer now moves a `KArc<Task>`, on the Linux
`get_task_struct`/`put_task_struct` shape: a container gains membership by
cloning one owning reference from the still-live task and parking it (the
intrusive `ready_link`/`remote_inbox_link` plus one leaked strong count are the
owning ref, I2); it loses membership by reclaiming and dropping that reference.
Four ostd primitives are the sole sanctioned hand-off
(`slopos-ostd/src/task/placement.rs`): `task_placement_clone` (mint an owning
handle from a live pointer — one atomic, the wake fast path, I4),
`task_placement_retain` (park one into a container), `task_placement_leak`/
`task_placement_reclaim` (park/recover a handle as a raw pointer), all wrapping
the ostd-internal `KArc::into_raw`/`from_raw`. The ready queue, the remote-wake
Treiber inbox, and the dispatch reference are converted; the dequeued task's
dispatch reference is a cloned handle held across the context switch, parked in
the `previous_task` PCR slot, then reclaimed and dropped by the successor on the
idle stack (finishing the half-built `finish_task_switch`). The switch tail
drains that slot *before* re-enabling interrupts and drops the reference as a
bare decrement — a terminated task's allocator-heavy retirement (buddy free +
cross-CPU TLB drain) is deferred to the idle dispatcher's reclaim drain rather
than run in the switch window, which both closes the re-entrant double-park
window and keeps the switch tail cheap.
`WAIT_REFS` is `KBTreeMap<u32, KernelSync<KArc<Task>>>` and the futex bucket
owns its waiter (`KernelSync<Option<KArc<Task>>>` keyed by id), both dropping
off the map/bucket path. Reclaim re-keys from the deleted `refcnt` to
`!on_cpu && KArc::strong_count(owner) == 1 && terminated`: a placement reference
forces `strong_count >= 2`, so a terminated task cannot be reclaimed while
queued/running, and every placement drop captures id+status before the drop and
routes retirement through the self-deferring `task_try_reclaim_id`
(`release_placement_arc`). The registry `owner` remains the interim anchor for
blocked/zombie tasks — it outlives every placement reference, which is what
makes those drops never-final and therefore lock- and IRQ-safe. `refcnt` and
its whole surface (`inc_ref`/`dec_ref`/`ref_count`, `task_inc_ref`/
`task_dec_ref`/`task_inc_ref_with_id`/`task_ref_count`, `TaskRefGuard`, the
`KernelSync<*mut Task>` fields in `scheduler.rs`/`futex.rs`) are deleted.

W3 verification:

- New/rewritten kernel `stest!`s: a placement leak/reclaim round-trip conserves
  the strong count and base pointer; the deferred slot drains exactly one
  owning reference; the remote-inbox duplicate-push and non-ready-drop tests now
  assert strong-count deltas instead of the old `refcnt`.
- `just build` (both the production and tests-enabled kernels),
  `check_stack_sizes` (the ~8 KiB `Task` is still only ever `KArc::try_init`-ed,
  never on a stack), `check_kernel_softfloat`, the full `just check-framekernel`
  source and host gates, and all 82 Verus obligations pass.
- Full QEMU testing under KVM passes: 2,649 kernel tests with no failures or
  skips; the 112-test scheduler suite (placement transitions, lost-wake
  regressions, inbox ownership) and the fork/exit/wait and reap-stampede churn
  tests are green, asserting that a destroyed task stops upgrading.
- Security triage raw findings: the clone-at-leaf liveness contract, the
  never-final placement drops, the reclaim gate, the off-lock wait-map/futex
  drops, and the dispatch-slot hand-off were reviewed. No confidence-scored or
  CVSS-eligible finding was produced; `CVSS.md` is unchanged.

**W4 — Teardown rebuild (complete 2026-07-11).** Teardown is now ownership-
driven. A parent owns each of its children — live and zombie — through an
intrusive `children` list on the parent `Task` (new `SiblingRole` + per-child
`sibling_link`), whose membership is one strong reference parked exactly like
ready-queue placement (`task_placement_retain` on link, `task_placement_reclaim`
on unlink, I2); there is no separate zombie list, and a zombie is simply a dead
child still parked in its parent's list. Three registry-lock-guarded,
allocation-free helpers are the sole hand-off (`sched/src/task/task_family.rs`):
`link_child` (park a child, or orphan it if the parent is already dying — the
under-lock parent-alive check plus the `set_status`-before-drain ordering closes
the stranded-child window), `take_one_child` (drain one), and `unlink_child`
(detach a specific child, reclaiming only when the list removal wins against a
concurrent drain). `waitpid` (`task_consume_zombie`) reaps by unlinking the child
and dropping the parent's owning reference off-lock; a dying parent's
`reparent_and_reap_children` drains its own list in O(children) — auto-reaping
zombies (Zombie → Terminated) and orphaning live children — deleting the former
whole-registry scan (`iter_tasks_mut` and `task_registry_len` are gone). Every
reclaimed reference drops off the registry lock via `release_placement_arc` and
is never the last reference (the registry owner outlives it until W5), so each
drop is a bare decrement and a zombie's retirement self-defers. The child→parent
link stays a `u32 parent_task_id` resolved through the registry weak-upgrade —
the shape Redox (`ppid` + `CONTEXTS`) and Asterinas (`AtomicPid` + `pid_table`)
both use, and the shape the signal-target and poll back-edges already use —
rather than an embedded `KWeak`, which would duplicate the registry's single
liveness index and add a cross-CPU data race on a non-atomic field. Fork, clone,
exec, and `task_set_parent` publish the parent edge via `link_child` after
registration; `clone_from_raw` resets the copied list head, sibling slot, and
parent id; `Task::drop` debug-asserts an empty children list and a detached
sibling slot. (The exit path already routes its final on-CPU reference through
the deferred previous-task slot from W1/W3, and futex cleanup and TTY hangup
already hold typed refs / ids from W3 — this workstream leaves those intact.)

W4 verification:

- New kernel `stest!`s: a linked child sits on its parent's children list and
  carries exactly one extra parked strong reference, and `waitpid`'s reap unlinks
  it, drops that reference, and reclaims the task; a dying parent drains a
  multi-child list, auto-reaping the zombies and orphaning the live children. The
  reworked `test_orphan_child_auto_reaped_on_parent_exit` and
  `test_waitpid_survives_task_churn` pass on the new `link_child` path.
- `just build`, `check_stack_sizes` (the `Task` grew by one intrusive list head +
  one link slot and is still only ever `KArc::try_init`-ed), and
  `check_kernel_softfloat` pass; the full `just check-framekernel` source and host
  gates pass, with no `unsafe` added outside `slopos-ostd`.
- The kernel test phase (2,674 planned — including the scheduler churn, reap-
  stampede, fork-exit-wait, and wait-exit-race suites that directly exercise
  teardown) passes with no failures on every completing run.
- Full `just test` under KVM completes green on most runs; a minority abort in
  the userland phase on one of three pre-existing, non-deterministic spots
  unrelated to task ownership — the OSTD user-mode round-trip's per-CPU
  `return_reason` slot (read after a preemption in the trampoline-return tail),
  the cross-core per-core reactor (`utest_percore_reactor`, a documented thread-
  per-core wakeup gap), and the deliberate panic-recovery unwind
  (`test_run_recoverable_cleanup` tripping the cross-CPU NMI watchdog). An A/B of
  ten baseline runs against eight W4 runs reproduces the identical failure
  signatures at comparable rates (baseline ~2/10, W4 ~3/8), confirming these are
  pre-existing and not a W4 regression; none touch the changed teardown, fork, or
  reap code, all of which stays in the green kernel phase.
- Security triage raw findings: the drain's off-lock reclaim, the `unlink_child`
  removal-gated reclaim (no double free against a concurrent drain), the
  `link_child` under-lock parent-alive check (no stranded child), the `Task::drop`
  leak tripwires, and the reclaim-gate interaction (a zombie's extra parent
  reference never trips the `strong_count == 1` gate early) were reviewed. No
  finding reached the confidence-80 threshold; `CVSS.md` is unchanged.

**W5 — Excision + acceptance.** The ownership half has landed; the raw-pointer
half has not. What follows is the state W5 works against and the work left.

**Ownership is now as the target architecture describes it.** `RegistryEntry` is
`{ id, weak }` — the registry observes tasks and owns none, so a lookup is a
liveness-checked upgrade and no entry can fabricate a strong reference.
`owns_pointer` compares against `KWeak::as_ptr`, which is defined without
upgrading and without reading through the pointer, so the cr3 scan no longer
mints a handle that could run the destructor under the registry lock.

What keeps a registered task alive is the **existence reference** — one strong
reference the task holds to itself, handed over at registration and taken back
exactly once when it is reaped, exactly as Linux does for `task_struct` and
releases in `release_task`. Containers do not cover every live state: a blocked
kernel thread sits in no queue, has no parent, and is named by its wait node only
through an opaque handle; a placement reservation has not reached its queue; a
freshly created or forked task is registered before it is published. The
existence reference covers all of them, and it is what keeps every container's
release provably non-final — hence still a bare decrement, safe under a lock.

Reap is unhash plus release-existence as one step, gated on `Terminated &&
!task_is_dispatch_pinned` — a statement about task *state*, with no strong-count
read anywhere. The gate is shared with the destructor gate so the two cannot
disagree, and it is load-bearing: `dispatch()` publishes `PCR.current_task`
without setting `on_cpu`, so unhashing a task that is still a CPU's current would
make `task_pointer_is_valid` report pointer corruption and send
`scheduler_tasks_for_cpu` down its recovery path on the dying task's own stack.
`task_put` is the sole release primitive; `release_placement_arc` and its
`_deferred` twin are gone, and the latch survives only as a dispatch-pin retry.
`register_task` hands its caller an owning handle that pins the task across the
rest of its construction. The `refcnt`, `task_inc_ref`/`task_dec_ref`, and
`KernelSync<*mut Task>` greps are all at zero.

**Remaining.**

- *Coverage for the ownership half.* A leak tripwire asserting the parked
  existence count equals registry occupancy at quiescent points (compare the two
  counters, not an absolute — per-CPU idle tasks are never reaped, and
  `KernelTestScope::drop` runs `task_shutdown_all` without `init_task_manager`); a
  blocked kernel thread surviving on the existence reference alone; the reap
  declining while dispatch-pinned and completing after the drain; and a
  parked-count assertion on the churn tests so a missed release fails loudly
  instead of leaking 8 KiB a thousand times. Finish the lookup-after-reap consumer
  audit; anything that turns out to need a reaped task to stay findable gets its
  own side table, never a delayed unhash.
- *The raw-pointer excision.* 110 signature-position `*mut Task` sites (91 in
  `sched/`, 19 in `core/`), plus the 112-function accessor layer over
  `*mut`/`*const TaskInner<K, U>` and its ~827 call sites. `placement.rs` and the
  PCR slot — the carve-outs this plan names — are already clean, so they absorb
  none of the 110. Ordered as: field atomicisation, then register/FPU/user-context
  cells carrying exclusivity as a witness parameter, then the `CurrentTask` borrow
  guard (invariant I5), then two mechanical accessor passes (retype to `&TaskInner`
  keeping `Option`; then bodies to inherent methods, dropping `Option`), then
  `per_cpu`, the switch core, lifecycle/table/family/futex, the `core/` syscall
  surface, and tests. Cell accessors must return `*mut T` or `&mut T` and never
  `T`: one `FpuCell::set(FpuState)` puts 2.6 KiB on the caller's frame and fails
  the stack gate.
- *Two carve-outs this plan failed to name.* `safestack_rt.rs` holds 17 non-`KArc`
  `Task` bodies in `.bss`, seeded by asm before the heap exists; retype that
  surface to the PCR's own `*mut ()` shape rather than adding a third named
  exception. `inspect.rs` needs no carve-out once its callers hand it `TaskRef`.
- *Defects found while doing the above, none of them in this plan's original
  scope.* `WaitTaskHandle = *mut c_void` launders a `*mut Task` through the
  driver-runtime vtable and `wake_one`/`wake_all` dereference it on any CPU — a
  use-after-free the acceptance grep cannot see; fix by making the handle a task
  id resolved through a weak upgrade. `newcomer_outranks_current` dereferences
  another CPU's PCR task to read its priority, racing that CPU's
  `drain_previous_task`; publish a per-CPU priority instead.
  `mark_task_terminated` and `deliver_pending_signal_core` each hold a `&mut Task`
  across calls that re-derive a reference to the same allocation, which the cells
  step is what allows to be retyped away. And a task created by `task_create` is
  `Blocked` and reachable between registration and publication, so a
  process-group signal can `unblock_task` it onto a runqueue before its publisher
  finishes — make "registered but not yet published" an explicit placement token
  that `wake_blocked_task` refuses, rather than a status coincidence.
- *Widen the acceptance greps* to `*mut TaskInner`, `WaitTaskHandle` and
  `DriverTaskHandle`: the first is the same defect one type parameter away, and
  the other two launder a task pointer through `c_void`.
- Machine-checked Verus obligations for the ownership core, and the closeout.

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
  W2 coverage is complete (upgrade returns `None` after death; id non-reuse
  across a 1000-task stampede; the cap rejects without consuming an id or slot;
  the handle path never aliases a later id). W3 coverage is complete (a
  placement leak/reclaim round trip conserves the strong count and base
  pointer; the deferred slot drains exactly one owning reference; the
  remote-inbox duplicate-push and non-ready-drop tests assert strong-count
  deltas). W4 coverage is complete (a linked child carries exactly one extra
  parked reference; `waitpid`'s reap drops it; a dying parent drains a
  multi-child list). The existence reference has host and Miri coverage
  (park/release round-trips exactly once, a second release yields nothing, a
  never-parked task releases nothing, and a cloned task does not inherit the
  flag); its kernel-side coverage is listed under W5.

  Note that the guard test no longer asserts that an outstanding guard keeps a
  terminated task *resolvable* — the reap unhashes independently of guards. It
  asserts the property that replaced it: the registration goes immediately,
  while the guard still pins the allocation until its last reference drops.
- Torture: the rewritten kernel churn tests (`test_serial_reap_stampede`,
  `test_fork_exit_wait_stress_10x100`, `test_task_wait_exit_race_1000`) now
  assert that a destroyed task stops upgrading — the memory-actually-returns
  property the pool never had. Userland spawn/kill stress (`mm_stress_test`,
  `ctrlc_flood_test`) and the strand sweep remain.
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
