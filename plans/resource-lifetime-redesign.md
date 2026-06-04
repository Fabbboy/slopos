# SlopOS Resource-Lifetime Redesign — Rip-and-Replace Blueprint

Status: architecture decision record + multi-week implementation campaign plan.
Scope: the fd / open-file / TTY / PTY / ring / spawn resource-lifetime surface.
Discipline constraints (CLAUDE.md): all `unsafe` lives in `slopos-ostd`; every other
kernel crate is `#![forbid(unsafe_code)]` and allocates only through `KBox/KVec/KArc/…`;
no function stack frame > 2 KiB; pre-alpha — breaking changes are fine, no external users.

---

## 1. Diagnosis — what is architecturally wrong today

The current design tracks the lifetime of one logical resource (a TTY/PTY/socket/pipe
backing object) with **two independent, hand-rolled counters joined only by convention**,
and then offers **several APIs that bypass both counters entirely**. The result is a
class of refcount-drift / premature-teardown / use-after-reuse bugs that the CVSS loop
keeps re-discovering. Five concrete structural defects:

### D1 — Dual hand-rolled refcounts with no type-level coupling
`OpenFile.refcount` is a bare `u16` (`fs/src/fileio/mod.rs:139`), bumped by
`incref_open_file` and dropped by `release_open_file`
(`fs/src/fileio/open_file_table.rs:52-82`). Separately, every TTY carries
`open_count: u32` mutated by `tty::open_ref` / `tty::close_ref`
(`drivers/src/tty/lifecycle.rs:90,129`). The *only* bridge between them is a single line:
on the last `OpenFile` drop, `release_open_file` calls `ops.release(handle)` →
`tty::close_ref` (`open_file_table.rs:77-81`, `drivers/src/tty_file_ops.rs:124-128`).
The invariant "exactly one `tty::open_ref` is owned by exactly one `OpenFile` entry, kept
alive by N fd aliases" is enforced purely by hand across ~6 call sites. Any path that
creates an `OpenFile` over a TTY handle without a matching `open_ref`, or releases an entry
whose `open_ref` it never owned, silently desyncs the two counters. There is no type that
makes the imbalance unrepresentable.

### D2 — Double `close_ref` on the TTY-open error path (the proven premature-close root cause)
`install_fd_entry` (`fs/src/fileio/fdops.rs:84-88`) runs `ops.release(handle)` on its
**error** arm (EMFILE/ENFILE/ENXIO). Both "open a tty fd" syscall paths —
`syscall_open_tty_fd` (`core/src/syscall/ui_handlers.rs:216-225`) and `TIOCGPTPEER`
(`core/src/syscall/fs/poll_ioctl_handlers.rs:99-106`) — take the owning `tty::open_ref`
in the **caller** and *also* run an explicit `tty::close_ref` on their own `fd < 0` error
arm. When `file_open_tty_fd` fails at `find_free_slot`/`alloc_open_file_entry`,
`install_fd_entry`'s `ops.release` fires `close_ref` **and** the syscall's error arm fires
`close_ref` again: **two decrements against one `open_ref`**. For a PTY master this drives
`open_count` 1→-1(saturated to 0) and triggers `free_pair_if_unused`, collapsing the whole
pair while userland still holds a live master fd. This is the exact `close_ref idx=2 -> 0
-> "pty master 2 closed -> hangup slave"` cascade observed in `test_output.log:231-242`:
**the audit reduced the premature-close to this double-decrement asymmetry plus the
release-without-owning-ref drift class (D1).** Single ownership (one `KArc`, drop = exactly
one `close`) makes the double-decrement impossible to express.

### D3 — Index-addressed APIs that bypass *both* refcounts
- `syscall_tty_read=146` / `syscall_tty_write=147` (`core/src/syscall/ui_handlers.rs:154-208`)
  address a TTY by raw `TtyIndex` and call `tty::read_cooked` / `tty::write_bytes`
  directly — **no fd table, no `OpenFile`, no refcount, no ownership check**. A stale or
  guessed index reads/writes (and via the ldisc, mutates buffers / raises signals / flushes)
  a PTY the caller never opened and whose `open_count` may be 0.
- `pty::mark_peer_closed` / `free_pair_if_unused` / `queue_packet_event` /
  `clear_peer_closed` / `set_pty_lock` / `set_packet_mode` (`drivers/src/tty/pty.rs`)
  all reach a TTY by raw index and mutate lifecycle/flag state outside any refcount.
  `free_pair_if_unused` frees both slots when both `open_count==0`, gated only by
  `PTY_ALLOC_LOCK` — so any premature `close_ref` (D2) immediately collapses the pair.
