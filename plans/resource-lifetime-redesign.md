# SlopOS Resource-Lifetime Redesign — Remaining Work

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
whole-table clone (`abi/src/spawn.rs`, `spawn_program_with_attrs`,
`fs/src/fileio/fdops.rs`); signal disposition is one ostd primitive with three
entry points (`task_reset_caught_handlers`, `task_default_signals_in_mask`,
`SYSCALL_SIGDEFAULT=118`). Index-addressed TTY I/O is retired — TTY access is
fd-only.

The **ring holds a real file reference per in-flight op**, not an fd integer:

- `InFlight` stores `file: Option<FileRef>` (`ring/src/ring_obj.rs`), resolved
  once at submit via `fileio_clone_file_ref` (`ring/src/enter.rs`). The held
  reference keeps the backing alive for the op's whole in-flight window even
  when userland `close()`s the fd or the number is reused. A single ref-based
  I/O path (`file_read_ref_nonblock` / `file_write_ref_nonblock` /
  `file_poll_ref` / `file_poll_fused_ref` / `fileio_handle_and_ops_from_ref`)
  drives both the submit-probe and the harvest-reprobe (`ring/src/opcode.rs`,
  `ring/src/net_glue.rs`); waiter registration and dedup key on open-file
  identity (`FileRef::ptr_eq`), not the fd number. `owner_pid` and the weak
  poll registration are defence-in-depth.
- Every terminal path (complete / cancel / timeout / multishot / ring
  teardown) detaches the row under the per-ring lock and drops its `FileRef`
  **after** releasing it: `Ring.pending_reap` collects retired rows, drained
  off-lock by `ring_enter`/`harvest`; `registry::remove` drops the removed
  `Ring` outside the registry spinlock. A completing op's file may be its
  backing's last alias, and that teardown must never run under a subsystem
  lock.

---

## 1. Diagnosis — what is still architecturally wrong

### Session/job-control links are raw ids
Session/job-control links (foreground pgrp, session) are still raw ids; hold
them as `KWeak` so a terminal never keeps a dead session alive (Asterinas
`JobControl { session: Weak, foreground: Weak }`), folded into the stage that
touches session state next.

---

## 2. Compatibility constraints (what must stay green)

- **≥ `TEST_COUNT_BASELINE` planned tests must stay green** (currently 2664;
  `scripts/check_test_count.sh`); `just check-test-count` guards count
  regression. New regression tests *raise* the baseline — never lower it.
- **POSIX shared-offset/status-flags semantics:** dup/dup2/fork/SCM_RIGHTS share
  the file offset and `O_NONBLOCK`/`O_APPEND` through the one `KArc<OpenFile>`;
  fork keeps cloexec, exec strips it (`fs::dup_shares_offset` and friends).
- **Framekernel gates every stage:** `check_unsafe_outside_ostd.sh`,
  `check_alloc_dep.sh` (route through `KArc`/`KVec`), `check_stack_sizes.sh`
  (2 KiB).
- **Lock ordering:** fileio table lock → object/backing lock, table lock dropped
  before any blocking op or backing `Drop`; every holder of a `FileRef`/backing
  must obey it too (never drop one under a subsystem lock — teardown can recurse
  into that subsystem). Parked holds that a killed task would leak (stacks are
  abandoned, not unwound) need the sendmsg park-custodian pattern
  (`net/src/unix_socket/mod.rs`, `SENDMSG_INFLIGHT`).
