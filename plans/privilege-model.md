# Privilege: stop userland setting its own capability bits

`SyscallContext`'s entire "Permission checks" section is three predicates, and
all three read `self.task.flags` (`core/src/syscall/context.rs:151-199`):

```rust
pub fn is_compositor(&self)        -> bool { self.has_flag(TASK_FLAG_COMPOSITOR) }
pub fn is_display_exclusive(&self) -> bool { self.has_flag(TASK_FLAG_DISPLAY_EXCLUSIVE) }
pub fn is_console_admin(&self)     -> bool { self.has_flag(TASK_FLAG_SYSTEM) }
```

`syscall_spawn_path` copies `SpawnAttrs` out of user memory and passes
`attrs.flags` straight through with no mask, no reject-list and no reserved-bit
check (`core/src/syscall/process_handlers.rs:178-226`); `spawn_program_with_attrs`
ORs in `USER_MODE` and hands the whole word to `task_build`, which writes it
verbatim into `task.flags` (`core/src/exec/mod.rs:233-262`,
`sched/src/task/task_lifecycle.rs:632`).

Every privilege the kernel recognises is therefore a value userland writes.

This document has two halves. The first is a contained fix that should land
immediately and is not open to design debate. The second is a spike, because
SlopOS has no uid, no credential and no capability object to hang authority on,
and choosing that shape is a real decision.

---

# Part 1 — Containment (land now)

## Classify the nine bits

There are exactly nine `TASK_FLAG_*` bits in one `u16`; bits `0x200..0x8000` are
undefined and unvalidated (`abi/src/task.rs:245-265`).

| Bit | Verdict | Why |
|---|---|---|
| `USER_MODE` 0x01 | forced on | `spawn_program_with_attrs` ORs it unconditionally |
| `KERNEL_MODE` 0x02 | reject with EINVAL | `task_build` already rejects it combined with USER_MODE, so it self-rejects as `NoMem` — not exploitable, but it should be diagnosed rather than mislabelled |
| `NO_PREEMPT` 0x04 | reject | **has zero setters anywhere in the tree** — no kernel path, no userland path, no test. It exists only as an attack surface |
| `SYSTEM` 0x08 | privileged | gates `require_console_admin` (font_set) |
| `COMPOSITOR` 0x10 | privileged | gates 5 syscalls *and* installs the global input sink |
| `DISPLAY_EXCLUSIVE` 0x20 | privileged | gates `roulette_draw` |
| `FPU_INITIALIZED` 0x40 | delete | dead — never written, never read, only appears in re-export lists |
| `NEW_PGRP` 0x80 | user-settable | only mints a group inside the parent's own session |
| `FOREGROUND` 0x100 | user-settable | already session-validated by `set_foreground_pgrp_checked` |
| `0x200..0x8000` | reject | undefined; reserved bits must fail closed so the ABI can grow |

So: `SPAWN_USER_SETTABLE = NEW_PGRP | FOREGROUND`. Everything else in
`attrs.flags` is rejected with `EPERM`, and any undefined bit with `EINVAL`.

Reject rather than silently mask. A silent mask makes a privileged spawn look
like it succeeded, which is worse than a clear failure for the legitimate callers
in Part 1's fallout below.

## Two adjacent holes that must close in the same commit

**`NO_PREEMPT` escalates from one CPU to the machine.** The timer tick returns
early for such a task (`sched/src/scheduler.rs:2350-2352`) and so does the
deferred post-IRQ reschedule (`:2155-2160`). On its own that costs one CPU. But
`set_cpu_affinity` is unprivileged and accepts an *arbitrary target task id* with
no caller-versus-target relation checked
(`core/src/syscall/process_handlers.rs:459-473`), so a process can pin one
`NO_PREEMPT` spinner per CPU and wedge every core. Restrict the affinity syscall
to the caller's own task, or to tasks in its own process, in the same change.

