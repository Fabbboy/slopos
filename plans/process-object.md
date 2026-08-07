# The `Process` object

SlopOS has no object for a process. A process is an identity *triple* that nothing
joins:

1. `Task.process_id: u32` — a plain field. Ids recycle and carry no generation. The
   allocator draws fresh ids first, then the **oldest** returned id from a FIFO ring
   (`mm/src/process_vm.rs:126-135`), and the comment explains the choice: "an id handed
   straight back to the next process is what makes a stale reference to the previous one
   resolve to something live." The FIFO is damage control for a hazard the id cannot
   fix.
2. `Task.process_vm_handle: AtomicU64` (`slopos-ostd/src/task/kernel_task.rs:406`) — a
   packed `Handle<ProcessVm>`, 16 slot bits and 48 generation bits. Its own doc states
   the problem outright: "`process_id` cannot do this job. Ids are recycled, so a task
   holding only an id can be handed the address space of whichever process holds that id
   *now* — which on a page fault means servicing the fault in a stranger's page tables."
3. A `ProcessVm` slot in `mm` (`static PROCESS_VMS: [SpinLock<ProcessVm>; 256]`) and a
   descriptor-table slot in `fs` (`static PROCESS_TABLES: [FileTableSlot; 256]`), joined
   only by a runtime-installed function pointer
   (`static PROCESS_FD_TABLE_TEARDOWN: AtomicPtr<()>`, `mm/src/process_vm.rs:358`).

Plus a `ProcessGroup` that lives in `slopos-ostd` and knows about none of it.

Two plans are blocked on this object. `plans/resource-accounting.md` needs a principal
to bill. `plans/authority-model.md` needs an owner for a credential — and says so:
building a credential before the object exists means building it twice. This plan is a
prerequisite for both and depends on neither.

---

## The object cannot contain what it names

The obvious design — one object owning the address space and the descriptor table, with
one `Drop` — **does not compile**, and the reason is structural rather than incidental:

```
abi          → bitflags only          (no first-party deps at all)
slopos-ostd  → { abi, ostd-derive }
mm           → ostd
fs           → { mm, ostd }
sched        → { fs, mm, ostd }
```

`KArc` is ostd's, and `Task` is ostd's, so `Process` must be ostd's. An ostd `Process`
therefore cannot name `ProcessVm` (mm) or `FdEntry` (fs). Every payoff an inlining
design would claim is premised on a field it cannot declare.

The tree already contains both the obstacle and the answer: `process_vm_handle` is a
packed handle stored as an **opaque u64** for precisely this reason.

**So: re-key, do not inline.** `mm` keeps `PROCESS_VMS`, `fs` keeps `PROCESS_TABLES`,
and their lookup key changes from `u32 process_id` to `Handle<Process>`. The object
supplies identity; the subsystems keep their storage.

Written down here so nobody re-proposes the inlining and rediscovers the DAG.

---

## What `Process` is

```rust
// slopos-ostd/src/process/mod.rs
pub struct Process {
    /// Display and ABI only. Never an authority key and never a lookup key.
    id: u32,
    /// Identity: slot bits plus generation, the existing `handle.rs` packing.
    handle: Handle<Process>,
    /// This process's resource account row. One per Process, minted with it.
    account: AccountId,
    /// The wait/orphan tree. MUTABLE — re-homed on reparent-to-init.
    parent: RcuArcSlot<Process>,
    /// The accounting tree. IMMUTABLE — set once to the spawner's account.
    account_parent: AccountId,
    children: /* intrusive list, the shape KArc<Task> ownership already uses */,
    task_count: AtomicU32,
}
```

`parent` and `account_parent` are deliberately different fields. Reparent-to-init is
required by the wait protocol and must not move a budget; an immutable accounting edge
is what makes charge migration unrepresentable rather than merely discouraged. Zircon
reached the same conclusion by making the upward edge `const` at all three levels of its
hierarchy — there is no `zx_process_set_job`.

`parent` is an `RcuArcSlot` for the reason `Task::process_group` already is
(`slopos-ostd/src/task/kernel_task.rs:419`): the writer is not the owner, so the store
lands on a field a reader on another CPU may be cloning from at that instant, and the
displaced reference must be released only after a grace period so no destructor runs on
the writer's stack.

`Task` gains `process_handle: AtomicU64` beside the existing `process_vm_handle`. Zero
new idioms; the packing, the generation check and the "packed 0 means none" convention
all already exist.

---

## What it buys, and what it does not

**Buys — each independently verifiable:**

- `pick_pid_slot_locked`'s kernel-descriptor-table fallback (`fs/src/fileio/mod.rs:425`)
  stops being reachable by construction rather than by a returned `None`. A slot is
  found by generation-checked handle or not at all.
  (`plans/kernel-hardening.md` item 2 makes it `None` first, because that isolation
  break should not wait for this plan.)
- The recycled-id hazard collapses. A stale handle fails the generation check instead of
  resolving to a stranger — the argument mm has already written for its own handle
  (`mm/src/process_vm.rs:39-47`), applied to the identity itself.
- `slot_for_pid`'s O(256) lock-free scan on every descriptor operation
  (`fs/src/fileio/mod.rs:347`) becomes a slot index.
- `PROCESS_FD_TABLE_TEARDOWN` (`mm/src/process_vm.rs:358-376`) is deleted. The Process
  drop lives in `sched`, which already depends on `fs`, so mm no longer needs a
  runtime-installed hook to reach it.
