# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
Run `git submodule update --init --recursive` after cloning to sync `third_party/limine`. The project uses [`just`](https://github.com/casey/just) as its command runner (install via `cargo install just` or your package manager). The `justfile` drives cargo + `rust-lld` via helper scripts in `scripts/`: `just setup` installs the pinned nightly from `rust-toolchain.toml` and verifies a Go toolchain (>= 1.22) is on PATH for `tools/run_tests/`, `just build` emits `builddir/kernel.elf`, and `just iso` regenerates `builddir/slop.iso`. For quick launches use `just boot` (interactive) or `just boot-log` (non-interactive, default 15 s timeout). Both boot targets rebuild a secondary image (`builddir/slop-notests.iso`) with `tests=off` on the kernel command line; override with `BOOT_CMDLINE=... just boot` and add `VIDEO=1` for a graphical window. CI and AI agents can call `just test`, which builds the Go-based `builddir/run_tests` binary (from `tools/run_tests/`) and dispatches to it: builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, parses the kernel's KTAP-grammar output, and renders a live progress bar + per-failure detail (see `docs/test_output.md`). Exits 0 green / 1 on any failure. Filter via `just test FILTER='glob'`; re-run only failures via `just test-rerun-failed`. Run `just --list` to see all available recipes.

## Knowledge Index (AI)
The `knowledge/` directory hosts a local semantic index for querying the codebase. Build it with:
- `python3 -m venv knowledge/.venv`
- `. knowledge/.venv/bin/activate`
- `pip install -r knowledge/requirements.txt`
- `python knowledge/index.py`
Use `python knowledge/query.py \"<question>\"` to ask about signatures, drivers, or file locations. Rebuild the index after large refactors or merges. Do not commit the venv or embedding database artifacts.

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
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (per `plans/FRAMEKERNEL_PLAN.md` AD-1/AD-2). Every other kernel crate (`abi`, `acpi`, `boot`, `core`, `drivers`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `mm`, `net`, `sched`, `service-core`, `video`, `windowing`) carries `#![forbid(unsafe_code)]`. Userland-side crates (`userland/`, `slibc/`, `slop-protocol/`, `ktesting/`, `appkit/`) are out of scope for this discipline.

Two documented exempt sites exist outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct.
- **`hermetic/src/macros.rs`** — `macro_rules!` body containing the Edition-2024 `#[unsafe(link_section = …)]` attribute used at the macro's expansion sites elsewhere. The keyword is required by the attribute grammar, not a runtime unsafe block.

Build-time gates enforce the discipline (run on every `just build`, also exposed via `just check-framekernel`):

- **`scripts/check_unsafe_outside_ostd.sh`** — fails if any `.rs` file under a kernel crate (other than `slopos-ostd/`, `slopos-ostd-derive/`, or the two exempt files above) contains an `unsafe` keyword that is not a comment, not the `#[unsafe(...)]` attribute form, and not `#[cfg(...)]`-gated. Mirrors `check_alloc_dep.sh`'s cfg-aware lookback. Belt-and-braces gate alongside the per-crate `#![forbid(unsafe_code)]` attribute.
- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency **and** fails if any kernel `.rs` file (other than `kernel/src/main.rs` and the `slopos-ostd/` tree) contains a bare `extern crate alloc;` / `use alloc::` / `use ::alloc::` statement (with `#[cfg(...)]`-aware lookback so cfg-gated usages that compile out of the kernel build are accepted).
- **`scripts/check_stack_sizes.sh`** — fails if any function in `builddir/kernel.elf` has a stack frame larger than `STACK_SIZE_THRESHOLD` (default **2048 bytes / 2 KiB**, matching Linux mainline's `CONFIG_FRAME_WARN` default on x86_64/arm64 but stricter in enforcement — SlopOS fails the build, Linux merely warns). This is the load-bearing enforcement of framekernel **Inv. 5'**. Driven by `-Zemit-stack-sizes`; inspects the final ELF's `.stack_sizes` section, so it catches NRVO failures, inlining, and trait-object dispatch that a source-level heuristic would miss.
- **`scripts/tcb_ratio.sh`** (via `just tcb-ratio`) — informational. Prints lines of `unsafe` in `slopos-ostd/` divided by total kernel Rust LoC. Phase 1 target ≤ 1.5 %; Phase 2 target ≤ 1.0 %.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

### Allocation discipline
**`slopos_ostd::mm::heap` is the only kernel allocation surface.** Every kernel crate routes heap allocation through `slopos_ostd`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The `kernel/src/main.rs` global-allocator carve-out above is the lone exception.

The in-place-init primitive (`slopos_ostd::Init<T, E>`, `Zeroable`, `init_from_closure`, `init_zeroed`) is **in-house** — defined in `slopos-ostd/src/mm/init.rs` with no external dependency on `pinned-init` or Rust-for-Linux's `pin-init`. Large structs must be constructed via `KBox::try_init(T::init_…())` / `PinBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack. `check_stack_sizes.sh` enforces the upper bound from the other direction.

## Testing Guidelines
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over the serial console in a KTAP-grammar format documented in `docs/test_output.md`. The host wrapper at `tools/run_tests/` (Go, builds to `builddir/run_tests`) parses that stream and renders a live progress bar + per-failure detail blocks. Before sending changes, run `just test` (non-interactive, auto-shutdown). For manual inspection use `just boot` (interactive) or `just boot-log` to capture a serial transcript in `test_output.log` (append `VIDEO=1` if you need a visible framebuffer). Note any observed regressions or warnings in your PR description.

### `just test` recipes
- `just test` — full run; dotted progress, per-failure blocks, summary line. Exits 0 green / 1 on any failure.
- `just test FILTER='slopos_mm::*'` — run only tests whose `<module>::<name>` matches the glob. Comma-separated globs supported (`'mm::*,core::*'`).
- `just test-rerun-failed` — re-run only the names in `builddir/last-fail.list` (written automatically after every non-aborted run).
- `just test-verbose [FILTER='…']` — also dump captured klog of every passing test.
- `just test-quiet [FILTER='…']` — render only failures + summary; suppresses pass lines on the wire too.
- `just test-raw` — passthrough QEMU stdout verbatim (KTAP + klog interleaved). Last-resort debugging.
- `just test-json builddir/events.jsonl` — also write one JSON event per line to PATH (machine-consumable).
- `just test-userland-only` — skip the kernel phase; run only the userland (`utest!`) phase.
- `just check-tests-host` — run the Go wrapper's own unit tests via `go test ./tools/run_tests/...` (host-side, no QEMU).
- `just check-test-count` — count-regression CI guard; fails if total planned tests across phases drops below `TEST_COUNT_BASELINE` (default 2425).

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
See `docs/test_output.md` for the wire grammar and the JSONL event schema. The wire format is stable; the JSONL schema is a strict superset suitable for downstream JUnit XML conversion or test-history regression detection.

## Commit & Pull Request Guidelines
Git history currently lacks structure; standardize on `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`) and keep subjects ≤72 chars. Add a body when explaining rationale, boot implications, or follow-ups. For PRs, include: brief motivation, testing artifacts (command + result), references to issues, and screenshots or serial excerpts when altering visible output or boot flow. Flag breaking changes and call out coordination needs with downstream scripts.

### Pre-commit (MANDATORY)
Before every `git commit`, **always** run `cargo fmt --all` and stage any reformatted files. If formatting fails, fix the issue before committing. Never commit unformatted Rust code.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads Limine, but offline environments should pre-clone `third_party/limine` to avoid network stalls. Rust crates are auto-discovered via the workspace, so most build changes belong in `justfile`, `scripts/`, `Cargo.toml`, and `targets/*.json`; ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered. VirtIO devices require MSI-X (preferred) or MSI as a minimum — legacy polling has been removed; probe panics if neither interrupt mechanism is available.**

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
