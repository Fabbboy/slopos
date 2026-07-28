# Task lifetime: finish the `*mut Task` excision

## Intent

`KArc<Task>` is the single owning handle for every kernel task, and the
ownership rules are machine-checked. What remains is the type-level half:
`slopos-ostd/src/task/accessors.rs` still carries a 54-function accessor layer
over `*mut TaskInner<K, U>`, and `sched/src/scheduler.rs` still binds raw task
pointers in the switch-core signatures that feed it. Those signatures carry no
lifetime, so their contract is enforced by review rather than by the compiler.

`scripts/check_task_ownership.sh` measures what is left:

| Check | Count | Where |
|---|---|---|
| 1 — raw task pointer in binding position | 77 | 54 in `accessors.rs`, 16 `scheduler.rs`, 2 each `switch.rs` / `task_table.rs`, 1 each `task_stats.rs` / `sched_tests.rs` / `context_tests.rs` |
| 4 — `task_borrow` / `task_borrow_mut` | 10 | 9 inside `accessors.rs` itself (4 macro bodies, the 2 definitions, 3 `children_*` helpers), 1 the re-export line naming them |
| 8 — return type names a lifetime no argument constrains | 33 | 3 on task paths (`task_borrow`, `task_borrow_mut`, `task_exit_info_ref`); 30 unrelated helpers in `ptr_buf.rs`, `dev/`, `cstr.rs`, `user/context.rs`, `interrupt_frame.rs`, `mm/paging`, `mm/slab`, `heap.rs` |

**Check 4 reaching zero is the terminal criterion.** Nothing outside the
accessor layer calls `task_borrow` any more, so every remaining hit is the layer
implementing itself and dies with the file — but the file cannot be deleted
until its **451 remaining accessor call sites** across `sched`/`core` move to
`&self` methods. The gate runs in warn mode until then.

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

## Remaining work

### 1. The scheduler switch core

`scheduler.rs` (16 raw bindings) and `switch.rs` (2) are what stands between the
accessor layer and deletion: the largest concentration of the 451 accessor call
sites is in `scheduler.rs`, and they cannot move to `&self` methods while the
enclosing signatures bind `*mut Task`.

This wants a session that opens on it, and `just boot-fast` ×3 after every
commit — a broken switch core does not boot, and unit tests will not say so.

