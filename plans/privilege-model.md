# Privilege: what should authority actually be?

SlopOS has no uid, no credential and no capability object to hang authority on.
Choosing that shape is a real decision, and this document is the spike that has
to make it.

The spawn boundary is contained: a request is filtered against a
`SPAWN_USER_SETTABLE` mask, and the privileged bits a child ends up with come
from a kernel-side table keyed on the program being loaded
(`core/src/exec/grants.rs`). That is containment, not a privilege model, and
the gaps below are not things a mask can fix.

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
- **The program-identity grant is only as strong as write protection on
  `/bin`,** and SlopOS has no file permissions. A task that can overwrite
  `/bin/roulette` still obtains `DISPLAY_EXCLUSIVE`.
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
   privilege is conceptually per-process. A `Process` object is the natural home
   for a credential, and building credentials before that object exists means
   building them twice. What a task carries today is a process id and a
   `Handle<ProcessVm>` naming its address space — enough to identify a process,
   not yet an object to hang a credential on.
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
asserts (`abi/src/spawn.rs:56-90`). It carries no must-be-zero contract today,
so claiming it is still free.

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
this document and becomes the real plan.

Do the spike **after** a `Process` object exists, so the credential has an owner
to live on rather than being retrofitted onto a task field and moved later.
