# SlopOS Resource-Lifetime Redesign — TTY/PTY, Spawn, and Ring — Rip-and-Replace Blueprint

Scope: the TTY / PTY / spawn / ring resource-lifetime surface.
Discipline constraints (CLAUDE.md): all `unsafe` lives in `slopos-ostd`; every other
kernel crate is `#![forbid(unsafe_code)]` and allocates only through `KBox/KVec/KArc/…`;
no function stack frame > 2 KiB; pre-alpha — breaking changes are fine, no external users.

The open-file core is the model to extend: fd slots hold
`FdEntry { open_file: KArc<OpenFile>, cloexec: bool }` (`fs/src/fileio/mod.rs`), the
`KArc` strong count is the dup/alias count, close is detach-then-drop, and poll
registrations hold `KWeak<OpenFile>` (`fs/src/fileio/poll.rs`). `slopos-ostd` provides
`KWeak`, `KArc::downgrade`/`upgrade`, `try_new_cyclic`, and `weak_count`
(`slopos-ostd/src/mm/heap.rs`). TTY/PTY, spawn inheritance, the index-addressed
syscalls, and the ring still sit outside this model; this plan brings them in and
deletes every bypass.

---

## 1. Diagnosis — what is architecturally wrong today

Liveness for a TTY/PTY is tracked by a hand-rolled counter plus a generation bitmap,
joined to fd ownership only by convention and bypassable by index; spawn inheritance
mutates the parent's own fd table; the ring holds a raw fd number. Teardown can
therefore fire early, late, twice, or on the wrong object. Five concrete structural
defects:

### D1 — TTY liveness is a parallel hand-rolled counter, coupled to fd ownership only by a shim
Every TTY carries `open_count: u32` (`drivers/src/tty/mod.rs:124`) mutated by
`tty::open_ref` / `tty::close_ref` (`drivers/src/tty/lifecycle.rs:90,123`). The *only*
bridge to fd ownership is the `FileOps::release` shim run from the `OpenFile` drop.
The invariant "exactly one `open_ref` is owned by exactly one `OpenFile`, kept alive by
N fd aliases" is enforced purely by hand across the open/error call sites — the
"must NOT close_ref again" comments on the tty-open error arms
(`core/src/syscall/ui_handlers.rs:240-242`,
`core/src/syscall/fs/poll_ioctl_handlers.rs:99-106`) are load-bearing: remove one
decrement-balancing convention and `open_count` underflows, `free_pair_if_unused`
collapses the pair, and userland holds fds over freed slots. No type makes the
imbalance unrepresentable. Single ownership (one `KArc`, drop = exactly one close)
makes the double-decrement impossible to express.

### D2 — Index-addressed APIs that bypass ownership entirely
- `syscall_tty_read=146` / `syscall_tty_write=147`
  (`core/src/syscall/ui_handlers.rs:175,204`; `abi/src/syscall/numbers.rs:76-77`)
  address a TTY by raw `TtyIndex` and call `tty::read_cooked` / `tty::write_bytes`
  directly — **no fd table, no `OpenFile`, no refcount, no ownership check**. A stale or
  guessed index reads/writes (and via the ldisc, mutates buffers / raises signals /
  flushes) a PTY the caller never opened and whose `open_count` may be 0.
- `pty::mark_peer_closed` / `free_pair_if_unused` / `queue_packet_event` /
  `clear_peer_closed` / `set_pty_lock` / `set_packet_mode` (`drivers/src/tty/pty.rs`)
  all reach a TTY by raw index and mutate lifecycle/flag state outside any refcount.
  `free_pair_if_unused` frees both slots when both `open_count==0`, gated only by
  `PTY_ALLOC_LOCK` — any premature `close_ref` (D1) immediately collapses the pair.
- `file_poll_unfused_by_idx` (`fs/src/fileio/poll.rs:172`) keeps an index-keyed release
  path alive alongside the `KWeak`-backed registrations.

