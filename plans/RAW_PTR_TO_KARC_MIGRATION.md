# Task lifetime: finish the `*mut Task` excision

## Intent

`KArc<Task>` is the single owning handle for every kernel task, and the
ownership rules are machine-checked. What remains is the type-level half:
`slopos-ostd` still carries a 75-function accessor layer over
`*mut TaskInner<K, U>`, and `sched`/`core` still bind raw task pointers in the
signatures that call it. Those signatures carry no lifetime, so their contract
is enforced by review rather than by the compiler.

`scripts/check_task_ownership.sh` measures what is left:

| Check | Count | Where |
|---|---|---|
| 1 — raw task pointer in binding position | 115 | 74 in `accessors.rs`, 17 `scheduler.rs`, 5 `switch.rs`, 5 `task_lifecycle.rs`, rest scattered |
| 4 — `task_borrow` / `task_borrow_mut` | 45 | 12 `core/src/syscall/tests.rs`, 6 `process_handlers.rs`, 4 `poll_ioctl_handlers.rs`, 2 each in `task_state.rs` / `task_family.rs` / `sched_tests.rs` / `signal.rs` / `dispatch.rs`, 1 `scheduler.rs` |
| 8 — return type names a lifetime no argument constrains | 34 | only 3 on task paths (`accessors.rs`); the rest are unrelated helpers in `ptr_buf.rs`, `dev/`, `pcr.rs`, `io_mem.rs`, `heap.rs` |

**Check 4 reaching zero is the terminal criterion.** Check 1 falls out of it —
74 of its 115 hits are inside the layer being deleted. The gate runs in warn
mode until then.

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

Ordered so each step's callers are converted before the callee it depends on.

### 1. `core/` syscall surface

14 of the 45 `task_borrow` sites and the last raw-pointer bindings outside
`sched`. `SyscallContext<'a>` already holds `&'a Task`; these are call sites
that still route around it.

- `process_handlers.rs` (6), `poll_ioctl_handlers.rs` (4), `signal.rs` (2 plus
  3 raw bindings), `dispatch.rs` (2 plus 2 raw bindings).
- `core/src/syscall/tests.rs` holds 12 — the single largest cluster. Convert
  last, after the production surface settles, so the fixtures are rewritten
  once.

### 2. `sched` remainder

`task_state.rs` (2), `task_family.rs` (2), `scheduler.rs` (1 borrow + 17 raw
bindings), `task_lifecycle.rs` (5 raw bindings), `switch.rs` (5).

`scheduler.rs` is the delicate one and wants a session that opens on it. Two of
its raw derivations come from the switch witnesses in `prepare_switch_to`,
feeding the TLB/FS_BASE/TSS/CR3 stages; unpicking those is design work, not a
mechanical conversion, and a wrong borrow there is silent rather than a build
error.

The productive shape to grep for first is a function that already takes `&Task`
or `&KArc<Task>` and converts it back — `from_ref(…).cast_mut()`, or
`let p = task_ref.as_ptr()` under a registry guard already in scope.

### 3. Dissolve the accessor layer

`slopos-ostd/src/task/accessors.rs`, 75 `pub fn`s over 1350 lines, is the
terminal step. Its replacement — `&self` methods on `TaskInner` in
`borrowed.rs` — exists; 28 of those methods have no caller yet and acquire one
here. Delete `task_borrow` and `task_borrow_mut` with the layer.

Two accessors cannot convert until their callers do, and both are named in
step 2: `futex_remove_task` (teardown holds a raw pointer) and
`save_task_context_from_interrupt_frame` (writes register state through
`as_ptr_nascent`).

### 4. Sealed `PcrTaskType` marker

`CurrentTask::get()` casts the type-erased PCR pointer to whatever
`TaskInner<K, U>` the caller names. It cannot be `unsafe fn`, because
`sched`/`core` forbid `unsafe` and could then never call it, and ostd cannot
name the concrete type (`KernelStack`/`UnsafeStack` live in
`sched/src/task_stack.rs`).

Resolution: an ostd-exported `macro_rules! declare_pcr_task_type!` whose body
carries the `unsafe impl PcrTaskType`, invoked once in `sched/src/task_struct.rs`.
The invocation site contains no `unsafe` token, and `#![forbid(unsafe_code)]`
does not reject an expansion from an external-crate macro —
`slopos_ostd::no_mangle_static!` is invoked six times in `sched/src/safestack_rt.rs`
today. **Verify that empirically with a throwaway build before committing to
it.** Bind the *publisher* to the same marker
(`pcr::publish_current_task::<K,U>`) so reader and writer agree by type rather
than by comment.

