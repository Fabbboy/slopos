# Authority

**Authority in SlopOS is a per-process, immutable, flat capability set that only the
kernel can raise, that only the holder can lower, and whose requirement is declared at
each operation's definition site and is total by compile-time construction.**

Every operation entry point names exactly one `Capability` in its `define_syscall!`
invocation. That value reaches the syscall table *through* the handler, so the
dispatcher's decision and the handler's witness read one artifact and cannot diverge, and
a `const fn` histogram over the table proves the classification covers all 168 slots and
that no single capability has grown past its recorded share. Where an operation names an
object, authority rides on the resolved object — a descriptor, a seat handle, a `TaskRef`
— and not on the caller's capability set.

The claim is **no unchecked authority**. Not "no ambient authority": SlopOS is a
Linux-ABI kernel whose syscalls take integers, so authorization is a credential consulted
against arguments, which is ambient by the standard definition. What is new is that
forgetting the check is a compile error rather than a review miss.

**Depends on `plans/process-object.md`** for an owner to hang a credential on, and on its
phase 4 for a pid that is a sound designator. The kernel-hardening pass landed first and
carries every fix that needs no capability system at all.

---

## What exists today

Authority is `task.flags: u16`, and privilege enters through exactly one place:
`core/src/exec/grants.rs`, a two-entry table keyed on the binary's path byte-for-byte
(`/bin/compositor` → `TASK_FLAG_COMPOSITOR` + `TaskPriority::High`; `/bin/roulette` →
`TASK_FLAG_DISPLAY_EXCLUSIVE`). The file describes itself accurately: "This is
containment, not a privilege model."

The gaps, all verified:

- **7 of 111 registered syscalls carry any authority check.** There are 64 `requires`
  clauses; the rest are existence checks. `TASK_FLAG_SYSTEM` gates exactly one syscall,
  `font_set`, through `is_console_admin` — whose own doc says it is "modelled on
  `capable(CAP_SYS_TTY_CONFIG)` until a proper capability bitfield exists".
- **The grant is a union computed with no reference to the spawner**
  (`core/src/exec/mod.rs:259-261`), and `syscall_spawn_path`
  (`core/src/syscall/process_handlers.rs:200`) has no `requires` clause. So any task
  obtains a `COMPOSITOR` child by spawning `/bin/compositor`.
- **`exec` never touches `task.flags`**, and `grant_for` is called only from the spawn
  path — so `COMPOSITOR` survives an exec of an arbitrary binary. There is no
  `no_new_privs` and no privilege drop.
- **`fork` inherits privilege by wholesale byte copy.** `flags` is absent from
  `clone_from_raw`'s explicit re-initialisation list.
- **No uid exists** to hang a credential on.
- **Two syscalls make ownership self-declared regardless of flags.** `fb_flip` stamps the
  caller as the global compositor after a successful flip
  (`core/src/syscall/ui_handlers.rs:231`) and `input_poll_batch` installs the caller as
  the global input sink (`:55`). Even with flags locked down, whoever acts last owns the
  screen.
- **`TASK_FLAG_SYSTEM` conflates authority with reliability policy**: it also makes a
  task's panic fatal rather than recovered (`sched/src/runtime.rs:90`,
  `sched/src/task/task_lifecycle.rs:434`).

Two CVE shapes are reproduced exactly. An entitlement surviving `exec` is one; an
entitlement inherited through an omission from a re-initialisation list is the other. They
were two years apart in the same vendor's kernel, on the same entitlement. Inheritance is
where ambient authority re-enters, and an omission from a re-init list is invisible in
review while an intersection at a named site is not.

---

## The six questions, decided

### 1. What is the principal?

**The `Process`, identified by a `KArc<Process>`, never by `process_id`.** Programs are
principals; users are not, because there are none. A pid drawn from a FIFO reuse ring with
no generation means a stale designator resolves to a live *different* principal — the
confused deputy, with the kernel as the deputy.

### 2. Where does authority come from, and how is it delegated?

**One raise site — the program-identity grant at load — bounded by a `Launch` capability.
Capabilities are not delegable. Objects are.**

```
spawn:  child.caps = if spawner.caps ∋ Launch { grant(image) } else { ∅ }
exec:   self.caps  = grant(image) ∩ self.caps
fork:   child.caps = parent.caps                 (copy; authority is per-process)
drop:   self.caps  = self.caps \ requested       (total, infallible)
```

Intersecting the parent's authority with the program grant at *spawn* is arithmetically
self-defeating and must not be written: the shell holds ∅, so `∅ ∩ DISPLAY_EXCLUSIVE = ∅`
and `/bin/roulette` could never draw. The bound has to be a separate right to *launch*,
not a floor on what is launched.

`Launch` is granted by the same table to the images that legitimately launch privileged
programs. The verified launcher set is two: init (`userland/src/apps/init_process.rs:97`)
and the shell (`userland/src/apps/shell/exec.rs:540,622,663` over
`userland/src/program_registry.rs:51-52`). `grants.rs:11-18` names the compositor's shelf
as a third; it contains no spawn call, and correcting that comment is
the launcher set is what bounds `Launch`.

