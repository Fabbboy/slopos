# Task lifetime: finish the `*mut Task` excision

## Intent

`KArc<Task>` is already the single owning handle for every kernel task. What
remains is the other half of that change: `sched/` and `core/` still bind raw
task pointers in ~167 argument positions and reach task state through a
110-function accessor layer over `*mut TaskInner<K, U>`. Those signatures carry
no lifetime, so the contract they encode is enforced by review rather than by
the type system — which is what let four use-after-free and data-race defects
sit in the tree unnoticed.

`scripts/check_task_ownership.sh` measures the remaining surface: check 1 is
the argument positions, of which 93 are the accessor layer itself, and check 4
is `task_borrow`/`task_borrow_mut`, the terminal criterion. Both reaching zero
is what "done" means. The gate runs in warn mode until then.

Finish the excision and delete this plan.

## Invariants (the contract)

- **I1** Every owning task reference is a `KArc<Task>`. Raw task pointers exist
  only inside the ostd placement/link primitives, the PCR slots, and the
  pre-heap `.bss` stubs the SafeStack runtime seeds — each derived from an
  owning reference whose holder is documented at the site.
- **I2** Linked ⇒ owned: a task on any queue, inbox, or wait map has its owning
  reference held *by that container*, moved in and out only through the
  placement state machine.
- **I3** The final drop never runs on the dying task's own stack, never with
  IRQs disabled, and never under a lock. The destructor frees to the buddy
  allocator, whose reuse path performs synchronous cross-CPU TLB drains, so a
  drop under a cli-lock is the known slab/LUF deadlock rather than a latency
  blip.
- **I4** Wake and enqueue paths allocate nothing; a `KArc` clone is one atomic.
- **I5** `current` is a borrow, never an owned handle. PCR offset 40 stays raw
  and ABI-frozen.
- **I6** Lookup is weak-upgrade only. Upgrading a terminated-but-referenced task
  is legal; fabricating a strong reference from a raw pointer is not.
- **I7** `KArc` is fallible everywhere and saturates on refcount overflow.
- **I8** Every strong reference to a task is rooted in a location that survives
  independently of that task's kernel stack — the PCR slots, a placement
  container, or an explicit named clone owned by a structure that outlives the
  stack. **No owning task handle may live in a stack frame that can be
  abandoned**, which here means any frame that can block: SlopOS tears a blocked
  task down from another CPU without unwinding, so a handle left on that stack
  is never dropped and leaks the task, its stacks and its address space.

## Baseline this works against

One task is one `KArc<Task>`, individually allocated and freed by its final
strong drop. The registry holds only `KWeak` and owns nothing; what keeps a
registered task alive is the **existence reference** it holds to itself, handed
over at registration and taken back exactly once at reap. Containers — ready
queue, remote inbox, dispatch slot, parent's children list, wait maps, futex
buckets — own their members through the placement primitives. `task_put` is the
sole release primitive, and the task graveyard makes a final drop safe from any
context. Ids are monotonic `u64` internally and never reused.

`SchedPlacement::Nascent` marks a task registered but never published, so a wake
cannot publish one mid-construction. `TaskOwnCell` plus the sealed
`CurrentTask` / `SwitchWindow` witnesses give exclusive access to register state
without a `&mut`; `cwd` is the only field migrated so far. The accessor layer is
a shim over `task_borrow`/`task_borrow_mut`, which is where the last two derefs
in that layer live.

## Remaining work

### 1. Register state into witness cells

Move **five** fields into `TaskOwnCell` — `context`, `fpu_state`, `switch_ctx`,
`user_ctx` and `saved_kernel_return_ctx` — exposed as typed operations on
`TaskInner`. **Not `saved_user_ctx_ptr`**: it is one pointer word, read and
written only in the PCR round-trip swap, whose other side is already atomic, so
it becomes an `AtomicPtr<UserContext>` and stays off the witness surface
entirely. Wrapping it would be the obvious way to "finish" this step and would
be wrong.

`SwitchWindow` covers the outgoing task — the dispatcher publishes the incoming
one into the PCR before the outgoing one's registers are saved, so `CurrentTask`
does not reach it. `context` is diagnostics plus the cr3 identity tag; only
`cr3` has a functional reader, and the switch never reads it.