- `file_poll_unfused_by_idx` (`fs/src/fileio/poll.rs:175`) releases an `OpenFile` by a
  packed `u64` token (16-bit slot / 48-bit generation), the *only* release path keyed by a
  value that can outlive an fd close. The ring double-tracks it (ring unregister +
  `POLL_REGISTRATIONS` SIGKILL cleanup), so the same token can be released twice →
  `OpenFile` refcount underflow → premature `ops.release` → `close_ref` on a still-open TTY.

### D4 — `PtyPeerHandle` + generation bitmap is a hand-rolled reimplementation of Weak
`drivers/src/tty/pty.rs:58-94` defends cross-end misrouting after free/reuse with a
`PtyPeerHandle { idx, generation }` validated against `TTY_GENERATIONS[slot]`. This is
exactly the bookkeeping that `Weak::upgrade() -> None` gives for free in Asterinas
(`PtyMaster { slave: Arc<PtySlave> }` + `Tty.weak_self`) and Redox (`controlterm: Rc`,
`subterm: Weak`). Hangup today is an imperative flag dance (`mark_peer_closed`,
`HUNG_UP`/`PEER_CLOSED` flags, `hangup()` in `lifecycle.rs:199-240`) rather than a
structural consequence of the owning reference dropping.

### D5 — Spawn mutates the parent's own fd table; whole-table-clone is the only inheritance
`spawn_program_with_attrs` (`core/src/exec/mod.rs:179-182`) has exactly one inheritance
mode: destroy the child's bootstrap table, then `fileio_clone_table_for_spawn` clones the
*entire* parent table (skipping cloexec). There is no per-fd action list. Consequently
every userland spawn site mutates **its own** fd table around the call:
`spawn_shell_on_slave` (`userland/src/apps/terminal/mod.rs:190-212`) saves fd 0/1/2,
dup2's the slave over them, spawns, then restores; `execute_registry_spawn`
(`userland/src/apps/shell/exec.rs:386-447`) does the same dance for pipe capture. This is
race-prone and non-reentrant (the terminal's *own* stdio is transiently clobbered; an SMP
async task in that window sees the wrong stdio), and it forces the fragile
`dup_above_stdio` workaround (`userland/src/apps/terminal/mod.rs:228-244`) because the
kernel offers no atomic "open/dup2 into the child without clobbering the parent".

Separately confirmed by the spawn audit but adjacent to lifetime: **execve never resets
`signal_actions`** (`core/src/exec/mod.rs:227-305`, `sched/.../task_cleanup_hooks.rs:72-75`)
— a stale handler pointer survives into a new image. Folded into the spawn-ABI stage (S3)
as a `POSIX_SPAWN_SETSIGDEF`-equivalent and an exec-time reset, since it lives in the same
code window.

**Unified statement:** liveness is tracked by two synchronized counters plus a generation
bitmap, joined by convention and bypassable by index; teardown can therefore fire early,
late, twice, or on the wrong object. The fix is to make a single owning reference the *only*
liveness fact, let Rust `Drop` be the *only* teardown trigger, and delete every bypass.

---

## 2. Target architecture — single-owner, Drop-driven lifetime

**One design, chosen decisively:** model every open-file backing object as a
`KArc<dyn FileBacking>` whose **strong count is the dup/alias count** and whose **`Drop` is
the close**. The fd-table slot holds a clone of that `KArc` (and nothing else lifetime-
bearing); `cloexec` is a per-fd-entry bit. TTY/PTY teardown is driven entirely by these
drops — the standalone `open_count` and the `PtyPeerHandle` generation bitmap are deleted.
This is the Asterinas/Redox model (`Arc<dyn FileLike>` / `Arc<LockedFileDescription>`,
release = last-drop) translated to SlopOS's `KArc`, justified below against alternatives.

### 2.1 The `OpenFile` becomes `KArc`-owned; Drop is the release

```rust
// fs/src/fileio/mod.rs  (rewritten)
pub(super) struct OpenFile {
    backing: KArc<dyn FileBacking>,   // strong count == alias count; Drop == close
    position: AtomicU64,              // shared file offset (dup/fork share it, per POSIX)
    status_flags: AtomicU32,          // O_NONBLOCK/O_APPEND live here (shared, per POSIX)
}
```