An authority-raising spawn accepts **no** caller-supplied descriptor actions, argv, envp or
cwd. Handing a privileged child an attacker-chosen environment is the other half of the
inheritance CVE above.

**`SpawnAttrs::_pad2` is not claimed.** Capability delegation has no in-tree consumer, a
16-bit field pinned by const asserts cannot survive a growing set (Capsicum exhausted 64
rights in one release and restructured to roughly a thousand), and `0` would have to mean
"delegate everything" or every existing spawn breaks the day the field is first read.
Object delegation goes through the descriptor-action ABI that already exists.

**Corollary fix.** `FdAction::Open` (`core/src/exec/mod.rs:71-76`, applied `:146`) opens an
arbitrary VFS path into the child with no reference to what the parent holds. That is
endowment by *name*, which voids the ABI as an attenuating channel. Redefine as
parent-side resolve plus install, or delete it and require open-then-transfer.

### 3. Per-task or per-process?

**Per-process**, published as `cred: RcuArcSlot<Cred>` on `Process`, with a per-task
`caps: AtomicU64` effective-mask cache for the hot path.

Three independent upstream mechanisms discovered per-thread credentials were wrong and
retrofitted synchronisation — a signal broadcast for `setuid`, a thread-sync flag for
seccomp, another for Landlock, which states outright that threads are not security
boundaries. Do not emulate the broadcast: a `Cred` replacement applies to every task of the
process in-kernel with the process quiesced, or returns `EBUSY` from a multi-tasked
process.

**`Cred` has no interior mutability — no atomics, no `Cell`, no lock — so mutate-in-place
is inexpressible.** This is not style. Asterinas kept the upstream refcounted credential
but made its fields atomics, and its signal-permission check reads a four-way uid
cross-product off a *target* thread: a torn read that turns deny into allow.

### 4. exec and fork

**exec intersects, at `core/src/exec/mod.rs:498`, beside `fileio_close_on_exec`.** The
insertion point is forced: the exec handler re-disables interrupts
(`core/src/syscall/process_handlers.rs:439-441`) and `task_cleanup_for_exec` at `:458`
holds a lock, both illegal for `RcuArcSlot::store`, which defers through `call_rcu`,
allocates, and on failure spins in `synchronize_rcu`. `do_exec`'s interrupts-on tail is the
only legal site and is also semantically right — past the point of no return, before the
child's first instruction.

**fork:** the `caps` mask copies. The owning `KArc<Process>` field **must** join
`clone_from_raw`'s explicit write list beside `process_group`, and must be re-established
with `replace_exclusive` rather than `store`, because `task_fork` holds a `PreemptGuard`
across the whole clone. Omitting it duplicates a raw owning pointer with no refcount
increment — a double free, not a policy bug. Three sites must agree on the neutral value
(`invalid()`, `init_invalid()`, `clone_from_raw`); pin it with a host test.

**Two rules must be stated, or the exec-intersection oversells itself.**

- **Reduction is infallible** — a total function on the lattice with no error return a
  caller can ignore. A historical local root came from an attacker making a privilege
  *drop* fail inside a program that ignored the result.
- **exec revokes almost nothing else.** Only close-on-exec descriptors close, and
  `unmap_existing_code_region` unmaps only the code window
  (`mm/src/process_vm.rs:1619-1628`) — the heap and the entire mmap window survive exec. A
  shared framebuffer or memfd mapping obtained under the old `Cred` is still mapped and
  writable. Declare exec explicitly **not** a revocation point for memory, and require every
  authority-bearing handle to be close-on-exec.

### 5. Do the self-declaring syscalls become capability-checked?

**No — they become object operations, and this is the highest-value item in the plan.**

`fb_flip` and `input_poll_batch` write their global owner on *every* call, at frame rate.
They are re-arms, not acquires, and no credential reaches them, because the object being
fought over is not itself the thing being authorized. An object that supports no access
check is not protected by any sandbox around it.

Decision: `screen_acquire() -> Screen` and `input_sink_acquire() -> InputSink`,
single-holder, with **two named seats** (compositor-primary, and virtcon for roulette plus
the kernel log). Ownership is *announced by the arbiter* and never conferred by presenting
a frame, with a kernel-log fallback seat that always wins so the display stays
recoverable. `fb_flip`, the cursor calls, `set_display_mode` and `roulette_draw` take
`&Screen`; `input_poll_batch` takes `&InputSink`.

**Two constraints.** The handle must be **linear**: nothing in the tree can express a
non-duplicable descriptor today — `fileio_clone_file_ref` snapshots any `OpenFile` with no
kind check (reached from `core/src/syscall/net_handlers.rs:616`), and the descriptor-action
clone and transfer arms duplicate anything. Add a per-`FileKind` transferability predicate
*before* the seat exists, and set `close_on_fork`. And release is by **arbiter revocation
on holder death**, not by holder `Drop` — a reference cycle among holders would otherwise
wedge the display unrecoverably, which is a documented failure of process descriptors
elsewhere.