### D3 — `PtyPeerHandle` + generation bitmap is a hand-rolled reimplementation of Weak
`drivers/src/tty/pty.rs:55-93` defends cross-end misrouting after free/reuse with a
`PtyPeerHandle { idx, generation }` validated against `TTY_GENERATIONS[slot]`
(`drivers/src/tty/table.rs:124`). This is exactly the bookkeeping that
`Weak::upgrade() -> None` gives for free in Asterinas (`PtyMaster { slave: Arc<PtySlave> }`
+ `Tty.weak_self`) and Redox (`controlterm: Rc`, `subterm: Weak`). Hangup is an
imperative flag dance (`mark_peer_closed`, `HUNG_UP`/`PEER_CLOSED` flags, `hangup()` in
`lifecycle.rs:185`) rather than a structural consequence of the owning reference
dropping.

### D4 — Spawn mutates the parent's own fd table; whole-table-clone is the only inheritance
`spawn_program_with_attrs` has exactly one inheritance mode: destroy the child's
bootstrap table, then `fileio_clone_table_for_spawn` (`fs/src/fileio/fdtable.rs:139`,
called from `core/src/exec/mod.rs:178`) clones the *entire* parent table (skipping
cloexec). There is no per-fd action list. Consequently every userland spawn site
mutates **its own** fd table around the call: `spawn_shell_on_slave`
(`userland/src/apps/terminal/mod.rs`) saves fd 0/1/2, dup2's the slave over them,
spawns, then restores; the shell's pipe-capture spawn does the same dance
(`userland/src/apps/shell/exec.rs`). This is race-prone and non-reentrant (the
terminal's *own* stdio is transiently clobbered; an SMP async task in that window sees
the wrong stdio), and it forces the fragile `dup_above_stdio` workaround
(`userland/src/apps/terminal/mod.rs:246`) because the kernel offers no atomic
"open/dup2 into the child without clobbering the parent".

Adjacent, folded into the same stage: **execve never resets `signal_actions`**
(`core/src/exec/mod.rs`, `sched/.../task_cleanup_hooks.rs`) — a stale handler pointer
survives into a new image. Fixed here as a `POSIX_SPAWN_SETSIGDEF`-equivalent and an
exec-time reset, since it lives in the same code window.

### D5 — The ring holds an fd integer, not a file reference
`ring/src/ring_obj.rs:23` stores `fd: i32` "re-validated each probe". Userland can
`close()` the fd (or the number can be reused) while an op is in flight;
`distinct_inflight_fds` (`ring/src/enter.rs:495`) keys on the raw number, so a
close+reuse window aliases two objects under one registration. `owner_pid`
(`ring_obj.rs:73`) and the protective poll incref are compensating checks for a
reference the ring should simply hold.

**Unified statement:** make a single owning reference the *only* liveness fact, let
Rust `Drop` be the *only* teardown trigger, and delete every bypass.

---

## 2. Target architecture — single-owner, Drop-driven lifetime

**One design, chosen decisively:** every TTY/PTY backing object lives behind a
`KArc<TtyBacking>` whose **`Drop` is the teardown**; the standalone `open_count` and the
`PtyPeerHandle` generation bitmap are deleted. Spawn gets a per-fd action ABI so no
caller ever mutates its own table. The ring holds a real `KArc<OpenFile>`. This is the
Asterinas/Redox model (`Arc<dyn FileLike>` / `Arc<LockedFileDescription>`, release =
last-drop) translated to SlopOS's `KArc`, justified below against alternatives.

### 2.1 TTY / PTY lifetime — open_count derived from ownership, hangup as a state transition

**Decision: remove the standalone `open_count` entirely.** The Linux
`tty_struct`/`tty_port` two-object split exists to separate transient per-open-session
state from long-lived hardware state under a manual kref scheme. SlopOS's "hardware" is
virtual (QEMU serial, in-memory PTY ring buffers), so the two-object split's value
collapses to "drop runs once". A `KArc<TtyBacking>` gives that directly. Concretely:

- A TTY slot's backing state moves behind `KArc<TtyBacking>`. Every fd referencing a TTY
  holds it transitively through the `OpenFile`'s backing (the TTY `FileOps` object owns a
  `KArc<TtyBacking>` clone). The `open_count` field is deleted; "is this TTY still open"
  is the absence of any `OpenFile` referencing it — i.e. the backing's `Drop`.
- **`open_ref`/`close_ref` are deleted.** `tty::open_ref` is replaced by "clone the
  `KArc<TtyBacking>` into the new `OpenFile`"; `tty::close_ref` is replaced by "drop the
  `OpenFile`, which drops its `KArc<TtyBacking>` clone." The IRON RULE (Linux: only the
  tty core open/release file_operations may mutate the count) becomes *unbreakable*:
  there is no count to mutate, and no ioctl/ldisc/driver path can clone-or-drop the
  owning `KArc` because they only ever borrow `&TtyBacking`.
- **`FileOps::release` is removed from the trait** (`abi/src/file_ops.rs:80`); teardown
  is the backing object's `Drop`, fired exactly once by `KArc` on last strong drop —
  idempotent by construction (the second drop can never happen). This is the Linux
  `->release` invariant (`fput`→`__fput` on last ref) obtained for free from `KArc`.
  `FileOps::dup` (`abi/src/file_ops.rs:83`, the SCM_RIGHTS path) goes with it: passing
  an fd clones the `KArc<OpenFile>` into the in-flight queue.

- **PTY pair via strong + weak.** The master's backing holds `KArc<PtySlave>` (strong);
  the slave's backing holds `KWeak<PtyMaster>` (weak), built with `try_new_cyclic` so
  the back-link is valid from birth. This breaks the cycle and makes hangup structural:
  - **Master last fd closed** → master backing drops → its `KArc<PtySlave>` drops; the
    slave observes "master gone" via `KWeak::upgrade() == None` on its next read/write
    (→ EOF / EIO), and the slave's wait queues are woken from the master's `Drop` impl
    (set a one-way peer-closed latch, then publish the BUS event). This *is*
    `tty_vhangup(slave)`: slave readers get EOF, writers EIO, session leader gets SIGHUP
    (the SIGHUP/session step stays explicit in the master `Drop`, §2.3).
  - **Slave last fd closed** → slave backing drops; master observes the missing slave
    via a wake flag set from slave `Drop` → master read sees EOF/EIO. Master is **not**
    vhangup'd (Linux rule).
  - `PtyPeerHandle` / `TTY_GENERATIONS` / `validate_peer` / `mark_peer_closed` /
    `clear_peer_closed` / `free_pair_if_unused` are **deleted** — "slot freed and
    reused" cannot misroute because you hold a typed `KWeak`/`KArc`, not an
    index+generation.

- **Hangup = a state transition, not a flag dance.** `hangup` (`lifecycle.rs:185`) is
  rewritten to operate on a `&TtyBacking` (obtained by upgrading/owning the `KArc`),
  flip a single `hung_up: AtomicBool` latch, swap the per-handle behavior to the
  "hung-up" path (read→0, write→EIO, ioctl→EIO — the Linux `hung_up_tty_fops` swap,
  modelled as an enum discriminant or an ops-pointer swap inside `TtyBacking`, read
  under the slot lock), detach the session, send SIGHUP+SIGCONT (§2.3), and publish the
  wakeup BUS events. It frees *nothing* — the fds and `KArc`s stay valid; only behavior
  changes — exactly Linux's "the fd is not closed by hangup; userspace still close()s,
  going through the harmless hung-up path." `is_hung_up`/`tty_hung_up_p` is the
  `hung_up` latch read.

### 2.2 Spawn — per-fd action ABI; the child table is an allow-list