- `FileBacking` is the renamed `FileOps`-bearing object. Its **`Drop`** runs what
  `ops.release(handle)` runs today (e.g. `tty::close_ref`, socket teardown, pipe-slot
  release). `release` is *removed from the trait*; teardown is `Drop`, fired exactly once
  by `KArc` on last strong drop — idempotent by construction (the second drop can never
  happen). This is exactly the Linux `->release` invariant (`fput`→`__fput` on last ref)
  obtained for free from `KArc`.
- The fd-table entry holds `KArc<OpenFile>` (one more Arc layer so that dup/dup2/fork share
  the *same* offset + status flags, matching the Linux "open file description" identity):

```rust
#[derive(Clone)]
pub(super) struct FdEntry {
    open_file: KArc<OpenFile>,   // None encoded as Option<KArc<OpenFile>>
    cloexec: bool,               // PER-FD, never shared, never on the backing object
}
```

  `FileTableSlotInner.descriptors` becomes `[Option<KArc<OpenFile>>-with-cloexec]`. The
  generational `HandleTable<OpenFile>` (`OPEN_FILES_STATE.open_files`) is **deleted**: slot
  occupancy / generation was a hand-rolled liveness layer; `KArc` strong count is liveness
  now. The fd-number→entry indirection stays a plain `[Option<FdEntry>; 32]` per process.

- **`close(fd)` = detach-then-drop** (the Linux/Asterinas iron rule): take the per-process
  table lock, `let entry = slot.take()` (clears pointer + cloexec bit), **drop the table
  lock**, then drop `entry`. The `KArc`'s last-strong drop runs the backing `Drop` *outside*
  the table lock — satisfying the MEMORY.md fileio-lock→object-lock ordering for free
  (mirrors Asterinas `close_file` *returning* the Arc so the drop happens lockless).

- **dup / dup2 / dup3** = clone the `KArc<OpenFile>` (strong++), install in the target slot
  with `cloexec=false` (plain dup/dup2) or `cloexec = (flags & O_CLOEXEC)` (dup3). dup2
  over an occupied slot: `take` the old entry first, install the clone, drop the old entry
  after releasing the lock. **No backing `release`, no offset reset** — POSIX dup semantics.
  cloexec is never copied because it lives in `FdEntry`, not the backing object (D1/D-rule).

- **fork** = `FileTable::clone` → clone every `KArc<OpenFile>` (strong++), copy each
  `cloexec` bit verbatim (fork keeps cloexec). **spawn** path is replaced by the fd-action
  ABI (§2.4) and no longer uses whole-table clone.

- **close-on-exec** = `close_files_on_exec` collects the `Option<KArc<OpenFile>>` of every
  cloexec entry into a `KVec`, clears the slots under the lock, drops the lock, then drops
  the `KVec` (Asterinas `close_files_on_exec` returning `Vec<Arc<..>>`).

### 2.2 cloexec placement — decided: per-`FdEntry` bit, never on the backing object
Already correct in spirit today (`FdEntry.cloexec`), but the redesign makes it *structural*:
because dup shares one `KArc<OpenFile>`, putting cloexec on the shared object would make
dup of a cloexec fd wrongly mark the source. cloexec stays a `bool` in `FdEntry`
(Asterinas `FdFlags::CLOEXEC`, Redox `cloexec: bool`), set/cleared by `fcntl(F_SETFD)` on
that one fd only.

### 2.3 TTY / PTY lifetime — open_count derived from ownership, hangup as a state transition

**Decision: remove the standalone `open_count` entirely.** The Linux `tty_struct`/`tty_port`
two-object split exists to separate transient per-open-session state from long-lived
hardware state under a manual kref scheme. SlopOS's "hardware" is virtual (QEMU serial,
in-memory PTY ring buffers), so the two-object split's value collapses to "drop runs once".
A `KArc<TtyBacking>` gives that directly. Concretely:

- A TTY slot's backing state moves behind `KArc<TtyBacking>`. Every fd referencing a TTY
  holds it transitively through `OpenFile.backing` (the `FileBacking` for a TTY owns a
  `KArc<TtyBacking>` clone). The `open_count` field is deleted; "is this TTY still open" is
  `KArc::strong_count(&backing) > <baseline>` or, more precisely, the absence of any
  `OpenFile` referencing it — i.e. the backing's `Drop`.