Land the seat with the frame-rate re-arm textually unchanged, then move the pointer-focus
re-seed to the bottom half keyed on the seat holder in a second commit. That re-seed
(`if get_pointer_focus() == 0 { set_pointer_focus(task_id, 0) }`) is a self-heal, and a
naive "consume a handle" rewrite deletes it — a lost-wake failure class has already bitten
this tree twice, and a two-commit split means a regression bisects to one of two.

### 6. Migration path for the unchecked syscalls

**Classify all 111 registered slots plus the 14 ring opcodes in one mechanical commit, all
as `None`, with the gate hard-failing from that commit, then ratchet.** `SYSCALL_TABLE_SIZE`
is 168 but the other 57 slots stay `handler: None` from the array initialiser and have
nothing to annotate.

Landlock is the existence proof that incremental-and-total is achievable — eight ABI
versions over five years, additive, with "not yet handled" an explicit state — but it had to
negotiate with distributions and container runtimes. SlopOS builds its own kernel and its
own userland, so there is no period in which unclassified is legal.

**`authority=off|warn|enforce` ships in the same commit**, in the `lockdep` idiom, with the
failure mode split by kind: invoking an operation your authority does not name is a program
bug and is loud; acting on an object you were not given is `EPERM`. Both OpenBSD and
FreeBSD ended up needing both modes, so build both now.

`warn` is operationally load-bearing, not a courtesy: `core/src/syscall/tests.rs` has 141
tests and only about fifty lines touch permission behaviour, and under `tests=on` init exits
before spawning the compositor, shell or terminal
(`userland/src/apps/init_process.rs:79-92`). One
`BOOT_CMDLINE='authority=warn roulette=skip' just boot-log` capture is the realistic
enumeration of what the desktop actually calls.

---

## The capability set

Derived mechanically by a complement rule: **a slot needs a capability only if its
footprint is neither the caller nor an object the caller already names by descriptor, *and*
no relation between caller and target answers it.** Eleven entries. No `Admin`, `System`,
`Misc`, `Other` or `All` — ever. No reserved-empty entries.

| Capability | Slots | Gates | Deletion condition |
|---|---:|---|---|
| `Power` | 2 | `halt`, `reboot`, plus the reachability class covering `roulette_result`'s reboot arm and kconsole's destructive pair | becomes a `/dev/power` descriptor delegated to init |
| `Launch` | 2 | `spawn_path` and `exec` where `grant_for(path) != 0` | none — the one raise site |
| `ProcSignal` | 2 | `kill` crossing a session, `kill(-1)`, `kill(<-1)` naming a foreign pgid, `terminate_task` crossing a session | none |
| `SysInspect` | 6 | `sys_info`, `process_list`, `cpu_info`, `percpu_stats`, `net_scan`, `net_info` — default scope becomes the caller's session | none; read-only, never fused with a mutating class |
| `DisplaySeat` | 1 | `screen_acquire` only | collapses into the seat handle |
| `InputSeat` | 1 | `input_sink_acquire` only | collapses into the seat handle |
| `ConsoleConfig` | 2 | `font_set`, `keymap_load` | becomes an ioctl on the console descriptor |
| `ConsoleIo` | 2 | `write`, `read` | dies when both route through the caller's controlling TTY |
| `ClipboardGlobal` | 2 | `clipboard_copy`, `clipboard_paste` | dies when the clipboard is memfd-plus-fd-passing only |
| `Fate` | 2 | `roulette`, `roulette_result` | none |
| `TestHarness` | 2 | `run_userland_tests`, `test_panic` | none; empty in shipped images |

`ConsoleIo` exists because of two slots that read as unprivileged under naive
classification: `syscall_user_write` writes the global kernel console with no descriptor,
and `syscall_user_read` reads a **hardcoded** TTY 0 rather than the caller's controlling
terminal. Both are fixed — `read` resolves the controlling terminal — and `ConsoleIo` is the
belt while that lands.

**The three `None` classes are counted separately** — `NoneSelf`, `NoneFd`,
`NoneRelation` — plus `Unimplemented` for the 57 empty slots. That is what stops "mark it
`None`" being the path of least resistance: a bare keyword's equilibrium is 161 of 168
`None` with a green gate. Even a carefully curated promise set skews heavily toward its
most permissive entry, so the distribution must be visible.

The four slots already correctly relation-enforced are the model for the rest:
`set_cpu_affinity`/`get_cpu_affinity` (`core/src/syscall/process_handlers.rs:537,558`),
`setpgid` (`:604-611,:630` — parent-or-self **and** same-sid both ways) and `vhangup`
(`:800-806`). `setsid` (`:650`) refuses an existing leader, so a task can only ever *leave*
a session and never join another's — which is what licenses session equality as the uid
substitute.

### Why this will not become a catch-all

Upstream measured 451 of 1,167 capability checks resolving to a single administrative
capability, up from 16 % a decade earlier, with the stated cause being no overall
coordination of capability use. Four mechanisms, strongest first:

1. **The derivation rule refuses a catch-all by construction.** A capability may be minted
   only for an operation that can name neither its target as a handle nor a relation to it.
   Any proposed catch-all fails admission, because its operations *do* name objects.
