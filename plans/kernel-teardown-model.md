# Two teardown models, one kernel — spike

SlopOS runs `panic-strategy: unwind` (`targets/x86_64-slos.json:17`), so a panic
runs destructors. It also tears a task down from another CPU without unwinding,
so an asynchronously killed task's destructors never run.

Both models are load-bearing and they contradict each other. Every resource class
that can be held across a blocking call therefore needs a hand-written parking
spine so the resource survives a stack that will be abandoned — and that count
grows linearly with the kernel.

Invariant I8 is the tax:

> **No owning task handle may live in a stack frame that can deschedule.** SlopOS
> tears such a task down from another CPU without unwinding, so the handle is
> never dropped and the task leaks with its stacks and its address space.

I8 is not a safety rule that happened to be needed. It is the levy the abandon
model imposes, and it is levied on every future blocking kernel path.

## What each model costs today

**The unwinder** is 58,721 lines of vendored Rust (`vendor/unwinding` +
`vendor/gimli`), pinned by `scripts/check_vendor_pin.sh`, linked into
`kernel.elf`, exempt from the unsafe gate, and absent from
`verification/STATUS.md`. It carries 197 lines of `unsafe` that the TCB ratio
does not count.

Its metadata is in the binary: `.eh_frame` is **1,524,260 bytes** and
`.gcc_except_table` a further **326,712** — about 1.8 MB, against a 9.0 MB
`.text`.

It also weakens the framekernel's own soundness invariant. `check_stack_sizes.sh`
enforces Inv. 5' at 2 KiB, and four entries in its allowlist are unwinder frames
that exceed it — 3864, 2744, 2616 and 2072 bytes. The ceiling had to rise to
4 KiB to accommodate `panic=unwind`, and a 3864-byte frame is most of a
guard-page's distance in a path that runs *while the kernel is already failing*.

**The abandon model** costs the parking spines. `sched/src/task/pending_spawn.rs`
exists solely so a half-built task survives a kill that skips destructors; the
wait-reference map in `scheduler.rs` is the same shape for a different resource;
`assert_switch_preempt_safe` is the runtime backstop that turns an I8 violation
into a panic rather than a silent leak.

## The alternative

Adopt the rule every other production kernel uses: **a task only ever exits from
its own context.**

Deliver kill as a flag rather than a remote teardown. Make every blocking
primitive — `WaitQueue::wait_*`, sleep, `ring_enter` — check it and return
`-ERESTARTSYS`/`-EINTR`. The frames then unwind by *returning*, so destructors
always run, on the task's own stack, at a point the task chose.

The plumbing already exists for signals. `handle_erestartsys`
(`core/src/syscall/dispatch.rs:86`) already inspects the syscall return value,
consults the pending set, and decides between transparent restart and `EINTR`.
Extending that from signals to kill is an extension of a working mechanism, not a
new one.

If that lands: I8 evaporates, the parking spines delete, and `panic=abort`
becomes viable — reclaiming ~1.8 MB of unwind metadata, 58,721 lines from the TCB
annex, and the 2 KiB stack ceiling that Inv. 5' actually wants.

## Why this is a spike and not a work plan

The change is deep enough that the wrong sequencing turns the tree red for a long
time, and several questions cannot be answered by reading code.

**1. What is `panic=unwind` actually buying?** The README's claim is that a panic
is caught, symbolized, billed to the offending task's oops ledger, and the machine
keeps going. That is a genuinely valuable property and it is demonstrably working.
The question is whether it requires *unwinding*, or whether the same recovery is
achievable by terminating the task and reclaiming its resources through an
ownership model that does not depend on destructors running — which is what the
kill-as-flag model would give. Answering this needs the panic paths enumerated and
each one's recovery mechanism identified.

**2. Can every blocking site become interruptible?** The completeness audit found
that interruptibility is currently opt-in per call site, and that **pipes,
`waitpid` and futex opted out** — while the trusted crate's own documentation
names the pipe path as the exemplar of the discipline. Every one of those must
become interruptible before kill-as-flag can replace remote teardown, and some may
have a reason they did not.

**3. What replaces the cross-CPU kill's promptness?** Remote teardown stops a task
now. A flag stops it at its next blocking point or syscall return. A task spinning
in a tight loop in userland is preempted and will see the flag; a task spinning in
the *kernel* with preemption disabled will not. Enumerate those sites.

**4. Does the oops ledger survive?** Per-task oops accounting, taint flags and the
panic-in-flight bookkeeping are all built around a recoverable unwind. If
`panic=abort` lands, that machinery needs a new substrate or an explicit
downgrade.

**5. What does exception-context panic do?** Panic recovery is already
structurally foreclosed there — the dispatcher bumps interrupt nesting before any
handler runs and `panic_handler_impl` refuses to unwind in interrupt context. So a
fraction of panics already do not unwind. Quantify which, because that fraction is
the part `panic=unwind` is not buying anything for.

**6. Is there a middle position?** Keep `panic=unwind` for the syscall-context
recovery it demonstrably provides, but *also* adopt kill-as-flag so I8 goes away.
That gets the maintainability win without giving up the recovery property, at the
cost of keeping the unwinder. This may well be the answer, and the spike should
price it explicitly rather than treating the choice as binary.

## Prior art

**Linux** has no kernel-side unwinding at all. `do_exit()` runs only in the dying
task's own context, and `signal_pending()`/`TIF_SIGPENDING` checks in every
interruptible sleep make blocking calls return so frames unwind by normal control
flow. Fault recovery comes from `__ex_table` entries rather than CFI — the same
mechanism `plans/rt-sigreturn-xrstor.md` proposes instantiating a third time here.

**FreeBSD** does the same with `sleepq` and `PCATCH`.

**Windows** is the counterexample that proves the cost: it *does* have kernel SEH
unwinding, and pays for it in every frame.

**Theseus** is the interesting outlier and the closest to SlopOS's situation — it
is a Rust kernel that deliberately keeps unwinding, uses it for fault recovery and
live evolution, and accepts the metadata cost. Worth reading before concluding
that unwinding is the thing to drop; its argument is that destructor-based
cleanup *is* the ownership model, and abandoning stacks is what forces hand-written
spines.

## Spike deliverables

1. An enumeration of every panic path, which of them currently unwind, and what
   each one's recovery depends on.
2. An enumeration of every blocking site, whether it is interruptible today, and
   what it would take to make it so.
3. An enumeration of kernel sites that spin with preemption disabled and would not
   observe a kill flag promptly.
4. A decision on questions 1–6, with the middle position priced.
5. If the decision is to move: a migration order in which each step leaves the
   tree green, starting with making the three opted-out blocking sites
   interruptible — which is worth doing regardless of the outcome.

Item 5's first step is independently valuable: a `waitpid` or pipe read that
cannot be interrupted is an unkillable process, which is a defect on its own
terms whatever this spike concludes.