- **ABI (decided signature):** keep `SYSCALL_SPAWN_PATH=64`; pass the action list as a
  UserPtr-to-array (all 6 arg regs are used; this is the `syscall_poll` struct-array
  idiom). Repurpose the unused arg slots / add a sibling `SYSCALL_SPAWN_EX` if 6 regs
  are insufficient (path ptr+len, argv ptr+len, attrs ptr → attrs struct carries
  priority, flags, actions ptr+count, sigdefault mask).

```rust
// abi/src/spawn.rs (new)
#[repr(u32)]
pub enum SpawnFdActionKind { CloneFd = 1, TransferFd = 2, Close = 3, Open = 4 }
#[repr(C)]
pub struct SpawnFdAction {
    pub kind: u32,
    pub src_fd: i32,            // CloneFd/TransferFd
    pub target_fd: i32,        // all except a future bare-Open variant
    pub open_path_ptr: u64,    // Open
    pub open_path_len: u64,    // Open
    pub open_flags: u32,       // Open
    pub open_mode: u32,        // Open
}
#[repr(C)]
pub struct SpawnAttrs {
    pub priority: u8, pub _pad: [u8;3], pub flags: u16, pub _pad2: u16,
    pub actions_ptr: u64, pub actions_len: u64,
    pub sigdefault_mask: u64,   // POSIX_SPAWN_SETSIGDEF-equivalent
}
```

  Kernel signature:
  `spawn_program_with_attrs(path, argv, attrs: &SpawnAttrs, actions: &[SpawnFdAction], parent_task_id) -> Result<u32, ExecError>`.
- **Execution rules (decided):** child starts with an **empty** fd table (allow-list,
  not the deny-list whole-table clone). Actions apply in array order to the child's
  (Blocked, unpublished) table — slotted exactly where `fileio_clone_table_for_spawn`
  sits today (`core/src/exec/mod.rs:178`), before `task_set_status(Ready)`.
  `CloneFd`/`TransferFd` clone the parent slot's `KArc<OpenFile>` into the child
  (cloexec cleared; honor adddup2 target==src cloexec-clear). `TransferFd` also `take`s
  the parent slot. All-or-nothing: any error tears down the scratch child table
  (existing `task_terminate` error path). A `FDIO_SPAWN_CLONE_STDIO`-style convenience
  expands to three `CloneFd` actions in the userland wrapper.
- **Signal fix (folded in):** `do_exec` resets caught signals to SIG_DFL preserving
  SIG_IGN (`task_cleanup_for_exec`); spawn applies `attrs.sigdefault_mask` in the
  parent-inherit window. Replaces the manual `default_signal(...)` calls in the shell's
  `run_in_child`.

### 2.3 Session / ctty teardown stays explicit (but driven from Drop/hangup)
SIGHUP is always paired with SIGCONT; on hangup and on session-leader exit, the tty is
cleared from every session member and `session`/`pgrp` links nulled
(`clear_session_controlling_tty`). Job-control links (foreground pgrp, session) are held
as `KWeak` so a terminal never keeps a dead session alive (Asterinas
`JobControl { session: Weak, foreground: Weak }`); `foreground()` becomes
`upgrade()-or-skip`. This is wired into the master `Drop` (PTY hangup) and into process
exit (`disassociate_ctty`-equivalent).

### 2.4 The ring holds a real file reference, not an fd integer
At **submit** time the op resolves the fd once to its `KArc<OpenFile>` and stores a
clone in the per-op inflight state; the held `KArc` keeps the backing alive for the
op's duration even if userland `close()`s the fd or the slot is reused (the io_uring
"the ring keeps a real struct file reference" rule). Dropped on op completion/cancel.
`owner_pid` and the protective poll incref demote to defence-in-depth.
`distinct_inflight_fds` keys off the held `KArc` identity, not the raw number,
eliminating the close+reuse cross-object poll registration (D5).