2. **A `const` histogram asserts on the distribution, not just coverage.** Per-capability
   entry-point counts and a totality assert summing to 168, evaluated by `rustc`. A
   capability that grows past its recorded count fails the build. This is the coordination
   upstream structurally could not have and SlopOS can, because the classification is one
   `const` array with one author.
3. **The resource axis is not fixed.** The actual fatal defect of a fixed capability set is
   that no least-privilege statement about an *individual object* is expressible. Here
   objects live on the handle axis, so the capability axis is only the residue.
4. **Deletion columns, asserted non-empty for the four transitional entries.** The set is
   designed to shrink.

**Rule for adding one:** it must pass the admission test, arrive with a non-zero
entry-point count and its recorded cap, carry a deletion condition or an explicit argument
that its operation can never name an object, not be named `Admin`/`System`/`Misc`/`Other`/
`All`, and land in exactly one class of the const-asserted partition — which fails the
build until the count and the class are both done.

---

## Type design

`adt_const_params` is not enabled, so a const-generic over the capability enum is
unavailable. Marker types plus a sealed trait, in the existing `CpuInitWitness` idiom.

```rust
// slopos-ostd/src/authority/mod.rs
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Capability {
    Unimplemented = 0,
    NoneSelf, NoneFd, NoneRelation,
    Power, Launch, ProcSignal, SysInspect, DisplaySeat, InputSeat,
    ConsoleConfig, ConsoleIo, ClipboardGlobal, Fate, TestHarness,
}

mod cap_seal { pub trait Sealed {} }

/// Sealed: exactly the eleven impls below exist, all in this module.
pub trait CapKind: cap_seal::Sealed {
    const BIT: u64;
    const CAP: Capability;
}

/// Immutable, refcounted, no interior mutability: "mutate in place" is
/// inexpressible, which is what makes the RCU publish the only writer path.
/// `Drop` is trivial — no owned handles, no locks, no callbacks — so a
/// reference may be released under any lock, the rule `job_control.rs` relies on.
pub struct Cred { caps: u64, grant_image: ImageId }

impl Cred {
    pub fn caps(&self) -> u64;
    /// Total and infallible: a drop can never fail.
    #[must_use] pub fn without(&self, mask: u64) -> Cred;
    /// The only widening, and only the loader reaches it.
    pub(crate) fn from_grant(g: Grant, bound: u64) -> Cred;
}

/// Authorization witness. ZST; private fields; `!Send + !Sync`; no `Copy`, no
/// `Clone`, and deliberately no re-mint from a borrowed one — `BspToken` has
/// one, but for authority that would be a laundering hole. Branded to the
/// `&'ctx Task` the SyscallContext already holds, so it cannot be stashed in a
/// static or captured beyond the request.
pub struct Cap<'ctx, R: CapKind> {
    _brand:    PhantomData<fn(&'ctx ()) -> &'ctx ()>,
    _kind:     PhantomData<fn() -> R>,
    _not_send: PhantomData<*const ()>,
}
const _: () = assert!(core::mem::size_of::<Cap<'static, Power>>() == 0);

/// The ONLY mint. The constructor is private to this module and the checker is
/// public, exactly as `IrqDisabled::with` is the only mint for `IrqDisabled`.
/// One `Relaxed` load and a mask — no RCU section, no refcount traffic.
pub fn check<'a, R: CapKind>(task: &'a Task) -> Result<Cap<'a, R>, Errno>;

/// Object-carrying family, for operations that name a target. Linear,
/// `#[must_use]`, consumed by value. Holds a `TaskRef`, never a bare `KArc`.
#[must_use]
pub struct Signalable<'ctx> { target: TaskRef, _brand: PhantomData<&'ctx ()> }
pub fn resolve_signal_target<'a>(caller: &'a Task, target: Tid)
    -> Result<Signalable<'a>, Errno>;