- **`open_ref`/`close_ref` are deleted.** `tty::open_ref` is replaced by "clone the
  `KArc<TtyBacking>` into the new `OpenFile`"; `tty::close_ref` is replaced by "drop the
  `OpenFile`, which drops its `KArc<TtyBacking>` clone." The IRON RULE (Linux: only the tty
  core open/release file_operations may mutate the count) becomes *unbreakable*: there is no
  count to mutate, and no ioctl/ldisc/driver path can clone-or-drop the owning `KArc`
  because they only ever borrow `&TtyBacking`.

- **PTY pair via strong + weak (requires a new `KWeak` in ostd — see §2.6).** The master's
  `FileBacking` holds `KArc<PtySlave>` (strong); the slave's `FileBacking` holds
  `KWeak<PtyMaster>` (weak). This breaks the cycle and makes hangup structural:
  - **Master last fd closed** → master `FileBacking` drops → its `KArc<PtySlave>` drops; the
    slave observes "master gone" via `KWeak::upgrade() == None` on its next read/write
    (→ EOF / EIO), and the slave's wait queues are woken from the master's `Drop` impl
    (set a `PEER_CLOSED`-equivalent that is now a one-way latch, then publish the BUS event).
    This *is* `tty_vhangup(slave)`: slave readers get EOF, writers EIO, session leader gets
    SIGHUP (the SIGHUP/session step stays explicit in the master `Drop`, §2.5).
  - **Slave last fd closed** → slave `FileBacking` drops; master observes the missing slave
    via its `KArc<PtySlave>` strong-count dropping to its baseline (or a wake flag set from
    slave `Drop`) → master read sees EOF/EIO. Master is **not** vhangup'd (Linux rule).
  - `PtyPeerHandle` / `TTY_GENERATIONS` / `validate_peer` / `mark_peer_closed` /
    `clear_peer_closed` / `free_pair_if_unused` are **deleted** — "slot freed and reused"
    cannot misroute because you hold a typed `KWeak`/`KArc`, not an index+generation.

- **Hangup = a state transition, not a flag dance.** `hangup(idx)` (`lifecycle.rs:199`) is
  rewritten to operate on a `&TtyBacking` (obtained by upgrading/owning the `KArc`), flip a
  single `hung_up: AtomicBool` latch, swap the per-handle behavior to the "hung-up" path
  (read→0, write→EIO, ioctl→EIO — the Linux `hung_up_tty_fops` swap, modelled as an enum
  discriminant or an ops-pointer swap inside `TtyBacking`, read under the slot lock), detach
  the session, send SIGHUP+SIGCONT (§2.5), and publish the wakeup BUS events. It frees
  *nothing* — the fds and `KArc`s stay valid; only behavior changes — exactly Linux's "the
  fd is not closed by hangup; userspace still close()s, going through the harmless hung-up
  path." `is_hung_up`/`tty_hung_up_p` is the `hung_up` latch read.