### Alternatives considered and rejected
- **Linux biased `file_ref` counter (stored = count−1).** Rejected: it exists only to
  make inc/put a single CAS-free atomic on a hot path and to harden RCU slab reuse
  (`SLAB_TYPESAFE_BY_RCU`). SlopOS has no RCU and uses `IrqMutex` everywhere; `KArc`
  gives the identical "last drop runs destructor" guarantee with zero new hand-rolled
  atomics.
- **Linux two-object tty_struct + tty_port kref split.** Rejected as overkill: the
  split buys a transient/long-lived separation justified by real hardware +
  atomic-context last-put. SlopOS's backing is virtual; a single `KArc<TtyBacking>`
  whose Drop runs once covers it. (Revisit only if a real hardware UART with
  carrier/DTR state appears.)
- **Keep the index APIs but add ownership checks.** Rejected: the index *is* the
  bypass; bolting a check on leaves the count-drift class alive. Delete them (§3).
- **Redox `(scheme, number)` userspace-daemon handle.** Rejected: microkernel-specific;
  SlopOS resources live in-kernel. Take only `Arc::try_unwrap`-on-last-close
  (= `KArc` Drop).
- **Deferred task-work / workqueue release (Linux `__fput_deferred`,
  `queue_release_one_tty`).** Rejected for now: needed only because Linux can hit
  last-put from IRQ/atomic context. SlopOS runs the destructor inline after dropping
  the table lock; revisit if a concrete IRQ-context-drop case appears.

---

## 3. What gets deleted

Functions / fields removed outright (their behavior subsumed by `KArc`/`KWeak` + `Drop`):

| Deleted | Location | Subsumed by |
|---|---|---|
| `tty::open_ref`, `tty::close_ref` | `drivers/src/tty/lifecycle.rs:90,123` | clone/drop of `KArc<TtyBacking>` |
| `open_count` field | `drivers/src/tty/mod.rs:124` | strong-count / backing Drop |
| `PtyPeerHandle`, `validate_peer` | `drivers/src/tty/pty.rs:55-93` | `KWeak`/`KArc` typed peer ref |
| `TTY_GENERATIONS` + generation bump on free | `drivers/src/tty/table.rs:124` | `KWeak::upgrade()==None` |
| `mark_peer_closed`, `clear_peer_closed`, `free_pair_if_unused` | `drivers/src/tty/pty.rs` | master/slave `Drop` impls |
| `syscall_tty_read` (146), `syscall_tty_write` (147) | `core/src/syscall/ui_handlers.rs:175,204` | fd-based `read`/`write` only |
| `file_poll_unfused_by_idx` | `fs/src/fileio/poll.rs:172` | `KWeak<OpenFile>` registrations only |
| `FileOps::release` trait method | `abi/src/file_ops.rs:80` | backing object's `Drop` |
| `FileOps::dup` (SCM_RIGHTS path) | `abi/src/file_ops.rs:83` | `KArc<OpenFile>` clone into in-flight queue |
| `fileio_clone_table_for_spawn` | `fs/src/fileio/fdtable.rs:139`, `exec/mod.rs:178` | fd-action ABI (§2.2) |
| `dup_above_stdio` userland workaround | `userland/src/apps/terminal/mod.rs:246` | fd-action ABI atomic install |
| dup2-save/restore dances | `terminal/mod.rs` (`spawn_shell_on_slave`), `shell/exec.rs` | fd-action ABI |

`SYSCALL_TTY_READ=146` / `SYSCALL_TTY_WRITE=147` numbers are retired (table size
unchanged; gaps are expected per the ID policy). The userland `tty::read`/`tty::write`
wrappers that target an index are removed; callers go through fd 0/1/2.

---

## 4. Migration plan (four stages, each independently green under `just test`)