- `process_has_other_live_tasks` (`sched/src/task/task_lifecycle.rs:265`) — an
  O(`MAX_TASKS`) registry walk under the task-manager lock, on every exit — becomes a
  `task_count` read.

**Does not buy: a single `Drop` that tears everything down.** `Process::drop` releases
the slot and zeroes the account row. Nothing else. Address-space and descriptor-table
teardown stay behind `exit_cleanup_mark` plus the `on_cpu` bail
(`sched/src/task/task_lifecycle.rs:1078-1080`), because that ordering is load-bearing in
two ways a refcount cannot express:

- `switch_to_kernel_address_space()` runs immediately before
  `cleanup_current_task_after_switch` (`sched/src/scheduler.rs:1333-1334`). The address
  space may only be destroyed after CR3 has moved off it. A refcount reaching zero
  carries no CR3 ordering.
- `destroy_process_vm` holds the `PROCESS_VMS` cli-lock across
  `flush_all_for_process` → `wait_for_acks` (`mm/src/tlb.rs:1129`), and a cli-lock held
  across a path that re-enables interrupts already deadlocked this tree against a peer
  panic once. A `Drop` invoked from an arbitrary last-reference release has no way to
  refuse that context.

The refcount replaces only the **decision** — "is this the last task of this process" —
not the teardown. A fourth latch bit `TASK_EXIT_CLEANUP_CHARGES` joins the three at
`slopos-ostd/src/task/ops.rs:24-29`.

**Promote, do not reinvent.** `ProcessResourceLease`
(`sched/src/task/task_lifecycle.rs:67`) is already an RAII lease over the pid, the
address space and the descriptor table, with `create_user_process`,
`clone_from_parent`, `disarm` and a `Drop`. It gains a `KArc<Process>` and a name; its
existing structure is the design.

---

## Phases

Each lands green on its own. `cargo fmt --all`, then
`just build && just _iso-tests && just test` per commit.

### Phase 1 — the object, with no consumers

`Process`, `AccountId`, the `Handle<Process>` packing, and the account arena
(`plans/resource-accounting.md` owns the arena's semantics; this phase only places it).
Pure safe Rust, no other crate changed, so the build is green by construction and
`tcb_ratio` falls (safe LoC grows the denominator; the numerator is untouched).

Host tests go in `slopos-ostd/tests/`, instantiating a **private** table rather than the
global root — a process-global counter assertion flakes under `cargo test`'s parallel
threads, which is a live known issue in this tree. Host tests do not move
`TEST_COUNT_BASELINE`.

### Phase 2 — `sched` adopts it

`ProcessResourceLease` gains its `KArc<Process>`; `Task` gains `process_handle`. The old
pid and the new handle coexist and nothing reads the handle yet, so this phase is
observably a no-op. Add the `TASK_EXIT_CLEANUP_CHARGES` latch bit.

The arena's generation counter must survive `init_task_manager`'s test-scope reset
(`sched/src/task/task_table.rs:555-590`, which force-reaps between scopes) for the
reason `VmSlotAlloc::reset` preserves `next_generation`
(`mm/src/process_vm.rs:180-193`): generations are the only thing separating a handle
minted before the reset from the slot's next occupant.

### Phase 3 — re-key mm, then fs, then delete the hook

- **3a.** `PROCESS_VMS`' lookup key becomes `Handle<Process>`; `process_id` is demoted to
  display-only.
- **3b.** `slot_for_pid` becomes `slot_for_process`. The lock-free scan becomes a slot
  index; the `INVALID_PROCESS_ID → KERNEL_TABLE` mapping (`fs/src/fileio/mod.rs:347-350`)
  becomes an explicit kernel-process handle rather than a sentinel comparison.
- **3c.** Delete `PROCESS_FD_TABLE_TEARDOWN`. `process_has_other_live_tasks` becomes a
  refcount read.

Both tables stay fixed arrays through this plan. Their sizing — boot-sized from measured
RAM rather than a compile-time 256 — is `plans/resource-accounting.md`'s capacity rule,
and it is easier after 3b than before, because a generation-checked slot index does not
depend on the never-reallocating-spine contract that the lock-free scan does.

### Phase 4 — the pid table

Weak-only entries, occupancy-checked allocation, zombie entries retained until reap, and
the FIFO reuse ring (`mm/src/process_vm.rs:126-135`) deleted — it exists only to soften
a hazard the handle removes.

This is a prerequisite for `plans/authority-model.md`'s `kill` authorization *and* for
`prlimit64`, both of which name a target by pid and are therefore new confused-deputy
surfaces until the designator is sound.

**Test:** a recycled id does not resolve to the prior principal.

---

## Verification

- Phase 1 changes no behaviour, so its gate is the build plus its own host tests.
- Phases 2–4 each keep `just test` green; Phase 3 is where a regression would appear, so
  run it per sub-phase rather than per phase.
- `just check-framekernel` after Phase 1 (a new crate-root or a new registry section
  would be caught there) and after Phase 3c.
- One `BOOT_CMDLINE='roulette=skip' just boot-log` after 3b, because the descriptor-table
  re-key touches every open path and no `tests=on` boot reaches the desktop.
- `just check-lockdep-headroom` after Phase 3: the account arena's creation path changes
  lock nesting, and class counts are deterministic so the cap is exact. Re-measure over
  several runs, not one, if edges or chains move.