Cell accessors must return `*mut T` or `&mut T`, never `T`: one `FpuState` by
value is 2.6 KiB on the caller's frame and fails the 2 KiB stack gate. This is
the highest-risk step in the plan — a mis-sequenced FPU save is silent
user-visible corruption, not a fault.

### 2. `CurrentTask` at the call sites

37 `scheduler_get_current_task()` sites. Roughly half want only "who am I" and
should read `pcr::current_task_id()` instead, which dereferences nothing. Then
narrow the cross-CPU reader to an opaque address type with `PartialEq` and no
deref: after the priority moved into the PCR, both surviving foreign-CPU readers
only compare, so a foreign task dereference becomes unrepresentable.

### 3. The pointer excision

96 signature-position raw task pointers (87 in `sched/`, 9 in `core/`), plus the
accessor shims and their call sites. Order, following the compiler: `per_cpu` →
switch core → `task_lifecycle` → registry and family → futex/sleep/stats/trap →
fork and clone returning ids → `core/` syscall surface → `boot/` → test suites.

- **The terminal criterion is deleting `task_borrow` and `task_borrow_mut`**, not
  the pointer grep. Both fabricate an output lifetime from nothing, which is what
  made the `&mut`-aliasing defects expressible. But they are **not the only two**
  of that shape: seven task functions declare a lifetime no argument constrains,
  so the caller picks it and two calls hand out two live references to one place.
  `check_task_ownership.sh` check 8 finds them, and all seven must go.
- `SyscallContext` holds a **bare `*mut Task`** — not an owning handle, and not a
  borrow. It becomes `SyscallContext<'a> { task: &'a Task, … }`. The reasoning
  for that is prior art, not a leak in the present code: every kernel surveyed
  holds a borrow of the current task on the syscall path, and an owning handle
  here would leak, because a blocked task is torn down from another CPU without
  unwinding. Its two constructors differ because the test fixture parks the BSP
  on a bootstrap stub, so `Current::get()` returns `None` there — which is the
  real reason a witness cannot simply be stored in the struct.
- `task_get_info` has no production callers. Delete it with its ~35 test sites in
  the test-suite pass; it is the only `*mut *mut Task` in the tree — though not
  the only double indirection, `slibc` has several.
- `inspect::wrap` goes once fork and clone return ids; `inspect::by_id` already
  does the same job through the registry.
- The intrusive-link accessors stay pointer-typed in return position — a Treiber
  successor *is* a raw pointer, and its lifetime is governed by the parked
  reference the link represents, not by a Rust borrow. Reference in, pointer out.

### 4. Two open soundness items

- `CurrentTask::get()` casts the type-erased PCR pointer to whatever
  `TaskInner<K, U>` the caller names. It cannot simply be `unsafe`, because
  `sched`/`core` forbid `unsafe` and could then never call it; it needs a sealed
  marker the kernel implements once. Latent rather than live: the kernel's
  `Current` alias is the only spelling and there is one monomorphisation.
- The witness is advisory while `task_borrow_mut` is public — any crate can reach
  a `&mut TaskInner` and bypass it. Closed by the terminal criterion above.

### 5. Coverage

In priority order:

- A wait-queue wake against a *reaped* waiter, plus a live positive control. The
  id-handle change has no end-to-end test, and the hazard is structurally
  reachable: teardown scrubs sleep entries and futex buckets but never unlinks
  wait-queue nodes.
- A process-group signal against a nascent task — the defect's actual vector.
  `task_create` publishes `pgid = task_id` before registering, so `kill(-pgid)`
  reaches one; the current test drives `unblock_task` directly instead.
- `newcomer_outranks_current`'s decision. An inverted comparison means the kernel
  never preempts on wake — a latency cliff no functional test would catch.
- Forked-child `cwd` inheritance — incidental today, via the bytewise clone, and
  one plausible edit from silently resetting every child to `/`.
- `is_bootstrap_task_ptr`'s stride, `rt_sigaction`'s full-field round-trip, and
  a task-id-never-reused assertion.
- FPU save/restore across a switch: per-task slot isolation, the dispatcher's
  save-prev/restore-next ordering, and the AVX upper halves a regression to
  `fxsave` would silently drop.

`check_test_count.sh`'s baseline sits at exactly the current planned count, so
there is no margin — these restore it. Bump it in the same commit, and correct
the stale figure in the agent guidelines.

### 6. Closeout

