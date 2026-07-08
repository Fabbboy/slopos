# SlopOS Resource-Lifetime Redesign — Spawn, Index APIs, and Ring — Remaining Work

Scope: the spawn / index-addressed-syscall / ring resource-lifetime surface.
Discipline constraints (CLAUDE.md): all `unsafe` lives in `slopos-ostd`; every other
kernel crate is `#![forbid(unsafe_code)]` and allocates only through `KBox/KVec/KArc/…`;
no function stack frame > 2 KiB; pre-alpha — breaking changes are fine, no external users.

## Baseline (current code)

Single-owner, Drop-driven lifetime is in place for the whole file layer:

- `OpenFile` owns its subsystem object as `KArc<dyn FileBacking>`
  (`abi/src/file_ops.rs`, `fs/src/fileio/mod.rs`); dropping the last
  `KArc<OpenFile>` alias drops the backing, whose `Drop` is the teardown.
  `FileOps` has no `release`/`dup`; every backend (tty, vfs, pipes, sockets,
  memfd, signalfd, pidfd, ring) ships a backing type.
- TTY/PTY: `KArc<TtyBacking>` (`drivers/src/tty/backing.rs`) is the owning
  handle; master holds slave strongly, slave holds master weakly
  (`try_new_cyclic`); the per-slot `KWeak` registry (`TTY_BACKINGS`) serves
  open-by-index; data paths pin peers by upgrading `KWeak` links carried in
  `TtyDriverKind`. Hangup is a latch + state transition, never a free.
- SCM_RIGHTS custody is owned `slopos_fs::FileRef` moves end-to-end
  (`fileio_clone_file_ref` → `AncillaryQueue`/park custodian →
  `fileio_install_file_ref`); a passed fd shares the open-file description.
- `syscall_openpty` returns a real master fd + slave pts number; userland
  `process::openpty()` yields `(OwnedFd, u32)`.

---

## 1. Diagnosis — what is still architecturally wrong

### D2 (residue) — index-addressed I/O syscalls bypass ownership
`syscall_tty_read=146` / `syscall_tty_write=147`
(`core/src/syscall/ui_handlers.rs`; `abi/src/syscall/numbers.rs:76-77`) address a
TTY by raw `TtyIndex` and call `tty::read_cooked` / `tty::write_bytes` with **no
fd, no backing, no ownership check**. A stale or guessed index reads/writes a
PTY the caller never opened. `file_poll_unfused_by_idx`
(`fs/src/fileio/poll.rs`) keeps an index-keyed release path alive alongside the
`KWeak`-backed registrations. `syscall_open_tty_fd=148` opens any TTY by raw
index with no policy.

### D4 — spawn mutates the parent's own fd table; whole-table-clone is the only inheritance
`spawn_program_with_attrs` has exactly one inheritance mode: destroy the child's
bootstrap table, then `fileio_clone_table_for_spawn` (`fs/src/fileio/fdtable.rs`,
called from `core/src/exec/mod.rs`) clones the *entire* parent table (skipping
cloexec). There is no per-fd action list. Consequently every userland spawn site
mutates **its own** fd table around the call: `spawn_shell_on_slave`
(`userland/src/apps/terminal/mod.rs`) dup2's the slave over fd 0/1/2, spawns,
then restores; the shell's pipe-capture spawn does the same dance
(`userland/src/apps/shell/exec.rs`). This is race-prone and non-reentrant, and
it forces the fragile `dup_above_stdio` workaround (`terminal/mod.rs`).

Adjacent, folded into the same stage: **execve never resets `signal_actions`**
(`core/src/exec/mod.rs`, `sched/.../task_cleanup_hooks.rs`) — a stale handler
pointer survives into a new image. Fix as a `POSIX_SPAWN_SETSIGDEF`-equivalent
plus an exec-time reset.

### D5 — the ring holds an fd integer, not a file reference
`ring/src/ring_obj.rs` stores `fd: i32` "re-validated each probe". Userland can
`close()` the fd (or the number can be reused) while an op is in flight;
`distinct_inflight_fds` (`ring/src/enter.rs`) keys on the raw number, so a
close+reuse window aliases two objects under one registration. `owner_pid` and
the protective poll incref are compensating checks for a reference the ring
should simply hold — `slopos_fs::FileRef` now exists for exactly this.

