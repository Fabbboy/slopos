# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
[`just`](https://github.com/casey/just) is the command runner; the `justfile` drives cargo + `rust-lld` via `scripts/`. Run `just --list` for all recipes. No git submodules — `scripts/ensure_limine.sh` fetches pinned Limine v12.3.1 into `third_party/limine` on first ISO build.

- `just setup` — install pinned nightly from `rust-toolchain.toml`; verifies Go >= 1.22 on PATH (for `tools/run_tests/`)
- `just build` — emits `builddir/kernel-dev.elf`; `just iso` regenerates `builddir/slop.iso`
- `just boot` (interactive) / `just boot-fast` (skips roulette) / `just boot-log` (non-interactive, 15 s timeout)
- `just test` — the CI/agent entry point (see Testing Guidelines)

Boot targets rebuild a secondary `builddir/slop-notests.iso` with `tests=off`; override via `BOOT_CMDLINE=... just boot`, add `VIDEO=1` for a graphical window.

## Knowledge Index (AI)
`knowledge/` hosts a local semantic index for querying the codebase. Build once with `python3 -m venv knowledge/.venv && . knowledge/.venv/bin/activate && pip install -r knowledge/requirements.txt && python knowledge/index.py`, then query via `python knowledge/query.py "<question>"` for signatures, drivers, or file locations. Rebuild after large refactors or merges. Never commit the venv or embedding database artifacts.

## Coding Style & Naming Conventions
All kernel code is Rust `#![no_std]` on nightly with `#![forbid(unsafe_op_in_unsafe_fn)]`. Keep unsafe blocks tiny and well-documented; prefer `pub(crate)` helpers and prefix cross-module APIs with their subsystem (e.g., `mm::`, `sched::`). Match the existing four-space indentation and brace-on-same-line style. Assembly sources (when needed) are Intel syntax (`*.s`) and should document register contracts.

### Comments
Write code that does not need comments. Most comments are useless: they restate the code, drift out of date, and add noise.

- Default to **no comment**. Express intent through naming, types, and structure instead.
- Comment only when the code genuinely cannot carry the meaning: a hardware/spec quirk, a non-obvious ordering or locking requirement, a deliberate deviation, or a *why* that is invisible from the diff.
- When a comment is warranted, keep it **concise** — one or two lines. No restating the signature, no narrating control flow.
- Treat the urge to comment as a smell. Needing one usually signals a hack or unobvious behaviour; prefer fixing the code so the comment becomes unnecessary. If the hack must stay, say why it exists, not what it does.
- Exempt from the above: `# Safety` sections, `///` public API docs, and register-contract notes in assembly. These are contracts, not commentary.

### Unsafe-code surface
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (the framekernel **AD-1/AD-2** discipline: one trusted crate holds all `unsafe`, every other kernel crate forbids it; CI-enforced by `scripts/check_unsafe_outside_ostd.sh`). Every other crate the kernel binary links (`abi`, `acpi`, `boot`, `core`, `drivers`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `keymap-core`, `ktesting`, `mm`, `net`, `pidfd`, `ring`, `sched`, `service-core`, `signalfd`, `video`, `vt`) carries `#![forbid(unsafe_code)]`, and `check_unsafe_outside_ostd.sh` asserts that from the binary's own dependency closure, so a new crate is covered the moment it is linked. Userland-side crates (`userland/`, `slibc/`, `slop-protocol/`, `appkit/`, `slopos-rt/`, `windowing/`) are out of scope for this discipline.

`forbid` is necessary but not sufficient: rustc drops any `unsafe_code` diagnostic whose primary span satisfies `in_external_macro`, so a macro defined in another crate expands `unsafe` into a forbid crate silently, and the call site holds no keyword for a source scan to find. `scripts/check_unsafe_expansion.sh` is what closes that — see below.

One documented exempt site exists outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct.

Three C-ABI entry points keep `#[unsafe(no_mangle)]` in `boot/src/ffi_boundary.rs` — `kernel_main`, `common_exception_handler`, `isr_iret_frame_corrupt`. Their callers are assembly, so the symbols have to resolve at link time; routing them through a registered hook would leave an uninstalled window across early boot in which a fault triple-faults instead of panicking. `check_unsafe_expansion.sh` allowlists them by name.

These gates enforce the discipline. The source-scanning gates run via `just check-framekernel` and CI (and from a build when `KERNEL_BUILD_GATES=1`); the two ELF-inspecting gates (`check_stack_sizes.sh`, `check_kernel_softfloat.sh`) run on every kernel build, for every variant. The source scans are kept off the default build path so the interactive boot loop stays fast.

Every gate carries a `--self-test`, run from `check-framekernel-gates` before the real scan. A check that has never been observed to reject has not been observed to work: the source scanners assert exact hit counts against planted violations *and* silence against the forms they deliberately accept, while the three ELF gates synthesise objects with `llc` (an oversized frame, a `movups`, an unblessed `link_section`) and assert the gate rejects them. A gate whose self-test fails is a build failure.

The build produces one ELF per variant — `builddir/kernel-dev.elf`, `kernel-release.elf`, `kernel-tests.elf` — because their codegen differs and a single shared path meant whichever build ran last silently answered for all three, to the gates, to gdb, and to the ISO builder. The two ELF gates take a required `--variant` and read their measured allowlist from `scripts/gates/{stack,vector}/<variant>.txt`. Those files also carry the input-sanity floors, so weakening a check is a diff on a tracked file rather than an edit to a script. Every allowlist entry must match something in the ELF being checked: an entry that matches nothing is a dead exemption and fails the gate, which is both the ratchet (a frame that shrinks hands its exemption back) and what makes a mis-stated `--variant` fail closed rather than pass on a list that happens to fit.

- **`scripts/check_unsafe_outside_ostd.sh`** — fails if any `.rs` file under a kernel crate (other than `slopos-ostd/`, `slopos-ostd-derive/`, or the exempt file above) contains an `unsafe` keyword that is not a comment, not the `#[unsafe(...)]` attribute form, and not `#[cfg(...)]`-gated. Mirrors `check_alloc_dep.sh`'s cfg-aware lookback. Also asserts that every crate in the kernel binary's dependency closure carries `#![forbid(unsafe_code)]` at its crate root, so a newly linked crate cannot start life without the lint.
- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency **and** fails if any kernel `.rs` file (other than `kernel/src/main.rs` and the `slopos-ostd/` tree) contains a bare `extern crate alloc;` / `use alloc::` / `use ::alloc::` statement (with `#[cfg(...)]`-aware lookback so cfg-gated usages that compile out of the kernel build are accepted).
- **`scripts/check_stack_sizes.sh`** — fails if any function in the kernel ELF has a stack frame larger than `STACK_SIZE_THRESHOLD` (default **2048 bytes / 2 KiB**, matching Linux mainline's `CONFIG_FRAME_WARN` default on x86_64/arm64 but stricter in enforcement — SlopOS fails the build, Linux merely warns). This is the load-bearing enforcement of SlopOS invariant **S-5** (bounded kernel stack use — named to avoid collision with the framekernel paper's own Inv. 5, *sensitive memory cannot be tampered with by user programs*). Driven by `-Zemit-stack-sizes`; inspects the final ELF's `.stack_sizes` section, so it catches NRVO failures, inlining, and trait-object dispatch that a source-level heuristic would miss. Above the threshold sits a second limit no allowlist can raise: the 4 KiB guard page. The target sets `"stack-probes": {"kind": "none"}`, so a frame larger than that steps clean over the guard in one instruction — a measured cap records how big a frame is, not whether that size is survivable. `min-records` is what stops a dropped `-Zemit-stack-sizes` from reading as a kernel with no large frames: `llvm-readobj` prints an empty `StackSizes [ ]` and exits **0** for an ELF carrying no section at all.
- **`scripts/check_kernel_softfloat.sh`** — fails if the kernel ELF touches XCR0-managed register state outside the sanctioned save/restore. The kernel must be built `+soft-float` so it never touches that register file: a syscall/exception entering from userland does **not** save the caller's FPU/vector state (only a full context switch does, via `xsave`/`xrstor`), so a single kernel instruction that disturbs it in a fault/IRQ path clobbers the interrupted user task's live registers. The scope is all four classes XCR0 enumerates, not just the vector one — x87 and MMX share one physical register file with XMM under XCR0 bit 0, and `xrstor64` overwrites the whole area at once — because XMM is only the instance rustc is likely to emit, and hand-written `asm!` is not subject to target features at all. The soft-float guarantee lives in `targets/x86_64-slos.json` (`features: …,-sse,…,+soft-float` + `rustc-abi: x86-softfloat`) — **not** in `.cargo/config.toml`, because a `RUSTFLAGS` env var fully overrides `target.*.rustflags`. The `slopos-ostd` xsave-conformance helpers in the `kernel/tests` build are one allowlist entry with a measured instruction budget rather than a whole-binary exemption, so a vector instruction anywhere *else* in the tests kernel still fails.
- **`scripts/check_unsafe_expansion.sh`** — expands every kernel crate with `-Zunpretty=expanded`, over each crate's feature configurations, and holds the result to a constant rather than a recorded count: zero executable `unsafe`, `unsafe impl` only of an allowlisted trait, `#[unsafe(link_section)]` only of a `link.ld` section, `#[unsafe(no_mangle)]` only of an asm-called symbol. This is the only mechanism that sees macro-injected `unsafe`; `forbid` and the source scan are both blind to it. A golden fixture fails the gate if a toolchain bump moves the compiler's own emitted shapes. ~16 s warm.
- **`scripts/check_process_designator.sh`** — fails if a process-keyed table entry point (`mm/src/process_vm.rs`, `fs/src/fileio/`) takes a bare `u32` process id, or if a lock-free scan for a matching id grows back. Ids recycle, so a `u32` parameter is a confused-deputy surface: a stale one designates whichever process holds that number *now*, and the kernel services the call against a stranger's address space or open files. The replacements — `slopos_ostd::process::ProcessId` and `slopos_fs::fileio::FdTable` — carry a generation and can only be built from a live process, so a stale one fails the check instead of resolving. Scope is deliberately narrow: a `u32` pid is still correct at the ABI boundary (`getpid` returns one, the PCR carries one across a syscall); what must not happen is a *table lookup* keyed on one.
- **`scripts/check_registry_sections.sh`** — holds the kernel ELF to `link.ld`'s section set and each linker registry's span to a whole number of entries. Catches a *dependency's* `link_section`, which no first-party scan can see, and the wrong-entry-size case that would make `registry_slice`'s `offset_from` unsound.
- **`scripts/check_authority_reachability.sh`** — walks the linked ELF's call graph from every syscall handler to the terminal power primitives, and fails unless each handler that can reach one is either classified `Power` itself or carries a stated reason in `scripts/gates/authority/<variant>.txt`. The `rustc`-level classification gate in `core/src/syscall/handlers.rs` covers *the table*, not *reachability*: `roulette_result` was classified, the gate was green, and its loss arm called `kernel_reboot` two syscalls from an unprivileged caller. The ELF is the input rather than the source because inlining, generic instantiation and trait-object dispatch all change who really calls whom. Indirect calls (`call *%rax`) are the seam it cannot see, which is why the kernel-initiated `PowerOps` callers are a tracked list rather than something it discovers. Runs against the dev kernel from `check-framekernel-gates`, and separately in CI against the **tests** and **release** ELFs — the tests kernel is the only variant whose allowlist carries `run_userland_tests`, which powers the machine off to end the run. Gated in CI rather than on the build path because the walk disassembles the whole ELF (~8 s).
- **`scripts/check_safe_contract_surface.sh`** — ratchet on safe `pub fn`s in `slopos-ostd/` that carry a `# Safety` section. Those are self-declared caller obligations the compiler does not check, so a fault lands in the trusted core while the cause is an ordinary safe call in a service crate. The baseline is **0**: every such contract is currently expressed instead, as a capability witness (`&IrqDisabled`, `&BspToken`, `Osxsave`), a validated newtype (`Xcr0Mask`), a linear handle (`ptr_buf::OneShotBuf`), an owning reference (`KArc`), a sealed trait (`ApTrampolineAbi`), a runtime-checked borrow (`sync::PerCpuSlot`), or a slice in place of a pointer and a length. Reach for those before raising the baseline. Not a count of safe fns containing `unsafe` — that is the design working, not a defect.
- **`scripts/tcb_ratio.sh`** (via `just tcb-ratio`) — a hard gate at `--max 1.0` from both `just check-framekernel-gates` and `KERNEL_BUILD_GATES=1` builds. Prints lines of `unsafe` in `slopos-ostd/` divided by total kernel Rust LoC. Read it as a trend, not as a TCB fraction comparable to other projects': the denominator is raw LoC including the 41 kLoC vendored DWARF reader, and published comparators measure post-LTO linked code size.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

### Allocation discipline
**`slopos_ostd::mm::heap` is the only kernel allocation surface.** Every kernel crate routes heap allocation through `slopos_ostd`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The `kernel/src/main.rs` global-allocator carve-out above is the lone exception.

The in-place-init primitive (`slopos_ostd::Init<T, E>`, `Zeroable`, `init_from_closure`, `init_zeroed`, `Field<T, U, OFF>` + `#[derive(SlotFields)]`) is **in-house** — defined in `slopos-ostd/src/mm/init.rs` with no external dependency on `pinned-init` or Rust-for-Linux's `pin-init`. Large structs must be constructed via `KBox::try_init(T::init_…())` / `PinBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack. `check_stack_sizes.sh` enforces the upper bound from the other direction. `init_struct_with`'s closure must return `Initialised<T>`, which only `SlotPtr::finish` mints, so a caller cannot claim success without going through the slot; `finish` additionally checks field coverage under `debug_assertions`.

### Licensing discipline

SlopOS is `GPL-3.0-or-later`. Two rules keep it that way.

**No verbatim code from a GPL-2.0-only source, ever.** GPL-2.0-only (Linux, the
seL4 kernel, `rust/kernel/**`) and CDDL (illumos) are incompatible with the GPL
version SlopOS ships under, and CDDL is incompatible with every GPL version.
Concepts, algorithms and interface facts are free to take — ABI numbers, `errno`
values, ioctl codes, struct layouts and hardware register offsets carry no
copyright, which is why the ABI-compatibility work is sound. Upstream *prose* is
not: never paste an upstream comment block, design essay or documentation
paragraph. When citing an influence in a comment, name the **specification or
the documented behaviour**, not the implementation file — "values follow the
Linux x86-64 ABI" is an interoperability statement, "derived from
`kernel/sched/fair.c`" is not. Keep the existing influence comments; they are
contemporaneous evidence that what was borrowed was the concept.

**Fonts load at runtime; never `include_bytes!` one into a shipped binary.**
`assets/fonts/*.ttf` are SIL OFL 1.1 and ship as separate files in
`/usr/share/fonts/`, which is aggregation and imposes nothing on the kernel.
Baking a font into `kernel.elf` or a userland binary would put OFL §5 ("must be
distributed entirely under this license") in direct conflict with GPLv3 §5(c)
("license the entire work, as a whole, under this License"). The
`include_bytes!` sites in `font/src/` are `#[cfg(test)]`-gated and must stay
that way. Each font's license text ships beside it, in `assets/fonts/` and on
the installed images.

New third-party code linked into a shipped binary needs an entry in
`NOTICE.md`; `MIT OR Apache-2.0` crates elect MIT there.

### Task-ownership discipline

**`KArc<Task>` is the only owning handle for a task, and `TaskRef` is the only
way to hold one outside `slopos-ostd`.** A raw task pointer says nothing about
whether the task is still alive, whether anyone else is mutating it, or who is
responsible for tearing it down; the owning handle says all three.
CI-enforced by `scripts/check_task_ownership.sh` (run from
`just check-framekernel`, hard-failing), whose header documents each check.

The invariants the gate protects:

- **I1** Raw task pointers exist only inside the ostd placement/link
  primitives, the PCR slots, and the pre-heap `.bss` stubs — the surfaces the
  gate lists as sanctioned. Everything else binds `&Task`, a guard, or a
  `TaskRef`.
- **I2** Linked implies owned: a task on any queue, inbox or wait map has its
  owning reference held *by that container*, moved in and out only through
  `slopos_ostd::task::placement`.
- **I3** The final drop never runs on the dying task's own stack, never with
  IRQs disabled, and never under a lock. `task_put` is the sole release; its
  destructor frees to the buddy allocator, whose reuse path performs
  synchronous cross-CPU TLB drains.
- **I4** Wake and enqueue allocate nothing; a `KArc` clone is one atomic.
- **I5** `current` is a borrow (`CurrentTask`), never an owned handle. PCR
  offset 40 stays raw and ABI-frozen. `IdleTask` is the same shape for the
  idle slot.
- **I6** Lookup is weak-upgrade only. Fabricating a strong reference from a
  raw pointer is not a thing that can be written.
- **I7** `KArc` is fallible everywhere and saturates on refcount overflow.
- **I8** **A task only ever exits from its own context.** Kill is a flag:
  `task_kill_and_wake` marks the target and wakes it, every blocking primitive
  returns `Err(WaitAbort::Killed)`, and the task unwinds by *returning*, so
  destructors run on its own stack at a point it chose. An owning task handle
  may therefore live in a stack frame that blocks. The residual is a kernel
  loop that reaches no blocking primitive at all: nothing can stop one, and
  `task_terminate`'s remote branch survives only as the bounded shutdown
  fallback and the IRQ-exit self-kill.

I1–I7 above are the tree's naming for this discipline. The proof in
`verification/proofs/task_ownership.rs` checks a model of it under the names
T1–T7, and `verification/STATUS.md` records which parts of the tree that model
does *not* reach — weak-memory ordering, the intrusive links and the raw-pointer
provenance are audited, not proved. (I1–I4 elsewhere in `STATUS.md` are
`mm::frame`'s refcount invariants, a different set.) Read that
file's header before changing `task_is_dispatch_pinned`, because the proof
keeps verifying whether or not the model still describes the tree.

## Testing Guidelines
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over serial in KTAP grammar. The Go host wrapper (`tools/run_tests/` → `builddir/run_tests`) parses that stream into a live progress bar + per-failure detail. `just test` builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and exits 0 green / 1 on any failure.

**Run `just test` before sending changes.** A green `just test` is necessary but **not** sufficient — it runs neither the framekernel gates nor the three boot-log ratchets, all of which CI runs and any of which can fail on a commit whose tests pass. See **Pre-commit (MANDATORY)** below for the full sequence. For manual inspection use `just boot` or `just boot-log` (serial transcript in `test_output.log`; `VIDEO=1` for a framebuffer). Note regressions or warnings in your PR description.

### `just test` recipes
- `just test` — full run; dotted progress, per-failure blocks, summary line.
- `just test 'slopos_mm::*'` — run only tests whose `<module>::<name>` matches the glob (positional — a `FILTER=` prefix is passed through literally and breaks the first glob). Comma-separated globs supported (`'mm::*,core::*'`). Filtered runs that create files must include `'*ext2_aaa*'` so the lex-first ext2 root mount runs.
- `just test-rerun-failed` — re-run only the names in `builddir/last-fail.list` (written automatically after every non-aborted run).
- `just test-verbose ['glob']` — also dump captured klog of every passing test.
- `just test-quiet ['glob']` — render only failures + summary; suppresses pass lines on the wire too.
- `just test-raw` — passthrough QEMU stdout verbatim (KTAP + klog interleaved). Last-resort debugging.
- `just test-json builddir/events.jsonl` — also write one JSON event per line to PATH (machine-consumable).
- `just test-userland-only` — skip the kernel phase; run only the userland (`utest!`) phase.
- `just check-tests-host` — run the Go wrapper's own unit tests via `go test ./tools/run_tests/...` (host-side, no QEMU).
- `just check-test-count` — count-regression CI guard; fails if total planned tests across phases drops below `TEST_COUNT_BASELINE`. The default lives in `scripts/check_test_count.sh` and is written down only there — read it from the script rather than restating it here, and bump it there when the suite grows. Measure the new value with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`; never guess it.
- `just check-quota-headroom` — resource-quota ratchet; asserts every account's peak stays under its measured cap in `scripts/gates/quota/<variant>.txt` and that nothing was denied. Same `--log` / `--emit-allowlist` / `--self-test` shape as the lockdep gate.
- `just check-lockdep-headroom` — lock-order ratchet; boots the test ISO and fails unless every phase the kernel reports (`boot`, `post-kernel-tests`, `post-userland-tests`) says `ACTIVE`, reports no violation, and keeps each pool under its recorded cap and the gate file's `max-fill-pct`. Gate data lives in `scripts/gates/lockdep/<variant>.txt` in the same measured-and-tracked style as the stack/vector gates, and a cap matching nothing fails as a dead entry. Class counts are deterministic so their caps are exact; edge and chain counts move with scheduling, so those caps carry the measured spread as margin — re-measure over several runs, not one, when raising them. `--emit-allowlist` writes a fresh baseline, `--log FILE` parses a capture instead of booting, and `--self-test` (run from `check-framekernel-gates`) drives ten crafted logs through the parser to prove the gate still rejects.

All three boot-based ratchets (`check_test_count.sh`, `check_lockdep_headroom.sh`, `check_quota_headroom.sh`) accept `--log`, and CI feeds them one `builddir/run_tests --raw --no-color` capture rather than booting QEMU once per question. Do the same locally — see the pre-commit sequence below — rather than paying three boots.

A ratchet failure is a **measurement to re-take, not a number to raise**. Bump a cap only with a fresh `--emit-allowlist` in the same commit, and say in the commit message which lock, test or account added the delta. Never edit a gate file by hand to make a run pass.

### Cmdline knobs
The kernel parses these from the Limine cmdline (threaded through `scripts/build_iso.sh`'s third positional arg, controlled by the `test_cmdline` justfile constant or the `TEST_CMDLINE=…` env override). For manual `just boot-log` invocations, set `BOOT_CMDLINE='tests=on tests.run=mm::*'` to run a subset.

| Key | Values | Effect |
|---|---|---|
| `tests` | `on` / `off` | master enable |
| `tests.shutdown` | `on` / `off` | write to `isa-debug-exit` after the run |
| `tests.verbosity` | `quiet` / `summary` / `verbose` | per-test emit policy |
| `tests.warn_ms` | integer | mark slower tests as `OVER_TIME` |
| `tests.run` | comma-separated globs | only run matching tests |
| `tests.skip` | comma-separated globs | skip matching tests |
| `lockdep` | `off` / `warn` / `panic` | lock-order validator policy; default `panic` |
| `kconsole` | `off` / `on` / `<hex mask>` | diagnostic-console permission mask; default `on` (informational only) |
| `kconsole.serial` | `on` / `off` | serial BREAK trigger; default `on` |
| `kconsole.arm_ms` | integer | how long the keyboard chord stays armed; default 3000 |
| `kconsole.max_lines` | integer | per-command line budget; default 512 |
| `kconsole.probe_ms` | integer | per-CPU answer budget for the all-CPU probe; default 250 |

`lockdep=warn` reports each distinct finding once (deduped per class pair) and
keeps booting, so one boot enumerates every ordering finding in the tree instead
of stopping at the first. `lockdep=off` keeps the held-lock stack — the poison
walk, the TLB ack-wait diagnostic and the watchdog all read it — but runs no
ordering checks, which is how the validator's own per-acquire cost is measured
without a separate build.

### Diagnostic console

`slopos_ostd::kconsole` is the kernel's magic-key facility: a key pressed on the
**physical console** makes the kernel describe itself. Press SysRq
(Alt+PrintScreen) to arm and one command key to run, or send a serial BREAK and
then the command key. Press the trigger then `h` for the list.

Commands live in the `.kconsole_registry` linker registry, so the crate that
owns a subsystem's data owns the command that prints it — `kcommand!` in `mm/`,
`sched/`, `core/`, `boot/`. OSTD defines the registry and never names an entry;
registration must happen in a crate only the kernel links, because OSTD is
linked into userland binaries too and their linker script brackets no kernel
section.

Three properties are load-bearing:

- **Only the physical console triggers it.** The keyboard hook sits in the IRQ
  handler ahead of layout resolution and consumes its keys, so they reach
  neither the TTY nor the focused GUI application; the serial trigger is a BREAK
  condition, which no byte pattern can forge. There is no call edge from any
  userland write path to `kconsole::request`, and that is the point — the
  facility this replaced was reachable by any process holding a PTY master.
- **One execution tier.** Every command runs at the bottom-half point with
  interrupts and preemption enabled. No command runs in NMI context and none may
  assume it can: a *returning* NMI handler must be fault-free, and the
  frame-pointer walk a backtrace needs is only fault-*recoverable*. The all-CPU
  probe therefore asks each CPU to describe itself from its own NMI handler
  rather than walking a peer.
- **Triggers only queue.** `request` is one `fetch_or` and one `gs`-relative
  byte store — what `bh::raise` permits from a hard IRQ and from under a
  cli-spinlock. The pending set is global rather than per-CPU because
  `bh::raise` marks only the calling CPU, and every CPU's timer tick pokes its
  own bottom half while anything is queued.

`KCMD_DESTRUCTIVE` commands are registered but refused unless the mask names
their bit, which the default does not: boot `kconsole=0x3` to enable them.

### Output format and JSONL events
The public KTAP docs describe the wire grammar and the JSONL event schema. The wire format is stable; the JSONL schema is a strict superset suitable for downstream JUnit XML conversion or test-history regression detection.

## Commit & Pull Request Guidelines
Subjects are `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`), ≤72 chars. Add a body for rationale, boot implications, or follow-ups. For PRs include: motivation, testing artifacts (command + result), issue references, and serial excerpts or screenshots when boot flow or visible output changes. Flag breaking changes and downstream-script coordination.

### Commit messages (MANDATORY)
**Always use the `caveman-commit` skill to write the commit message. Never hand-write one.**

- Load it with `/skill:caveman-commit`, or read `~/.agents/skills/caveman-commit/SKILL.md` directly if skill commands are unavailable.
- The skill only *generates* the message; staging and running `git commit` remain your job.
- This applies to every commit, including one-line and trivial ones.

### Branching (MANDATORY)
Commit directly to `develop` (the working branch). Do **not** create a new branch unless explicitly asked to — this overrides any default "branch before committing on the default branch" behavior. Likewise, do not open PRs or push to a remote unless explicitly asked.

### Pre-commit (MANDATORY)
Before every `git commit`, **always** run `cargo fmt --all` and stage any reformatted files. If formatting fails, fix the issue before committing. Never commit unformatted Rust code.

**`just test` alone is not the bar.** CI runs gates that `just test` does not, and a commit that only satisfies fmt + tests routinely fails on them — most often the lockdep ratchet. Reproduce the whole `ci` job locally:

```sh
cargo fmt --all                       # then stage the reformatted files
just fmt                              # CI: Check formatting
just test-host                        # CI: Host-side unit tests
just build                            # CI: Build kernel
just check-framekernel-gates          # CI: Framekernel gates (self-tests + all source/ELF scans)

# CI: Run tests — one raw capture, which the three ratchets then parse.
just _build-run-tests
set -o pipefail
builddir/run_tests --raw --no-color 2>&1 | tee builddir/ci-test.log

scripts/check_authority_reachability.sh --variant tests builddir/kernel-tests.elf
scripts/check_test_count.sh        --log builddir/ci-test.log
scripts/check_lockdep_headroom.sh  --log builddir/ci-test.log
scripts/check_quota_headroom.sh    --log builddir/ci-test.log   # not yet a CI step; run it anyway
```

The capture is reused deliberately: booting QEMU once per ratchet is three boots for three questions, and a second boot could disagree with the one that was graded.

A changed lock order, a new lock, a new test, or a new quota account will move a ratchet. That is the gate working — re-measure with the gate's `--emit-allowlist` and explain the delta in the commit message. Never hand-edit `scripts/gates/**` to silence a run.

Two CI jobs are *not* in the sequence above because they are slow and run as separate jobs: `just check-miri` (KernMiri) and `just verify` (Verus). Run them when touching `slopos-ostd/` or `verification/`; `just check-framekernel` is the recipe that runs the gates plus both.

Commit order: `cargo fmt --all` → the sequence above → stage → `caveman-commit` for the message → `git commit`.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads the pinned Limine binary release into `third_party/limine`; offline environments should pre-populate that directory (it only needs `limine-bios.sys` + `BOOTX64.EFI`) or set `LIMINE_URL`/`LIMINE_VERSION` to avoid network stalls. Rust crates are auto-discovered via the workspace, so most build changes belong in `justfile`, `scripts/`, `Cargo.toml`, and `targets/*.json`; ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered. VirtIO devices require MSI-X (preferred) or MSI as a minimum — legacy polling has been removed; probe panics if neither interrupt mechanism is available.**

## Safety & Execution Boundaries
Keep all work inside this repository. Do not copy kernel binaries to system paths, do not install or chainload on real hardware, and never run outside QEMU/OVMF. The scripts already sandbox execution; if you need fresh firmware or boot assets, use the provided automation instead of manual installs. Treat Limine, OVMF, and the kernel as development artifacts only and avoid touching `/boot`, `/efi`, or other host-level locations.


## Security Triage & CVSS Ledger (MANDATORY)

All agents must run a recurring vulnerability review loop for newly written and recently changed code.

### Required cadence
1. Run a security sweep after each major milestone and before any release/PR handoff.
2. Re-scan subsystems touched by recent commits (at minimum: syscall paths, memory management, filesystems, drivers).

### Triage workflow (strict order)
1. **List all findings first** in a raw triage section (do not score as CVSS yet).
2. For each finding, assign a **confidence score (0-100)** using this model:
   - Evidence quality (0-40): direct code proof, exact path/line references
   - Exploitability clarity (0-30): realistic attacker path and impact
   - Reproducibility (0-30): deterministic repro or strong step-by-step plausibility
3. Only findings with **confidence >= 80** are considered **guaranteed issues**.
4. Only guaranteed issues get a CVSS vector/score entry.
5. Use `scripts/cvss_calc.py` to compute CVSS v3.1 vectors/scores consistently across agents.

### CVSS file lifecycle requirements
1. Maintain `CVSS.md` as the single living ledger of **open findings only** (pre-alpha policy).
2. Every entry must include:
   - Stable internal ID (e.g., `SLOPOS-YYYY-NNNN`)
   - Status: `open` or `needs-retest`
   - Confidence score and reasoning
   - CVSS vector + score (only if confidence >= 80)
   - Exact evidence paths/lines
3. When an issue is fixed (pre-alpha policy):
   - **Remove it** from `CVSS.md`. SlopOS is pre-alpha with no audit-trail obligation, so resolved findings are deleted rather than retained as historical `fixed` records.
   - Internal IDs stay stable for findings that remain open; gaps in the numbering are expected.
4. When new guaranteed issues are found, append them with incremented IDs.

### Repro/examples (required when possible)
1. Add a minimal repro recipe for each guaranteed issue when technically feasible.
2. Repro can be a syscall sequence, malformed input artifact, or concise PoC steps.
3. If no safe repro is possible, document why and provide nearest deterministic validation method.

### Non-negotiable rule
- Never present speculative issues as CVSS-scored vulnerabilities.
- Confidence-gated, evidence-backed issues only.

---