**Priority is under-validated in the other direction.** Only `KernelIo` is
rejected, so an unprivileged spawn may take `TaskPriority::High` — numerically
above `Normal`, the compositor's own tier — or `Idle`, whose doc comment says
"Per-CPU idle loop only — never used by user-spawned tasks"
(`abi/src/task.rs:184-199`). Restrict userland to `Normal` and `Low`.

## Fallout: three spawn paths forward `spec.flags` and only one is init

The privileged bits are needed by two `ProgramSpec` entries: `compositor` carries
`COMPOSITOR` and `roulette` carries `DISPLAY_EXCLUSIVE`
(`userland/src/program_registry.rs:39,55`). Init spawns both, and init is the one
userland task holding `SYSTEM`.

But two other paths forward `spec.flags` verbatim and neither is init:

- the shell's registry-spawn path (`userland/src/apps/shell/exec.rs:414-421`)
- the compositor's app launcher (`userland/src/apps/compositor/input.rs:927-931`)

A rule keyed purely on "the caller holds SYSTEM" therefore breaks `roulette`
typed at the shell and `roulette` launched from the compositor. Neither is
exercised by `just test` — init exits before reaching the roulette spawn when
tests are enabled, and the test cmdline sets `roulette=skip` — so **CI will not
catch this regression**. Verify by hand with `just boot`.

The interim rule that keeps those paths working: a caller may pass a privileged
bit only if it already holds that same bit. Init holds `SYSTEM` and can grant
`SYSTEM`; the shell and compositor cannot grant `DISPLAY_EXCLUSIVE` and must
instead be reached through a path that can. If that proves too restrictive for
the launcher, the honest interim is to let the kernel apply the registry's flags
by program identity rather than accept them from the caller at all — which is
where Part 2 goes anyway.

## Phase 1 work

| # | Work | Done when |
|---|---|---|
| 1 | `SPAWN_USER_SETTABLE` mask in `syscall_spawn_path`; EPERM for privileged bits, EINVAL for undefined ones | A spawn requesting `COMPOSITOR`/`SYSTEM`/`DISPLAY_EXCLUSIVE`/`NO_PREEMPT` from an unprivileged task fails |
| 2 | Restrict `set_cpu_affinity` to the caller's own task or process | A task cannot pin another process's task to a CPU |
| 3 | Restrict user-requestable priority to `Normal`/`Low` | `High` and `Idle` are rejected with EINVAL |
| 4 | Delete `TASK_FLAG_FPU_INITIALIZED`; make the `KERNEL_MODE` rejection an explicit EINVAL | The dead bit is gone and the conflicting-mode case reports the right errno |
| 5 | Grant rule for the three legitimate spawn paths, verified with `just boot` | Compositor and roulette still start, from init and from the shell |

Tests: a `utest!` that attempts each privileged bit and asserts EPERM; a `utest!`
that spawns with `NO_PREEMPT` and asserts rejection; a scheduler `stest!` proving
the timer tick preempts a task that no longer carries the bit. Note the
compositor/roulette paths cannot be covered by `just test` — say so in the commit
message rather than pretending otherwise.

---

# Part 2 — Spike: what should authority actually be?

Part 1 stops the bleeding. It does not give SlopOS a privilege model, and the
gaps below are not things a mask can fix.

## What the spike must confront

- **7 of 111 wired syscalls carry any privilege check.** The rest carry at most
  `requires(task_id)`, which is an existence check, not authorization.
- **`halt` and `reboot` have no check at all** (`core/src/syscall/core_handlers.rs:57-67`).
  One instruction from any task powers off the machine.
- **`kill` performs no caller-versus-target authorization**
  (`core/src/syscall/signal.rs:168-211`): `pid > 0` accepts any live task id
  including init and the compositor, and `pid == -1` walks every active task.
- **There is no uid to hang a credential on.** `getuid`, `geteuid`, `getgid`,
  `getegid` are literally `-> u32 { 0 }`
  (`core/src/syscall/process_handlers.rs:594-597`).
- **`execve` does not touch `task.flags`.** A task holding `COMPOSITOR` keeps it
  across an exec of an arbitrary binary — there is no `no_new_privs`, no
  privilege drop on exec.
