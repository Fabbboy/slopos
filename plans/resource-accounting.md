# Resource accounting

**Every kernel allocation is charged to an `Account`, and the charge is a linear token
that lives inside the thing it accounts for.**

One node type. An account is a row in a fixed `.bss` arena, named by a
generation-stamped id, one per `Process` plus one root owned by the kernel, whose parent
edge is set once at creation to the spawner's account and never re-homed — so the
accounting tree *is* the spawn tree, no syscall mints an account, and charge migration is
unrepresentable. `try_charge` debits the leaf and every ancestor and hands back a linear
`Reservation`; the charged object's only constructor consumes that reservation and stores
a `Charge` in a private field; the same `Drop` that releases the registry slot refunds.

Because `slopos-ostd` is the only crate permitted `unsafe` and macro expansion is
CI-scanned, no service crate can forge, clone or bypass a `Charge` — so "the counter is
incremented before the object exists" is a compiler-enforced fact rather than a review
rule. `Charge::drop` touches nothing but atomics on `.bss` and holds no counted
reference, so it is legal from a hard IRQ, from under a cli-spinlock, from the IRQ-off
switch tail, and from a dying task's own unwind.

Repeatable form: *one account per process, parent fixed at spawn, hierarchical debit,
over-committed ceilings, the charge lives in the object, refund is atomics-only, and the
id is generation-stamped so a stale refund is a defined no-op.*

**Depends on `plans/process-object.md` phases 1–3** for the account's owner and for a
process identity that does not recycle. `plans/kernel-hardening.md` lands first, because
two of its items are what stop the tables lying about their capacity.

---

## Why there is nothing today

Zero per-process counters exist for any kernel object. A case-insensitive search for the
whole rlimit/quota/ucount/racct family returns nothing in kernel code. Every object table
is a fixed global array, and one unprivileged process can exhaust any of them. Exhaustion
is graceful everywhere — a typed errno or `None`, never a panic — so the failure mode is
**permanent silent denial**, which is why none of it appears in a 2819-test suite that
contains exactly one drive-to-full test and no cross-process denial test at all.

A per-process page counter existed and was deleted for having 21 writes and zero readers.
That is the governing lesson, and it is a rule below, not a footnote.

---

## The nine questions, answered

**1. What is the principal?** The `Account`, one per `Process`, named by a
generation-stamped `AccountId`. Not the pid: ids recycle with no generation. Not the
`ProcessGroup` or `Session`: `TASK_FLAG_NEW_PGRP` is user-settable and passes through
`validate_spawn_flags`, so a fresh group is one spawn away, and `fork(); setsid()` mints
unbounded sessions — a pgrp- or session-keyed quota is escapable in two syscalls. There
is no uid to fall back on (`getuid`/`geteuid`/`getgid`/`getegid` all return a literal 0).
FreeBSD keys jail accounting on the jail's *name string* and a recreated jail inherits
the previous occupant's counters; that is the same hazard, in production, and the
generation stamp is what avoids it.

**2. Where do the numbers come from?** Measured, never chosen, in
`scripts/gates/quota/<variant>.txt`, in the tracked-allowlist idiom the
stack/vector/lockdep gates already establish (a dead entry fails, `--self-test`,
`--emit-allowlist`). **Two numbers per kind in two places**: the enforced runtime default
and the gate ceiling. Deriving the enforced default from a boot-time value is how Linux
shipped limits that could not subsequently be raised. Every row carries its own
`peak: AtomicU32`, because a dump-time `used` is not a peak. `HandleTable::high_water()`
already exists and can start reporting before any of this lands. Windows and XNU
independently concluded caps cannot be chosen a priori — one ships `PeakJobMemoryUsed`
explicitly so an operator can derive a limit, the other tracks a running maximum on seven
ledger entries.

**3. `RLIMIT_NOFILE`?** `FILEIO_MAX_OPEN_FILES = 32` is already below what this tree's
own compositor needs, so it is raised in `plans/kernel-hardening.md` item 3 *before*
anything is declared or charged. Then: soft 64, hard 256, published through `prlimit64`
— **but never before the enforcement point exists.** A limit that reports success and
enforces nothing actively defeats userland self-limiting; Asterinas ships a 16-entry
resource table with ten limits having zero enforcement sites, and Redox reports
`RLIM_INFINITY` for everything.

**4. Remote-driven charges?** A remotely-triggered allocation is **never** charged to a
local principal's general quota. Half-open state belongs to the passive path with its own
small bound consulted at demultiplex time — Escort's conclusion, from the observation that
an attacker can consume every port before a single message reaches a user process.
Charging it to the listener's principal converts a SYN flood into a remote exhaustion of
that principal's entire budget. So: the SYN queue keeps a fixed cap charged to nobody, and
a connection joins the accepting process's account **at accept**. Wiring that queue is
`plans/kernel-hardening.md` item 5 and is a precondition, not a parallel task.

**5. Where does the storage live?** Three-way split.
*Pure data* — `ResourceKind`, `KIND_COUNT`, the sealed axis marker types, default limits,
errno mapping — in `abi`, which carries no `#![feature(...)]` at all and is depended on by
26 crates including userland-side ones, so no nightly gate may be added there.
*Mechanism* — `Account`, the arena, `try_charge`, `Reservation`, `Charge`, the sealed
`Charged` trait, and the relocated `FileBacking` — in `slopos-ostd`, because
`check_safe_contract_surface.sh` scans only `slopos-ostd/src`, so putting anything that
produces or consumes a token in `kernel-services` would move contract surface *out of the
ratchet's sight*. Only the declarative `requires(quota(...))` plumbing and per-call-site
errno wiring belong in `kernel-services`.
All safe Rust, so `tcb_ratio` (currently 0.524 %, hard gate 1.0) only falls.