- **An `IdleTask` borrow guard in OSTD**, mirroring `CurrentTask`: minted from
  the PCR idle slot, `!Send`/`!Sync`, sound because the idle task holds its own
  existence reference and is never reaped. It **must be local-CPU only**
  (`TaskAddr` exists precisely because reading another CPU's task races its
  switch tail); take `cpu_id == pcr::get_current_cpu()` as a documented,
  debug-asserted precondition. It must be reachable from `sched` — `enter_scheduler`
  needs it for `kernel_stack_top` and to `dispatch` before the stack switch, and
  `sched_tests.rs`'s idle-tick test needs it too.
  `task_is_dispatch_pinned` then gains a third disjunct ("is some CPU's idle
  task"). That predicate is shared with `destroy_context_is_safe` **and**
  modelled in `verification/proofs/task_ownership.rs` — check the new disjunct
  against `(T6) pinned ⇒ exist_refs == 1` and `pinned ⇒ registered`, and update
  the proof's `pinned` comment, which says it is set and cleared by two writes.
- **`prepare_switch_to` drops its two raw projections** (`scheduler.rs:451,454`)
  — both already derive from `w.task()`, a `&Task` behind a witness. The FPU
  stages are already witness-driven; the middle band (TLB, FS_BASE, TSS RSP0,
  CR3) is all plain reads a `&Task` serves. The one blocker is
  `task_pcr_round_trip_swap`, the only *write* in that band and the last
  unwitnessed `as_ptr_nascent` caller: give it a witness-taking sibling. Review
  its four orderings explicitly (`Acquire` on both loads, `Release` on both
  stores across `saved_user_ctx_ptr` ↔ `pcr.user_ctx_ptr`) — it is hand-written
  and therefore the one most likely to drift.
- **`dispatch`, `execute_task`, `run_ready_task_from_idle`,
  `switch_from_current_to_idle`, `install_idle_task`, `task_is_idle_candidate`,
  `ensure_idle_switch_ctx_valid`, `reset_task_quantum`,
  `save_/restore_live_recovery_depth`, `consume_time_slice`,
  `published_priority`, `task_has_no_preempt_flag`, `wait_ref_acquire`** take
  borrows. `run_ready_task_from_idle` already holds `dispatch_ref: TaskRef`
  across the whole window.
  `Current::get()`'s documented soundness proof does *not* cover the span
  between `execute_task` publishing the incoming task and
  `dispatch(cpu_id, idle_task)`; use the idle guard there, not `Current`.
  Deleting the corrupt-current recovery path (`scheduler_tasks_for_cpu`) makes
  three doc sites false (`scheduler.rs`'s `task_is_dispatch_pinned`,
  `task_table.rs`'s `reap_task_registration`, and the Verus `(T6)` comment) —
  update them in the same commit.
- **`run_switch(prev: Option<&TaskInner>, next: &TaskInner, …)`.**
  **DECISION POINT.** This does not compile against `sched/src/context_tests.rs`
  as things stand: the test's `publish` closure calls
  `prev.fpu_restore_to_cpu_mut(xcr0)` (`&mut self`) while `Some(&*prev)` would
  hold a shared borrow across the whole call — E0502, not an aliasing nicety.
  Restructuring the test to establish `pat_prev` before entering `run_switch`
  must be checked against what the test actually proves (that save-prev works);
  the alternative is keeping the raw endpoints and adding `switch.rs` to the
  gate's `SANCTIONED_SURFACES` with the two-stack rationale, which is the plan
  conceding its own terminal step.
- `task_stats.rs`'s `task_record_context_switch` and `context_tests.rs`'s
  `prev_ptr` are blocked on the two items above and fall out with them.
  `TaskInner` has no `PartialEq`, so that function's identity test must be
  `TaskAddr`-based.
- `task_pointer_is_valid`'s callers die with the borrows; decide whether its
  definition stays a documented survivor or narrows to `TaskAddr`.
  `TaskRegistry::owns_pointer` and `task_is_current_on_any_cpu` stay raw by
  design — both docs say why, and upgrading in `owns_pointer` would run the
  destructor under the registry lock.

### 2. Dissolve the accessor layer

`slopos-ostd/src/task/accessors.rs` — 54 hand-written `pub fn`s plus five macro
families over 1004 lines — is the terminal step. Its replacement, `&self`
methods on `TaskInner` in `borrowed.rs` and `kernel_task.rs`, mostly exists.

- The mechanical block: accessors that map 1:1 onto existing methods, or compose
  over `inbox_link()` / `reclaim_link()` (`task_reclaim.rs` is the worked
  example). Confirm `task_children_peek`'s two implementations agree
  (`peek_front()` vs `iter().next()`) before swapping — it has no callers, so
  this is cheap to settle.
- The accessors with no `&self` equivalent. Most are one-liners over a public
  field (`task_has_flag`, `task_is_invalid`) or need only an id
  (`task_waiter_count`, `task_wake_all_waiters`). Do **not** add
  `fn kernel_stack_top(&self)` / `fn time_slice(&self)` beside identically-named
  public fields — that reads as shadowing at every call site.
- The remaining plain-field setters (`parent_task_id`, `cpu_affinity`,
  `time_slice`, `time_slice_remaining`, `kernel_stack_top`). Justify each retype
  on the **data race**, not on counter deltas.
  `time_slice`/`time_slice_remaining` have exactly two consumers, both rewritten
  by step 1 — delete all four accessors with their last caller instead of
  atomising. For the others, sweep every plain access first: `diag.rs` reads
  `kernel_stack_top` by `read_volatile` (an atomic cannot be read that way),
  `kernel_task.rs`'s `reset_runtime_state` copies `time_slice_remaining =
  time_slice`, plus sites in `task_lifecycle.rs`, `core_handlers.rs`,
  `work_steal.rs`, `driver_hooks.rs`, `task_session.rs`. If `kernel_stack_top`
  is atomised, say why `kernel_stack_base` is not — `kernel_stack_bounds` reads
  both together.
- Relocate the non-accessors that already have the target signature
  (`child_exit_event`, `signal_pending_event`, `task_kernel_stack_seed_ret`,
  `task_set_fs_base`, `task_reset_caught_handlers`,
  `task_default_signals_in_mask`, `task_clone_from`) and move their importers
  with them (`pidfd/src/file_ops.rs`, `signalfd/src/file_ops.rs`,
  `task_lifecycle.rs`).
- Delete `task_borrow` / `task_borrow_mut`, `accessors.rs`, and
  `sched/src/task/task_accessors.rs`'s re-export list. `task_exit_info_ref` —
  the third task-path check-8 hit — dies with them; `exit_info()` already
  replaces it.

### 3. `spawn` violates I8

`spawn_program_with_attrs` holds a `PendingTask` — the *sole* reference to a
child that already owns its kernel stack, data stack and process VM — across
`do_exec`'s blocking ELF read (`vfs_open`, `stat`, a 4 KiB-chunked read loop over
the whole file, `process_vm_load_elf_data`). A `SIGKILL` on the spawner reaps it
without unwinding, so `PendingTask::drop` never runs, and because the token's
whole purpose is to be unregistered, nothing can find the orphan afterwards —
not `task_find_by_id`, not a registry walk, not `task_slot_census`, not
shutdown-time reclamation.

Park the token in a spine keyed by spawner id, mirroring `WAIT_REFS` in
`scheduler.rs` — the in-tree template for exactly this ("a blocked task that is
killed never unwinds its own stack, so the reference must be released from
teardown").

**Do not hook the release in `mark_task_terminated`.** `task_terminate` runs
that *before* it tests whether the victim is still executing, so the killer's CPU
would call `task_abandon` → `cleanup_owned_process` →
`destroy_process_vm`/`fileio_destroy_table_for_process` on a `process_id` the
spawner is concurrently inside `do_exec` or `apply_fd_actions` on. That converts
today's leak into a cross-CPU use-after-free. Hook instead where the tree already
establishes the victim is not executing: `cleanup_terminated_task_resources`
(off-CPU branch, first statement is the `on_cpu` bail) and
`cleanup_current_task_after_switch` (deferred branch, victim's own CPU after the
register swap) — the same pair `task_terminate` uses to decide when the kernel
stack may be freed — with `task_shutdown_all`'s drain as backstop.

Other constraints: `launch_init` spawns from a boot step *before*
`enter_scheduler`, so `pcr::current_task_id()` is `INVALID_TASK_ID` there — a
pre-scheduler frame cannot be async-killed, so skip parking when there is no
current task rather than keying on the sentinel. The spine's lock must never be
taken while holding the registry lock; say so in the module doc. Five call sites
route through this (`boot_services.rs`, `early_init.rs`'s panic-syscall smoke,
`process_handlers.rs`'s posix_spawn, `exec/utest.rs`,
`tests/heap_allocator_tests.rs`). Measure the stack frame before and after on
**both** the `just build` and `just test` configurations — that function already
carries a `[u8; TASK_NAME_MAX_LEN]`, three out-params and a `PendingTask` against
a 2 KiB ceiling.

The regression test must kill the spawner while it is **on-CPU on a peer**, not
merely blocked — the blocked case takes `task_terminate`'s benign branch and
would ship the use-after-free green. Assert via `task_existence_parked_count()` /
`task_slot_census` / process-VM accounting.

### 4. Seal `PcrTaskType`

`CurrentTask::get()` casts the type-erased PCR pointer to whatever
`TaskInner<K, U>` the caller names. It cannot be `unsafe fn`, because
`sched`/`core` forbid `unsafe` and could then never call it, and ostd cannot name
the concrete type (`KernelStack`/`UnsafeStack` live in `sched/src/task_stack.rs`).

`unsafe impl PcrTaskType for TaskInner<KernelStack, UnsafeStack>` outside OSTD is
**E0117** — `PcrTaskType` has no type parameters, so `Self` is the only slot, and
`TaskInner<…>` is a foreign non-`#[fundamental]` ADT. RFC 2451 does not cover it,
and a macro cannot make an illegal impl legal. The working shape, probed on the
pinned toolchain:

- `pub unsafe trait PcrStackTy {}` in OSTD;
- an OSTD-exported `declare_pcr_stack_type!` invoked twice in
  `sched/src/task_struct.rs` — foreign trait for a *local* type, always legal;
- a blanket `unsafe impl<K: PcrStackTy, U: PcrStackTy> PcrTaskType for
  TaskInner<K, U>` **inside** OSTD.

`CurrentTask::<K,U>::get()` then carries `where K: PcrStackTy, U: PcrStackTy`, as
does `pcr::set_current_task`'s typed wrapper, binding reader and writer by type
rather than by comment. The macro's unsafe token does not trip the invoking
crate's `#![forbid(unsafe_code)]` and needs no `#[allow_internal_unsafe]` —
`slopos_ostd::hermetic_state!` already expands both an `unsafe impl` and an
`unsafe fn` inside `sched` on every `just test`.

### 5. Check 8, repo-wide

30 hits beyond the task paths, across ~100 call sites: `util/ptr_buf.rs` (10 fns,
~63 sites), `dev/mod.rs` (7), `util/cstr.rs` (2, 12 sites),
`irq/interrupt_frame.rs` (2), `user/context.rs` (2), `mm/paging/{tables,walker}.rs`
(4), `mm/slab/page.rs` (2), `mm/heap.rs::KBox::leak` (1). Every one is the same
shape this migration exists to delete: a safe function that fabricates a
reference, with a caller-chosen lifetime, from an address — which lets a
`#![forbid(unsafe_code)]` crate reach UB without writing the keyword.

Approach, in order of preference per site: (a) take the token/guard that already
proves the claim, the way `with_parked` does; (b) scoped-closure form
(`with_ref(ptr, |r| …)`), whose higher-ranked lifetime the caller cannot choose;
(c) for `KBox::leak`, `-> &'static mut T` with `T: 'static` — the honest type for
an allocation that is deliberately never freed, and both in-tree callers are
already `'static`, so it removes the hit with no exemption.
`InterruptFrame::from_ptr_mut` and `UserContext::from_ptr_mut` are best fixed by
having OSTD's entry glue form the borrow once and hand `&mut _` to
`boot/`/`core/`.

This is the largest single item and is beyond the original Acceptance below,
which scoped check 8 to task paths. Land it last so it can be descoped without
blocking closeout.

### 6. Closeout

Fold I1–I8 into the module docs that already carry most of the rationale
(`placement.rs`, `task_reclaim.rs`, `cell.rs`, `task_table.rs`), each
cross-referencing the Verus corollary that is its machine-checked form. Add the
task-ownership contract to the agent guidelines beside the unsafe-surface and
allocation-discipline sections. Drop `TASK_OWNERSHIP_GATE_WARN=1` from the
`check-framekernel-gates` recipe, strip the migration narration from the comment
above it, remove the warn-mode block from the gate script, grep CI for the
variable, and re-run `scripts/check_task_ownership.sh --self-test` after any
header or regex edit.

The docs-repo page (`content/docs/architecture/task-ownership.mdx`, written and
uncommitted in `/home/nil0ft/repos/slopos-docs`) states that the only routes to a
`&mut TaskInner` are `KArc::get_mut` on the sole pre-registration strong
reference, and `Drop`. That is false while `task_borrow_mut` is public, so the
page **must not be committed until check 4 reads zero** — it describes the
finished architecture, not the tree. Re-read it against the finished tree for
anything else it asserts, register it in `architecture/meta.json`, and add the row
to `verification/verus-status.mdx`.

Then delete this plan.

## Constraints the tree imposes

Check these before designing against any of these areas — each contradicts a
plausible assumption.

- **The gate counts macro *bodies*, not generated functions.** The five accessor
  macro families contribute one check-1 line and one check-4 line each, at the
  macro body, however many rows they generate. Deleting a row moves **neither**
  counter; only deleting a hand-written accessor, or a whole family, does. Any
  step justified by a counter delta must be derived on that basis.
- **`TaskRef` is the owning task handle outside OSTD, and `KArc<Task>` is not
  nameable there.** The destructor is allocator-heavy and its context predicate
  lives in `task_put`, so a handle reaching `KArc::drop` would skip the
  preempt-guard and dispatch-pin gates — and `Task::drop`'s tripwire is a
  `debug_assert!`, so in a release build that is a buddy/TLB deadlock, not a
  panic. Hence the guard: `TaskRef::drop` routes every release, and `sched`'s
  four constructors (`from_placement`, `clone_of`, `take_existence`, the
  registry upgrade) are the only ways to obtain one. `into_arc` is `pub(super)`
  and `node()` replaces any lender that would hand back a cloneable
  `&KArc<Task>`, so the wrong `Drop` has no expression that reaches it. Keep it
  that way: widening either is what puts an un-guarded handle back in a binding.
  `kernel_shutdown` is the matching constraint from the other side — task
  teardown runs there with interrupts *on*, because the destructor waits on
  cross-CPU TLB drains.
- **A function that ends in `task_reap` must take the guard, not a borrow.**
  The reap releases the task's existence reference off-lock and may run the
  destructor inline, so a `&Task` parameter has no guarantee its referent
  outlives the call — and `sched` forbids `unsafe`, so nothing at the call site
  can express a fallback.
- **The dispatch reference is transient, not a container membership.**
  `ReadyQueue::dequeue` *moves* the membership reference out and returns
  `KArc<Task>`, deliberately, so a task is not "pinned by nothing" across an
  unbounded `on_cpu` spin. So `pinned ⇒ containers ≥ 1` is false; what holds is
  `pinned ⇒ exist_refs == 1`, which is stronger and makes `pinned ⇒ strong > 0`
  a derived corollary. The dispatcher needs no new borrow primitive — it already
  holds an owning handle.
- **The PCR idle slot cannot own a leaked reference.** Three test paths write it
  directly: the hermetic fixture snapshots and restores it as a bare address,
  one test nulls and restores it, and `create_idle_task_for_cpu` is called
  repeatedly. A leaked handle would leak one reference per re-install.
- **The FPU owner tag records ownership, not content freshness.** It answers
  "which task does this register file belong to", never "does the file still
  agree with the save area". Signal delivery saves via a keep-ownership save, so
  a handler clobbers live vector registers with both halves of the tag
  untouched. `fpu_restore_to_cpu` must therefore stay unconditional; a
  tag-driven skip discards exactly the state `sigreturn` reinstates, and fails
  `signal_preserves_vector_regs`. Separating the two questions needs a per-task
  generation counter bumped on save and recorded on restore, which does not
  exist. Until it does, `fpu_state_valid` has no sound call site.
- **The FPU owner tag cannot live inside `FpuState`.** That struct carries a
  `size_of == FPU_STATE_SIZE` assertion plus `Copy` and `Zeroable`; an atomic
  field breaks all three. It is a hardware XSAVE buffer and stays one — the tag
  belongs beside it on the task, which is also where Linux keeps `last_cpu`.
- **`task_borrow` needs one sanctioned survivor.** `task_put` consults the
  dispatch-pin predicate *after* winning the one-to-zero release, where no
  handle exists and a placement clone would be resurrection. That is
  `with_parked`, sound for the opposite reason to every other borrow: not
  because someone else keeps the task alive, but because nobody else can reach
  it.
- **A raw task-pointer gate must not cover return position.** "Reference in,
  pointer out" is sanctioned — a Treiber successor *is* a raw pointer, governed
  by the parked reference the link represents. Retyping such a site to
  `NonNull<Task>` in place is not a fix: it passes check 1 by token substitution
  while staying exactly what the gate's preamble calls a handle with no owner.
  Move it into a sanctioned surface instead.
- **Nothing may open a SafeStack frame between `dispatch()` and the register
  swap.** The kernel runs on two stacks per task. `RSP` is swapped atomically by
  `switch_registers`; the *data* stack, which holds every address-taken local,
  is selected by `PCR.current_task`, so it swaps when the dispatcher republishes
  the PCR — several frames earlier. A frame opened in between is carved out of
  the incoming task's data stack and released when the *calling* task next runs,
  on whatever CPU picks it up, unsynchronised against the CPU that owns that
  stack by then. The owner's next prologue then lays a frame over its own live
  locals. `run_switch` takes the publication as an argument for exactly this
  reason, and asserts the ordering; a new address-taken local in the closure or
  in `switch_context` would reintroduce it silently.

## Verification

Every commit: `cargo fmt --all`, `just build`, and a full green `just test`.

- Clear `builddir/.kernel-elf-gates.stamp` before `just build` — the ELF gates
  cache on ELF hash and print `skipped` otherwise.
- Read `just test`'s **pass count**, not its exit code. The current baseline is
  **2698 passed, 0 failed** (2675 kernel + 23 userland).
- `just test` leaves a tests-build kernel at `builddir/kernel.elf`, so
  `check_kernel_softfloat.sh` run afterwards reports on a different
  configuration with different exemptions.
- Re-measure `scripts/check_task_ownership.sh` after each step, not each
  milestone.
- Review every `Ordering::` in the diff. A silent Acquire→Relaxed downgrade
  during a mechanical retype is a scheduler that works under TCG and loses
  wakeups under KVM.
- Confirm KVM before trusting any full-suite failure — no `/dev/kvm` means a
  silent TCG fallback and a panic-recovery NMI hang that reads as a real
  regression.
- `scheduler.rs` additionally wants `just boot-fast` ×3: a broken switch core
  does not boot, and unit tests will not tell you.

## Acceptance

- No `*mut Task` or `*mut TaskInner` in argument position outside the ostd
  placement/link primitives, the PCR slots, and the SafeStack stubs; no
  `KernelSync<*mut Task>`; no task handle laundered through `c_void`.
- `task_borrow` and `task_borrow_mut` are gone.
- No `unsafe impl Send`/`Sync` justified by "caller holds a task refcount".
- Zero hits for `refcnt|task_inc_ref|task_dec_ref|inc_ref|dec_ref` across kernel
  crates, page-table refcounts excepted.
- Check 8 reads zero — repo-wide, not only on task paths (see step 5).
- Every framekernel gate green, the Verus set green, and full `just test` green
  under KVM.
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