Each stage compiles, passes `just check-framekernel` + `check_stack_sizes.sh`, and is
green under `just test` (target: ≥ the `TEST_COUNT_BASELINE` planned tests in the
justfile; never regress).

### Stage 1 — TTY/PTY open_count → ownership; hangup as state transition
- **Files:** `drivers/src/tty/lifecycle.rs` (delete open_ref/close_ref/open_count;
  rewrite `hangup` to flip a `hung_up` latch + ops-swap, no free),
  `drivers/src/tty/pty.rs` (master `KArc<PtySlave>` strong, slave `KWeak<PtyMaster>`
  weak via `try_new_cyclic`; move `mark_peer_closed`/`free_pair_if_unused` bodies into
  `Drop` impls; delete `PtyPeerHandle`/generations), `drivers/src/tty_file_ops.rs` +
  `fs/src/fileio/mod.rs` (TTY backing `Drop` replaces the `release`→`close_ref` shim),
  `fs/src/fileio/fdtable.rs` (bootstrap console fds clone the console
  `KArc<TtyBacking>` instead of 3× `open_ref`), then remove `FileOps::release`/`dup`
  from `abi/src/file_ops.rs` once the last backend is Drop-driven.
- **Tests added:**
  - `tty::ioctl_never_changes_open_state` (Linux IRON RULE / check_tty_count): run every
    ioctl (TIOCGWINSZ/TCGETS/TCSETS/TIOCEXCL…) and assert backing strong-count unchanged.
  - `tty::pty_master_close_hangs_up_slave` (D3): close last master fd → slave read EOF,
    write EIO, slave wait queues woken, session SIGHUP'd.
  - `tty::pty_slave_close_marks_master` (D3): slave close → master EOF/EIO, master not
    hung.
  - `tty::scm_rights_tty_balanced` (D1): pass a tty fd via SCM_RIGHTS, assert one extra
    strong ref, balanced on receiver close (replaces the divergent `ops.dup` path).
  - `fs::open_tty_fd_emfile_no_double_teardown` (D1): force `EMFILE` on a tty open;
    assert the backing's teardown fires exactly once.
- **Risk:** high — the PTY pair Drop ordering is the trickiest part (master Drop must
  wake the slave *before* the slave can observe a dangling state). Use `try_new_cyclic`
  so the weak back-link is valid from birth. Pin terminal startup as a regression: it
  must NOT trigger a master close before the event loop.

### Stage 2 — spawn fd-action ABI + signal-disposition fix; userland consumers
- **Files:** `abi/src/spawn.rs` (new), `abi/src/syscall/numbers.rs`,
  `core/src/syscall/process_handlers.rs` (copy_from_user the action array),
  `core/src/exec/mod.rs` (apply actions; reset signals),
  `userland/src/syscall/process.rs` (new `spawn_path_with_actions` + stdio convenience),
  `userland/src/apps/terminal/mod.rs` (replace the `spawn_shell_on_slave` dance with
  `[CloneFd slave→0/1/2]`; delete `dup_above_stdio`),
  `userland/src/apps/shell/exec.rs` (replace pipe-capture and redirect dances).
- **Tests added:**
  - `spawn::empty_table_unless_actions` (D4): spawned child with no actions has no fds.
  - `spawn::clone_fd_shares_backing` / `spawn::transfer_fd_moves` (D4): strong-count and
    parent-slot assertions.
  - `spawn::actions_all_or_nothing` (D4): a mid-list error leaves no partial child.
  - `exec::execve_resets_caught_signals_keeps_ignored`.
  - userland `fork_test`/terminal/shell stay green (the dance removal must be behavior-
    preserving).
- **Risk:** medium — ABI churn. The empty-default-table change is the biggest
  behavioral shift; gate it behind the convenience wrapper so existing callers keep
  stdio.