**6. Fail the syscall or deny the object class?** Fail the syscall, with the errno each
call site already returns — all four are already correct in-tree: `fork` → `EAGAIN`,
per-process descriptor → `EMFILE`, registry full → `ENFILE`, vm-clone → `ENOMEM`. Mint no
new code: `IntoSyscallResult` clamps unrecognised negatives to `EINVAL`. The object-class
policy bitmap is **cut from this plan** — it needs a privilege principal, a delegation
path and a set-policy syscall, none of which exist here; it belongs to
`plans/authority-model.md`. Added instead: a **`quota=warn` cmdline tier** in the
`lockdep=off|warn|panic` idiom, so the numbers can be measured on a system that does not
die at the first over-limit.

**7. Memory accounting?** Yes, in two tiers, plus the tier the inventory omitted, and with
**no per-page owner metadata**. Tier 1 is one token per countable object. Tier 2 is one
`Charge<Pages>` per VMA/region, consume-and-reissue on growth, refunded by the region's
own `Drop` with the exact count it holds — which deletes the deleted counter's
`total_freed + unmapped_pages` error-path arithmetic outright. Rejected: exact per-page
sharing attribution and everything downstream of it. Zircon bought that exactness with an
O(1) → O(#parent_pages) regression on child-VMO creation and a versioned ABI break;
Linux pays 8 bytes per page unconditionally and it is still the unfixed zombie-cgroup
pinning source; L4 measured its mapping database at 25–50 % of kernel memory *without* an
adversary. This tree already ate the analogous bug once, sizing per-frame metadata by the
highest MMIO frame and spending 4 GiB on an iGPU BAR. Also rejected for the first phases:
an RSS-shaped resource — illumos declined to put it in its synchronous framework at all,
and XNU needed a kill daemon, an exception type, a 20-entry tag taxonomy and a
page-granular panic leeway to make its version work.

**8. Per-process task cap?** Yes, and it is the missing bound: `MAX_TASKS` is 8192 global
with no per-process bound, while `MAX_PROCESSES` at 256 is hit first. **Two** kinds, as
both illumos and FreeBSD independently concluded: `Task` and `Process`, charged to the
same leaf. `CLONE_VM` siblings share one Process and one Account with no new check —
`task_clone` already gives a `share_vm` child the parent's `process_id` and
`process_vm_handle`, and `CLONE_FILES`/`CLONE_SIGHAND` without `CLONE_VM` are `EINVAL`.
**Placement matters:** the task charge must not live in `Task`, whose destruction is
deferred to the graveyard — a `Drop`-refund there means a process that exits a thousand
tasks keeps a thousand charged until the drain, producing spurious `EAGAIN` on fork under
exactly the load the quota exists to bound. The tree already avoids this for its own
count, adjusting `num_tasks` at `exit_cleanup_mark(TASK_EXIT_CLEANUP_ACCOUNTED)`. The task
charge is adjusted at the same latch.

**9. Compositor limits?** Not special-cased. The three-way numeric inconsistency that
makes them look special is fixed in `plans/kernel-hardening.md` item 3. One thing must be
recorded rather than discovered: **`unix_connect` already implements client-donates-to-
server by accident** — the *connecting client's* syscall allocates side B's slot, the pair
entry and both 16 KiB FIFOs (`net/src/unix_socket/mod.rs:271-288`). Make that donation
explicit with a comment saying why, or a future cleanup that moves the allocation to
`accept` silently flips 32 clients' worth of kernel storage onto the compositor's budget.
That is Genode's confused-deputy denial of service, verbatim.

---

## Capacity: boot-sized, declared, or charged — never merely present

Deleting the global caps and making every table growable is the wrong shape: allocating on
the charge or free path is the deadlock class this tree has already shipped once, when a
slab lock was held across a lazy-unmap drain. The allocator is where every subsystem
meets.

But compile-time constants are equally wrong for a general-purpose system. No usable
desktop OS has a hard 256-process, 16-mount, 64-pipe, 256-open-file ceiling.

**The rule:**

- **Boot-sized** is the default for object tables. The spine is allocated once at boot,
  off-lock, sized from measured RAM, and never reallocs — so the lock-free-scan contract
  and the "nothing here may allocate" cli-lock discipline both survive untouched. This is
  already proven in-tree: `ensure_registry_allocated`
  (`sched/src/task/task_table.rs:372`) allocates the 8192-slot task spine once outside the
  manager lock and never grows, precisely so "every mutation under the cli-spinlock is a
  plain slot write". Linux sizes its descriptor, dentry and inode tables from RAM at boot
  for the same reason.
- **Charged** is the per-principal ceiling: an account row, not an array bound. The sum of
  the per-principal limits is what bounds the boot-sized spine.
- **Declared** is the residue: a fixed array with a measured peak and a written rationale
  in the gate file. Three land here and are named individually rather than waved at:
  - `VM_SLOT_ALLOC` (`mm/src/process_vm.rs:141`) — an *allocator*, not a capacity limit,
    and `.bss` because "nothing here may allocate". Reshaped to the same
    allocate-spine-off-lock pattern so the `.bss` contract holds at a boot-sized width.
  - `EventBus`'s static `WaitQueue` arrays — genuinely fixed, and they need restructuring
    regardless (`plans/kernel-hardening.md` item 12: `queue_for` does `% MAX_SOCKETS`, so
    AF_INET sockets 0 and 64 share a wait queue once the slab grows past 64).
  - `PACKET_POOL` — correct as it stands, charged to the root at boot.

`HandleTable` already has both modes, and the growable one is already in production for
pipes, AF_UNIX and the AF_INET slab, so this is a re-sizing and re-keying job rather than
a new mechanism.

---

## The type design

**Named `Account`.** Not `Ledger`: `slopos-ostd/src/wl_currency.rs` opens "The Wheel of
Fate's Ledger" and is itself a global balance adjusted at the syscall boundary, and two
ledgers mutated at the same boundary is a collision a search cannot disambiguate. Not
`Job`: `job_control.rs` owns that word for `Session`/`ProcessGroup`. "Ledger" survives as
the name of the whole tree and of the `kcommand` that dumps it.

### Kinds — in `abi`, pure data

```rust
// abi/src/quota.rs
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    FdSlot = 0, ObjectRow = 1, Task = 2, Process = 3,
    Pages = 4, PinnedBytes = 5, Custody = 6, KernelMeta = 7,
}
pub const KIND_COUNT: usize = 8;

pub enum Unit { Count, Bytes, Pages }
pub enum Refund { OnDrop, OnExitLatch }   // per kind, never per call site

pub struct FdSlot;      pub struct ObjectRow;   pub struct TaskCount;
pub struct ProcCount;   pub struct PagesAxis;   pub struct PinnedBytesAxis;
pub struct CustodyAxis; pub struct KernelMetaAxis;
```

`Charge<const R: ResourceKind>` is rejected: an enum const-generic parameter needs
`adt_const_params` and `#[derive(ConstParamTy)]`, and `abi` carries no `#![feature]`.
Sealed marker types buy strictly more — an associated cost, an associated amount type, and
per-axis trait impls — for zero feature gates.

### The axis trait and the token — in `slopos-ostd`

```rust
mod sealed { pub trait Sealed {} }

pub trait ResourceAxis: sealed::Sealed + Copy {
    const KIND: ResourceKind;
    const NAME: &'static str;      // self-describing, so the dump needs no side table
    const UNIT: Unit;
    const REFUND: Refund;
    const COST: u32;               // cost is a property of the type, not the call site
    type Amount: Copy;             // () for count axes, so Charge is one word
}
/// Marker supertrait: only an axis whose resource is a reservation gets a refund path.
pub trait Refundable: ResourceAxis {}

/// Packed slot plus generation — the same shape as `Handle<ProcessVm>`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AccountId(u64);

/// Stack-only proof that headroom was taken. Drop refunds.
#[must_use]
pub struct Reservation<A: Refundable> { account: AccountId, amount: u32, _a: PhantomData<A> }
impl<A: Refundable> Drop for Reservation<A> {
    fn drop(&mut self) { refund_raw(self.account, A::KIND, self.amount) }
}

/// Resident charge. Mintable ONLY by consuming a Reservation, inside the charged
/// object's constructor. Private fields; no Clone, no Copy, no Default.
#[must_use]
pub struct Charge<A: Refundable> { account: AccountId, amount: u32, _a: PhantomData<A> }
impl<A: Refundable> Charge<A> {
    pub fn commit(r: Reservation<A>) -> Self;            // the only minter
    pub fn try_extend(self, r: Reservation<A>) -> Self;  // no `&mut Charge` exists anywhere
    pub fn shrink(self, n: u32) -> Self;
    pub fn amount(&self) -> u32;
}
impl<A: Refundable> Drop for Charge<A> {
    fn drop(&mut self) { refund_raw(self.account, A::KIND, self.amount) }
}

const _: () = assert!(core::mem::size_of::<Charge<FdSlot>>() <= 16);
```

The two-phase split is what makes the hierarchical rollback free. `try_charge` debits
level 0 upward; on refusal at level *k* it returns a partial `Reservation` whose `Drop`
unwinds levels `0..k`. Linux hand-writes that cancel loop and ships a warning plus a
repair store for when it goes wrong; Asterinas allocates a rollback vector, which is
illegal here.

### The arena — `.bss`, no heap, no lock

```rust
pub const MAX_ACCOUNT_DEPTH: u8 = 8;      // bounded so every walk terminates

struct AccountRow {
    used:    [AtomicU32; KIND_COUNT],
    limit:   [AtomicU32; KIND_COUNT],
    peak:    [AtomicU32; KIND_COUNT],     // the gate reads this, not `used`
    denials: [AtomicU32; KIND_COUNT],     // a refusal nobody can see is a silent denial
    parent:  AtomicU32,                   // arena index, written once at creation
    depth_remaining: AtomicU8,            // parent - 1; creation refused at 0
    generation: AtomicU32,                // monotonic, survives test-scope reset
    live: AtomicBool,
}

pub fn try_charge<A: Refundable>(a: AccountId, n: u32) -> Result<Reservation<A>, TryChargeError>;
pub struct TryChargeError { pub refused_by: AccountId, pub kind: ResourceKind, pub errno: Errno }
```

`refund_raw` bounds-checks the slot, compares the generation, and on match walks at most
`MAX_ACCOUNT_DEPTH` ancestors doing `fetch_sub`. **On generation mismatch it is a
deliberate no-op**, because the row was zeroed when the slot was released. No lock, no
allocation, no free, no wait.

**No account operation may take a lock.** That is a property of the layout, not a
preference: a lock on the charge path takes an inbound edge from every charge-site class at
once — the TCP shard, `UNIX_STATE`, `PROCESS_VMS`, the descriptor-table slot, the ring
registry, memfd, signalfd, the vnode table — and any path holding the account and then
touching a subsystem lock closes a cycle. Charging walks *up* a bounded chain, so it can
take no locks at all. FreeBSD serialised its whole accounting framework on one global lock
and the answer eleven years later is still that it ships disabled by default.

**No `has_headroom(kind) -> bool` is exposed, ever.** A check-then-charge split is the
proximate cause of an entire multi-commit fix series upstream, including an off-by-one
that permitted one task past the limit. The `Reservation` is the only observation of
headroom.

**Bounded depth** is what makes every hierarchical walk terminating and stack-bounded, and
it gives a free nesting-order for a future depth-ordered disposition. Zircon fixes the
same bound at 32 and refuses child creation at zero.

### Coverage as a compile error, not a scanner

`trait FileBacking: Send + Sync {}` moves from `abi` into `slopos-ostd`. Verified
feasible: zero references from `userland/`, `slibc/`, `appkit/`, `slop-protocol/`,
`windowing/` or `slopos-rt/`, and all ten impl sites are in crates that already depend on
ostd.

```rust
pub trait Charged: sealed::Sealed { fn object_charge(&self) -> &Charge<ObjectRow>; }
pub trait FileBacking: Send + Sync + Charged {}
```

`Sealed` is implementable only by `#[derive(Charged)]` in `slopos-ostd-derive`. Then every
`impl FileBacking for X {}` fails to compile unless `X` carries a charge, the coercion
sites building `KArc<dyn FileBacking>` fail too, and a backing added years from now is
covered the moment it is written. A source scan cannot answer "does type X have a field of
type `Charge`" — every existing scanner is a line-local match with a fixed lookback — and
an expansion scan would fail *open* if the type were renamed.

### Constructors

Split by object size, not uniformly. Small objects — the ten `FileBacking` impls, registry
rows — take the token by value: `MemfdBacking::new(charge: Charge<ObjectRow>, handle) -> Self`.
Large objects — `Task`, `ProcessVm` — receive it through the existing post-allocation
handle, since `KArc::try_init(Task::init_invalid())` already exists and the charge is
written afterwards through `PendingTask`.

`fn new(r: Reservation<A>, ..) -> Result<Self, (Reservation<A>, Errno)>` is rejected for
large `T`: the `Ok` arm is exactly the by-value rvalue that `Initialised<T>`/`SlotPtr`
exists to prevent, against a 2048-byte frame cap measured on the *debug* ELF. Mark every
mint, refund and extend helper `#[inline]` and **never** `#[inline(always)]` —
`slopos-ostd/src/mm/init.rs:438-443` records the measurement: a 28-field initialiser is 248 bytes as
written and 2576 bytes with `inline(always)`.

### Drop-refunds, and the invariant that replaces a false claim

**`Charge` has a `Drop` that refunds.** The `ParkedTask` comparison does not transfer:
that type has no `Drop` because destroying in the wrong context is worse than leaking. For
a charge the polarity is inverted — refunding is atomics-only and harmless in any context,
and leaking is the permanent-silent-denial failure this plan exists to delete.

**"A missing refund is unrepresentable" would be false, and must not be written
anywhere.** Rust is affine, not linear: `mem::forget`, `ManuallyDrop`, `KBox::leak` and a
`KArc` cycle are all safe, and `mem::forget` is *already called four times inside the
`#![forbid(unsafe_code)]` `drivers/` crate* (`drivers/src/irq.rs:57-58`,
`drivers/src/touchpad/mod.rs:283-284`).

What holds instead is narrower and load-bearing:

> **A `Charge` lives in exactly one place for exactly the lifetime of the thing it
> accounts for.** Never `Option<Charge<_>>` — use a distinct uncharged type for the empty
> state, because `Option::take()` is a safe separation. No accessor returns `Charge<_>` by
> value. Field visibility is at most the defining module.

Under that invariant every safe way to lose the token also leaks the object, and a charge
on a leaked object **is correct** — a leaked memfd really does still hold its registry
slot. And because the id is generation-stamped rather than counted, a leaked charge is
self-healing: its amount is discarded when the slot is reused, rather than pinning a node
forever. The residual is a charge on an object freed without running drop glue, which
requires `unsafe` and is therefore confined to ostd.

### Why the account is a `.bss` row and not a `KArc`

A counted account reference inside a `Charge` would make the refund a potential last
release, and therefore a heap free. Verified contexts where a `Charge` drop actually
happens:

- `fileio_destroy_table_for_process` (`fs/src/fileio/fdtable.rs:140-159`) reached from
  `cleanup_current_task_after_switch` — **IRQs off**, on the idle stack, with a peer CPU
  possibly spinning IRQ-off in `while next_task.on_cpu()`.
- `task_terminate` holds a `PreemptGuard` across cleanup.
- A driver completion path drops a `FileRef` with `VIRTIO_NET_STATE.lock()` held.

None satisfies the three facts `drop_context.rs` requires. And the three obvious rescues
all fail: a context-asking `account_put` degenerates to "always defer" and duplicates the
task reclaim machinery for a node made of atomics; `call_rcu` **allocates**, and its
out-of-memory fallback runs `synchronize_rcu` inline, whose own comment says an ordinary
spinlock held across it would wedge the machine; and pinning the account for its
`Process`'s lifetime is falsified, because charges provably outlive their process — an
in-flight `SCM_RIGHTS` `FileRef` is owned by a `ConnectionPair` rather than by either
process, `PinnedUserBuffer`'s keepalive frames take a second reference *specifically* so
NIC DMA survives process exit, established PCBs are not reclaimed on exit, and a task
refused release sits in the graveyard.

A `.bss` row plus a generation-stamped id has **no release point**, so the context question
does not arise. Keep the process-to-account pin anyway, as a redundant invariant, so the
generation check is a tripwire rather than the primary mechanism.

Costs, stated plainly: a boot-sized `MAX_ACCOUNTS`; the generation counter must survive
`init_task_manager`'s test-scope reset for the reason `VmSlotAlloc::reset` preserves
`next_generation`; and `used` is a counter rather than a title, so the equality invariant
becomes a runtime obligation rather than a type-level one.

---

## Who pays

**Three axes, not two.**

| Axis | Payer | Minted | Refunded |
|---|---|---|---|
| **SLOT** — a descriptor number in a table | the **holder** of that table | `fileio_install_file_ref` | entry removal / table teardown |
| **OBJECT** — registry row plus backing | the **creator**, once, never moves | object construction | the `FileBacking` Drop that frees the slot |
| **CUSTODY** — an alias held by kernel state owned by neither party | the **sender** | enqueue | receiver installs, or the queue drops |

Custody is mandatory rather than a refinement: 8 in-flight descriptors × 2 directions × 16
pairs is 256 `FileRef`s held by no descriptor table at all, against a per-process limit of
32. Linux answered the identical hole with a per-*user* in-flight counter because no
process-scoped principal outlived the sender.

| Resource | Rule | Reason |
|---|---|---|
| descriptor table | SLOT → holder | raised first in `plans/kernel-hardening.md` |
| process address spaces | `Process` kind → spawner's account | boot-sized spine; the account bounds it |
| tasks | `Task` kind → the process's account, **adjusted at the exit latch** | the graveyard defers destruction; a Drop-refund gives spurious `EAGAIN` |
| TCP established PCBs | OBJECT → the **accepting** process, at accept | remote-triggered; inheriting the *listener's* principal is a wart not to copy |
| TCP listeners | OBJECT → the `listen()` caller | a local principal exists |
| TCP connection buffers | OBJECT → same account as the PCB | de-panic first (`plans/kernel-hardening.md` item 4) |
| SYN queue entries | **no account**; fixed cap at demux | charging a local principal is a remote denial of it |
| SlopRing registry | OBJECT → creator | — |
| AF_UNIX sockets / pairs | OBJECT → creator; **the pair and both FIFOs → the connecting client** | already donated by accident; record it or a cleanup flips it onto the compositor |
| AF_INET sockets | OBJECT → creator; `SO_RCVBUF` bytes → `Pages` on the same account | 64 sockets at `RECV_BUF_MAX` can drain the 256-frame global pool |
| input event queues | **pre-charged at registration, amount = full capacity** | `resolve_queue` is reached from the PS/2 IRQ handler — no principal, no errno path |
| futex waiters | SLOT → the blocking thread's account | the *namespace* stays shared; filling one bucket denies others, and no quota fixes that. Noted, not solved |
| memfd | OBJECT → creator once; **each mapping is a separate `Pages` charge on the mapper** | a per-alias object charge double-counts a shared memfd |
| pipes | OBJECT → the `pipe2` caller, charge in the **registry entry** | there are two backings releasing into one slot: a charge in each double-refunds, a charge in one refunds while the object lives |
| signalfd, vnodes, mounts, ICMP/UDP demux | OBJECT → creator / opener / binder | — |
| TTY | OBJECT → the `/dev/ptmx` opener (the master); `TtySlaveOpen` carries none | every slave fd aliases one `TtySlaveOpen`, so it is not per-fd. Reserve a slice for the root account |
| pinned memory | `PinnedBytes` → the ring owner; **keepalive frames mint a second independent charge** | the second pin deliberately outlives the ring; sharing one charge reproduces a known memory-lock bypass at the DMA boundary |
| `SCM_RIGHTS` in flight | CUSTODY → sender | — |
| ring in-flight `FileRef` | CUSTODY → ring owner | the refund site already exists |
| **page tables** | `KernelMeta` → the address space's account | the omitted tier |
| **task kernel + data stacks** | `KernelMeta` → the forking account | 8192 × 48 KiB, not table-shaped, the largest single unbounded denial |
| **slab backing pages** | root account at refill | do **not** give an account its own slab: per-cgroup caches measured 45–65 % utilisation upstream, and shared-slab accounting recovered ~40 % of kernel memory |
| kernel tasks, in-kernel sockets | **the root account, explicitly passed** | a lookup-failure default reproduces the kernel-descriptor-table fallback at account scope |

**The root account** is created at boot with its limit set from the **measured**
available-frame count, never from an infinity. Without it, "the sum of per-principal
bounds is the global bound" is vacuous at the top, and the tree is not *total* — so the
dump cannot be reconciled against the buddy allocator's committed pages and a discrepancy
reads as a known gap rather than a bug. Zircon buckets its own memory by subsystem and
attributes none of it to a job, so its hierarchy and reality disagree by an unbounded
amount; that is the thing to avoid.

**Charge-site context invariants**, with a `debug_assert!` tripwire in the mint function:
no charge site in hard-IRQ context, none in softirq, none in the RCU-callback tier. Every
remote- or IRQ-driven queue is pre-charged at the syscall that creates it, with amount
equal to its fixed capacity — so a full queue is a bound the owner already paid for, and
dropping an event stops being an accounting event.

**Placement prohibition.** A `Charge` or an `AccountId` may never appear in a
`#[derive(Pod)]` type, in a struct mapped into a user address space, in a DMA-visible
descriptor, or in a `link_section` registry entry. Windows put the owning quota pointer in
the pool block header and it became a documented arbitrary-refcount-decrement primitive,
mitigated only by a boot-random cookie.

---

## What this does that other systems do not

Claims that do **not** survive scrutiny and must not be written: "the charge lives inside
the charged object" (Windows NT has shipped a quota pointer in the object header since NT
3.x; FreeBSD carries a creator credential on map entries and objects); "an RAII token
whose Drop refunds" (Genode ships exactly that in production); "linear authority
accounting" (KeyKOS space banks, 1992, and seL4 untyped predate it); "a missing refund is
unrepresentable" (false in stable Rust, and refuted by four calls in this repo); "no
machine-checked quota proof exists" (NiStar's container quotas, including the equality
invariant and the finite root limit, are inside an SMT-checked noninterference proof).