### 2.4 The ring holds a real file reference, not an fd integer
`ring/src/ring_obj.rs:22-23` stores `fd: i32` "re-validated each probe". Replaced by: at
**submit** time the op resolves the fd once to its `KArc<OpenFile>` and stores a clone in
the per-op inflight state; the held `KArc` keeps the backing alive for the op's duration
even if userland `close()`s the fd or the slot is reused (the io_uring "the ring keeps a
real struct file reference" rule). Dropped on op completion/cancel. `owner_pid`
(`ring_obj.rs:73`) and the protective poll incref (`poll.rs:148-149`) demote to
defence-in-depth. `distinct_inflight_fds` (`enter.rs:495`) keys off the held `KArc`
identity, not `row.fd`, eliminating the close+reuse cross-object poll registration (D3).

### 2.5 Session / ctty teardown stays explicit (but driven from Drop/hangup)
SIGHUP is always paired with SIGCONT (already done: `lifecycle.rs:230-231`); on hangup and
on session-leader exit, the tty is cleared from every session member and `session`/`pgrp`
links nulled (`clear_session_controlling_tty`, `lifecycle.rs:229`). Job-control links
(foreground pgrp, session) are held as `KWeak` so a terminal never keeps a dead session
alive (Asterinas `JobControl { session: Weak, foreground: Weak }`); `foreground()` becomes
`upgrade()-or-skip`. This is wired into the master `Drop` (PTY hangup) and into process exit
(`disassociate_ctty`-equivalent).

### 2.6 Required ostd addition: `KWeak<T>` (the one new unsafe-bearing primitive)
`KArc` today has **no weak support** (`heap.rs:894-950`: `try_new`/`try_init`/`get_mut`/
`strong_count`/`Clone`/`Deref`/`AsRef` only — no `Weak`, no `downgrade`, no `new_cyclic`).
The strong/weak PTY topology and `KWeak` job-control links require adding to
`slopos-ostd/src/mm/heap.rs`:
- `KWeak<T>` wrapping `alloc::sync::Weak<T>`; `KArc::downgrade(&self) -> KWeak<T>`;
  `KWeak::upgrade(&self) -> Option<KArc<T>>`; `KArc::try_new_cyclic` (for the
  master↔slave self-reference, mirroring Asterinas `Arc::new_cyclic`); `KArc::weak_count`.
This is the *only* new `unsafe`-bearing surface and it lives where it must (ostd). It is a
thin forward to `alloc::sync::Weak`, so the TCB-ratio impact is a handful of lines.

### Alternatives considered and rejected
- **Linux biased `file_ref` counter (stored = count−1).** Rejected: it exists only to make
  inc/put a single CAS-free atomic on a hot path and to harden RCU slab reuse
  (`SLAB_TYPESAFE_BY_RCU`). SlopOS has no RCU and uses `IrqMutex` everywhere; `KArc` gives
  the identical "last drop runs destructor" guarantee with zero new hand-rolled atomics.
- **Linux two-object tty_struct + tty_port kref split.** Rejected as overkill: the split
  buys a transient/long-lived separation justified by real hardware + atomic-context last-
  put. SlopOS's backing is virtual; a single `KArc<TtyBacking>` whose Drop runs once covers
  it. (Revisit only if a real hardware UART with carrier/DTR state appears.)
- **Keep the index APIs but add ownership checks.** Rejected: the index *is* the bypass;
  bolting a check on leaves the count-drift class alive. Delete them (§3).
- **Redox `(scheme, number)` userspace-daemon handle.** Rejected: microkernel-specific;
  SlopOS resources live in-kernel. Take only `Arc::try_unwrap`-on-last-close (= `KArc` Drop).
- **Deferred task-work / workqueue release (Linux `__fput_deferred`, `queue_release_one_tty`).**
  Rejected for now: needed only because Linux can hit last-put from IRQ/atomic context.
  SlopOS runs the destructor inline after dropping the table lock; revisit if a concrete
  IRQ-context-drop case appears.

---

## 3. What gets deleted

Functions / fields removed outright (their behavior subsumed by `KArc`/`KWeak` + `Drop`):

| Deleted | Location | Subsumed by |
|---|---|---|
| `OpenFile.refcount: u16` | `fs/src/fileio/mod.rs:139` | `KArc<OpenFile>` strong count |
| `incref_open_file`, `release_open_file` | `fs/src/fileio/open_file_table.rs:52-82` | `KArc::clone` / `KArc` Drop |
| `alloc_open_file_entry`, `get_open_file_mut` | `open_file_table.rs:24-50` | `KArc::try_new(OpenFile{..})` / Deref |
| `pack_open_file_token`/`unpack_open_file_token` + token poll path | `open_file_table.rs:14-22`, `poll.rs:175` | hold a `KWeak<OpenFile>` registration |
| `HandleTable<OpenFile>` in `OpenFilesState.open_files` | `mod.rs:217` | `Option<KArc<OpenFile>>` in fd slots |
| `FdEntry.valid` | `mod.rs:146` | `Option<KArc<OpenFile>>` (None = empty) |
| `tty::open_ref`, `tty::close_ref` | `drivers/src/tty/lifecycle.rs:90,129` | clone/drop of `KArc<TtyBacking>` |
| `Tty.open_count` field | `drivers/src/tty` lifecycle struct | strong-count / backing Drop |
| `PtyPeerHandle`, `validate_peer`, `PtyPeerHandle::snapshot` | `drivers/src/tty/pty.rs:58-94` | `KWeak`/`KArc` typed peer ref |
| `TTY_GENERATIONS` + generation bump on free | `drivers/src/tty/table.rs` | `KWeak::upgrade()==None` |
| `mark_peer_closed`, `clear_peer_closed`, `free_pair_if_unused` | `drivers/src/tty/pty.rs:407-481` | master/slave `Drop` impls |
| `syscall_tty_read` (146), `syscall_tty_write` (147) | `core/src/syscall/ui_handlers.rs:154-208` | fd-based `read`/`write` only |
| `FileOps::release` trait method | `abi/src/file_ops.rs:80` | backing object's `Drop` |
| `FileOps::dup` (SCM_RIGHTS path) | `abi/src/file_ops.rs:83`, `net_handlers.rs:622` | `KArc<OpenFile>` clone into in-flight queue |
| `fileio_clone_table_for_spawn` | `fs/src/fileio/fdtable.rs:143`, `exec/mod.rs:181` | fd-action ABI (§2.4 of spawn) |
| `dup_above_stdio` userland workaround | `userland/src/apps/terminal/mod.rs:228-244` | fd-action ABI atomic install |
| dup2-save/restore dances | `terminal/mod.rs:190-212`, `shell/exec.rs:386-447,565-620` | fd-action ABI |

`SYSCALL_TTY_READ=146` / `SYSCALL_TTY_WRITE=147` numbers are retired (table size 162
unchanged; gaps are expected per the ID policy). The userland `tty::read`/`tty::write`
wrappers that target an index are removed; callers go through fd 0/1/2.

---

## 4. Migration plan (five stages, each independently green under `just test`)

Each stage compiles, passes `just check-framekernel` + `check_stack_sizes.sh`, and is green
under `just test` (target: ≥ the current `TEST_COUNT_BASELINE` planned tests; never regress).

### Stage 0 (prep, lands with S1) — add `KWeak` to ostd
- **Files:** `slopos-ostd/src/mm/heap.rs` (add `KWeak`, `downgrade`, `upgrade`,
  `try_new_cyclic`, `weak_count`); re-export in `slopos-ostd/src/lib.rs`.
- **Tests:** ostd unit `stest!`s: weak upgrade after strong drop returns None; cyclic
  self-ref does not leak (strong/weak counts reach 0).
- **Risk:** low — thin wrapper over `alloc::sync::Weak`. Only new `unsafe` is whatever
  `new_cyclic` needs (none beyond `alloc`'s own). TCB-ratio bump is negligible.

### Stage S1 — `OpenFile` → `KArc`; close = detach-then-drop; cloexec stays per-fd
- **Files:** `fs/src/fileio/mod.rs` (`OpenFile`, `FdEntry`, `OpenFilesState`),
  `open_file_table.rs` (delete incref/release/alloc/token), `fdops.rs` (open/close/dup/dup2/
  dup3/fcntl rewritten to clone/take/drop `KArc`), `fdtable.rs` (clone = `KArc` clone,
  close-on-exec = collect-then-drop `KVec`), `poll.rs` (registration holds `KWeak<OpenFile>`
  instead of a packed token). `FileBacking`/`FileOps` keep `release` *temporarily* as a
  thin `Drop` shim so backends migrate incrementally.
- **Tests added (audit bug classes → regressions):**
  - `fs::dup_does_not_copy_cloexec` (D1): dup of a cloexec fd leaves source cloexec, new fd
    not cloexec.
  - `fs::close_twice_is_safe` / `fs::close_while_dup_keeps_object_alive` (D1/D2): closing one
    of two dup'd fds does not run backing teardown; closing the last does, exactly once.
  - `fs::open_tty_fd_emfile_no_underflow` (D2): force `EMFILE` on a tty open; assert the
    backing's `Drop`/close fires exactly once (no double `close_ref`).
- **Risk:** medium-high — touches every fd path. The offset/status-flags move to
  `Atomic*` inside `KArc<OpenFile>` must preserve shared-offset POSIX semantics across
  dup/fork (pin with a `fs::dup_shares_offset` test). Watch `check_stack_sizes.sh`:
  construct `OpenFile`/backings via `KArc::try_init` to avoid stack materialization.

### Stage S2 — TTY/PTY open_count → ownership; hangup as state transition
- **Files:** `drivers/src/tty/lifecycle.rs` (delete open_ref/close_ref/open_count; rewrite
  `hangup` to flip a `hung_up` latch + ops-swap, no free), `drivers/src/tty/pty.rs` (master
  `KArc<PtySlave>` strong, slave `KWeak<PtyMaster>` weak via `try_new_cyclic`; move
  `mark_peer_closed`/`free_pair_if_unused` bodies into `Drop` impls; delete
  `PtyPeerHandle`/generations), `drivers/src/tty_file_ops.rs` + `fs/src/fileio/mod.rs:517`
  (TTY `FileBacking::Drop` replaces `ops.release`→`close_ref`), `fileio/fdtable.rs:21-30`
  (bootstrap console fds clone the console `KArc<TtyBacking>` instead of 3× `open_ref`).
- **Tests added:**
  - `tty::ioctl_never_changes_open_state` (Linux IRON RULE / check_tty_count): run every
    ioctl (TIOCGWINSZ/TCGETS/TCSETS/TIOCEXCL…) and assert backing strong-count unchanged.
  - `tty::pty_master_close_hangs_up_slave` (D4): close last master fd → slave read EOF,
    write EIO, slave wait queues woken, session SIGHUP'd.
  - `tty::pty_slave_close_marks_master` (D4): slave close → master EOF/EIO, master not hung.
  - `tty::scm_rights_tty_balanced` (D1): pass a tty fd via SCM_RIGHTS, assert one extra
    strong ref, balanced on receiver close (replaces the divergent `ops.dup` path).
- **Risk:** high — the PTY pair Drop ordering is the trickiest part (master Drop must wake
  the slave *before* the slave can observe a dangling state). Use `try_new_cyclic` so the
  weak back-link is valid from birth. Pin the `test_output.log` startup cascade as a
  regression: terminal startup must NOT trigger a master close before the event loop.

### Stage S3 — spawn fd-action ABI + signal-disposition fix; userland consumers
- **New ABI (decided signature):** keep `SYSCALL_SPAWN_PATH=64`; pass the action list as a
  UserPtr-to-array (all 6 arg regs are used; this is the `syscall_poll` struct-array idiom,
  `core/src/syscall/fs/poll_ioctl_handlers.rs:236-269`). Repurpose the unused arg slots /
  add a sibling `SYSCALL_SPAWN_EX` if 6 regs are insufficient (path ptr+len, argv ptr+len,
  attrs ptr → attrs struct carries priority, flags, actions ptr+count, sigdefault mask).

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
- **Execution rules (decided):** child starts with an **empty** fd table (allow-list, not
  the deny-list whole-table clone). Actions apply in array order to the child's (Blocked,
  unpublished) table — slotted exactly where `fileio_clone_table_for_spawn` sits today
  (`exec/mod.rs:181`), before `task_set_status(Ready)`. `CloneFd`/`TransferFd` clone the
  parent slot's `KArc<OpenFile>` into the child (cloexec cleared; honor adddup2 target==src
  cloexec-clear). `TransferFd` also `take`s the parent slot. All-or-nothing: any error tears
  down the scratch child table (existing `task_terminate` error path). A
  `FDIO_SPAWN_CLONE_STDIO`-style convenience expands to three `CloneFd` actions in the
  userland wrapper.