Fold I1–I8 into the module docs that already carry most of the rationale
(`placement.rs`, `task_reclaim.rs`, `cell.rs`, `task_table.rs`), add the
task-ownership contract to the agent guidelines beside the unsafe-surface and
allocation-discipline sections, and drop `TASK_OWNERSHIP_GATE_WARN=1` so the
gate goes hard. The docs-repo page is written but **must not be committed**
until every register field is a cell and the gate's `task_borrow` check reads
zero — it describes the finished architecture, not the tree. Then delete this
plan.

## Constraints the tree imposes

Facts that an earlier reading of this plan got wrong. Check them before
designing against any of these areas again.

- **The dispatch reference is transient, not a container membership.**
  `ReadyQueue::dequeue` *moves* the membership reference out and returns
  `KArc<Task>`, deliberately, so a task is not "pinned by nothing" across an
  unbounded `on_cpu` spin. So `pinned ⇒ containers ≥ 1` is false; what holds is
  `pinned ⇒ exist_refs == 1`, which is stronger and makes `pinned ⇒ strong > 0`
  a derived corollary. The dispatcher therefore needs no new borrow primitive —
  it already holds an owning handle.
- **The PCR idle slot cannot own a leaked reference.** Three test paths write it
  directly: the hermetic fixture snapshots and restores it as a bare address,
  one test nulls and restores it, and `create_idle_task_for_cpu` is called
  repeatedly. A leaked handle would leak one reference per re-install.
- **The FPU owner tag cannot live inside `FpuState`.** That struct carries a
  `size_of == FPU_STATE_SIZE` assertion plus `Copy` and `Zeroable`; an atomic
  field breaks all three. It is a hardware XSAVE buffer and stays one — the tag
  belongs beside it on the task, which is also where Linux keeps `last_cpu`.
- **`SyscallContext` holds a bare `*mut Task`.** Earlier revisions of this plan
  said it holds an owning `TaskRef`, and argued at length that an owning handle
  would leak because a blocked task is torn down without unwinding. The argument
  is sound and the premise was invented: the struct has always been a raw
  pointer. So the target is still a borrow — every kernel surveyed holds one on
  the syscall path, and Asterinas hit the leak twice before fixing it
  structurally — but the leak was never present here, and nobody should go
  looking for the handle that supposedly causes it.
- **`task_borrow` needs one sanctioned survivor.** `task_put` consults the
  dispatch-pin predicate *after* winning the one-to-zero release, where no
  handle exists and a placement clone would be resurrection. That is
  `with_parked`, sound for the opposite reason to every other borrow: not
  because someone else keeps the task alive, but because nobody else can reach
  it.
- **A raw task-pointer gate must not cover return position.** "Reference in,
  pointer out" is sanctioned — a Treiber successor *is* a raw pointer, governed
  by the parked reference the link represents.

## Acceptance

- No `*mut Task` or `*mut TaskInner` in argument position outside the ostd
  placement/link primitives, the PCR slots, and the SafeStack stubs; no
  `KernelSync<*mut Task>`; no task handle laundered through `c_void`.
- `task_borrow` and `task_borrow_mut` are gone.
- No `unsafe impl Send`/`Sync` justified by "caller holds a task refcount".
- Zero hits for `refcnt|task_inc_ref|task_dec_ref|inc_ref|dec_ref` across kernel
  crates, page-table refcounts excepted.
- Every framekernel gate green, the Verus set green, and
  full `just test` green under KVM.
- `just test` wall-time and `just boot-fast` within noise of the pre-migration
  baseline — measured, not assumed.

## Out of scope

- Thread-per-core placement and re-dispatch fixes, sequenced after this so
  scheduler surgery is not done twice.
- RCU or epoch-based lockless lookup. `KArc` buys existence, not lockless
  traversal.
- Intrusive list mechanics beyond ownership; page-table page refcounts.

## Prior art

Linux's two-refcount `task_struct` (final put in `finish_task_switch`;
PREEMPT_RT forbids it in atomic context — our I3), Rust-for-Linux's
`ARef<Task>`, `current`-as-borrow, and `ListArc` membership token, Asterinas's
deferred previous-task drop and `Arc<Task>` run queue, and Redox's
weak-refs-in-runqueue. In-tree templates: `OpenFile` for strong-owns /
weak-observes, and the ring's detach-under-lock, drop-off-lock discipline.