What survives, each with its caveat:

1. **The token is unforgeable by the compiler plus CI, not by review.** Escort put the
   principal at the allocation site — the owner passed as an argument to the kernel
   allocator — and then needed a *runtime* policy check that the passed owner matched the
   caller, because C could not stop a caller passing someone else's. Here a `Charge` with a
   private field cannot be fabricated by `net`, `fs`, `mm`, `sched`, `ring`, `drivers` or
   `core`. *Caveat:* that is unforgeability, not linearity — see 4.
2. **Coverage is a compile error.** `FileBacking: Charged` with a sealed accessor means a
   new backing cannot be written without a charge. Genode's own capability-quota retrofit
   shipped knowingly incomplete and the gap persisted because nothing failed when it was
   missing. *Caveat:* covers the `FileBacking` family only; other kinds need the gate.
3. **The amount is carried, not recomputed.** FreeBSD recomputes a size at the refund
   site; this tree's own deleted counter recomputed the unmapped prefix. Same bug shape.
   *Caveat:* a token can still name the wrong amount, which is why L1 is an equality with a
   runtime audit.
4. **Double-refund is a move-checker error.** Windows needed a dedicated bug check for
   quota underflow; XNU needs a panic-on-negative flag compiled out of shipping kernels
   plus a per-entry drift histogram across 33 entries and a page-granular leeway. The
   `!Clone` half comes free here. *Caveat:* under-refund is not covered by types; the audit
   covers it.