- **Signal fix (folded in):** `do_exec` resets caught signals to SIG_DFL preserving SIG_IGN
  (`task_cleanup_for_exec`, `sched/.../task_cleanup_hooks.rs:72-75`); spawn applies
  `attrs.sigdefault_mask` in the parent-inherit window (`exec/mod.rs:189-205`). Replaces the
  manual `default_signal(...)` calls in `run_in_child` (`shell/exec.rs:678-681`).
- **Files:** `abi/src/spawn.rs` (new), `abi/src/syscall/numbers.rs`,
  `core/src/syscall/process_handlers.rs:95` (copy_from_user the action array),
  `core/src/exec/mod.rs:101-225` (apply actions; reset signals),
  `userland/src/syscall/process.rs` (new `spawn_path_with_actions` + stdio convenience),
  `userland/src/apps/terminal/mod.rs` (replace dance with `[CloneFd slave→0/1/2]`),
  `userland/src/apps/shell/exec.rs` (replace pipe-capture and redirect dances).
- **Tests added:**
  - `spawn::empty_table_unless_actions` (D5): spawned child with no actions has no fds.
  - `spawn::clone_fd_shares_backing` / `spawn::transfer_fd_moves` (D5): strong-count and
    parent-slot assertions.
  - `spawn::actions_all_or_nothing` (D5): a mid-list error leaves no partial child.
  - `exec::execve_resets_caught_signals_keeps_ignored` (spawn-audit finding).
  - userland `fork_test`/terminal/shell stay green (the dance removal must be behavior-
    preserving).
