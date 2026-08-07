# Kernel hardening: the fixes that need no framework

Twelve defects reachable from an unprivileged process today. None needs a principal,
a credential, a `Process` object or an accounting framework — each is a local fix with
a local test, and every one of them is currently load-bearing for something else:
`plans/authority-model.md`'s program-identity grant is void until item 1 lands, and
`plans/resource-accounting.md` cannot measure a peak until items 2 and 3 stop the
tables lying about their capacity.

Land this before either framework plan. Each item is an independent commit; strip it
from this file as it lands.

The suite has **one** drive-a-registry-to-full test (`net/src/tests/tcp_tests.rs:471`)
and **zero** cross-process denial tests, which is why none of this is visible today.
Every item below names the test that makes it visible. Bump
`TEST_COUNT_BASELINE` (`scripts/check_test_count.sh`) in the same commit that adds
tests, measured with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh` — never
guessed.

---

## 1. `/bin` is writable, so program identity grants nothing

`core/src/exec/grants.rs` keys privilege on the binary's path and says so plainly:
"only as strong as write protection on `/bin`, and SlopOS has no file permissions."
That is not a future risk. It is live:

- `unpack_cpio_into_root` (`fs/src/cpio.rs:187`, called from
  `boot/src/boot_services.rs:87`) unpacks the archive **into** the root filesystem via
  `write_file` (`fs/src/cpio.rs:286`) with `create`, `truncate` and `writable` all set.
  The cpio is not retained as a read-only image.
- The root is `RamFs` or ext2 (`fs/src/vfs/init.rs:23,25`), and neither `write`,
  `create`, `truncate`, `unlink` nor `rename` consults any permission.
- `syscall_fs_unlink` (`core/src/syscall/fs/path_handlers.rs:97`) carries no `requires`
  clause at all; `syscall_fs_open` carries only `requires(let pid: process_id)`.
- `do_exec`'s only integrity gate is `(file_stat.mode & 0o111) == 0 → ExecError::NoExec`
  (`core/src/exec/mod.rs:455-457`), and the execute bit survives a rewrite.

So any process overwrites `/bin/compositor`, spawns it, and the kernel hands the
replacement `TASK_FLAG_COMPOSITOR` and `TaskPriority::High`.

**Fix.** A per-inode `sealed` bit, set by `unpack_cpio_into_root` on every file it
creates, refused by `write`, `create`, `truncate`, `unlink` and `rename`. One flag,
five refusals. Cover **both** root filesystems (`fs/src/vfs/init.rs:23` mounts ext2
when initialised, `:25` mounts `RamFs`) or the test ISO diverges from the shipped one
on exactly the property the grant key depends on.

This is phase one of program identity. Phase two — keying the grant on a measured
image hash rather than a path — belongs with persistent storage, because a hash is only
worth computing once the path can change under a running kernel.

**Repro.** `open("/bin/compositor", O_WRONLY|O_TRUNC)` from the shell, write any ELF
with mode `0755`, `spawn_path("/bin/compositor")`, observe `TASK_FLAG_COMPOSITOR`.

**Tests.** `stest!` that a sealed inode refuses `write`/`truncate`/`unlink`; `stest!`
that a `/tmp` inode still accepts all three (the seal must not be global).

---

## 2. A userland process's descriptor table can become the kernel's

`pick_pid_slot_locked` (`fs/src/fileio/mod.rs:401`) walks `PROCESS_TABLES` for a free
slot and, failing, returns `Some(KERNEL_TABLE.inner.lock())` (`:425`) — installing a
userland process's descriptors into the kernel's own table, shared with every other
process that also fell back. Three call sites reach it: `install_fd_entry`
(`fs/src/fileio/fdops.rs:70`), `file_pipe_create` (`:613`) and `fileio_install_file_ref`
(`:950`, the `SCM_RIGHTS` receive path).

`MAX_PROCESSES` is 256 against `MAX_TASKS` 8192, so exhausting the slots is a fork
loop.

**Fix.** Return `None`; the three call sites already have an `EMFILE`/`ENFILE` path.
Raising the capacity instead would *hide* this — a table whose exhaustion path
redirects into a more privileged domain is an isolation bug, not a sizing bug, and the
redirect must go before the resize.

`slot_for_pid`'s lookup (`fs/src/fileio/mod.rs:347-350`) maps `INVALID_PROCESS_ID` to
`KERNEL_TABLE` deliberately and correctly; only the allocation fallback is wrong.

**Tests.** The suite's first cross-process denial test: fill the slots from process A,
assert process B's `open` returns `ENFILE`, assert B never observes an fd A installed.

---

## 3. The capacity numbers are below what this tree's own userland needs

`FILEIO_MAX_OPEN_FILES = 32` (`fs/src/fileio/mod.rs:26`) is the only per-process bound
in the kernel, and the compositor already exceeds a reasonable share of it: three
standard descriptors, a listen fd, a `Ring::setup` fd, a readiness-notifier fd,
framebuffer and clipboard memfds, and up to `MAX_CLIENTS = 32` client fds. `dup2`/`dup3`
reject any target ≥ 32 while `slibc` advertises `FD_SETSIZE = 1024`.

The AF_UNIX numbers are inconsistent three ways: `slop_protocol::server::MAX_CLIENTS`
is 32, `MAX_UNIX_SOCKETS` is 32 (`abi/src/event.rs:19`) and therefore
`MAX_UNIX_PAIRS` is 16 (`net/src/unix_socket/pair.rs:26`). Each connected client takes
two socket slots, so the compositor tops out near fifteen clients system-wide and no
per-process quota moves that.

**Fix.** `FILEIO_MAX_OPEN_FILES` → 256 and `MAX_UNIX_SOCKETS` → at least
`2 * MAX_CLIENTS + 2`. `Option<FdEntry>` is 16 bytes, so 256 descriptors is 4 KiB per
slot and 1 MiB of `.bss` across `MAX_PROCESSES` — still a fixed array, still
lock-free-scannable, no allocation added.

**Before merging, confirm no function copies a `FileTableSlotInner` by value.** The
target sets `"stack-probes": {"kind": "none"}`, so a 4 KiB descriptor array on a frame
steps clean over the 4 KiB guard page in one instruction. Run
`scripts/check_stack_sizes.sh` on **all three variants** — `just check-framekernel-gates`
runs only `--variant dev`.

**Tests.** Open 256 descriptors in one process and assert the 257th is `EMFILE`;
connect `MAX_CLIENTS` AF_UNIX clients and assert none is refused.

---

## 4. A remote peer panics the kernel from softirq under a cli-spinlock

`net/src/tcp/mod.rs:261`, `:428` and `:568` call `.expect("tcp: kernel OOM allocating
connection buffer")` on a 32 KiB × 2 allocation. All three run inside
`with_pcb_and_bufs` — under the `TCP_PCB_SLOTS` cli-spinlock, in softirq context, driven
by an unauthenticated remote peer. Allocation failure is a kernel panic with interrupts
disabled and a lock held.

**Fix.** Propagate the failure as `TcpError::OutOfMemory` (which already maps to
`ENOMEM` via `net/src/tcp/tuple.rs:60`) and drop the segment. Allocate the buffer
*before* taking the lock — the pre-reserve-then-lock pattern is three files away in
`net/src/unix_socket/mod.rs:865`.

**Tests.** `stest!` that a connection whose buffer allocation fails returns an error
rather than panicking (drive it through the existing allocation-failure test hook).

---

## 5. The bounded SYN queue has zero production callers

`TcpListenState::on_syn` (`net/src/tcp/listener.rs:257`) is a complete, tested,
bounded SYN queue with `SYN_QUEUE_MAX`, `SYN_RETRIES_MAX` and real retransmit timers.
Every caller is in `net/src/tests/tcp_socket_tests.rs`. The live path builds an
`AcceptedConn` in `net/src/tcp/pcb/listen.rs:98-105`, and `install_accepted_child`
(`net/src/tcp/mod.rs:173`) installs it straight into the 64-slot shard table,
discarding the result (`:194`).

So ~64 unanswered SYNs from anywhere on the network permanently deny the whole TCP
stack, and the refusal is silent.

**Fix.** Route the live path through `on_syn`. Half-open state then lives in a
per-listener bounded queue, and a connection reaches the shard table only at `accept`.

This is also the precondition for charging TCP connections at all: until half-open
state is bounded per listener, charging a shard slot to the listener's principal turns
a remote SYN flood into a remote exhaustion of that principal's entire budget.

**Also fix here.** Closing a listening socket clears its accept queue
(`net/src/socket.rs:2765-2770`) but never releases the child PCBs already installed in
the shard table; those slots return only via RST, FIN or `TIME_WAIT` expiry.

**Tests.** Drive `SYN_QUEUE_MAX + 1` half-open connections and assert the shard table
is untouched; assert closing a listener releases its established children.

---

## 6. The global input sink is acquired at frame rate and never released

`compositor_task_id` (`drivers/src/input_event.rs:78`) is written only at `:504`, from
`input::register_compositor`, which `syscall_input_poll_batch` calls on **every** call
(`core/src/syscall/ui_handlers.rs:55`). It is read at `:365`, `:366`, `:406`, `:443`
and `:470` to route every key and pointer event.

`input_cleanup_task` (`drivers/src/input_event.rs:597`) clears `keyboard_focus` and
`pointer_focus` and frees the queue slot. It never clears `compositor_task_id`. The
video side does the equivalent correctly (`video/src/lib.rs:120-122`).

So a task that holds `TASK_FLAG_COMPOSITOR`, calls `input_poll_batch` once and exits
takes all keyboard and pointer input with it, permanently, with no way back short of
reboot. Chained with item 7's missing `kill` authorization, that is a ten-line
unprivileged program.

**Fix.** Clear `compositor_task_id` in `input_cleanup_task` when it matches the
departing task, exactly as `video/src/lib.rs:120-122` does. The structural replacement
— a single-holder input seat with arbiter revocation — is `plans/authority-model.md`'s
work; this is the one-line stop-loss and belongs here.

**Tests.** `utest!` that a task registering as the input sink and exiting leaves the
sink clear and a second task can claim it.

---

## 7. `kill` performs no caller-versus-target authorization

`core/src/syscall/signal.rs`'s entire authorization is
`signal_may_name(flags) = (flags & TASK_FLAG_USER_MODE) != 0` (`:82-84`) — a *category*
check, so init and the compositor are named as readily as a sibling. Three arms, worst
first:

- **`pid < -1`** → `collect_targets_for_group(pgid, …)` (`:86`) accepts an arbitrary
  pgid with no session and no membership test. A targeted cross-session kill, cheaper
  and quieter than the broadcast.
- **`pid == -1`** → `collect_targets_for_all` (`:94`) walks every active user task and
  exempts nothing, including init.
- **`pid > 0`** → any live user task.

`syscall_terminate_task` (`core/src/syscall/process_handlers.rs:340`) is a second kill
primitive: `requires(compositor)` and then a self-exclusion, so a compositor-flagged
task terminates init.

The target ids come free from `process_list`, which lives in
`core/src/syscall/core_handlers.rs` — a module with **zero** `requires` clauses across
its thirteen handlers.

**Fix, in this plan.** The relation the tree can already express: same process → allow;
same `ProcessGroup` or same `Session` → allow; otherwise `EPERM`. Never init.
`pid == -1` and `pid < -1` require the relation for every target they collect rather
than for none. `terminate_task` takes the same rule.

The capability that covers the remaining cross-session case (`ProcSignal`), and the
resolve-once `Signalable` handle that removes the pid designation entirely, are
`plans/authority-model.md`'s. The relation check is not — it needs nothing that does
not exist, and shipping without it while the framework lands leaves a trivially
exploitable primitive in the tree.

Why `Session` equality is sound as the relation: `setsid`
(`core/src/syscall/process_handlers.rs:650`) refuses a caller that already leads a group
or session, and `setpgid` (`:604-611`, `:630`) requires parent-or-self **and** same-sid
both ways. A task can therefore leave a session but never join another's.

**Tests.** `utest!` per arm: cross-session `pid > 0` is `EPERM`; `pid < -1` naming a
foreign pgid kills nothing; `pid == -1` spares init; same-group kill still works.

---

## 8. `waitpid` reaps any zombie in the system

`syscall_waitpid` (`core/src/syscall/process_handlers.rs:308`) carries no `requires`
clause beyond existence, and `task_consume_zombie` checks only `status() == Zombie`. So
any task reaps any zombie anywhere, consumes its `ExitInfo`, and the real parent then
gets `ECHILD` — a denial of the exit-status protocol, and a cross-process information
leak of the exit code.

**Fix.** Require the parent relation; `ECHILD` otherwise. The `parent_task_id` field
already exists (`slopos-ostd/src/task/kernel_task.rs`, re-established in
`clone_from_raw`).

**Tests.** `utest!` that a non-parent's `waitpid` on a foreign zombie returns `ECHILD`
and the parent's subsequent `waitpid` still succeeds.

---

## 9. The keyboard layout is rewritable by any task

`syscall_keymap_load` (`core/src/syscall/keymap_handlers.rs:18`) carries no `requires`
clause of any kind, while its sibling `syscall_font_set` carries
`requires(console_admin)`. Its doc justifies being unprivileged on a per-session
premise that the single global layout table does not satisfy, and the input validator
answers integrity, not authority.

**Fix.** `requires(console_admin)` now, matching `font_set`; correct the doc comment's
per-session claim. The `ConsoleConfig` capability that replaces both is in
`plans/authority-model.md`.

**Tests.** `utest!` that an unprivileged `keymap_load` returns `EPERM` and the layout
is unchanged.

---

## 10. `read` and `write` bypass the descriptor layer

`syscall_user_write` (`core/src/syscall/core_handlers.rs:124`) has no `requires` clause
and calls `platform::console_puts` — a direct write to the global kernel console with
no descriptor involved. `syscall_user_read` (`:140`) calls
`tty::read_cooked(TtyIndex(0), …)` — the **hardcoded** TTY 0, not the caller's
controlling terminal, so any task reads the operator's cooked keystrokes.

These read as unprivileged under any naive classification, which is exactly why they
are listed: they are the two slots where "it takes no descriptor, so it needs no
authority" is wrong.

**Fix, in this plan.** Route both through the caller's controlling TTY descriptor,
which `fs/src/fileio/mod.rs:549` already resolves via `current_task_pgrp_handle`. A
task with no controlling terminal gets `ENOTTY`.

**Tests.** `utest!` that a task whose controlling TTY is a PTY reads from that PTY and
not from TTY 0.

---

## 11. Teardown allocates under the descriptor-table cli-lock

`drain_descriptors` (`fs/src/fileio/fdtable.rs:64`) builds its `KVec` **inside** the
slot lock and, on push failure, drops the entry inline — nesting the whole
`FileBacking` teardown chain, including `unix_close` taking `UNIX_STATE`, beneath the
table lock. Allocating under a cli-lock is the shape that produced this tree's
slab-lock-across-LUF-drain deadlock: the allocator is where every subsystem meets.

The correct pattern is three files away — `net/src/unix_socket/mod.rs:865`
pre-reserves before taking the state lock.

**Fix.** `KVec::with_capacity(FILEIO_MAX_OPEN_FILES)` outside the lock; refuse on
reservation failure. The detach-then-drop half is already right (`fdtable.rs:159`).

**Tests.** Covered by the existing teardown tests once the reservation moves; add a
`lockdep=warn` boot capture to the commit message rather than a new test.

---

## 12. Loose ends with no home of their own

- **`POLL_REG_TABLE`** discards its insert failure and still returns a non-zero token.
  `file_poll_unfused_by_token` then finds nothing, `poll_unwait` never runs, and the
  task stays registered on the wait queue — a real leak of the registration. Cap the
  map, propagate the failure, return zero on failure.
- **`SO_RCVBUF`** resize panics, and `RECV_BUF_MAX = 262144` across 64 sockets can
  drain the 256-frame global `PACKET_POOL`. Bound it against the pool.
- **`net/src/unix_socket/mod.rs:272,278`** return the bare literal `-23`.
  `IntoSyscallResult` clamps unrecognised negatives to `EINVAL`, so the caller sees the
  wrong errno. Return `Errno` values.
- **`EventBus::queue_for`** (`slopos-ostd/src/sync/event_bus.rs:82-84`) does
  `% MAX_SOCKETS`. Its comment asserts the modulo is the identity, which holds for
  pipes, TTYs and AF_UNIX but is false for AF_INET: `SlabSocketTable` grows to
  `MAX_CAPACITY = 1024` (`net/src/socket.rs:366`), so sockets 0 and 64 share wait
  queues. Either bound the slab at `MAX_SOCKETS` or give the arm a collision re-check
  like `child_exit` already has.
- **`check_safe_contract_surface.sh`** is the only gate with no `--self-test`, and "a
  check that has never been observed to reject has not been observed to work." Plant
  four cases: a safe `pub fn` with `# Safety` (must fire), an `unsafe fn` with
  `# Safety` (must not), a `# Correctness` section (must not — documents the deliberate
  blind spot), and a blank-line-separated doc block (must not — documents the awk
  reset).
- **`core/src/exec/grants.rs:11-18`** names the compositor's shelf as a third launcher
  of `/bin/roulette`. `userland/src/apps/compositor/dock.rs` contains no spawn call.
  The two real launchers are init (`userland/src/apps/init_process.rs:97`) and the
  shell (`userland/src/apps/shell/exec.rs:540,622,663` over
  `userland/src/program_registry.rs:51-52`). Correct the comment — the launcher set is
  what bounds the `Launch` capability.
- **`slopos-ostd/src/task/drop_context.rs`** says the buddy allocator's reuse path
  "spins on cross-CPU shootdowns". The quiesce epoch removed that:
  `mm/src/mmu/quiesce.rs:8` states "Nothing here ever waits on another CPU", and a free
  during an open epoch is quarantined (`mm/src/page_alloc/buddy.rs`). The real reasons
  are the allocator locks and the dispatch-pinned stack. A stale rationale on a
  load-bearing context check is how the next person reaches the wrong conclusion.

---

## Gate story

Per commit: `cargo fmt --all`, then `just build && just _iso-tests` (every `tests` mod
is `cfg(test-hooks)` and off by default, so `just build` alone does not compile them),
then `just test`. Item 3 additionally runs `scripts/check_stack_sizes.sh` on all three
variants. Item 11 attaches a `lockdep=warn` boot capture.

Items 1, 6, 7, 9 and 10 change behaviour the automated suite cannot see: init runs
`run_userland_tests()` then `exit_with_code(0)`
(`userland/src/apps/init_process.rs:79-92`), and the roulette, compositor, shell and
terminal spawns are all *below* that exit — so no `tests=on` boot ever reaches the
desktop. Confirm each with `BOOT_CMDLINE='roulette=skip' just boot-log` and quote the
transcript in the commit message.