5. **A leaked charge is self-healing**, because the account is named by a
   generation-stamped slot rather than a counted reference. The upstream answer to the same
   problem is a forwarding stub plus a reparenting pass on offline, and the page-cache half
   is still unfixed. *Caveat:* until the slot is reused the row is wrong, which the audit
   reports.
6. **A machine-checked proof that a failed charge is a no-op at every level, over a
   partial batch.** That is the property the hand-written cancel loop upstream is trying to
   have and occasionally fails to, and the property XNU's own rollup comment concedes it
   cannot offer. *Caveat:* the sequential skeleton only — Verus has no weak-memory model.

---

## What this deliberately does not do

- **Charge migration, in any form.** Rejected from cgroup v1 (14 years live, deprecated
  for cost: cache pages never moved, a failed trylock silently left the page behind, and it
  forced page-to-cgroup locking into both MM and FS code), from XNU (ownership change needs
  correction factors to undo double charges its own transferability makes possible), from
  Zircon pre-attribution-rewrite (attribution jumped on object death, manufacturing memory
  events with no physical change), and from FreeBSD (a credential change must re-walk two
  jail chains). The immutable `account_parent` makes it unrepresentable. *Narrow exception,*
  from KeyKOS's two destroy variants: on account release the outstanding amount moves one
  hop up the immutable chain, which is a no-op on every ancestor because they were already
  debited.