- **Risk:** medium — ABI churn; the in-flight terminal/shell split (MEMORY) must rebase on
  the new wrapper. The empty-default-table change is the biggest behavioral shift; gate it
  behind the convenience wrapper so existing callers keep stdio.

### Stage S4 — retire index-based mutation APIs
- **Files:** delete `syscall_tty_read`/`syscall_tty_write` (`ui_handlers.rs:154-208`) and
  retire numbers 146/147; remove `pty.rs` index mutators already folded into Drop in S2;
  remove `file_poll_unfused_by_idx` token path (S1 already moved poll to `KWeak`); audit
  `vconsole`/`switch_active_tty`/`set_active_tty` (`lifecycle.rs:36-66`) — these are *input
  routing*, not lifetime, and stay (they read a slot, never mutate refcount), but assert via
  test they never touch ownership.
- **Tests added:** `tty::no_index_io_path` (compile-time: the index read/write syscalls no
  longer exist); `tty::poll_after_close_reuse_no_crossobject` (D3): register a poll, close +
  reuse the fd number, assert no cross-object readiness (the `KWeak` upgrade fails).
- **Risk:** low-medium — mostly deletion; ensure no in-tree caller of 146/147 remains (grep
  before delete; the kernel `tty::read_cooked`/`write_bytes` stay as ldisc internals, only
  the *syscalls* go).