```

Terminal primitives move into ostd and demand the witness:

```rust
// slopos-ostd/src/platform/power.rs
pub fn shutdown(_cap: &Cap<'_, Power>, reason: &CStr) -> !;
pub fn reboot  (_cap: &Cap<'_, Power>, reason: &CStr) -> !;
```

### The table entry carries the classification

```rust
// core/src/syscall/common.rs
#[repr(C)] #[derive(Copy, Clone)]
pub struct SyscallEntry {
    pub handler: Option<SyscallHandler>,
    pub name: KernelSync<*const c_char>,
    pub cap: Capability,          // new
}
```

`define_syscall!` gains a **mandatory** `cap(X)` clause and emits a same-named module (a
function and a module occupy different namespaces):

```rust
define_syscall!(syscall_reboot (ctx) cap(Power) requires(let p: cap(Power))
    -> SyscallResult { power::reboot(&p, c"user reboot"); SyscallResult::NoReturn });

// expands to:
pub fn syscall_reboot(ctx: &SyscallContext) -> SyscallResult { … }
pub mod syscall_reboot {
    pub const DEF: SyscallEntry = SyscallEntry {
        handler: Some(super::syscall_reboot), name: /* "reboot" */, cap: Capability::Power,
    };
}
```

`syscall_table!`'s per-slot syntax is **unchanged**; one edit inside one macro body makes
the arm read `table[$num as usize] = $handler::DEF;`. Omitting `cap(X)` is a macro-arm
mismatch — a compile error at the handler's own definition, in the crate that owns it.
Forgetting to register leaves the slot `Unimplemented`, which moves the `Unimplemented`
count and fails the histogram assert.

The new `@reqs` muncher arm must be written in the **binding** shape of the `task_id` arm
(`core/src/syscall/macros.rs:117-147`), never the shape of the `compositor` /
`display_exclusive` / `console_admin` arms (`:149-166`), which do
`if let Err(e) = … { return … }` and throw the `Ok` away. Retire those three arms as the
capability classes absorb them, so the wrong template stops being available to copy.

### Where the witness is required, and where the check alone suffices

**The decision lives in the dispatcher, not the handler.** `syscall_handle` already holds
the `SyscallEntry` before it builds the context (`core/src/syscall/dispatch.rs:36-37`), so
the required capability is a byte in a cache line just touched and the check is one compare
against the per-task mask. That is **cheaper than today's seven `require_*` calls**, it is
total by construction, the handler cannot forget, and `authority=off|warn|enforce` becomes
one branch in one function.

A bare witness proving "someone checked something somewhere" is not enough, and the
counterexample is in-tree: `syscall_terminate_task`
(`core/src/syscall/process_handlers.rs:340-357`) is `requires(compositor)` and then
terminates `target_id` with only a self-exclusion, so it kills init — and adding a bare
`&Cap<Kill>` leaves it byte-identical. Type-qualifier work on the Linux kernel in 2002
found an exploitable bug of exactly that shape: the variable that is checked is not the
variable subsequently used. The `IrqDisabled` analogy breaks because interrupt state is a
global scalar with no subject and no object — there is nothing to mis-bind.

So, three tiers:

- **Witness required** where the terminal primitive is (or moves) into ostd and has more
  than one caller: `Power` today. The shutdown and reboot primitives are currently function
  pointers in `kernel-services/src/platform.rs:57,62` — a *peer* service crate of `core` —
  so until they move, this tier is a convention rather than an enforcement.
- **Object-carrying witness** wherever a target is named: `Signalable<'ctx>` for `kill` and
  `terminate_task`; the resolved descriptor entry for everything descriptor-shaped.
- **Check alone suffices** where the operation's authority *is* the object: `DisplaySeat`
  and `InputSeat` gate the acquire and nothing else. Threading a display capability into
  `fb_flip` on top of `&Screen` is pure ceremony, and layering a per-syscall bit on a handle
  is the same design as the global owner stamp it replaces — ambient, last-actor-authoritative.

---

## Designation versus authority

**SlopOS keeps ambient authority, deliberately and permanently, at every syscall that names
a principal or a global by integer or by path.** Zircon can claim no ambient authority only
because it has no fork, no exec, no global filesystem namespace and no operation that names
a principal by number. SlopOS has all four. Invoking a syscall in a Linux ABI loses the type
and lifetime information of its arguments because they reduce to raw integers; no witness
crosses that boundary, and `Cap<R>` is an intra-kernel proof that a check on integers ran.

The model is Capsicum's hybrid, drawn by a derivable line rather than by taste: **authority
rides on the object wherever the operation names one; the capability set is the residue.**
Capsicum is the only capability retrofit onto POSIX that shipped and was adopted, at roughly
a hundred lines of application change against tens of thousands for the alternatives, and
its two load-bearing observations describe this design attempted in C in 2010 — rights
checked inside the descriptor-lookup function, and changing that function's signature so the
compiler finds the missed paths.

**The pid-naming surface is eight slots**: `kill`, `terminate_task`, `waitpid`,
`pidfd_open`, `getpgid`, `set_cpu_affinity`, `get_cpu_affinity`, `setpgid`. The last three
are already relation-enforced. There is **no ptrace and no `/proc`** — a class to keep
dodging.

Migrations, in priority order:

1. **`kill` and `terminate_task` → `Signalable`**, resolved once at the top of the handler.
   The relation check that must not wait for this plan is
   landed as the privilege-dominance rule in `syscall_kill`.
2. **`waitpid` → parent relation** (landed).
3. **`pidfd_open` must be authorized at mint time before anything moves onto it.** It
   carries two existence checks and no relation (`core/src/syscall/pidfd_handlers.rs:9-19`),
   so today it is a pid in a different encoding — routing `kill` through it would launder the
   designation problem into the object layer where it is harder to see. Fix, then add
   `pidfd_send_signal`.
4. **The pid table** — `plans/process-object.md` phase 4.
5. **`write`/`read` → controlling TTY**, which deletes `ConsoleIo`
   (landed).
6. **The path namespace** — the initramfs seal (landed).

**Rights on handles, when they land: in the table slot, never in the token.**
`Handle::from_parts` is `pub const` and its doc says forging is harmless because the table
validates slot and generation; `Handle<T>` is unconditionally `Copy + Send + Sync`. A
forgeable, freely duplicable token cannot carry authority. Rights go in `FdEntry`
(`fs/src/fileio/mod.rs:246-250`), tested at `get_fd_entry`/`snapshot_fd` (`:457-476`) —
this tree's equivalent of the descriptor-lookup choke point. Stamped from the creating
`Cred` at creation, immutable, travelling **with** the entry, and **never** re-derived from
the current holder's `Cred`; the memfd clipboard's fd handoff already exercises that hazard.

**The ring is not a capability bypass.** All 14 opcodes were checked: thirteen name a
descriptor and only `OP_OPENAT` names a path; none reaches power, display, signal or process
targeting. So the ring needs its opcodes classified, an opcode allow-list fixed at
`ring_setup` and monotone thereafter (`ring/src/ring_obj.rs:103` already carries
`owner_pid`), and `OP_OPENAT` routed through the same path rule as `fs_open`. A `Cred`
snapshot is *not* needed because no opcode consults a capability — and the reachability gate
is what makes that omission safe, since an opcode that ever reaches a capability class fails
the build. Never build an entry point that runs work under a credential other than its
creator's; that is a confused deputy by construction.

---

## What this does that other systems do not

Claims killed: "no other kernel ships ZST capability witnesses" (Tock shipped twelve such
traits around 2018 on millions of devices, though it mints with `unsafe impl`, which this
tree's zero-baseline contract-surface gate would reject — the *visibility*-based mint is the
stronger form); "no ambient authority" (unreachable under a Linux ABI); "authority
confinement" (seL4's is a refinement invariant over an abstract policy graph whose
well-formedness predicate *excludes* subjects holding grant rights — borrowing the phrase
for a monotone bitmask is how the confinement myth started); "delegation attenuates, as in
Genode" (Genode documents the opposite: the originator does not diminish its authority by
delegating).