- **A separate account object distinct from `Process`.** Genode ran exactly this experiment
  — a RAM session as the account and a PD session as the domain — and merged them, paying an
  API-deprecation cost, because the relationship was one-to-one in every real system and the
  flexibility was needless complexity. Escort's two node kinds immediately bred a
  memory-transfer relation between them, which is where its bugs lived.
- **A bank holding title to storage instead of a counter.** The closest call. Title buys
  revocation, and revocation still needs the same reclaimable implementors a counter needs —
  so until those exist, title buys nothing at the cost of rewriting the frame allocator's
  signature everywhere. What the argument *wins*: `KernelMeta` is a mandatory tier rather
  than an omission; the charge site must name the account **explicitly as an argument** at
  frame allocation and never resolve it from `current()`; and one reclaimable class must
  land, scheduled, in the final phase. Revisit if `KernelMeta` lands early — a witness on
  the frame allocator with `current_frame_allocator()` made crate-private is most of a bank
  already.
- **Per-page or per-frame owner metadata.** See question 7.
- **Kill as a memory-enforcement lever.** It collides with the discipline that a task only
  exits from its own context. For the one unrefusable case — a demand fault, which has no
  syscall return path — the disposition is a **signal, so the task unwinds itself**, with a
  depth-ordered subtree kill as the bounded last resort. Never a cross-CPU teardown.
