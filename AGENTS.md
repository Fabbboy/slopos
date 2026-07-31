# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
There are no required git submodules. The Limine bootloader is downloaded by `scripts/ensure_limine.sh`, which fetches a pinned Limine binary release (v12.3.1) into `third_party/limine` on first ISO build. The project uses [`just`](https://github.com/casey/just) as its command runner (install via `cargo install just` or your package manager). The `justfile` drives cargo + `rust-lld` via helper scripts in `scripts/`: `just setup` installs the pinned nightly from `rust-toolchain.toml` and verifies a Go toolchain (>= 1.22) is on PATH for `tools/run_tests/`, `just build` emits `builddir/kernel.elf`, and `just iso` regenerates `builddir/slop.iso`. For quick launches use `just boot` (interactive), `just boot-fast` (skips roulette), or `just boot-log` (non-interactive, default 15 s timeout). Boot targets rebuild a secondary image (`builddir/slop-notests.iso`) with `tests=off` on the kernel command line; override with `BOOT_CMDLINE=... just boot` and add `VIDEO=1` for a graphical window. CI and AI agents can call `just test`, which builds the Go-based `builddir/run_tests` binary (from `tools/run_tests/`) and dispatches to it: builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, parses the kernel's KTAP-grammar output, and renders a live progress bar + per-failure detail. Exits 0 green / 1 on any failure. Filter via `just test 'glob'` (positional — a `FILTER=` prefix is passed through literally and breaks the first glob); re-run only failures via `just test-rerun-failed`. Run `just --list` to see all available recipes.

## Knowledge Index (AI)
The `knowledge/` directory hosts a local semantic index for querying the codebase. Build it with:
- `python3 -m venv knowledge/.venv`
- `. knowledge/.venv/bin/activate`
- `pip install -r knowledge/requirements.txt`
- `python knowledge/index.py`
Use `python knowledge/query.py \"<question>\"` to ask about signatures, drivers, or file locations. Rebuild the index after large refactors or merges. Do not commit the venv or embedding database artifacts.

## Coding Style & Naming Conventions
All kernel code is Rust `#![no_std]` on nightly with `#![forbid(unsafe_op_in_unsafe_fn)]`. Keep unsafe blocks tiny and well-documented; prefer `pub(crate)` helpers and prefix cross-module APIs with their subsystem (e.g., `mm::`, `sched::`). Match the existing four-space indentation and brace-on-same-line style. Assembly sources (when needed) are Intel syntax (`*.s`) and should document register contracts.

### Unsafe-code surface
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (the framekernel **AD-1/AD-2** discipline: one trusted crate holds all `unsafe`, every other kernel crate forbids it; CI-enforced by `scripts/check_unsafe_outside_ostd.sh`). Every other crate the kernel binary links (`abi`, `acpi`, `boot`, `core`, `drivers`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `keymap-core`, `ktesting`, `mm`, `net`, `pidfd`, `ring`, `sched`, `service-core`, `signalfd`, `video`, `vt`) carries `#![forbid(unsafe_code)]`, and `check_unsafe_outside_ostd.sh` asserts that from the binary's own dependency closure, so a new crate is covered the moment it is linked. Userland-side crates (`userland/`, `slibc/`, `slop-protocol/`, `appkit/`, `slopos-rt/`, `windowing/`) are out of scope for this discipline.

`forbid` is necessary but not sufficient: rustc drops any `unsafe_code` diagnostic whose primary span satisfies `in_external_macro`, so a macro defined in another crate expands `unsafe` into a forbid crate silently, and the call site holds no keyword for a source scan to find. `scripts/check_unsafe_expansion.sh` is what closes that — see below.

One documented exempt site exists outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct.

Three C-ABI entry points keep `#[unsafe(no_mangle)]` in `boot/src/ffi_boundary.rs` — `kernel_main`, `common_exception_handler`, `isr_iret_frame_corrupt`. Their callers are assembly, so the symbols have to resolve at link time; routing them through a registered hook would leave an uninstalled window across early boot in which a fault triple-faults instead of panicking. `check_unsafe_expansion.sh` allowlists them by name.

These gates enforce the discipline. The source-scanning gates run via `just check-framekernel` and CI (and from a build when `KERNEL_BUILD_GATES=1`); the two ELF-inspecting gates (`check_stack_sizes.sh`, `check_kernel_softfloat.sh`) run on every `just build`. The source scans are kept off the default build path so the interactive boot loop stays fast.

- **`scripts/check_unsafe_outside_ostd.sh`** — fails if any `.rs` file under a kernel crate (other than `slopos-ostd/`, `slopos-ostd-derive/`, or the exempt file above) contains an `unsafe` keyword that is not a comment, not the `#[unsafe(...)]` attribute form, and not `#[cfg(...)]`-gated. Mirrors `check_alloc_dep.sh`'s cfg-aware lookback. Also asserts that every crate in the kernel binary's dependency closure carries `#![forbid(unsafe_code)]` at its crate root, so a newly linked crate cannot start life without the lint.
- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency **and** fails if any kernel `.rs` file (other than `kernel/src/main.rs` and the `slopos-ostd/` tree) contains a bare `extern crate alloc;` / `use alloc::` / `use ::alloc::` statement (with `#[cfg(...)]`-aware lookback so cfg-gated usages that compile out of the kernel build are accepted).
- **`scripts/check_stack_sizes.sh`** — fails if any function in `builddir/kernel.elf` has a stack frame larger than `STACK_SIZE_THRESHOLD` (default **2048 bytes / 2 KiB**, matching Linux mainline's `CONFIG_FRAME_WARN` default on x86_64/arm64 but stricter in enforcement — SlopOS fails the build, Linux merely warns). This is the load-bearing enforcement of framekernel **Inv. 5'**. Driven by `-Zemit-stack-sizes`; inspects the final ELF's `.stack_sizes` section, so it catches NRVO failures, inlining, and trait-object dispatch that a source-level heuristic would miss.
- **`scripts/check_kernel_softfloat.sh`** — fails if `builddir/kernel.elf` contains any x86 vector (XMM/YMM/ZMM) instruction. The kernel must be built `+soft-float` so it never touches the vector register file: a syscall/exception entering from userland does **not** save the caller's FPU/vector state (only a full context switch does, via `xsave`/`xrstor`), so a single kernel vector instruction in a fault/IRQ path clobbers the interrupted user task's live XMM/YMM. The soft-float guarantee lives in `targets/x86_64-slos.json` (`features: …,-sse,…,+soft-float` + `rustc-abi: x86-softfloat`) — **not** in `.cargo/config.toml`, because a `RUSTFLAGS` env var fully overrides `target.*.rustflags`. This gate is the belt-and-braces that catches any regression that re-acquires SSE. Skipped for `kernel/tests` builds (the `slopos-ostd` xsave-conformance test helpers carry deliberate named-register vector asm).
- **`scripts/check_unsafe_expansion.sh`** — expands every kernel crate with `-Zunpretty=expanded`, over each crate's feature configurations, and holds the result to a constant rather than a recorded count: zero executable `unsafe`, `unsafe impl` only of an allowlisted trait, `#[unsafe(link_section)]` only of a `link.ld` section, `#[unsafe(no_mangle)]` only of an asm-called symbol. This is the only mechanism that sees macro-injected `unsafe`; `forbid` and the source scan are both blind to it. A golden fixture fails the gate if a toolchain bump moves the compiler's own emitted shapes. ~16 s warm.
- **`scripts/check_registry_sections.sh`** — holds `kernel.elf` to `link.ld`'s section set and each linker registry's span to a whole number of entries. Catches a *dependency's* `link_section`, which no first-party scan can see, and the wrong-entry-size case that would make `registry_slice`'s `offset_from` unsound.
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
- **I8** **No owning task handle may live in a stack frame that can
  deschedule.** SlopOS tears such a task down from another CPU without
  unwinding, so the handle is never dropped and the task leaks with its stacks
  and its address space. Park it somewhere that outlives the frame (see
  `sched/src/task/pending_spawn.rs` and the wait-reference map in
  `scheduler.rs`), or cover the frame with a `PreemptGuard` and keep it
  non-blocking — `assert_switch_preempt_safe` turns a violation of the latter
  into a panic naming the frame.

I1–I7 above are the tree's naming for this discipline. The proof in
`verification/proofs/task_ownership.rs` checks a model of it under the names
T1–T7, and `verification/STATUS.md` records which parts of the tree that model
does *not* reach — weak-memory ordering, the intrusive links and the raw-pointer
provenance are audited, not proved. (I1–I4 elsewhere in `STATUS.md` are
`mm::frame`'s refcount invariants, a different set.) Read that
file's header before changing `task_is_dispatch_pinned`, because the proof
keeps verifying whether or not the model still describes the tree.

## Testing Guidelines
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over the serial console in KTAP grammar. The host wrapper at `tools/run_tests/` (Go, builds to `builddir/run_tests`) parses that stream and renders a live progress bar + per-failure detail blocks. Before sending changes, run `just test` (non-interactive, auto-shutdown). For manual inspection use `just boot` (interactive) or `just boot-log` to capture a serial transcript in `test_output.log` (append `VIDEO=1` if you need a visible framebuffer). Note any observed regressions or warnings in your PR description.

### `just test` recipes
- `just test` — full run; dotted progress, per-failure blocks, summary line. Exits 0 green / 1 on any failure.
- `just test 'slopos_mm::*'` — run only tests whose `<module>::<name>` matches the glob. Comma-separated globs supported (`'mm::*,core::*'`). Filtered runs that create files must include `'*ext2_aaa*'` so the lex-first ext2 root mount runs.
- `just test-rerun-failed` — re-run only the names in `builddir/last-fail.list` (written automatically after every non-aborted run).
- `just test-verbose ['glob']` — also dump captured klog of every passing test.
- `just test-quiet ['glob']` — render only failures + summary; suppresses pass lines on the wire too.
- `just test-raw` — passthrough QEMU stdout verbatim (KTAP + klog interleaved). Last-resort debugging.
- `just test-json builddir/events.jsonl` — also write one JSON event per line to PATH (machine-consumable).
- `just test-userland-only` — skip the kernel phase; run only the userland (`utest!`) phase.
- `just check-tests-host` — run the Go wrapper's own unit tests via `go test ./tools/run_tests/...` (host-side, no QEMU).
- `just check-test-count` — count-regression CI guard; fails if total planned tests across phases drops below `TEST_COUNT_BASELINE`. The default lives in `scripts/check_test_count.sh` and is written down only there — read it from the script rather than restating it here, and bump it there when the suite grows. Measure the new value with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`; never guess it.

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

### Output format and JSONL events
The public KTAP docs describe the wire grammar and the JSONL event schema. The wire format is stable; the JSONL schema is a strict superset suitable for downstream JUnit XML conversion or test-history regression detection.

## Commit & Pull Request Guidelines
Git history currently lacks structure; standardize on `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`) and keep subjects ≤72 chars. Add a body when explaining rationale, boot implications, or follow-ups. For PRs, include: brief motivation, testing artifacts (command + result), references to issues, and screenshots or serial excerpts when altering visible output or boot flow. Flag breaking changes and call out coordination needs with downstream scripts.

### Branching (MANDATORY)
Commit directly to `develop` (the working branch). Do **not** create a new branch unless explicitly asked to — this overrides any default "branch before committing on the default branch" behavior. Likewise, do not open PRs or push to a remote unless explicitly asked.

### Pre-commit (MANDATORY)
Before every `git commit`, **always** run `cargo fmt --all` and stage any reformatted files. If formatting fails, fix the issue before committing. Never commit unformatted Rust code.

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