What survives:

1. **Unforgeability by visibility plus CI, not by `unsafe`.** A private-field ZST in ostd
   whose only mint is a checker function, with macro-injected `unsafe` closed by
   `check_unsafe_expansion.sh` — which `forbid(unsafe_code)` is structurally blind to.
   *Caveat:* holds within the kernel only; nothing crosses the syscall boundary.
2. **Classification totality is a `rustc` error, with no script and no allowlist file**,
   because the capability value reaches the table through the handler so there is exactly one
   artifact. *Caveat:* covers the table, not reachability — `roulette_result` proves a
   slot-level gate can be green while an unprivileged reboot sits two syscalls away.
   Reachability is a script.
3. **A distribution ratchet on the capability set** — per-capability entry-point counts as
   `const` asserts, making the coordination whose absence upstream named as the cause of its
   catch-all a compile error rather than a decade-late measurement. *Caveat:* detects
   breadth, not over-broadness inside one entry.
4. **The dual-coding dilemma escaped.** Policy separate from code diverges; policy only in
   code loses the analysable artifact. Here the `const` table *is* the artifact and `rustc`
   checks it against the mint. *Caveat:* only because both come from one token — a
   hand-maintained parallel list would reintroduce the divergence, which is why no coverage
   gate file is built.
5. **Making permission-check static analysis unnecessary for the paths it covers.** A 2019
   analysis found 36 permission-check errors in one Linux release across three subsystems, 14
   confirmed, and the hook-placement problem is why such tools exist. A witness minted only
   by the checker and demanded by the primitive turns that into a compile error. *Caveat:*
   only where the terminal primitive is in ostd and takes the witness — today `Power` alone,
   and only after it moves.

---

## What this deliberately does not do

| Rejected | From | Why |
|---|---|---|
| uid/gid credentials | Linux | Documenting the model needed a finite-state automaton, and that work found inconsistencies *within* one kernel. Multi-valued mutable credential state is empirically unanalysable. The ABI stays **honest**: `getuid`/`geteuid`/`getgid`/`getegid` keep returning 0, `setuid`/`setgid` to any other id return `EPERM`, `capget` reports empty, `capset` returns `EPERM`. A hardcoded 0 with `setuid` unwired tells every ported program it is root and activates dead privilege-dropping code. |
| POSIX capability sets | Linux | The resource axis is finite and fixed, so no least-privilege statement about an object is expressible. |
| pledge-style promise sets | OpenBSD | Identical edit count to a capability column, and pledge's entire payoff is letting a program *you do not control* narrow itself. Every program in a SlopOS image is in-tree and launched with a fixed program identity. Kept: the failure-mode split. |
| Fuchsia's no-ambient-authority model | Zircon | `write` takes an integer; there is no rights channel at open time for a Linux program. Kept: rights on handles, and named seats. |
| Cred as a projection of the account tree | both research clusters | A quantity hierarchy re-parented for accounting reasons must not silently change who may signal whom. See below. |
| Type-level rights parameters on objects | Asterinas | Removed from its address-space, memory-object and inode handles across four commits after shipping in the published paper, netting roughly −800 lines. Never `Handle<R>`, `Cred<R>`, `TaskRef<R>`. |
| Rights inside `Handle<T>` | — | `from_parts` is `pub const`, its doc says forging is harmless, and the token is unconditionally `Copy`. Rights go in `FdEntry`. |
| Revocation apparatus keyed on signatures | macOS | Solves a third-party distribution problem SlopOS does not have. If revocation is wanted, interpose on the handle; never mutate the subject's word. |
| Consent prompts as the model | Android | Install-time prompts measured 17 % attention and 3 % comprehension. Kept: the structural half — hand back an object handle — which the memfd clipboard already does. |
| Cross-process capability revocation | — | It is what licenses the `Relaxed` cached-mask read. Every narrowing happens in the acting task's own context. |
| Killing another task's threads | Zircon | The one production capability kernel that shipped it removed it: locks left held including the heap lock, leaked stacks, inconsistent runtimes, and it defeats RAII cleanup. Independent confirmation of this tree's own rule that a task exits only from its own context. |
| A pluggable security-hook framework | Linux, Asterinas | Its own paper concedes policies do not compose in general and punts to the first-loaded module; SlopOS has nobody to punt to. Composition here is intersection over one vocabulary. |
| Reserved-empty capabilities | — | A capability with zero entry points is a dead exemption. Add them when an operation exists. |