- **Exception- or unwind-based quota failure.** Windows raised on quota exhaustion by
  default, then made not-raising the preferred flag, then deprecated the family entirely in
  favour of returning null. Matters concretely for a `panic=unwind` kernel.
- **A forced-charge escape.** `try_charge` is refuse-or-succeed with no third state, which
  is also what makes the equality invariant meaningful. The dying-task case is handled by
  making refunds unconditional and by **never charging on a teardown path**.
- **A "this counter may be wrong" flag.** FreeBSD needed one for 8 of 25 resources —
  exactly the shareable ones — and silently floors negatives at zero. The three-axis split
  makes those cases exact instead.
- **A check-only headroom predicate**, **a shared default account** (which upstream makes
  one system-wide counter for nearly every process), **an id-plus-global-hash-table
  identity**, **a garbage collector over shared references** (two implementations upstream
  and a string of CVEs including one exploited in the wild), **general peer-to-peer quota
  transfer** (Genode needs a write-once reference account, two-step routing through the
  common ancestor, per-session revert bookkeeping and peculation detection at close to make
  it safe), and **the object-class policy bitmap** (question 6).
- **Withholding the numbers from userland.** A Linux-ABI kernel must publish real limits
  through `prlimit64`, because a caller that cannot query a bound cannot back off
  gracefully. The adjacent discipline stays: the *gate's* numbers live in a tracked file,
  not in a syscall userland can pin.

