# Signal return: validate the XSAVE image, and make a ring-0 #GP survivable

`syscall_rt_sigreturn` reads a `SignalFrame` from the raw user RSP with no
cookie, no magic and no "a signal was actually delivered" check, then copies the
2688 bytes that follow it into the task's own `fpu_state` and immediately
`xrstor64`s them (`core/src/syscall/signal.rs:306-371`). Nothing inspects those
bytes between the copy and the instruction.

A ring-0 `#GP` from that `xrstor64` has no fixup: the exception dispatcher
consults a fault-recovery mechanism for exactly one vector, page fault
(`boot/src/idt.rs:585-589`), and vector 13 falls straight through to
`panic_handler_for` (`:615`). Panic recovery is structurally foreclosed here
because the dispatcher bumps interrupt nesting before any handler runs
(`:471`) and `panic_handler_impl` refuses to unwind in interrupt context
(`boot/src/panic.rs:201-204`).

So: three instructions from any process halt the machine.

```c
void *p = mmap(NULL, 8192, PROT_READ|PROT_WRITE, MAP_ANON|MAP_PRIVATE, -1, 0);
memset(p, 0xFF, 8192);
asm volatile("mov %0, %%rsp; mov $105, %%eax; syscall" :: "r"(p));
```

## What is already closed — do not re-fix

The classic sigreturn RFLAGS escalation **is already handled**. sigreturn commits
GPRs through `UserContext::set_regs`, which force-overwrites CS and SS with the
OSTD user selectors and runs RFLAGS through `sanitize_user_rflags`
(`slopos-ostd/src/user/context.rs:366-373`, `:39-61`). IOPL, IF, AC, NT, RF, VM,
VIF and VIP are all unsettable from userland and IF is forced on; TF and ID are
deliberately permitted. The IRQ-exit delivery path applies the same mask
(`core/src/syscall/signal.rs:465`). There is no `sigaltstack` and no
ucontext/mcontext surface, so `rt_sigreturn` is the only syscall that restores
CPU register state from user memory.

Signal numbers reaching `rt_sigaction` are bounded against `NSIG` by the
`Signum` newtype, and the sigaction path indexes the action table through the
checked accessors on `TaskInner`. That is not a hazard this plan needs to
carry.

The exposure is the XSAVE image and nothing else.

## What XRSTOR actually requires

SlopOS uses the **standard, non-compacted** XSAVE format exclusively: the only
save is `xsave64` and the only restore is `xrstor64`
(`slopos-ostd/src/task/fpu.rs:139-175`). `has_xsavec()` is detected but has no
consumer in the save/restore path — only a conformance test reads it. XCR0 is
X87|SSE, plus AVX where supported, plus the three AVX-512 component bits when all
three are supported, identical on every CPU
(`slopos-ostd/src/cpu/x86_64/xsave.rs:120-135`).

That narrows validation to the standard-format header at offset 512:

- `XSTATE_BV` must contain no bit outside the active XCR0.
- `XCOMP_BV` must be zero — a non-zero bit 63 selects compacted format, which
  this kernel never produces and `xrstor64` will fault on.
- The remaining 48 reserved header bytes must be zero.
- `MXCSR` (offset 24) must have no reserved bit set: `mxcsr & !MXCSR_MASK` must
  be zero, where `MXCSR_MASK` comes from the FXSAVE image at boot. This is the
  condition people forget, and it is a `#GP` on restore.

Two structural notes that shape the fix. First, `fpu_xrstor` is an `#[inline]`
Rust fn wrapping `asm!`, so it has no stable symbol band — unlike the two
existing fault-recoverable sites, which are `global_asm!` blocks with explicit
start/end/fault labels. Second, and more important:

**The scheduler restores the same buffer.** `prepare_switch_to` unconditionally
XRSTORs the next task's `fpu_state` with IRQs disabled
(`sched/src/scheduler.rs:482-487`). If sigreturn writes a bad image into
`fpu_state` and merely declines to load it, the machine dies at the next context
switch instead. **Validation must happen before the copy reaches `fpu_state`, or
the buffer must be repaired on rejection.**

## Design: validate first, fixup as the backstop

Neither half suffices alone.

Validation alone leaves the kernel one un-modelled `#GP` condition away from the
same halt — and one future XCR0 component away, since `xsave.rs:120-135` will
grow. It also cannot protect the scheduler's XRSTOR at all.

