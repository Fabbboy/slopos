# Sharpen the lockup detector

SlopOS detects a wedged CPU by having each CPU watch the next one and NMI it if
it has not recorded a timer tick recently (`sched/src/runtime.rs:505`,
`check_watchdog_for_neighbor`). The threshold has been widened twice to stop it
firing on healthy machines:

- `WATCHDOG_PER_CPU_THRESHOLD = 250` (`:485`), raised to clear a font-change
  repaint that holds `VCONSOLE_STATE` across a full 4K framebuffer redraw.
- `WATCHDOG_RECOVERY_GRACE_MULT = 16` (`:502`), a documented ~40 s detection
  hole granted because a CPU unwinding a caught panic runs long with interrupts
  disabled.

Both are time graces: they buy freedom from false positives by giving up
sharpness. Neither is a tuning value to settle on — each is a defect in what the
detector measures, and one of them is a defect in something else entirely.

## The grace nobody needed

The unwinder is not inherently slow. It has no search index.

`Cargo.toml:97` selects the `unwinding` crate's `fde-static` FDE finder, which
resolves a return address by walking `.eh_frame` from the start and fully
parsing every FDE **and its CIE** until one covers the PC
(`vendor/gimli/src/read/cfi.rs:693`, `EhFrame::fde_for_address`). There is no
binary-search table to consult: `link.ld:46` emits `.eh_frame` and nothing else,
and `targets/x86_64-slos.json:7` sets `"eh-frame-header": false`, which is what
stops rustc asking the linker for one. No kernel variant carries an
`.eh_frame_hdr` section.

`raise_exception` resolves a frame in **both** unwind phases
(`vendor/unwinding/src/unwinder/mod.rs:127,160,207`), so an unwind of depth D
costs 2·D full scans. `scripts/build_kernel.sh:89` passes `--release` only for
the release variant, so `dev` and `tests` — the two that boot under `just boot`
and `just test` — do those scans at `opt-level=0`.

Measured against the real ELFs with the vendored gimli (host harness, not yet
confirmed in-kernel):

| ELF | `.eh_frame` | FDEs | linear @`-O3` | linear @`opt-level=0` | indexed |
|---|---|---|---|---|---|
| `kernel-tests.elf` | 1.54 MB | 44,751 | 1.37 ms | 47.3 ms | 1.5 µs |
| `kernel-dev.elf` | 1.08 MB | 31,252 | 1.26 ms | — | — |
| `kernel-release.elf` | 177 KB | 3,831 | 0.40 ms | — | — |

A 20-frame catch is `2 × 20 × 47.3 ms ≈ 1.9 s` natively and 19-38 s under TCG.
That is the 40 s window, and it explains why the problem is invisible in
release, which has 11.7× fewer FDEs.

## Steps

**R1 — Emit and use the index.** Deletes `WATCHDOG_RECOVERY_GRACE_MULT`
outright.

1. `Cargo.toml:97` — feature `"fde-static"` → `"fde-gnu-eh-frame-hdr"`. The
   indexed finder already exists at
   `vendor/unwinding/src/unwinder/find_fde/gnu_eh_frame_hdr.rs` and needs
   `__executable_start` and `__etext`, both already in `link.ld`.
2. `targets/x86_64-slos.json:7` — `"eh-frame-header": true`.
3. `link.ld` — add an output section before `.eh_frame`, or lld places it as an
   orphan:
   ```
   .eh_frame_hdr ALIGN(8) : { __GNU_EH_FRAME_HDR = .; KEEP(*(.eh_frame_hdr)) } :rodata
   ```
   `/DISCARD/`'s `*(.gnu*)` does not match it.
4. `scripts/check_registry_sections.sh` pins the ELF's section set — update it in
   the same commit or the build fails closed, as designed.

Confirm in-kernel before deleting anything: a `stest!` that panics at a known
depth inside `run_recoverable` and asserts elapsed `clock::monotonic_ns()` is
under a few milliseconds. Then delete `WATCHDOG_RECOVERY_GRACE_MULT`, the
`recovery_depth_for_cpu(target)` branch in `check_watchdog_for_neighbor`, and
`pcr::recovery_depth_for_cpu` if it has no other consumer.

**R2 — Measure progress, not elapsed time.** Two defects, one fix.

The current check is `current_tick − target_tick > threshold`: a global clock
against a per-CPU stamp, so the threshold has to be calibrated in wall time —
exactly what TCG's 10-30× slowdown and host steal time destroy. And
`check_watchdog_for_neighbor` is called from `scheduler_loop`
(`sched/src/runtime.rs:565`), the **idle** loop, so a CPU busy running a task
never checks its neighbour at all. The detector is substantially weaker than it
reads.

Adopt Linux's buddy-detector semantics, which do no clock arithmetic:

- Add `PCR.heartbeat: AtomicU64` as a tail field (`pcr.rs:250-253` documents that
  tail additions keep every asm-critical offset byte-identical). Retire
  `WATCHDOG_TICKS` (`sched/src/scheduler.rs:114`).
- Bump it from `scheduler_timer_tick`, where the `WATCHDOG_TICKS` store is today
  (`:2318`), before any lock is taken.
- Move the check into `scheduler_timer_tick` so it runs whether or not the CPU
  is idle.
- The checker keeps its own per-CPU `last_seen` snapshot and a `stale` count,
  and compares the target's heartbeat **against its own previous reading** — no
  clock. Fire after 3-5 consecutive unchanged samples.