### Stage 3 — retire index-based mutation APIs
- **Files:** delete `syscall_tty_read`/`syscall_tty_write`
  (`core/src/syscall/ui_handlers.rs:175,204`; unregister in
  `core/src/syscall/handlers.rs`) and retire numbers 146/147; remove the `pty.rs` index
  mutators folded into Drop in Stage 1; remove the `file_poll_unfused_by_idx` token
  path (`fs/src/fileio/poll.rs:172`); audit `vconsole`/`switch_active_tty`/
  `set_active_tty` (`lifecycle.rs`) — these are *input routing*, not lifetime, and stay
  (they read a slot, never mutate ownership), but assert via test they never touch it.
- **Tests added:** `tty::no_index_io_path` (compile-time: the index read/write syscalls
  no longer exist); `tty::poll_after_close_reuse_no_crossobject` (D2): register a poll,
  close + reuse the fd number, assert no cross-object readiness (the `KWeak` upgrade
  fails).
- **Risk:** low-medium — mostly deletion; grep for in-tree callers of 146/147 before
  deleting (the kernel `tty::read_cooked`/`write_bytes` stay as ldisc internals, only
  the *syscalls* go).

### Stage 4 — ring holds real file references
- **Files:** `ring/src/ring_obj.rs` (per-op state holds `KArc<OpenFile>` not `fd: i32`),
  `ring/src/enter.rs` (resolve once at submit; `distinct_inflight_fds` keys on `KArc`
  identity; drop on completion/cancel), `ring/src/registry.rs` (`owner_pid` →
  defence-in-depth).
- **Tests added:** `ring::op_survives_fd_close` (close the fd mid-op; op still completes
  against the held backing); `ring::no_reuse_aliasing` (D5): close+reuse the fd number
  between submit and harvest; op targets the original object. Pin a strong-count
  assertion after op completion (no leak).
- **Risk:** medium — must drop the held `KArc` on *every* terminal path (complete,
  cancel, ring teardown, owner exit) or leak the backing. Co-located reactor tests
  (thread-per-core gap, KNOWN_ISSUES) must stay green.

---

## 5. Compatibility constraints (what must stay green)

- **≥ `TEST_COUNT_BASELINE` planned tests must stay green every stage**;
  `just check-test-count` guards count regression. New regression tests (above) *raise*
  the baseline — update it deliberately, never lower it.
- **Userland test bins that pin behavior:** `userland/src/bin/tests/fork_test.rs` (fork
  fd inheritance + cloexec keep), `io_capture_test.rs` (fd save/restore lifetime),
  `ring_test.rs`, `signalfd_test.rs`, `pidfd_e2e_test.rs`, `spin_signal_test.rs`.
  Kernel-side: `fs/src/tests.rs`, `ring/src/tests.rs`, `sched/src/sched_tests.rs`,
  `drivers/src/tty_tests/*` (e.g. `test_ldisc_signals.rs`).
- **POSIX shared-offset/status-flags semantics:** dup/dup2/fork share the file offset
  and `O_NONBLOCK`/`O_APPEND` through the one `KArc<OpenFile>`; fork keeps cloexec,
  exec strips it — all must stay true (`fs::dup_shares_offset` and friends).
- **Framekernel gates every stage:** `check_unsafe_outside_ostd.sh` (the
  fs/drivers/ring/core changes stay forbid-unsafe), `check_alloc_dep.sh` (route through
  `KArc`/`KVec`, never bare `alloc`), `check_stack_sizes.sh` (2 KiB — build backings
  via `KArc::try_init`).
- **SMP "task invisible until initialized" invariant:** Stage 2 must keep applying fd
  actions while the child is Blocked/unpublished (`exec/mod.rs` TASK_NEW window),
  preserving the Release-store publish at `task_set_status(Ready)`.
- **Lock ordering:** fileio table lock → object/backing lock, table lock dropped before
  any blocking op or backing `Drop` — the detach-then-drop close path enforces this
  structurally; Stage 1's TTY `Drop` bodies must obey it too (never run a backing
  `Drop` under the table lock).