Session/job-control links (foreground pgrp, session) are still raw ids; hold
them as `KWeak` so a terminal never keeps a dead session alive (Asterinas
`JobControl { session: Weak, foreground: Weak }`), folded into the stage that
touches session state next.

---

## 2. Remaining design

### 2.1 Spawn — per-fd action ABI; the child table is an allow-list

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
  sits today (`core/src/exec/mod.rs`), before `task_set_status(Ready)`.
  `CloneFd`/`TransferFd` clone the parent slot's `KArc<OpenFile>` into the child
  (cloexec cleared; honor adddup2 target==src cloexec-clear). `TransferFd` also `take`s
  the parent slot. All-or-nothing: any error tears down the scratch child table
  (existing `task_terminate` error path). A `FDIO_SPAWN_CLONE_STDIO`-style convenience
  expands to three `CloneFd` actions in the userland wrapper.
- **Signal fix (folded in):** `do_exec` resets caught signals to SIG_DFL preserving
  SIG_IGN (`task_cleanup_for_exec`); spawn applies `attrs.sigdefault_mask` in the
  parent-inherit window. Replaces the manual `default_signal(...)` calls in the shell's
  `run_in_child`.
- Session/ctty teardown stays explicit but Drop/hangup-driven; job-control links
  become `KWeak` (`foreground()` = upgrade-or-skip) when this stage touches the
  session structs.

### 2.2 The ring holds a real file reference, not an fd integer
At **submit** time the op resolves the fd once to a `FileRef` and stores it in
the per-op inflight state; the held reference keeps the backing alive for the
op's duration even if userland `close()`s the fd or the slot is reused (the
io_uring "the ring keeps a real struct file reference" rule). Dropped on op
completion/cancel. `owner_pid` and the protective poll incref demote to
defence-in-depth. `distinct_inflight_fds` keys off reference identity
(`KArc::ptr_eq`), not the raw number.

---

## 3. What gets deleted

| Deleted | Location | Subsumed by |
|---|---|---|
| `syscall_tty_read` (146), `syscall_tty_write` (147) | `core/src/syscall/ui_handlers.rs` | fd-based `read`/`write` only |
| `syscall_open_tty_fd` (148) or an ownership-checked replacement | `core/src/syscall/ui_handlers.rs` | fd-based opens (`openpty`, `/dev/pts/N`, `/dev/tty`) |
| `file_poll_unfused_by_idx` | `fs/src/fileio/poll.rs` | `KWeak<OpenFile>` registrations only |
| `fileio_clone_table_for_spawn` | `fs/src/fileio/fdtable.rs`, `exec/mod.rs` | fd-action ABI (§2.1) |
| `dup_above_stdio` userland workaround | `userland/src/apps/terminal/mod.rs` | fd-action ABI atomic install |
| dup2-save/restore dances | `terminal/mod.rs` (`spawn_shell_on_slave`), `shell/exec.rs` | fd-action ABI |

Retired syscall numbers leave gaps (table size unchanged; gaps are expected per
the ID policy). The userland `tty::read`/`tty::write` index wrappers go with
them; callers use fd 0/1/2.

---

## 4. Migration plan (three stages, each independently green under `just test`)

Each stage compiles, passes `just check-framekernel` + `check_stack_sizes.sh`, and is
green under `just test` (target: ≥ the `TEST_COUNT_BASELINE` planned tests in the
justfile; never regress).

### Stage A — spawn fd-action ABI + signal-disposition fix; userland consumers
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

### Stage B — retire index-based I/O APIs
- **Files:** delete `syscall_tty_read`/`syscall_tty_write`
  (`core/src/syscall/ui_handlers.rs`; unregister in `core/src/syscall/handlers.rs`)
  and retire numbers 146/147; decide `syscall_open_tty_fd` (148): delete, or gate
  to consoles the caller may claim; remove the `file_poll_unfused_by_idx` token
  path (`fs/src/fileio/poll.rs`); audit `vconsole`/`switch_active_tty`/
  `set_active_tty` — input routing, not lifetime; they stay (they read a slot,
  never mutate ownership), but assert via test they never touch it.