---

## Phases

Depends on `plans/process-object.md` phases 1–3. `cargo fmt --all`, then
`just build && just _iso-tests && just test` per commit.

### Phase 1 — the mechanism, with one kind end to end

`ResourceAxis`, `Reservation`, `Charge`, `try_charge`, `refund_raw`, the `quota=warn`
tier, and **`FdSlot` only**. Lands with every reader in the same commit — see the rule
below.

Plus `scripts/check_charge_linearity.sh`, on `scripts/lib/gate_common.sh`: reject
`mem::forget`, `ManuallyDrop` and `.leak()` on charge-bearing types, `Option<Charge<_>>`
fields, `.take()` on a charge field, and any non-mint function returning `Charge<_>`.
`--self-test` plants each of the five and asserts exact hit counts **and** silence on the
forms it deliberately accepts; the inline allowlist's dead entries fail. Cite
`drivers/src/irq.rs:57-58` in the gate header as proof the escape is live.

### Phase 2 — move `FileBacking`, close coverage by compile error

`trait FileBacking` moves `abi` → `slopos-ostd` with the sealed `Charged` supertrait.
`#[derive(Charged)]` emits the private field, refuses to expand alongside `Clone`/`Copy`,
and registers a `.charge_audit_registry` entry — registered from `mm/`, `sched/` and
`core/`, **never** from ostd, whose `#[used]` static would break every userland link. All
ten backings gain their object charge, with the per-class placement rules above, especially
the pipe (registry entry, not either backing) and the PTY (master only).

### Phase 3 — the audit, the gate, the proof

An in-kernel `quotacheck` as a `kcommand`: walk the audit registry, sum live amounts per
account per kind, compare against `used`, print `charged/refunded/live/peak/denials`, and a
`stest!` that fails on any non-zero delta immediately after an exhaustion test. This is the
only mechanism that can see a forgotten or unwinder-skipped charge, and it is the runtime
form of the equality invariant. Plus `Account::drop`'s `debug_assert!(used == 0)` per kind
— which must be the *account's* Drop, not the charge's, or it trips
`check_drop_panic_free.sh`.

`scripts/check_quota_headroom.sh` + `scripts/gates/quota/<variant>.txt`, mirroring
`check_lockdep_headroom.sh` mechanically: one line per row at the three phase points
already wired (`boot`, `post-kernel-tests`, `post-userland-tests`); one full-line regex with
an explicit unparseable-line branch; a `min-kinds` floor and a peak-of-zero-fails rule as
the `min-records` analogue; `require-phase` with its unmatched branch, because a phase that
stopped reporting looks exactly like a phase that passed; dead-entry failure; `--log`,
`--emit-allowlist`, and `--self-test` over at least eleven crafted logs including a clean
accept as the positive control and `peak == cap` exactly (must pass).

**The gate cannot measure a desktop today.** Under `tests=on`, init calls
`run_userland_tests()` then `exit_with_code(0)`
(`userland/src/apps/init_process.rs:79-92`), and the roulette, compositor, shell and
terminal spawns are all *below* that exit — so no automated boot has a compositor listen
socket, client AF_UNIX pairs, or a desktop descriptor population, which are precisely the
tight resources. Add a deterministic session-smoke `utest!` inside the tests ISO that
spawns the compositor plus N clients and holds the peak population open across the
`post-userland-tests` dump point. Without it, do not claim to measure a desktop session.

`verification/proofs/resource_ledger.rs` — house style: a standalone Verus crate, flat
scalar state rather than a `Map`, `&&&` conjunct chains with obligation numbers in
comments, a `Step` enum with one variant per atomic-bounded operation each doc-commented
with its real `file:line`, then one `proof fn` per obligation, then the paired witnesses.

Steps: `TryChargeOk`, `TryChargeDeniedAtLevel(k)`, `RefundLive`, `RefundStale`,
`SubAccountCreate`, `SubAccountDrop`, `LowerLimit`, `SlotRelease`, `ExtendOk`,
`ExtendDenied`, `Shrink`.

Obligations:

- **L1 — equality, not inequality:** `forall i. used[i] == live_sum[i]`. An inequality
  cannot catch a phantom refund, which is the failure the token exists to eliminate.
- **L2 as a step property:** no successful `try_charge` leaves `used > limit`. The global
  form is false the instant `LowerLimit` exists, and upstream exhibits three incompatible
  behaviours in one kernel on exactly this point.
- **L3a:** no charge is refunded twice — delivered by the by-value consume, and the half
  that matters, since double-refund is the under-count. **L3b:** the quiescent equality,
  which the audit checks. "Refunded exactly once" is **deleted** as an obligation: it
  assumes `Drop` always runs, which is false for a fault frame the unwinder skips.
- **L4:** a denied `try_charge` is identity on every row, **including a batch that succeeds
  for k and fails at k+1**. The partial-batch unwind is the hard part.
- **L5:** a stale-generation refund is identity.