A fixup alone leaves the vector register file architecturally undefined on every
rejection, and converts a deterministic `EINVAL` into an IST round-trip that
userland can spam.

Linux ships exactly this layering — `copy_user_to_xstate` validates, and the
`__ex_table` entry catches what validation misses, with `force_sig(SIGSEGV)` on
failure. Both halves already exist here in usable form.

### Validation

Add `fn validate_xsave_image(bytes: &[u8; FPU_STATE_SIZE], xcr0: u64) -> bool` in
`slopos-ostd/src/task/fpu.rs`, checking the four conditions above. Call it in
`restore_fpu_from_sigframe` **on the copied bytes, before they are written into
`fpu_state`** — copy into a scratch buffer, validate, then commit.

The scratch buffer is 2688 bytes and the stack-frame ceiling is 2 KiB, so it must
be heap-allocated via `KBox::try_init` or written through the existing
`with_fpu_bytes_mut` accessor with validation performed in place before the
XRSTOR. Prefer the latter: validate the destination buffer after the copy but
before `fpu_restore_to_cpu`, and on rejection **reset the buffer to the
init-state image** rather than leaving it poisoned. That protects the scheduler
path for free.

### Disposition on rejection

Return `EFAULT` from the syscall and reset the FPU to init state. Do not silently
continue with a stale image.

There is an ordering problem to fix while here: sigreturn commits the
general-purpose registers *before* attempting the FPU restore
(`core/src/syscall/signal.rs:337-356` precedes `:364-366`), so a rejected image
today would leave the task resumed at the attacker's RIP and RSP with an
unrestored FPU. Validate the FPU image first, then commit GPRs, so the syscall
either fully succeeds or fully fails.

### The #GP fixup

Instantiate the existing idiom a third time. Both current fixups follow the same
shape: a `global_asm!` block with `.global` start/end/fault labels, an
`is_*_ip(rip)` range predicate, a `*_fault_ip()` accessor, and a branch that
rewrites `frame_ref.rip`
(`slopos-ostd/src/user/copy.rs:62-105`,
`slopos-ostd/src/arch/x86_64/kernel_ptr.rs:89-128`, consulted at
`boot/src/idt.rs:753-773`).

Move `xrstor64` out of the inline `asm!` into a `global_asm!` block with a symbol
band so it becomes addressable, then add one `#GP` consult in the dispatcher —
structurally identical to the `#PF` consult four lines away. On fixup, reset the
FPU to init state and return failure to the caller.

Defer generalising this into a sorted `__ex_table`. It is the right end state but
buys no additional attacker coverage over three range comparisons, and a
mis-sorted binary search consulted from IST context is a worse failure than the
comparisons it would replace.

## Phases

| # | Work | Done when |
|---|---|---|
| 1 | `validate_xsave_image` + call it in `restore_fpu_from_sigframe` before the XRSTOR; reset to init state on rejection | The three-line repro returns EFAULT and the machine survives |
| 2 | Reorder sigreturn so the FPU image is validated before the GPR commit | A rejected sigreturn leaves the task's registers unchanged |
| 3 | `xrstor64` moved to a `global_asm!` symbol band; `#GP` fixup consult in the dispatcher | An artificially-injected un-modelled `#GP` at that site is recovered rather than fatal |

## Tests

- `utest!` — the three-line repro: all-`0xFF` frame, expect `EFAULT`, process
  survives, machine survives. This is the regression test for the whole plan.
- `utest!` — a *valid* sigreturn still works: deliver a real signal, return from
  the handler, assert the FPU state round-trips. Guards against over-strict
  validation breaking normal signal delivery.
- `stest!` — table-driven `validate_xsave_image`: XSTATE_BV bit outside XCR0,
  non-zero XCOMP_BV, dirty reserved bytes, reserved MXCSR bits, and a known-good
  image from a real `xsave64`. Pure logic, cheap, and the place to encode the
  Intel SDM rules.
- `stest!` — poison a task's `fpu_state` through the sigreturn path, force a
  context switch, and assert `prepare_switch_to` does not fault. This is the test
  that proves the second XRSTOR site is covered.

Use the real `size_of::<SignalFrame>()` in any offset arithmetic rather than a
literal: it is 20 × `u64` = **160** bytes (`abi/src/signal.rs:200-228`).