- **Tests added:** `tty::no_index_io_path` (compile-time: the index read/write syscalls
  no longer exist); `tty::poll_after_close_reuse_no_crossobject` (D2): register a poll,
  close + reuse the fd number, assert no cross-object readiness (the `KWeak` upgrade
  fails).
- **Risk:** low-medium — mostly deletion; grep for in-tree callers of 146/147/148
  before deleting (the kernel `tty::read_cooked`/`write_bytes` stay as ldisc
  internals, only the *syscalls* go).

### Stage C — ring holds real file references
- **Files:** `ring/src/ring_obj.rs` (per-op state holds `FileRef` not `fd: i32`),
  `ring/src/enter.rs` (resolve once at submit via `fileio_clone_file_ref`;
  `distinct_inflight_fds` keys on reference identity; drop on completion/cancel),
  `ring/src/registry.rs` (`owner_pid` → defence-in-depth).
- **Tests added:** `ring::op_survives_fd_close` (close the fd mid-op; op still completes
  against the held backing); `ring::no_reuse_aliasing` (D5): close+reuse the fd number
  between submit and harvest; op targets the original object. Pin a strong-count
  assertion after op completion (no leak).
- **Risk:** medium — must drop the held `FileRef` on *every* terminal path (complete,
  cancel, ring teardown, owner exit) or leak the backing. Co-located reactor tests
  (thread-per-core gap, KNOWN_ISSUES) must stay green. Note: a `FileRef` held
  across a `wait_event` park leaks if the task is killed while blocked (stacks are
  abandoned, not unwound) — mirror the sendmsg park-custodian pattern
  (`net/src/unix_socket/mod.rs`, `SENDMSG_INFLIGHT`) for any parked hold.

---

## 5. Compatibility constraints (what must stay green)

- **≥ `TEST_COUNT_BASELINE` planned tests must stay green every stage**;
  `just check-test-count` guards count regression. New regression tests *raise*
  the baseline — update it deliberately, never lower it.
- **Userland test bins that pin behavior:** `userland/src/bin/tests/fork_test.rs` (fork
  fd inheritance + cloexec keep), `io_capture_test.rs` (fd save/restore lifetime),
  `ring_test.rs`, `signalfd_test.rs`, `pidfd_e2e_test.rs`, `spin_signal_test.rs`,
  `pty_flow_test.rs`, `ctrlc_flood_test.rs` (both consume `openpty() ->
  (OwnedFd, u32)`).
  Kernel-side: `fs/src/tests.rs`, `ring/src/tests.rs`, `sched/src/sched_tests.rs`,
  `drivers/src/tty_tests/*` (e.g. `test_ldisc_signals.rs`).
- **POSIX shared-offset/status-flags semantics:** dup/dup2/fork/SCM_RIGHTS share the
  file offset and `O_NONBLOCK`/`O_APPEND` through the one `KArc<OpenFile>`; fork keeps
  cloexec, exec strips it — all must stay true (`fs::dup_shares_offset` and friends).
- **Framekernel gates every stage:** `check_unsafe_outside_ostd.sh` (the
  fs/drivers/ring/core changes stay forbid-unsafe), `check_alloc_dep.sh` (route through
  `KArc`/`KVec`, never bare `alloc`), `check_stack_sizes.sh` (2 KiB — build backings
  via `KArc::try_init`).
- **SMP "task invisible until initialized" invariant:** Stage A must keep applying fd
  actions while the child is Blocked/unpublished (`exec/mod.rs` TASK_NEW window),
  preserving the Release-store publish at `task_set_status(Ready)`.
- **Lock ordering:** fileio table lock → object/backing lock, table lock dropped before
  any blocking op or backing `Drop` — the detach-then-drop close path enforces this
  structurally; every new holder of a `FileRef`/backing must obey it too (never drop
  one under a subsystem lock — teardown can recurse into that subsystem).
