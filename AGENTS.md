# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
Run `git submodule update --init --recursive` after cloning to sync `third_party/limine`. The project uses [`just`](https://github.com/casey/just) as its command runner (install via `cargo install just` or your package manager). The `justfile` drives cargo + `rust-lld` via helper scripts in `scripts/`: `just setup` installs the pinned nightly from `rust-toolchain.toml`, `just build` emits `builddir/kernel.elf`, and `just iso` regenerates `builddir/slop.iso`. For quick launches use `just boot` (interactive) or `just boot-log` (non-interactive, default 15 s timeout). Both boot targets rebuild a secondary image (`builddir/slop-notests.iso`) with `itests=off` on the kernel command line; override with `BOOT_CMDLINE=... just boot` and add `VIDEO=1` for a graphical window. CI and AI agents can call `just test`, which generates `builddir/slop-tests.iso` with `itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and fails if the harness reports anything but a clean pass. Run `just --list` to see all available recipes.

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

### Allocation surface
**`slopos-alloc` is the only kernel allocation surface.** Every kernel crate (everything outside `userland/`, `slibc/`, `slop-protocol/`, `ktesting/`, and `slopos-alloc/` itself) routes heap allocation through `slopos_alloc`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The lone exception is `kernel/src/main.rs`, which keeps `extern crate alloc;` for the `#[global_allocator]` / `#[alloc_error_handler]` declarations.

Two build-time gates enforce the discipline (run on every `just build`, also via `just check`):

- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency.
- **`scripts/check_stack_sizes.sh`** — fails if any function in `builddir/kernel.elf` has a stack frame larger than `STACK_SIZE_THRESHOLD` (default 2560 bytes / 2.5 KiB). Driven by `-Zemit-stack-sizes`.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

## Testing Guidelines
There are no unit tests yet; rely on QEMU boot verification and the interrupt test harness. Before sending changes, rebuild the ISO and run `just test` (non-interactive, auto-shutdown). For manual inspection use `just boot` (interactive) or `just boot-log` to capture a serial transcript in `test_output.log` (append `VIDEO=1` if you need a visible framebuffer). Inspect the output for the roulette banner (`=== KERNEL ROULETTE: Spinning the Wheel of Fate ===`) and any warnings. Note any observed regressions or warnings in your PR description.

## Interrupt Test Configuration
- Build defaults are baked into the Rust harness: enabled=false, suite=all, verbosity=summary, timeout=0, shutdown=false.
- Runtime overrides are parsed from the Limine command line: use `itests=on|off|basic|memory|control`, `itests.suite=...`, `itests.verbosity=quiet|summary|verbose`, and `itests.timeout=<ms>`.
- Toggle automatic shutdown after the harness with `itests.shutdown=on|off`; when enabled the kernel writes to QEMU’s debug-exit port after printing the summary so the VM terminates without intervention.
- Boot logs summarize the active configuration before running tests when debug logging is enabled, and the harness reports totals in `test_output.log`.
- The timeout value is parsed but currently not enforced by the stub harness; keep it at 0 for now.

## Interrupt Test Harness
- The harness is now Rust-based; enable it with `itests=on|off` on the Limine command line (defaults to off).
- Suites include `basic`, `memory`, `control`, `scheduler`, and `all`; outputs are stubbed but wired to the W/L system.
- Verbosity still accepts `quiet|summary|verbose` to control serial chatter.
- Enable `itests.shutdown=on` in automation to halt/QEMU-exit once the summary banner is printed—`just test` wires this in automatically (writes 0 to port `0xf4` for pass, 1 for fail).

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
1. Maintain `CVSS.md` as the single living ledger.
2. Every entry must include:
   - Stable internal ID (e.g., `SLOPOS-YYYY-NNNN`)
   - Status: `open`, `fixed`, or `needs-retest`
   - Confidence score and reasoning
   - CVSS vector + score (only if confidence >= 80)
   - Exact evidence paths/lines
3. When an issue is fixed:
   - Do not delete it.
   - Mark it `fixed`, include fix date and commit hash (if available), and keep historical traceability.
4. When new guaranteed issues are found, append them with incremented IDs.

### Repro/examples (required when possible)
1. Add a minimal repro recipe for each guaranteed issue when technically feasible.
2. Repro can be a syscall sequence, malformed input artifact, or concise PoC steps.
3. If no safe repro is possible, document why and provide nearest deterministic validation method.

### Non-negotiable rule
- Never present speculative issues as CVSS-scored vulnerabilities.
- Confidence-gated, evidence-backed issues only.

---
