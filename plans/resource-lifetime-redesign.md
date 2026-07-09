# SlopOS Resource-Lifetime Redesign — Index APIs and Ring — Remaining Work

Scope: the index-addressed-syscall / ring resource-lifetime surface.
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

Spawn is a per-fd **action allow-list** (posix_spawn file-actions), not
whole-table clone:

- `SYSCALL_SPAWN_PATH=64` takes `path`, `argv`, and a `SpawnAttrs` pointer
  (`abi/src/spawn.rs`: `SpawnFdAction{CloneFd,TransferFd,Close,Open}`,
  `SpawnAttrs{priority,flags,actions,sigdefault_mask}`, offset-asserted).
  `spawn_program_with_attrs` decodes actions to an owned `FdAction` and, when
  `parent_process_id != INVALID_PROCESS_ID`, rebuilds the child from an empty
  table (`fileio_create_empty_table_for_process` + `apply_fd_actions`, all in
  the Blocked/unpublished window; all-or-nothing via `task_terminate`).
  `launch_init` (no parent) keeps its console bootstrap. `fileio_clone_table_for_spawn`
  is gone; primitives `fileio_install_file_ref_at` / `fileio_take_file_ref` /
  `fileio_open_at_fd` back the actions (`fs/src/fileio/fdops.rs`).
- Userland `spawn_path`/`spawn_path_with_attrs` inject clone-stdio; terminal
  (`spawn_shell_on_slave`) and shell (`execute_registry_spawn`) pass explicit
  actions — the dup2 save/restore dances and `dup_above_stdio` are gone.
  **fd inheritance is now an explicit action:** init clones the readiness
  notifier (fd 3, `userland/src/readiness.rs`) into the compositor via a
  `CloneFd` action (`spawn_service_inheriting`) — a plain clone-stdio spawn
  would EOF init's readiness gate immediately and race the terminal ahead of a
  ready compositor.
- Signal disposition: one ostd primitive, three entry points —
  `task_reset_caught_handlers` (execve resets caught handlers to SIG_DFL,
  preserving SIG_IGN and the blocked/pending state) and
  `task_default_signals_in_mask` (spawn `SpawnAttrs.sigdefault_mask` and the new
  `SYSCALL_SIGDEFAULT=118` batch-reset). The shell's `run_in_child` uses one
  `process::sigdefault(...)` call for job-control defaults instead of four
  `default_signal` calls (kept because execve preserves SIG_IGN and an in-child
  builtin never execs).

Index-addressed TTY I/O is retired: `syscall_tty_read` / `syscall_tty_write` /
`syscall_open_tty_fd` (146-148) and their userland wrappers are gone — TTY access
is fd-only (`read`/`write` on fds 0/1/2, `openpty`, `/dev/pts/N`, `/dev/ptmx`,
`/dev/tty`). The poll wait-queue release path is opaque-token → `KWeak<OpenFile>`
only (`file_poll_unfused_by_token`, backed by `POLL_REG_TABLE`, with a task-death
leak-guard); the dead fd-resnapshot release variant is removed. `read_cooked` /
`write_bytes` stay as ldisc internals behind the fd path.

---

## 1. Diagnosis — what is still architecturally wrong

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

### 2.1 The ring holds a real file reference, not an fd integer
At **submit** time the op resolves the fd once to a `FileRef` and stores it in
the per-op inflight state; the held reference keeps the backing alive for the
op's duration even if userland `close()`s the fd or the slot is reused (the
io_uring "the ring keeps a real struct file reference" rule). Dropped on op
completion/cancel. `owner_pid` and the protective poll incref demote to
defence-in-depth. `distinct_inflight_fds` keys off reference identity
(`KArc::ptr_eq`), not the raw number.

---

## 4. Migration plan (each stage independently green under `just test`)

Each stage compiles, passes `just check-framekernel` + `check_stack_sizes.sh`, and is
green under `just test` (target: ≥ the `TEST_COUNT_BASELINE` planned tests in the
justfile; never regress).

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

- **≥ `TEST_COUNT_BASELINE` planned tests must stay green every stage**
  (currently 2662; `scripts/check_test_count.sh`); `just check-test-count` guards
  count regression. New regression tests *raise* the baseline — update it
  deliberately, never lower it.
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
- **Lock ordering:** fileio table lock → object/backing lock, table lock dropped before
  any blocking op or backing `Drop` — the detach-then-drop close path enforces this
  structurally; every new holder of a `FileRef`/backing must obey it too (never drop
  one under a subsystem lock — teardown can recurse into that subsystem).