---

## Phases

Phases 1–2 need **no** `Process` object. `cargo fmt --all`, then
`just build && just _iso-tests && just test` per commit.

### Phase 1 — the seats

Two commits. **1a:** `Screen` and `InputSink` single-holder objects with named seats and
arbiter revocation, a per-`FileKind` transferability predicate rejecting them in
`fileio_clone_file_ref` and in the descriptor-action arms, `close_on_fork` set — with both
frame-rate re-arms left textually unchanged. **1b:** move the pointer-focus re-seed to the
bottom half keyed on the seat holder, deleting the self-install.

*Gates:* `just boot` by hand, because the suite cannot see this. `utest!` for seat
exclusivity, seat non-transferability over fd passing, and virtcon fallback after primary
death.

### Phase 2 — classification, mechanical, everything `None`

The `Capability` enum in ostd; `cap: Capability` in `SyscallEntry`; the mandatory `cap(X)`
clause emitting `$handler::DEF`; `Unimplemented` for the 57 empty slots; the `const fn`
histogram and totality asserts; the dispatcher check between `syscall_lookup` and the
handler call; `authority=off|warn|enforce` with the kind-split failure mode; the 14 ring
opcodes classified with the opcode set fixed at `ring_setup`.

*Gates:* compile errors are the gate for coverage.
`scripts/check_authority_reachability.sh` with `--self-test` and `--emit-allowlist` for the
witness-call-site question. **Re-measure and re-emit `scripts/gates/stack/{dev,tests}.txt`
in this commit** — `syscall_select` sits at exactly 2200 bytes, capped at its measured size
in both variants, with zero headroom.

### Phase 3 — `Power`, the first real capability

Move the shutdown and reboot primitives out of `kernel-services/src/platform.rs:57,62` into
ostd behind `&Cap<'_, Power>`; classify `halt` and `reboot`; two-key the roulette reboot arm
against a boot-mask bit in the idiom `syscall_test_panic` already uses
(`core/src/syscall/test_handlers.rs:123-129`); enumerate the kernel-initiated callers
(kconsole, watchdog, panic path) in the reachability gate's tracked list.

### Phase 4 — the remaining classification

`ConsoleConfig`, `ConsoleIo`, `SysInspect`, `ClipboardGlobal`, `Fate`, `TestHarness`,
`DisplaySeat`, `InputSeat`. Pure classification over phases 1–3's mechanism. `SysInspect`
additionally defaults `process_list` to session scope.

### Phase 5 — `ProcSignal` and object designation

The `Signalable` family; `kill`'s four arms; `terminate_task`; the `getpgid` relation;
authorize `pidfd_open` at mint time; add `pidfd_send_signal`; keep the pid forms as a
resolve-once shim. Requires `plans/process-object.md` phase 4.

### Phase 6 — `Cred` and `Launch`

`RcuArcSlot<Cred>` on `Process`; the per-task `caps: AtomicU64`; the exec-intersection at
`core/src/exec/mod.rs:498`; `Cap<Launch>` bounding the grant; an authority-raising spawn
accepting no caller-supplied inputs; `FdAction::Open` redefined or deleted; the
`TASK_FLAG_SYSTEM` split.

**`TASK_FLAG_SYSTEM`, decided:** rename `0x08` to `TASK_FLAG_PANIC_FATAL` and **leave it in
`SPAWN_PRIVILEGED`**; delete `is_console_admin`/`require_console_admin`
(`core/src/syscall/context.rs:172,195`) and the `console_admin` macro arm. Exactly three
readers exist — one authority (whose sole consumer is `font_set`) and two reliability — and
exactly two setters, both kernel-only. **Do not retire `0x08` into `SPAWN_RESERVED`:**
`abi/src/task.rs` records `0x0040` as a retired bit that must not be reused, so retiring
`0x08` burns it forever and costs `0x0200` for no gain. The partition asserts and the
`EPERM` path are untouched. `launch_init` becomes the sole non-empty-`Cred` mint site —
exactly the kernel-only root that `Launch` wants.

### Phase 7 — descriptor rights