Five paired broken witnesses, each an `exists` with an explicit trigger plus a `forall`
showing the real step preserves it: hierarchical debit combined with committed child limits
(double-counts, violates L1); debiting `0..k` then returning `Err` without unwinding (L4); a
refund skipping the generation compare (L1); a `Clone`able charge (L3a); a
check-then-charge split (L2's step form).

Named as out of model, in the file header and by name in `verification/STATUS.md`: the
row's `fetch_add`/`fetch_sub` ordering and the refund-versus-slot-release ordering, because
Verus has no weak-memory model. Covered by KernMiri under both Stacked and Tree Borrows,
plus the in-kernel audit. Classify the module **Unaudited** alongside `handle` and
`wl_currency`, and update the obligation total in `verification/proofs/README.md` in the
same commit.

### Phase 4 — the remaining kinds

`ObjectRow` across the ten backings; `Task` and `Process` counts at the exit latch;
`Custody`; then **`PinnedBytes` early**, because it is the only genuinely unbounded
resource and it is a tier-2 charge, so it proves the hard tier rather than deferring it —
give `PinnedUserBuffer::pin` an `AccountId` in place of its bare recycled pid, and give the
keepalive frames their own second charge with its refund site in the driver's TX reclaim.
Then `Pages` per VMA.

### Phase 5 — `KernelMeta`, then reclaim

Page tables (three mint sites in `slopos-ostd/src/mm/page_table.rs`), task kernel and data
stacks, slab refill charged to the root, and the `unix_connect` FIFOs. Sequenced last so it
is not what shakes out the mechanism; in scope because it is plausibly the largest
unaccounted consumer in the tree.

Then **reclaim**, scheduled rather than deferred indefinitely: a `Reclaimable` trait with
two initial implementors that are provably safe to drop — the per-CPU stack-VA cache
(already a pool that can shrink to zero) and clean page-cache frames (the dirty bit already
exists) — plus the demand-fault disposition and the depth-ordered subtree kill as the
bounded last resort. Bounding acquisition without bounding holding time is a first-come
land grab with better bookkeeping, and Zircon is nine years of evidence that "later" means
never.

---

## The rule that killed the last counter

The deleted per-process page counter had 21 writes and zero readers. So:

> **A counter lands in the same commit as its readers.** All three of them: the enforcement
> point that refuses, an exhaustion test proving the refusal refunds exactly once and leaks
> nothing, and a `kcommand` that prints it.

The exhaustion-test shape already exists —
`core/src/syscall/tests.rs:3817` fills a process's descriptor table, attempts one more, and
asserts the failed mint drops exactly once by checking the strong count returns to baseline.
Generalise it per resource class.

**Test budget.** 18 resource classes × 3 (drive-to-full → exact errno; refusal refunds
once; cross-process isolation) plus about 6 for the account's own behaviour and the
`kcommand` ≈ **+55 to +62** KTAP-counted tests. Measure with
`TEST_COUNT_BASELINE=0 scripts/check_test_count.sh` and bump the baseline in the same
commit; never guess. Filtered runs that create files must include `'*ext2_aaa*'`.

---

## Residual risks, accepted knowingly

1. **Over-committed ceilings give no admission guarantee.** A parent may hand out child
   ceilings summing past its own; the bound comes from the debit reaching every ancestor.
   This is KeyKOS's sub-bank rule and it is deliberate — the reservation form needs an
   admission decision nobody can make correctly at fork time, and it guarantees slack. A
   ceiling can therefore be unreachable, and that is not a bug.
2. **`used` is a counter, not a title,** so the type system cannot verify it matches
   reality. A linear token guarantees the *token* is unique, never that the resource it
   names is — Theseus carried an overlapping-frames bug for four years on exactly this gap.
   Mitigations are L1 as an equality and the runtime audit. There is no third line of
   defence.
3. **A refund is not a page.** Tier-1 charges over the shared size-class slab restore a
   number while the page stays pinned by a stranger's object. Genode's rule is explicit that
   tracking amounts is insufficient and the allocations must come from independent backing
   stores. This tree does not do that, so **tier-1 charges bound object count, not bytes,
   and a bytes-denominated limit must never be enforced through the shared slab.**
4. **Refuse-only until reclaim lands.** Between phase 1 and phase 5 the quota bounds
   acquisition and not holding time, on a kernel with no shrinker, no eviction, no
   out-of-memory disposition and no swap. Attribution is still worth having, but the phase
   must not slip.
5. **Shared namespaces are covert channels regardless of accounting.** A bounded shared
   table is probeable by any process through its errno. Futex buckets are the in-tree
   instance: fill one bucket and deny every other process whose word hashes there. The fix
   is per-principal *namespace* partitioning, which is strictly stronger than a
   per-principal count, and out of scope.
6. **Zombie rows.** A charge outliving its process keeps a row's numbers wrong until the
   slot is reused. Bounded and self-healing, but the `kcommand` must list rows with live
   charges and no live process, or it is a number with no reader.
7. **The audit is not free.** Escort measured end-to-end kernel accounting at 8 %
   throughput, and 15–50 % under load with protection domains. The gate file should carry a
   throughput floor alongside the peaks, or a regression of that size passes.

## Open, needing measurement rather than decision

- Whether `check_stack_sizes.sh` passes on all three variants after the constructor
  changes. The token is 16 bytes and cannot itself move a frame, but the `inline(always)`
  scar is measured on the debug ELF and each variant has its own allowlist.
- Whether the lockdep edge and chain caps move. Charging takes no locks by construction, but
  the arena's creation path and the `unix_connect` donation change lock nesting.
  Re-measure over several runs.
- The real peaks. Every number here is a shape, not a value, and they are unknown until the
  session-smoke `utest!` exists.
- Whether `CLONE_VM && !CLONE_THREAD` should mint a fresh `process_id` sharing the vm handle
  — matching Linux, and making a per-`Process` count meaningful — or whether `Task` is the
  only countable kind at first. Today such a child gets the *parent's* pid and shares its
  descriptor table whether or not `CLONE_FILES` was requested. Decide deliberately.