At 100 Hz that detects in 30-50 ms instead of 2.5 s, and it cannot false-positive
under emulation or host steal time: a stalled host stalls the checker
identically.

**R3 — A touch API, and the discipline that keeps it honest.**

Add `slopos_ostd::watchdog_touch()`, bumping the current CPU's heartbeat, and
call it from the two legitimately long sections: each row of `render_all_cells`
(`drivers/src/tty/vconsole.rs:1341`), and the `panic_serial_write` loop plus once
per backtrace frame.

Write the rule next to the definition and hold to it:

> Touch only from a loop whose trip count is bounded by data already in hand,
> which acquires no lock and performs no wait. **Never from a wait loop.**

That rule is the whole difference between a progress heartbeat and a renamed
grace. A touch in a wait loop converts a real deadlock into a silent permanent
hang — it makes the CPU look alive precisely while it is doing nothing. This is
the documented failure mode of Linux's `touch_nmi_watchdog()` and the reason its
call sites are almost all bounded console/rendezvous loops.

With the repaint touching, `WATCHDOG_PER_CPU_THRESHOLD` drops to the small
consecutive-miss count from R2. Better still, snapshot the cell grid under the
lock and render outside it, so the repaint stops being an IRQ-off section at all.

**R4 — Let the spinner detect itself, and prove the cycle.**

The failure that actually happens is a CPU spinning on a ticket lock with IRQs
off. "No timer tick" is a weak proxy for that: one bit, arriving seconds late,
naming only the victim. A spinner is *executing* and can detect its own wedge
immediately — and SlopOS's ticket lock can distinguish the two cases a
test-and-set lock cannot: `now_serving` advancing means contention (benign),
`now_serving` frozen means the holder is wedged.

- Add `SpinLock.holder_cpu: AtomicU16`, set after acquire and cleared before
  release. It fits existing padding — `next_ticket: u16, now_serving: u16,
  poisoned: bool, level: u8` is 6 bytes — and there are no size assertions
  across its 245 use sites.
- Add `PCR.waiting_on: AtomicPtr<()>`, published before the spin, cleared on
  acquire.
- Bound the spin loop (`spin.rs:278-291`) on a `now_serving`-unchanged counter,
  not a wall clock. On breach, walk *me → lock I want → its `holder_cpu` → that
  CPU's `waiting_on` → …*. Returning to yourself is a **proof** of deadlock:
  print the whole cycle. No cycle means the holder is wedged elsewhere —
  escalate to the peer NMI.

Model the escalation ladder on the one already in `mm/src/tlb.rs:686-743`
(bounded spin → re-send → declare dead → dump held locks → NMI), which is the
same shape and already works.

The peer detector still has to exist: self-detection cannot see an IRQs-off loop
outside any lock, a wedge in the NMI/IST paths themselves, or hardware stalls,
and a spinner corrupted by the bug cannot be trusted to report. Sharp front line,
broad backstop.

**R5 — Stop making every threshold lethal.**

`nmi_watchdog_handler` always ends in `panic!` (`boot/src/idt.rs:388`). That is
the force that produced both graces: if every detection kills the machine, every
threshold must be tuned for zero false positives across every host. Split it
Zircon-style — first breach dumps (RIP/RSP/RBP, all CPUs' held locks, the
wait-for chain) and records an oops; sustained breach at a larger multiple
panics. Add knobs beside `panic.oops_limit=` (`boot/src/early_init.rs:553`):
`watchdog=off`, `watchdog.miss_threshold=`, `watchdog.panic=on|off`.

Fix the stale ">500 ms" comment at `boot/src/idt.rs:285` while there.

**R6 — Turn the lock-order validator back on.** Tracked separately in
`plans/lockdep-effectiveness.md`; sequence it against R4, which shares the
`holder_cpu` / `waiting_on` plumbing.

The two are complements, not alternatives: R4 catches cycles that actually
happen, at the moment they happen, with a printed proof. Lockdep catches the ones
that merely could, before they ever do. Neither subsumes the other, and R4 works
while the validator is off — which it is in every boot today.

**R7 — Deferred panic console (optional).**

Only worth doing once R1 lands and the UART is genuinely the top cost.
`early_console::write_byte` (`slopos-ostd/src/early_console.rs:46`) polls the LSR
then writes, so ≥2 port traps per byte. `fblog`'s 64 KiB ring
(`slopos-ostd/src/fblog.rs:33`) is already the recording half of an nbcon-style
split; add an emergency-priority direct path, and a `0xE9` debugcon sink for
emulator runs (one `out` per byte, no status poll).

## Open questions

1. **Does the FDE measurement reproduce in-kernel?** The table above is a host
   harness over the real ELFs with the vendored gimli. R1's own `stest!` settles
   it, and should land before anything is deleted.
2. **Does lld place `.eh_frame_hdr` correctly under a custom linker script with
   `:rodata` phdr assignment?** If the section lands wrong, the indexed finder
   reads garbage — worse than the linear scan. Verify the section address and
   `PT_GNU_EH_FRAME` before trusting it.
3. **What is the real worst-case IRQ-off section?** Record per-CPU worst
   observed duration in a counter and print at shutdown, then set thresholds from
   measurement rather than from bug reports. Zircon does exactly this.
4. **Does `.eh_frame_hdr` change the stack-gate allowlists?** The unwinder's
   frames (3864/2744/2616/2072/2072 in `scripts/gates/stack/dev.txt`) may shift
   or drop below 2048 once the finder changes; dead entries fail the gate by
   design, so expect the ratchet to fire.