Latent rather than live: the kernel's `Current` alias is the only spelling and
there is one monomorphisation.

### 5. Closeout

Fold I1–I8 into the module docs that already carry most of the rationale
(`placement.rs`, `task_reclaim.rs`, `cell.rs`, `task_table.rs`), each
cross-referencing the Verus corollary that is its machine-checked form. Add the
task-ownership contract to the agent guidelines beside the unsafe-surface and
allocation-discipline sections. Drop `TASK_OWNERSHIP_GATE_WARN=1` so the gate
goes hard.

The docs-repo page (`content/docs/architecture/task-ownership.mdx`, written and
uncommitted in `/home/nil0ft/repos/slopos-docs`) states that the only routes to
a `&mut TaskInner` are `KArc::get_mut` on the sole pre-registration strong
reference, and `Drop`. That is false while `task_borrow_mut` is public, so the
page **must not be committed until check 4 reads zero** — it describes the
finished architecture, not the tree. Register it in `architecture/meta.json` and
add the row to `verification/verus-status.mdx`.

Then delete this plan.

## Constraints the tree imposes

Check these before designing against any of these areas — each contradicts a
plausible assumption.

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
  agree with the save area". Signal delivery saves via `fpu_save_current_keep`
  and *keeps* ownership, so a handler clobbers live vector registers with both
  halves of the tag untouched. `fpu_restore_to_cpu` must therefore stay
  unconditional; a tag-driven skip discards exactly the state `sigreturn`
  reinstates, and fails `signal_preserves_vector_regs`. Separating the two
  questions needs a per-task generation counter bumped on save and recorded on
  restore, which does not exist. Until it does, `fpu_state_valid` has no sound
  call site.
- **The FPU owner tag cannot live inside `FpuState`.** That struct carries a
  `size_of == FPU_STATE_SIZE` assertion plus `Copy` and `Zeroable`; an atomic
  field breaks all three. It is a hardware XSAVE buffer and stays one — the tag
  belongs beside it on the task, which is also where Linux keeps `last_cpu`.
- **`SyscallContext` holds a borrow, and never held an owning handle.** The
  target is `&'a Task` because every kernel surveyed holds a borrow of the
  current task on the syscall path, and an owning handle would leak here: a
  blocked task is torn down from another CPU without unwinding. Asterinas hit
  exactly that leak twice (issues #785/#1491) before fixing it structurally.
  Its two constructors differ because the test fixture parks the BSP on a
  bootstrap stub, so `Current::get()` returns `None` there — which is the real
  reason a witness cannot simply be stored in the struct.
- **`task_borrow` needs one sanctioned survivor.** `task_put` consults the
  dispatch-pin predicate *after* winning the one-to-zero release, where no
  handle exists and a placement clone would be resurrection. That is
  `with_parked`, sound for the opposite reason to every other borrow: not
  because someone else keeps the task alive, but because nobody else can reach
  it.
- **A raw task-pointer gate must not cover return position.** "Reference in,
  pointer out" is sanctioned — a Treiber successor *is* a raw pointer, governed
  by the parked reference the link represents.
- **Converting a plain field to an atomic can raise both counters.** The
  conversion moves its accessor out of the macro families into a hand-written
  function, and the obvious form takes `*const TaskInner` and calls
  `task_borrow`. Write the replacement as an `&self` method from the start, and
  re-measure after each field rather than after the step.

## Verification

Every commit: `cargo fmt --all`, `just build`, and a full green `just test`.

- Clear `builddir/.kernel-elf-gates.stamp` before `just build` — the ELF gates
  cache on ELF hash and print `skipped` otherwise.
- Read `just test`'s **pass count**, not its exit code.
- `just test` leaves a tests-build kernel at `builddir/kernel.elf`, so
  `check_kernel_softfloat.sh` run afterwards reports on a different
  configuration with different exemptions.
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
- Check 8 reads zero **on task paths**; the unrelated helpers it also matches
  (`ptr_buf.rs`, `dev/`, `pcr.rs`, `io_mem.rs`, `heap.rs`) are a separate
  backlog and not a blocker here.
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