- **fork/clone inherit privilege by wholesale byte copy** of `TaskInner`
  (`clone_from_raw`, `slopos-ostd/src/task/kernel_task.rs:1503-1533`); `flags` is
  not in the explicit re-initialisation list, so privilege propagates to every
  descendant automatically.
- **Two syscalls make ownership self-declared regardless of flags.**
  `input_poll_batch` installs the caller as the global input sink when it holds
  `COMPOSITOR` (`core/src/syscall/ui_handlers.rs:54-57`), and `fb_flip` stamps
  the caller as the global compositor task id after a successful flip
  (`:231`). Even with flags locked down, whoever flips last owns the screen.

## Questions the spike must answer

1. **What is the principal?** A uid, a per-process credential object, or a
   capability handle? SlopOS has no users and no login, so importing uid/gid
   wholesale may be cargo-culting a model whose purpose (multi-user time-sharing)
   does not apply. A capability model fits a single-user desktop better and fits
   this kernel's existing shape better — see below.
2. **Where does authority come from?** Something must be the root. Init is the
   obvious holder, but that only works if authority can be *delegated* — the
   shell and compositor need to launch privileged programs without themselves
   being able to mint arbitrary authority.
3. **Is authority per-task or per-process?** `task.flags` is per-task today, but
   privilege is conceptually per-process. This interacts directly with
   `plans/process-identity.md`: a `Process` object is the natural home for a
   credential, and building credentials before that object exists means building
   them twice.
4. **What happens on exec and on fork?** Linux answers with `no_new_privs`,
   setuid semantics and explicit `cred` copying. SlopOS must answer explicitly
   rather than inheriting whatever the byte copy does.
5. **Do the self-declaring syscalls become capability-checked?** Screen ownership
   and input sink ownership should be grants, not races.
6. **What is the migration path for the 104 unchecked syscalls?** Auditing them
   all at once is not realistic. A default-deny table with an explicit
   per-syscall required-capability column is auditable; a default-allow model
   with checks sprinkled in is not.

## The strongest candidate, and why

**A capability token modelled on `KernelIoToken`.** This kernel already solved
this exact problem once, for the `KernelIo` scheduling tier
(`slopos-ostd/src/sync/kernel_io_task.rs:1-108`): the ABI surface rejects the
user-supplied value at the syscall boundary, the only mint site is a
macro-generated trampoline, and the token is `!Send + !Sync` so authority cannot
be laundered to another task. That is a finished, in-tree, reviewed design for
"privilege is a typed witness, not a bit".

Extending it: a `Capability` set held in a `cred: RcuArcSlot<Cred>` field on the
task or process. `RcuArcSlot` is already written and already used for
`Task::process_group` (`slopos-ostd/src/sync/rcu.rs:675-712`), giving lock-free
readers and grace-period-deferred release — which is precisely what Linux's
RCU-protected `struct cred` needs and what SlopOS would otherwise have to build.

`SpawnAttrs` has spare ABI room for a capability field without a layout change:
`_pad2: u16` sits directly after `flags`, and the layout is pinned by const
asserts (`abi/src/spawn.rs:56-90`).

Prior art worth reading before deciding: Linux `struct cred` + `capable()` and
the well-documented failure of POSIX capabilities to be composable; Fuchsia's
handle-rights model, where there is no ambient authority at all and every
operation names a handle carrying its own rights; seL4's capability derivation;
Windows tokens with restricted SIDs. Fuchsia is the closest fit for a
single-user desktop with a compositor, and its job/process hierarchy also
supplies the resource-limit story that `plans/resource-accounting.md` needs.

## Spike deliverables

A short design note answering questions 1–6, a proposed `Cred`/`Capability`
shape with the exec and fork semantics written down, a default-deny syscall
capability table listing all 111 handlers with their required capability, and a
migration sequence that keeps the tree green at each step. That note replaces
this Part 2, and becomes the real plan.

Do the spike **after** `plans/process-identity.md` phases 1–3, so the credential
has an owner to live on rather than being retrofitted onto a task field and moved
later.
