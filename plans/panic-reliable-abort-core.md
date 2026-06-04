# Reliable Abort Core — world-class fatal-fault / panic architecture

Status: design (researched against Linux, FreeBSD, XNU, Windows, Asterinas, Theseus,
Redox, Zircon, LLVM SafeStack). Replaces the ad-hoc panic path that recursively
faults (data-stack overflow) and wedges other CPUs (serial contention + TLB
shootdown spin), hiding the original fault behind "RECURSIVE PANIC DETECTED".

## Problem (observed)
A `panic!` from deep TASK context (RSP on the 32 KiB task safe stack) runs panic
`core::fmt` on the 16 KiB task **data** stack (SafeStack unsafe stack). It overflows
into the data-stack guard page → recursive `#PF` inside the panic path. A single
global `PANIC_IN_PROGRESS` flips → "RECURSIVE PANIC DETECTED" + HALT; the original
fault never prints. Meanwhile peer CPUs contend on the `SERIAL` spinlock and a TLB
shootdown initiator spins forever (`wait_for_acks`) on a wedged CPU.

## Root principle (from the research)
- **IST** (Linux/x86): hardware switches RSP unconditionally for fault vectors that
  may fire on a broken stack. SlopOS already does this (#DF/#SF/#GP/#PF on IST 1-4).
- **The SafeStack twist no mainstream kernel has:** there are TWO stacks. IST only
  switches RSP (safe stack). The **data/unsafe** stack must be switched in software,
  in the same naked entry, before any instrumented frame runs.
- **Stop-the-world first** (Linux `panic_cpu` cmpxchg + `crash_smp_send_stop`,
  FreeBSD `stop_cpus`, XNU `mp_kdp_enter`): one CPU is elected to drive the console;
  all others are NMI-stopped *before* printing — which also dissolves the TLB wedge.
- **Recursion guard degrades to format-free** (Linux `die_nest_count`/`bust_spinlocks`,
  Asterinas `IN_PANIC`): a fault while reporting drops to a `&'static`-only abort.

## Architecture: "Reliable Abort Core"
Composition over rewrite — SlopOS already owns ~80% (IST safe stacks, per-CPU
`EXC_DSTACK` data stack, RSP-derived `__safestack_pointer_address`, naked
`reset_ist_unsafe_sp`, `send_nmi_to_cpu`, `panic_abort_raw`, lock-free
`early_console`, `poison_all_held_locks`).

1. **Dual-stack emergency switch** — one new `#[unsafe(naked)]` ostd trampoline,
   `slopos_ostd::…::emergency::run_on_emergency_stacks(f: extern "sysv64" fn() -> !)`:
   `mov rsp, [PCR.panic_safe_sp]`; raw store `[PCR.ist_unsafe_sp] = PCR.panic_unsafe_sp`
   (re-prime the data-SP with NO instrumented epilogue to undo it — modeled on
   `reset_ist_unsafe_sp`); `jmp f`. boot/ (forbid-unsafe) calls the safe wrapper.
2. **Per-CPU guard-paged emergency SAFE + DATA stacks** — two new VA regions in
   `mm/memory_layout_defs.rs` (const-assert drift-guarded to ostd consts), mapped in
   `ist_stacks.rs` via the existing `Frame::alloc_zeroed` + `map_page_4kb` pattern;
   tops primed into new PCR slots before any IST selector is live.
3. **PCR additions** (appended after `ist_unsafe_sp` so asm-critical offsets ≤184
   stay byte-identical): `panic_safe_sp`, `panic_unsafe_sp` (`SyncUnsafeCell<u64>`),
   `panic_depth: AtomicU32`. `offset_of!`-derived offset consts; safe setters.
4. **Owner election + recursion guard** — replace the single global
   `PANIC_IN_PROGRESS` with `PANIC_OWNER: AtomicU32 = NO_OWNER`
   (`compare_exchange(NO_OWNER, cpu)`); first winner drives, peers quiet-`cli;hlt`
   (never touch the console). Per-CPU `panic_depth`: a 2nd fatal entry on the same
   CPU tails into `panic_abort_raw` (format-free floor).
5. **Format-free reporter** — runs on the emergency stacks using only the existing
   bounded writers (`MessageBuffer` 256 B, `HexBuffer` 32 B) + `early_console`. No
   `core::fmt` `Argument` array as the primary path (Inv. 5' 2 KiB frame cap is
   structural, not just headroom). `PanicInfo` parts stashed in statics before the
   bare-`fn` trampoline.
6. **NMI stop-the-world** — `send_nmi_all_excluding_self()` (new, mirrors
   `send_ipi_all_excluding_self` with `ICR_DELIVERY_NMI`) broadcast by the owner
   after winning, before printing. NMI handler gains a top branch: if
   `panic_owner_claimed()`, force-ack own TLB shootdowns,
   `poison_all_held_locks_no_halt()`, record stopped-ack, `cli;hlt` (never
   backtrace). Owner waits bounded (rdtsc deadline), proceeds on timeout.
7. **TLB-shootdown interlock** — `wait_for_acks` spin also breaks on
   `panic_owner_claimed()`; the NMI stop handler force-acks on the target side
   (set-only, never clear — preserves the no-reset invariant).

## Crate placement (framekernel-clean)
ONE new ostd module carries the only new `unsafe` (naked trampoline). PCR slots +
setters in ostd. New mm regions + const-asserts. boot/ and mm/ stay forbid-unsafe,
orchestrating via safe ostd surfaces. `send_nmi_all_excluding_self` in drivers/apic,
registered through an ostd safe surface (mirrors `register_send_nmi_fn`). Passes
`check_unsafe_outside_ostd.sh` + `check_stack_sizes.sh` by construction.

## Implementation phases
- **Phase A (diagnosability core):** PCR fields + emergency stacks + naked trampoline
  + owner election + per-CPU recursion guard + format-free reporter. Fixes "panic
  recursively halts and hides the real fault." Steps 1-4, 6 (stacks), 7.
- **Phase B (SMP stop-the-world):** NMI broadcast + panic-aware NMI handler + TLB
  interlock. Fixes the multi-CPU cascade. Steps 5, 8, 9.

## Test strategy (harness catches panics → uncaught panic exits QEMU)
- Caught-panic headroom (stest, in-harness): deep recursion → panic inside
  `catch_panic!`; assert harness records failure and continues.
- Direct-call mechanism unit tests (stest, no divergence): a test-only
  `run_on_emergency_stacks_returning(f)` asserts `read_rsp()` ∈ emergency-safe bounds
  and `ist_unsafe_sp == emergency-data top` inside `f` (proves BOTH stacks switched);
  guard-fault classifier returns `Some`/`None` at the boundary.
- Election + recursion (stest, pure atomics). NMI stop-handshake (stest, SMP, run
  last). TLB-interlock (stest). Guard-overflow degrade → `just boot-log` only
  (intentionally halts). Static: `check_stack_sizes.sh` + `check_unsafe_outside_ostd.sh`.

## Separate from this: the actual crash TRIGGER
`slop-protocol send_with_fd` does not loop on partial `sendmsg` (connection.rs),
unlike `send`. A partial write commits the SCM_RIGHTS fd but drops the frame tail →
byte-stream desync → a later fd-bearing message pops the WRONG fd → compositor maps a
mis-associated/freed memfd → the originating fault. Fix: loop the send (cmsg on the
first chunk, plain-send the tail); resync/close the FdFifo on any short-write+disconnect.
Deterministic repro: mock `sendmsg` short-count, assert the fd popped for the 2nd
message equals fd2. This is what STOPS the crash; the Reliable Abort Core makes any
future fault diagnosable.