`rights: FdRights` on `FdEntry`, tested in `get_fd_entry`/`snapshot_fd`, stamped at
creation, immutable, travelling with the entry. No-duplicate and no-transfer subsume phase
1's ad-hoc kind predicate.

### Verus obligations — `verification/proofs/authority.rs`

House style: abstract `Step` enum, `_inv` predicate, mandatory load-bearing broken
witnesses.

- **S1 (monotone):** every step except `Exec` and `Spawn` preserves or shrinks `caps`.
- **S2 (exec):** `caps' = grant(image) ∩ caps`.
- **S3 (spawn bound):** `child.caps = if Launch ∈ parent.caps { grant(image) } else { ∅ }`.
- **S4 (drop totality):** `without` is total.

Required broken witnesses, each of which must fail `_inv`: a step that widens; an exec
keeping the grant un-intersected; an unauthorized launcher raising; a `Cred` replace adding
a bit; a drop that returns an error.

Record in `verification/STATUS.md` that the `RcuArcSlot` publication ordering and the cached
mask's republish are **audited, not proved** — Verus has no weak-memory model.

---

## Interface with the accounting plan

| Field of `Process` | Owner |
|---|---|
| `cred: RcuArcSlot<Cred>` | authority — immutable value, no interior mutability, trivial `Drop` |
| `account: AccountId`, `account_parent` | accounting — parent immutable after creation |
| `id` (generation-bearing) | authority — `plans/process-object.md` phase 4; `prlimit64` blocks on it |
| address-space handle, descriptor table | accounting owns capacity; authority owns per-entry rights |
| `parent`, `children`, `task_count` | shared; `plans/process-object.md` defines them |
| `caps: AtomicU64` (on `Task`) | authority — a cache; the `Cred` is the record |

**Authority is flat. The account tree is the single hierarchy and carries no authority.**
Both research clusters recommended making `Cred` a rights projection of the account tree
with `child.rights ⊆ parent.rights`. That conflates a quantity hierarchy with a reachability
relation, and the coupling runs the wrong way: re-parenting an account to raise a budget, or
merging two for a shared quota, would silently grant signal authority between principals
that had none, and a subdivision made purely for accounting granularity would partition the
signal graph. Zircon is not a counterexample — its job is the process-containment node, its
policy is a coarse deny-list ratchet explicitly *not* the authority model (the root policy
starts empty with allow as the zero value), and the fine-grained authority lives entirely on
handles. Borrowing the topology without that caveat imports the shape and drops the reason
it is sound.

`Cred` needs no hierarchy: it already gets monotonicity from exec-intersection plus the
spawn bound. Where a relation is needed it is the existing
`Task → KArc<ProcessGroup> → KArc<Session>` strong DAG, which exists for authority reasons
and has unforgeable identity.

Three cross-plan constraints:

- **`prlimit64` names a target pid and is therefore a new confused-deputy surface.** It must
  not be specified before `plans/process-object.md` phase 4, and it lands with
  capability-free relation authorization: same-process free, a foreign target needs the
  relation.
- **A `Cred` `Drop` must stay trivial**, so it may not own an account reference. The
  `Process` holds both side by side.
- **Fixing a capacity does not fix an authority bug.** The descriptor-table exhaustion
  fallback redirects into a more privileged domain; growing the table hides it. The redirect
  goes first (landed: the lookup refuses a pid with no table).

---

## Residual risks

1. **The Wheel of Fate's reboot.** Decided: `Cap<Fate>` on the spin syscalls, and the reboot
   arm additionally requires a boot-mask bit — default on for an interactive boot, off in
   `tests=on` images — mirroring the kconsole destructive-command mask. The alternative is
   deleting the reboot, which changes the joke. Settle before phase 3.
2. **`Launch` is global rather than per-image.** One capability, two grantable images, both
   display-related. Revisit if the grant table grows past about four entries.
3. **exec is not a revocation boundary for memory** and the plan says so explicitly rather
   than pretending. Every authority-bearing mapping must be close-on-exec.
4. **`syscall_select` has zero stack headroom** — 2200 bytes, capped at measured, in both
   variants. The witness is a ZST and free, but a `Result`-returning check adds a temporary
   in an unoptimised build. Re-measure in phase 2 or the build fails.
5. **`Power`'s kernel-initiated callers are gate-enforced, not compile-enforced.** kconsole
   commands register from `mm`, `sched`, `core` and `boot`, so the second mint must be public
   and its callers held to a tracked list. That is the one place the compile-error claim has
   a documented seam.
6. **A capability can be over-broad while passing every count.** The distribution ratchet
   detects breadth, not scope creep inside one entry. Mitigation: `SysInspect` is split from
   every mutating class on day one, and the admission test is applied at review.
7. **`Cred` replacement on a multi-tasked process returns `EBUSY`** rather than running a
   quiesce protocol, because a signal-based broadcast is what three upstream mechanisms had
   to build and then retrofit. If a real workload needs a live drop, the quiesce is later
   work, not a broadcast emulation.
8. **The honest ceiling.** After every phase, SlopOS still authorizes integer arguments
   against a credential attached to the caller. That is Linux's shape with enforcement Linux
   does not have. Do not write "object-capability system" or "no ambient authority" into any
   document.