### Stage S5 — ring holds real file references
- **Files:** `ring/src/ring_obj.rs` (per-op state holds `KArc<OpenFile>` not `fd: i32`),
  `ring/src/enter.rs:432-495` (resolve once at submit; `distinct_inflight_fds` keys on
  `KArc` identity; drop on completion/cancel), `ring/src/registry.rs` (`owner_pid` →
  defence-in-depth).
- **Tests added:** `ring::op_survives_fd_close` (close the fd mid-op; op still completes
  against the held backing); `ring::no_reuse_aliasing` (D3): close+reuse the fd number
  between submit and harvest; op targets the original object.
- **Risk:** medium — must drop the held `KArc` on *every* terminal path (complete, cancel,
  ring teardown, owner exit) or leak the backing. Pin with a strong-count assertion test
  after op completion. Co-located reactor tests (MEMORY thread-per-core gap) must stay green.

---

## 5. Compatibility constraints (what must stay green)

- **≥ `TEST_COUNT_BASELINE` (2425) planned tests must stay green every stage**;
  `just check-test-count` guards count regression. New regression tests (above) *raise* the
  baseline — update it deliberately, never lower it.
- **Userland test bins that pin behavior:** `userland/src/bin/tests/fork_test.rs` (fork fd
  inheritance + cloexec keep), `io_capture_test.rs` (the destructive-disk-test save/restore
  pinned in MEMORY — S1/S5 must not disturb its fd lifetime), `ring_test.rs`,
  `signalfd_test.rs`, `pidfd_e2e_test.rs`, `spin_signal_test.rs`. Kernel-side:
  `fs/src/tests.rs`, `ring/src/tests.rs`, `sched/src/sched_tests.rs`,
  `drivers/src/tty_tests/*` (e.g. `test_ldisc_signals.rs`).
- **The in-flight terminal/shell split continues on top:** S3's fd-action wrapper is the
  seam the split rebases onto; coordinate so the split lands its dup2-dance removal via the
  new `spawn_path_with_actions` rather than re-introducing save/restore.
- **POSIX shared-offset/status-flags semantics:** dup/dup2/fork must continue to *share* the
  file offset and `O_NONBLOCK`/`O_APPEND` (now `Atomic*` in `KArc<OpenFile>`); pin with
  `fs::dup_shares_offset`. fork keeps cloexec; exec strips it — both must stay true.
- **Framekernel gates every stage:** `check_unsafe_outside_ostd.sh` (only `KWeak`/`KArc`
  internals in ostd may use unsafe — the fs/drivers/ring/core changes stay forbid-unsafe),
  `check_alloc_dep.sh` (route through `KArc`/`KVec`, never bare `alloc`),
  `check_stack_sizes.sh` (2 KiB — use `KArc::try_init` for backings).
- **SMP "task invisible until initialized" invariant:** S3 must keep applying fd actions
  while the child is Blocked/unpublished (`exec/mod.rs` TASK_NEW window), preserving the
  Release-store publish at `task_set_status(Ready)`.
- **Lock ordering (MEMORY):** fileio table lock → object/backing lock, table lock dropped
  before any blocking op or backing `Drop`. The detach-then-drop close path (§2.1) enforces
  this structurally.
