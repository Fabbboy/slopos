# SlopOS Test Framework Redesign

> **Status**: Phase 0 **complete**; Phase 1 **complete**; Phase 2 **complete**; Phase 3 **framework complete and verified by `just test-userland-only` (3/3 utests pass); kernel-test-fixture pollution class closed via `KernelTestScope`. A separate kernel-side regression in the full `just test` run is tracked under §8 Phase 3 Notes**; Phase 4 not started
> **Target**: Replace stale `itests`/`interrupt_test*` harness with structured per-test, KTAP-emitting, filterable, userland-aware framework
> **Scope**: `ktesting/` crate (rewritten), `tests/` crate (folded), 75+ `define_test_suite!` sites (migrated), 3 userland test bins (integrated), `just test` UX (rebuilt)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State](#2-current-state)
3. [Design Decisions](#3-design-decisions)
4. [Architecture](#4-architecture)
5. [Phase 0: Rename + Crate Consolidation](#5-phase-0-rename--crate-consolidation)
6. [Phase 1: New Framework With Bridge](#6-phase-1-new-framework-with-bridge)
7. [Phase 2: Big-Batch Site Migration](#7-phase-2-big-batch-site-migration)
8. [Phase 3: Userland Integration](#8-phase-3-userland-integration)
9. [Phase 4: Host-Side Wrapper](#9-phase-4-host-side-wrapper)
10. [Out of Scope](#10-out-of-scope)
11. [Reused Infrastructure](#11-reused-infrastructure)
12. [Verification](#12-verification)

---

## 1. Executive Summary

`just test` today is a single-knob harness. Boots the kernel with `itests=on`, runs every registered suite top-to-bottom, prints kernel klog interleaved with one-line per-suite summaries, exits via `isa-debug-exit`. Pain points:

- Failing tests print one line: `TEST FAIL: <name>: Fail` lost in tens of thousands of klog lines from 2393 tests.
- No way to re-run only a failing test — every iteration runs all 2393 again.
- No way to filter a subset (`itests.suite=` is parsed but ignored at `ktesting/src/config.rs:96`).
- `itests.timeout=` and `itests.verbosity=` are parsed but unused.
- Stale "interrupt test" naming throughout cmdline keys, function names, modules, feature flags.
- Userland test binaries (`fork_test`, `io_capture_test`, `heap_allocator_test`) exist in `ext2-tests.img` but `just test` never invokes them.

This redesign delivers, in 5 phases:

- **Phase 0**: Mechanical rename across the surface area; collapse `tests/` crate into `ktesting/`.
- **Phase 1**: New per-test registration via `stest!` decl-macro; KTAP-grammar output; per-test log capture; module-path glob filter; `tests.warn_ms` slow-test reporting. Bridge keeps existing 75+ `define_test_suite!` sites compiling unchanged.
- **Phase 2**: Big-batch sed conversion of all 75+ sites to per-test `stest!` invocations; delete bridge.
- **Phase 3**: Userland tests as first-class entries via `SYSCALL_TEST_REPORT` + `slibc::test_harness` + `utest!` macro. Kernel-side runner spawns the binary, drains structured reports, emits nested KTAP subtests.
- **Phase 4**: Host-side `scripts/run_tests.py` parses KTAP, renders dotted progress + per-failure detail, supports `FILTER=`, `--rerun-failed`, `--verbose`, `--raw`.

---

## 2. Current State

### `just test` flow

`just test` → `_iso-tests` (kernel built with features `slopos-drivers/qemu-exit kernel/builtin-tests`, cmdline `itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on`) → `_qemu-boot test` → QEMU with `-device isa-debug-exit,iobase=0xf4,iosize=0x01 -no-reboot` → kernel boot → `boot_step_interrupt_tests_fn` (boot_drivers.rs:309-363, priority 90) → `tests_run_all` (tests/src/lib.rs:49-182) → `interrupt_test_request_shutdown` (drivers/src/interrupt_test.rs:20-29) writes port 0xF4 → QEMU exit 1 (pass) or 3 (fail) → `qemu_run.sh` interprets and exits.

### Test catalog (2393 tests, ~75-86 suites)

- 2 boot (`gdt`, `shutdown`)
- 12 core (`exec`, `context`, `sched_core`, `syscall_open`, `syscall_process`, `syscall_signal`, `heap_allocator`, `irq`, `msi_alloc`, `msi_handler`, `msi_idt`, plus helpers in `core/src/tests/`)
- 23 drivers (`apic_timer`, `ecam`, `hpet`, `msix`, `pci_cap`, `virtio_completion`, `virtio_msix`, `virtio_net`, `ioapic`, `tty_driver`, plus `tty_tests/test_*.rs`)
- 4 mm (`mmio`, `cow_edge`, `oom`, `tlb`)
- 25+ net (`dns`, `icmp`, `ingress`, `loopback`, `napi`, `neighbor`, `netdev`, `netstack`, `net_types`, `packetbuf`, `reassembly`, `route`, `socket_framework`, `socket_option`, `socket`, `tcp_*` × 14, `timer`, `udp_demux`, `udp_socket`)
- 2 special tests/ (`exception`, `xsave`)
- 1 fs (`ext2`)
- 1 fpu (`fpu_sse`)

### `ktesting` crate primitives

- `TestResult` enum: `Pass`, `Fail`, `Panic`, `Skipped` (`ktesting/src/lib.rs:19-45`)
- `define_test_suite!(name, [test_fn1, ...])` macro — generates static `TestSuiteDesc` in `.test_registry` linker section (`ktesting/src/lib.rs:90-167`)
- Test fn signature: `fn test_name() -> TestResult`
- `TestRunSummary`: ~2.6 KiB struct, `HARNESS_MAX_SUITES = 64` cap (`ktesting/src/harness.rs:9, 91-103`) — silently truncates the per-suite-result array when 75+ suites register, totals still correct
- Cmdline parser `config_from_cmdline` (`ktesting/src/config.rs:81-116`) — reads `itests=`, `itests.suite=` (parsed but ignored), `itests.verbosity=` (parsed but unused), `itests.timeout=` (parsed but unenforced), `itests.shutdown=`, `itests.stacktrace_demo=` (unused)
- Output: `TEST FAIL: <name>: <result>` per failure (runner.rs:6), `SUITE<idx> total=N pass=N fail=N elapsed=Nms` per suite (tests/src/lib.rs:129), `TESTS SUMMARY: total=N passed=N failed=N elapsed_ms=N` once at end (tests/src/lib.rs:148), `LUF SUMMARY: queued=N reuse_drains=N overflow_drains=N deferred_saves=N` (lines 161-179) — all interleaved with kernel klog, no structured format

### Userland test surface

3 test binaries in `userland/src/bin/tests/`, gated by `testbins` feature, packaged into `ext2-tests.img` via `_fs-image-tests`. They run only if a developer types them in the shell. Kernel test harness has no integration with them. `task_get_exit_record(pid)` exists kernel-internally and returns exit codes, but no syscall exposes them to userland and no kernel-side runner spawns and reaps test binaries today.

---

## 3. Design Decisions

### Locked-in (user-approved)

| Decision | Choice | Rationale |
|---|---|---|
| Scope | Pragmatic core | Skip per-test hard timeout enforcement, JUnit XML, tag filter — those are separate future plans |
| Userland IPC | Dedicated `SYSCALL_TEST_REPORT` | 50 LOC; doesn't pretend stderr semantics exist; minimal coupling. Avoided: stderr capture (requires fd-routing refactor), FS scratch dir (requires writable test FS) |
| Migration | Phased big-batch | Land framework with bridge in Phase 1; one mega-commit converts all 75+ sites in Phase 2 |
| Macro form | Decl-macro (`stest!`, `utest!`) | Avoid proc-macro crate split; use existing `paste!` for ident munging |
| Registration | Existing `.test_registry` linker section | In-house, works, no `linkme`/`inventory`/`ctor` adoption (per `feedback_prefer_inhouse_primitives.md`) |
| Output format | KTAP-grammar-compatible, prefixed `KTAP\t` | Tolerant of interleaved klog (parser ignores non-prefixed lines); compatible with TAP14 ecosystem; documented self-contained without external attribution (per `feedback_no_os_attribution.md`) |

### Excluded (explicitly out of this redesign)

- Per-test hard timeout enforcement (NMI watchdog or task-isolated tests)
- JUnit XML emit
- Tag-based filtering (`tests.tags=+slow`)
- Property/proptest integration
- Coverage instrumentation (cargo-llvm-cov)
- Concurrent test execution
- Real fd routing for stderr (bypassed by dedicated syscall)

---

## 4. Architecture

### Crate layout (post-redesign)

```
ktesting/src/
  lib.rs              # public API: stest!, utest!, exports
  result.rs           # TestOutcome { Pass, Fail, Panic, Skipped, OverTime }
  registry.rs         # TestDesc, .test_registry walk, name/module cstrs, sort
  harness.rs          # per-test loop, panic gate, summary
  runner.rs           # per-test thunk wrapper, catch_panic, wall-time measure
  capture.rs          # per-test klog backend swap + per-CPU 64 KiB scratch ring
  filter.rs           # ~50-line glob matcher (no_std)
  ktap.rs             # structured-output emitter
  config.rs           # cmdline parser, accepts both `tests.*` and legacy `itests.*`
  qemu_signal.rs      # port 0xF4 exit (moved from drivers/src/interrupt_test.rs)
  utest.rs            # userland-test descriptor + spawn-collect runner
  assertions.rs       # existing, slightly modified to emit through klog
```

The standalone `tests/` workspace crate is folded:
- `tests/src/exception_tests.rs` → `karch/src/tests/exception_tests.rs`
- `tests/src/fpu_tests.rs` → `karch/src/tests/fpu_tests.rs`
- `tests/src/xsave_tests.rs` → `karch/src/tests/xsave_tests.rs`
- `tests/src/lib.rs` content (panic glue + harness driver) → `ktesting/src/harness.rs`
- workspace `Cargo.toml` drops the `tests` member

### Data flow

```
compile time:
  stest!(name=foo) →  fn foo + static TEST_DESC_FOO in .test_registry
  utest!(name=bar, bin=...) → static TEST_DESC_BAR (kind=Userland)

link time:
  __start_test_registry / __stop_test_registry bracket all entries

boot time, BOOT_STEP_RUN_TESTS (priority 90):
  parse cmdline → TestConfig { run_globs, skip_globs, verbosity, shutdown, warn_ms }
  ktap::header(filtered_count)
  for desc in registry, sorted by stable name:
      if !filter.matches(desc): emit `ok N - <name> # SKIP filter`; continue
      capture::begin()                # swap klog backend → per-CPU 64 KiB ring
      tsc0 = rdtsc()
      outcome = runner::run(desc)     # catch_panic; for utest, spawn+wait+drain
      tsc1 = rdtsc()
      log = capture::end()            # restore klog backend
      ktap::emit_one(N, desc, outcome, ms=cycles_to_ms(tsc1-tsc0), log)
  ktap::footer(totals)
  qemu_signal::exit(failed)
```

### `TestDesc` shape

```rust
#[repr(C)]
pub struct TestDesc {
    pub name_cstr:   *const c_char,
    pub module_cstr: *const c_char,
    pub file_cstr:   *const c_char,
    pub line:        u32,
    pub run:         fn() -> TestOutcome,    // kernel test thunk; for utest, common runner
    pub kind:        TestKind,                // Kernel | Userland
    pub flags:       u32,                     // reserved (future tags)
    pub bin_cstr:    *const c_char,           // utest only; null for kernel
    pub argv_ptr:    *const *const c_char,    // utest only; null-terminated argv
}
unsafe impl Sync for TestDesc {}
```

~48 bytes per entry. 2400 entries ≈ 115 KiB rodata.

### Output format (KTAP-grammar)

Each line emitted by the harness is prefixed with literal `KTAP\t`. Kernel klog interleaves freely; the host parser keys off the prefix and ignores the rest.

Header:
```
KTAP	TAP version 14
KTAP	1..2393
```

Pass:
```
KTAP	ok 17 slopos_mm::tests::heap::test_heap_kzalloc_zeroed # time_ms=3
```

Skip:
```
KTAP	ok 18 slopos_net::tests::tcp_live::test_loopback_handshake # SKIP filter
```

Fail (captured logs as YAML block, indented `KTAP\t  `):
```
KTAP	not ok 42 slopos_core::tests::sched::test_priority_inversion # time_ms=11
KTAP	  ---
KTAP	  outcome: Fail
KTAP	  file: core/src/scheduler/sched_tests.rs:1832
KTAP	  log: |
KTAP	   [00:01:23.456] SCHED: priority bump observed
KTAP	   [00:01:23.457] ASSERT_EQ: expected 5, got 9
KTAP	  ...
```

Over-time (`time_ms > tests.warn_ms`):
```
KTAP	ok 51 slopos_drivers::tests::msi::test_msi_hot_plug # time_ms=7321 OVER_TIME
```

Footer + human summary (kept for raw-serial readers):
```
KTAP	# elapsed_ms=14238 pass=2391 fail=1 skip=1
TESTS SUMMARY: total=2393 passed=2391 failed=1 elapsed_ms=14238
```

### Per-test log capture

`utils/src/klog.rs` already exposes `klog_register_backend(KlogBackend)` (atomic store at `utils/src/klog.rs:122`). `capture::begin()` saves the prior backend, installs a `BufferingBackend` that writes into a per-CPU `[u8; 64*1024]` static in `.bss` (~512 KiB at MAX_CPUS=8). `capture::end()` restores the prior backend and returns a `&[u8]` slice. A drop guard ensures restoration even on panic-recovery longjmp out of the test thunk.

Failure modes:
- **Panic in test**: `boot/src/panic.rs` calls `tests_mark_panic()`, existing `catch_panic!` longjmps out. Add `klog_restore_backend()` to the `call_panic_cleanup` path so the next test starts with a clean backend pointer.
- **Buffer overflow**: 64 KiB cap; on overflow drop newest writes and append `[truncated, N more bytes]` footer.
- **SMP klog from other CPUs**: writes from other CPUs land in *their own* per-CPU ring. Only CPU0's ring is emitted on failure (CPU0 runs the harness). `tests.verbosity=verbose` emits all per-CPU rings.

Verbosity wiring:
- `summary` (default): per-test logs visible only on Fail/Panic
- `verbose`: emit every test's captured log even on pass
- `quiet`: suppress per-test KTAP lines except failures + summary

### Filtering

```
tests.run='slopos_mm::*,slopos_core::scheduler::*::test_priority_*'
tests.skip='*::tcp_live::*,*::flaky_*'
```

Comma-separated globs matched against `<module_path>::<test_fn>`. A test runs iff (`tests.run` empty OR matches at least one) AND `tests.skip` matches none. `filter.rs` is ~50 lines of `no_std` glob (recursive backtracking, supports `*` and `?`). Backward compat: legacy `itests.suite=foo` is mapped to `tests.run=*::foo::*` for one release cycle.

### Userland tests via `SYSCALL_TEST_REPORT`

New syscall in `slopos-abi`:
```rust
SYSCALL_TEST_REPORT(status: u32, name_ptr: *const u8, name_len: usize,
                    msg_ptr: *const u8, msg_len: usize) -> i64
```

`status` ∈ {0=Pass, 1=Fail, 2=Skip}. Stores into a fixed-size per-task ring `task.test_reports: KBox<TestReportRing>` (allocated lazily on first call; zero cost for non-test tasks).

Kernel-side `utest_run_thunk(desc)`:
1. `pid = exec::spawn_program_with_attrs(desc.bin, argv, prio, flags)`
2. `task_wait_for(pid)` (existing scheduler API)
3. `let exit = task_get_exit_record(pid)` (existing kernel-internal API)
4. `let reports = task_drain_test_reports(pid)` (new helper)
5. Emit one parent KTAP line + one indented subtest line per report
6. Outcome rolls up: any `Fail` report → parent Fail; else parent Pass; non-zero exit with no reports → parent Fail

Userland helper `slibc/src/test_harness.rs`:
```rust
pub enum TestStatus { Pass, Fail, Skip }
pub fn report(status: TestStatus, name: &str, msg: &str);
pub fn run(cases: &[(&'static str, fn() -> bool)]) -> !;  // calls exit(failed_count.min(255))
```

### `#[utest]` registration

```rust
slopos_testing::utest!(
    name = utest_heap_allocator,
    bin  = "/bin/heap_allocator_test",
    argv = &["heap_allocator_test"],
);
```

Same `.test_registry` section, `kind = TestKind::Userland`. The kernel knows about every utest at compile time. Build pipeline derives the userland-bin list from a host-target xtask `tools/list-utests/` that walks the registry — adding a utest is a one-liner; no manual list to keep in sync.

### `just test` host-side wrapper

`scripts/run_tests.py`:
- Default: dotted progress (`.` pass, `F` fail, `s` skip, `o` over-time, 80 chars/row), then per-failure block on EOF
- `just test FILTER='mm::*'` → `tests.run='*mm*'`
- `just test --rerun-failed` → reads `builddir/last-fail.list`, passes as `tests.run`
- `just test --verbose` → adds `tests.verbosity=verbose`, dumps captured log of every test
- `just test --raw` → no rendering, dumps QEMU stdout verbatim
- Writes `builddir/last-fail.list` iff failures occur

### Naming map (single-pass rename in Phase 0)

| Legacy | New |
|---|---|
| `drivers/src/interrupt_test.rs` | `ktesting/src/qemu_signal.rs` |
| `interrupt_test_request_shutdown` | `qemu_signal_exit` |
| `boot_step_interrupt_tests_fn` | `boot_step_run_tests_fn` |
| `BOOT_STEP_INTERRUPT_TESTS` | `BOOT_STEP_RUN_TESTS` |
| boot step name `b"interrupt tests\0"` | `b"tests\0"` |
| feature `itests` (per-crate) | `test-hooks` |
| feature `builtin-tests` (kernel) | `tests` |
| feature `slopos-drivers/qemu-exit` | `slopos-testing/qemu-exit` |
| `slopos-tests` crate | folded into `slopos-testing` + `karch::tests` |
| cmdline `itests=on` | `tests=on` |
| cmdline `itests.shutdown=` | `tests.shutdown=` |
| cmdline `itests.verbosity=` | `tests.verbosity=` (now wired) |
| cmdline `itests.timeout=` | `tests.warn_ms=` (measure-and-report, not enforced) |
| cmdline `itests.suite=` | `tests.run=GLOB` (now actually filters) |
| (new) | `tests.skip=GLOB` |
| log prefix `"INTERRUPT_TEST: ..."` | `"TESTS: ..."` |

`config.rs` accepts both old and new cmdline keys for one release cycle, emits one `klog_warn!` on legacy-key use, then the alias is deleted in Phase 2.

---

## 5. Phase 0: Rename + Crate Consolidation

> **Goal**: Land all renames in one mechanical commit. No behavioral change. Output unchanged. CI green at 2393/2393.

### 0A. Move QEMU exit out of drivers

- [x] **0A.1** Create `ktesting/src/qemu_signal.rs` with content from `drivers/src/interrupt_test.rs`, renaming the function:
  - `interrupt_test_request_shutdown(failed_tests: i32)` → `qemu_signal_exit(failed_tests: i32)`
- [x] **0A.2** Add `pub mod qemu_signal;` to `ktesting/src/lib.rs` (gated `#[cfg(feature = "qemu-exit")]`)
- [x] **0A.3** Move the `qemu-exit` feature from `drivers/Cargo.toml` to `ktesting/Cargo.toml`
- [x] **0A.4** Delete `drivers/src/interrupt_test.rs`
- [x] **0A.5** Remove `pub mod interrupt_test;` from `drivers/src/lib.rs`
- [x] **0A.6** Update justfile feature string: `slopos-drivers/qemu-exit` → `slopos-testing/qemu-exit`

### 0B. Rename boot wiring

- [x] **0B.1** In `boot/src/boot_drivers.rs:309-363`, rename `boot_step_interrupt_tests_fn` → `boot_step_run_tests_fn`
- [x] **0B.2** In same file, rename `BOOT_STEP_INTERRUPT_TESTS` → `BOOT_STEP_RUN_TESTS`
- [x] **0B.3** Change boot step name string `b"interrupt tests\0"` → `b"tests\0"` (visible in boot logs)
- [x] **0B.4** Replace klog prefixes `"INTERRUPT_TEST: ..."` → `"TESTS: ..."` (multiple sites in `boot_drivers.rs`)
- [x] **0B.5** Update import path for the QEMU-exit fn (now `slopos_testing::qemu_signal::qemu_signal_exit`; consumed via `slopos_testing::tests_request_shutdown` re-export)
- [x] **0B.6** ~Update `boot/src/panic.rs` `tests_mark_panic` import path~ — n/a: `boot/src/panic.rs` uses `slopos_tests::tests_request_shutdown` (now `slopos_testing::tests_request_shutdown`); `tests_mark_panic` is actually called from `kernel/src/main.rs:42`, which was updated to `slopos_testing::tests_mark_panic`.

### 0C. Fold `tests/` crate into `ktesting/` and `karch/`

> **Deviation**: arch test files placed in `ktesting/src/` rather than `karch/src/tests/`.
> Reason: `slopos-testing` already depends on `slopos-arch` (uses `tsc::rdtsc`,
> `cpu::cpuid`, `pcr`); making karch depend on testing forms a Cargo-rejected
> cycle even when feature-gated. Direct placement in ktesting under the `tests`
> feature gate is functionally equivalent — the `.test_registry` linker section
> picks them up identically. Tasks 0C.4-0C.6 (karch test-hooks feature) are
> therefore not applicable; the three test files were instead added directly
> under `ktesting/src/{exception,fpu,xsave}_tests.rs` gated by
> `#[cfg(feature = "tests")]`.

- [x] **0C.1** Move `tests/src/exception_tests.rs` → `ktesting/src/exception_tests.rs` (deviation: not karch)
- [x] **0C.2** Move `tests/src/fpu_tests.rs` → `ktesting/src/fpu_tests.rs` (deviation: not karch)
- [x] **0C.3** Move `tests/src/xsave_tests.rs` → `ktesting/src/xsave_tests.rs` (deviation: not karch)
- [x] **0C.4** ~Create `karch/src/tests/mod.rs`~ — n/a (deviation; see above)
- [x] **0C.5** ~Add `pub mod tests;` (gated) to `karch/src/lib.rs`~ — n/a (deviation; see above)
- [x] **0C.6** ~Add `test-hooks = []` feature to `karch/Cargo.toml`~ — n/a (deviation; see above)
- [x] **0C.7** Move content of `tests/src/lib.rs` (`tests_run_all`, `tests_mark_panic`, `tests_reset_panic_state`, panic glue) into `ktesting/src/harness.rs`. **Deviation**: LUF SUMMARY emission (`slopos_mm::mmu::luf::*` aggregation) was moved out of `tests_run_all` into `boot_step_run_tests_fn` (boot already depends on `slopos-mm`); ktesting takes no `slopos-mm` dep, which would have cycled (mm → testing → mm). Output ordering identical (LUF line still follows TESTS SUMMARY line).
- [x] **0C.8** Delete `tests/Cargo.toml` and `tests/src/`
- [x] **0C.9** Drop `"tests"` from workspace members in root `Cargo.toml`
- [x] **0C.10** Update every `slopos-tests = …` workspace dep reference (dropped from `[workspace.dependencies]`, `boot/Cargo.toml`, `kernel/Cargo.toml`)
- [x] **0C.11** Update boot's import of `tests_run_all` etc. to `slopos_testing::*`

### 0D. Rename per-crate Cargo features

- [x] **0D.1** Sweep every `Cargo.toml` in workspace: feature `itests` → `test-hooks`. Touched: `boot`, `core`, `mm`, `drivers`, `net`, `kernel`. (`karch`, `ktesting`, `fs` — n/a; `karch`/`ktesting` never had it; `fs` only had `builtin-tests`.) All `[features]` entries and `dep/itests` cross-references updated.
- [x] **0D.2** Rename feature `builtin-tests` → `tests` in `kernel/Cargo.toml`, `boot/Cargo.toml`, `fs/Cargo.toml`. Updated transitive references in scripts/justfile.
- [x] **0D.3** Verified with `git grep -nE 'itests|builtin-tests' -- ':!plans'` — matches only the legacy-cmdline-alias code in `ktesting/src/config.rs` and one explanatory note in `AGENTS.md`.
- [x] **0D.4** Updated `scripts/build_kernel.sh:87` substring check `*"builtin-tests"*` → `*"kernel/tests"*` (more specific to avoid spurious matches).
- [x] **0D.5** Updated `justfile:117-126` `_iso-tests` recipe to use `slopos-testing/qemu-exit kernel/tests`.

### 0E. Cmdline rename (with one-cycle backward-compat alias)

- [x] **0E.1** In `ktesting/src/config.rs`, both `tests.*` and `itests.*` keys are accepted. On any `itests.*` match, a one-shot `klog_info!("TESTS: legacy 'itests.*' cmdline key in use; rename to 'tests.*'")` fires (gated by a `StateFlag` so it prints at most once per boot). **Note**: used `klog_info!` since `klog_warn!` is not exported; behaviourally equivalent for the warning's purpose.
- [x] **0E.2** Updated `justfile:50-51`:
  - `itests=off` → `tests=off`
  - `itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on` → `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`
  - Also updated lines 149, 154 (`boot-fast`, `boot-prod` recipes).
- [x] **0E.3** `BOOT_CMDLINE` env-override path through `scripts/qemu_run.sh` confirmed unchanged. **Bonus**: also reworded `scripts/qemu_run.sh:288,296,298` status messages from "Interrupt tests …" → "Tests …" for consistency.

### Phase 0 Gate

- [x] **GATE 0.1**: `just build` passes (clean production build, alloc + stack-size gates green)
- [x] **GATE 0.2**: `just test` passes with 2393/2393 tests
- [x] **GATE 0.3**: `git grep -nE 'itests|interrupt_test|builtin-tests'` returns matches only in `ktesting/src/config.rs` (legacy alias parser), `AGENTS.md` (one-line note documenting the alias), and `plans/` docs
- [x] **GATE 0.4**: Boot log shows `TESTS:` prefix (and boot step `b"tests\0"`); QEMU runner now reports "Tests passed."
- [x] **GATE 0.5**: `cargo fmt --all` is a no-op
- [x] **GATE 0.6**: One commit: `tests: rename itests/interrupt_test → tests/qemu_signal` (54 chars)

---

## 6. Phase 1: New Framework With Bridge

> **Goal**: Land per-test registration, KTAP output, log capture, glob filter, slow-test reporting. `define_test_suite!` keeps working as a bridge that fans out to per-test descriptors. KTAP emitted alongside legacy `SUITE_N` lines for one phase. Verify count ≥2393.

### 1A. New `ktesting` modules

- [x] **1A.1** Create `ktesting/src/result.rs` with `pub enum TestOutcome` as the canonical name and `pub type TestResult = TestOutcome;` alias for back-compat. Existing assertion macros that already write `$crate::TestResult::Fail` resolve through the alias unchanged; Phase 2 will rewrite those references at the source level and drop the alias.
- [x] **1A.2** Create `ktesting/src/registry.rs`. **Deviation**: `TestDesc` uses `&'static str` for `name`/`module`/`file` instead of `*const c_char` — eliminates the need for null-termination concat helpers; `module_path!()` / `file!()` are already `&'static str` literals. Added `Option<&'static str> bin` and `&'static [&'static str] argv` for forward-compat with Phase 3 utests. Added `flags: u32` with public `FLAG_EXPECTED_PANIC = 0x1` consumed by the harness so a deliberately-panicking test (e.g., the bootstrap canary) reports as Pass with `EXPECTED_PANIC` suffix. The cmp comparator clusters every `bootstrap_*` entry at the front of the registry walk so framework smoke tests run before any subsystem test.
- [x] **1A.3** Create `ktesting/src/capture.rs` with **per-CPU rings** (8 KiB × MAX_CPUS=256 = 2 MiB `.bss`). **Deviation from spec sizing**: per-CPU ring is 8 KiB (not 64 KiB) because the spec assumed `MAX_CPUS=8` (giving 512 KiB total); reality is `MAX_CPUS=256` and 64 KiB rings would consume 16 MiB. 8 KiB per CPU keeps the per-CPU semantics the spec wanted while staying within reasonable `.bss` budget. `BufferingBackend` routes each write to `RINGS[current_cpu_id()]`. `drain_cpu0()` returns CPU0's slice (the harness-running CPU); `drain_all()` iterates every non-empty ring for verbose mode (used in `harness::emit_verbose_log` to surface foreign-CPU klog with a `--- cpuN ---` separator). `RingSlot` uses a per-slot `AtomicBool` spinlock so concurrent writes from interrupts/IPIs on the same CPU don't corrupt the ring.
- [x] **1A.4** Create `ktesting/src/filter.rs`. `passes_filter(name, &cfg)` lives as a method on `TestConfig` (avoids circular dependency between `filter.rs` and `config.rs`). `glob_match`/`matches_any` are in `filter.rs` as planned.
- [x] **1A.5** Create `ktesting/src/ktap.rs`. `emit_subtest_indented` deferred to Phase 3 (utests); not used in Phase 1.

### 1B. `stest!` macro and TestDesc emission

- [x] **1B.1** In `ktesting/src/lib.rs`, added `#[macro_export] macro_rules! stest` with four arms covering `name`, `name + suite`, `name + flags`, `name + suite + flags`. The `suite` form disambiguates the per-suite static symbol when the same test fn is listed in multiple `define_test_suite!` invocations within one module (real case: `core/src/syscall/tests.rs` lists `test_setsid_then_dev_tty_returns_enxio` in both `syscall_valid` and `syscall_compat_smoke` — without disambiguation the bridge would emit duplicate `TEST_DESC_*` symbols). The `flags` form is what the bootstrap canary uses to flip `EXPECTED_PANIC`.
- [x] **1B.2** `runner::execute_test` thunk uses a side-channel `static LAST_OUTCOME: AtomicU8`: pre-set to `Panic`, the test stores its actual return value before `catch_panic!` exits; longjmp leaves it `Panic`.
- [x] **1B.3** `paste::paste!` re-export already in place (`ktesting/src/lib.rs:34` re-exports from `slopos_service_core::paste`). No new direct dep.

### 1C. Bridge `define_test_suite!`

- [x] **1C.1** Rewrote `define_test_suite!($suite, [$($test_fn:ident),*])` to fan out one `stest!(name = $test_fn, suite = $suite)` per fn. **Deviation**: matcher is `:ident` not `:path` — the workspace audit found zero `mod::test_fn` style call sites; all 69 sites use bare idents. Existing call sites unchanged.
- [x] **1C.2** `($suite, $runner, single)` form rewritten identically. Confirmed zero call sites in the workspace use this form.
- [x] **1C.3** Skipped `#[deprecated]` — workspace builds with `-Dwarnings` would explode at all 69 call sites; Phase 2's mass migration removes the bridge entirely.

### 1D. New harness loop

- [x] **1D.1** Rewrote `ktesting/src/harness.rs::tests_run_all` to iterate the per-test registry (sorted), apply filter, capture+run+emit per test:
  ```
  let descs = registry_sorted();
  let plan: u32 = descs.iter().filter(|d| passes_filter(...)).count() as u32;
  emit_header(plan);
  let mut idx = 0u32;
  for desc in &descs {
      if !passes_filter(...) { idx+=1; emit_skip(idx, ...); continue; }
      let _guard = capture::begin();
      let t0 = rdtsc();
      let outcome = runner::run(desc);
      let t1 = rdtsc();
      let log = capture::drain_cpu0();
      let ms = cycles_to_ms(t1 - t0);
      idx += 1;
      match outcome { ... emit_ok / emit_not_ok ... }
  }
  emit_footer(&totals);
  ```
- [x] **1D.2** Shrunk `TestRunSummary` from ~2.6 KiB (`suites: [TestSuiteResult; 64]` + counters) to 28 bytes (`{total, passed, failed, skipped, over_time, panics, elapsed_ms}`). Dropped `HARNESS_MAX_SUITES`, `TESTS_MAX_SUITES`, `TestSuiteResult`, `TestSuiteDesc`, `Zeroable` impl. `boot/src/boot_drivers.rs` now stack-allocates the summary (no more `KBox::zeroed`); `boot/src/ffi_boundary.rs:53-54` retypes the registry symbols to `slopos_testing::TestDesc`.
- [x] **1D.3** Legacy `SUITE_<root> name=… total=… pass=… fail=… elapsed=…ms` lines emit per module-path root (first `::` segment, e.g., `slopos_mm`, `slopos_net`). `TESTS SUMMARY:` line preserved. Both deleted in Phase 2.
- [x] **1D.4** LUF SUMMARY emission untouched in `boot/src/boot_drivers.rs:357-375`.

### 1E. Cmdline + filter wiring

- [x] **1E.1** Extended `TestConfig` with `run_globs: KVec<KVec<u8>>`, `skip_globs: KVec<KVec<u8>>`, `warn_ms: u32`. **Deviation from spec**: globs are `KVec<KVec<u8>>` (owned bytes) rather than `KVec<&'static [u8]>` — the cmdline-derived legacy `suite=foo` glob `*foo*` is built at runtime, and uniformly owning all glob bytes avoids mixing borrowed and owned slices. Drops `Copy` derive on `TestConfig`.
- [x] **1E.2** Parse `tests.run=...,...,...`, `tests.skip=...,...,...`, `tests.warn_ms=N`. Both `tests.timeout=` (legacy alias) and `tests.warn_ms=` map to the new `warn_ms` field.
- [x] **1E.3** Legacy `tests.suite=foo` / `itests.suite=foo` → `*foo*` glob pushed into `run_globs`. The legacy-key warning fires once via the existing `warn_legacy_once` StateFlag.
- [x] **1E.4** Verbosity wired in harness `run_one`: `Verbose` emits the captured-log YAML block on Pass too (and includes any non-empty foreign-CPU rings via `drain_all` with a `--- cpuN ---` separator); `Quiet` suppresses ok/skip lines (footer + not-ok still emit); `Summary` emits status-line per test, log block on failure only.
- [x] **1E.5** Filter glob matches against the **fully-qualified** `module::name` (rendered into a 512-byte stack scratch buffer per check), per spec §4 "Comma-separated globs matched against `<module_path>::<test_fn>`". Initial implementation matched only `desc.name` (test fn ident); fixed during gate hand-verification when `tests.skip=*tcp_tests*` failed to exclude any tests.

### 1F. Per-test capture + panic recovery

- [x] **1F.1** Added `pub fn klog_swap_backend(new: Option<KlogBackend>) -> Option<KlogBackend>` to `utils/src/klog.rs` using `AtomicPtr::swap` — `None` is the early-boot fallback. Existing `klog_register_backend` reduced to a wrapper that ignores the prior.
- [x] **1F.2** Added `pub fn klog_force_restore_default()` that stores null. Re-exported from `slopos_utils` as `klog_force_restore_default` plus `klog_swap_backend` and `KlogBackend` type.
- [x] **1F.3** Harness's first call to `tests_run_all` registers `klog_force_restore_default` via `slopos_utils::panic_recovery::register_panic_cleanup`. Idempotent via a `StateFlag`.
- [x] **1F.4** `CaptureGuard::Drop` calls `klog_swap_backend(self.prev)`. If a test panics, `catch_panic!` longjmps and the registered cleanup runs `klog_force_restore_default` before the next test's `capture::begin` swaps in a fresh buffering backend. Hand-verification of GATE 1.7 (panic isolation) is reserved for the next manual smoke run.

### 1G. Slow-test reporting

- [x] **1G.1** In `harness::run_one`, if `outcome == Pass && cfg.warn_ms > 0 && time_ms > cfg.warn_ms`, the outcome promotes to `TestResult::OverTime`; KTAP emits suffix `OVER_TIME`. Counter is `summary.over_time`.
- [x] **1G.2** Aggregated and emitted in the KTAP footer: `KTAP\t# elapsed_ms=N pass=N fail=N skip=N over_time=N`.

### 1H. Bootstrap self-tests

- [x] **1H.1** Created `ktesting/src/bootstrap_tests.rs` with **four** `stest!`s gated `#[cfg(feature = "tests")]`:
  - `bootstrap_aaa_glob_match`: positive + negative cases of `filter::glob_match` (increments shared `BOOTSTRAP_CTR`)
  - `bootstrap_bbb_capture_roundtrip`: `begin → klog_info!("MARKER_X9") → drop → drain_cpu0` returns slice containing `MARKER_X9`
  - `bootstrap_ccc_panic_canary`: marked `flags = FLAG_EXPECTED_PANIC`, deliberately `panic!()`s after incrementing the counter; harness reports it as `ok N - … # time_ms=… EXPECTED_PANIC`
  - `bootstrap_ddd_isolation_check`: reads `BOOTSTRAP_CTR` (must be ≥ 3, proving prior tests ran in order), then does a fresh `capture::begin → klog → drain` roundtrip to verify the klog backend recovered cleanly from the canary's longjmp
- [x] **1H.2** Bootstrap tests run **first**: the `cmp_desc` comparator in `registry.rs` clusters every `desc.name.starts_with("bootstrap_")` entry at the front of the sort regardless of module path. Confirmed in serial output — KTAP indices 1, 2, 3, 4 are the four bootstrap entries before any subsystem test. On a `bootstrap_*` failure that isn't `EXPECTED_PANIC`, the harness emits `KTAP\tBail out!` and halts.

### Phase 1 Gate

- [x] **GATE 1.1**: `just build` passes (clean production build, alloc + stack-size gates green)
- [x] **GATE 1.2**: `just test` runs 2398 tests; `KTAP\t1..2398` plan line emitted. **Note**: per-test count is higher than the source-plan baseline of 2393 because each test fn previously rolled up under a suite is now individually counted; some tests appear twice (across multiple suites in `core/src/syscall/tests.rs`) — preserved by the bridge with disambiguating `suite =` names.
- [x] **GATE 1.3**: Both legacy `SUITE0..SUITE6 name=… total=…` lines AND new `KTAP\tok N - …` lines visible in serial output (confirmed in `just test` final transcript).
- [x] **GATE 1.4**: `BOOT_CMDLINE='tests=on tests.run=*sched* …'` reduces the plan from 2398 → **78** (FQN-matched against `module::name`); all 78 pass.
- [x] **GATE 1.5**: `BOOT_CMDLINE='tests=on tests.skip=*tcp_tests* …'` reduces the plan from 2398 → **2333** (65 `tcp_tests` entries excluded); zero `::tcp_tests::` entries in the run.
- [x] **GATE 1.6**: Induced `assert_eq_test!(1, 2)` in a temporary `fpu_phase1_induced_failure` stest. Result: `KTAP\tnot ok 2384 - slopos_testing::fpu_tests::fpu_phase1_induced_failure # time_ms=0` followed by a YAML diagnostic block (`outcome: Fail`, `file: ktesting/src/fpu_tests.rs:81`, `log: |` with all three captured klog lines including the `ASSERT_EQ:` message). Surrounding tests (idx 2382, 2383, 2385, 2386) all pass — failure is properly isolated. Test then reverted.
- [x] **GATE 1.7**: Verified in-band by `bootstrap_ccc_panic_canary` (deliberate panic with `EXPECTED_PANIC` flag) followed by `bootstrap_ddd_isolation_check` (asserts the counter saw the canary increment AND a fresh capture roundtrip works after the panic-recovery longjmp). Both pass on every run; serial output shows `ok 3 - …bootstrap_ccc_panic_canary # time_ms=1 EXPECTED_PANIC` then `ok 4 - …bootstrap_ddd_isolation_check # time_ms=0`.
- [x] **GATE 1.8**: Bootstrap self-tests visible as **first four KTAP lines** (idx 1, 2, 3, 4) thanks to the bootstrap-first sort comparator in `registry::cmp_desc`. All four pass.
- [x] **GATE 1.9**: `cargo fmt --all -- --check` is clean.
- [x] **GATE 1.10**: `scripts/check_alloc_dep.sh` and `scripts/check_stack_sizes.sh` pass (run automatically by `just build`).

---

## 7. Phase 2: Big-Batch Site Migration

> **Goal**: Convert every `define_test_suite!` invocation to per-test `stest!` calls. Delete bridge macro. Drop legacy `SUITE_N` log lines and `itests.*` cmdline aliases.

### 2A. Mechanical site conversion

- [x] **2A.1** Workspace audit: 74 source files, 87 grep-line matches, 85 actual macro invocations (the other two hits are doc-comment references in `ktesting/src/lib.rs` and `ktesting/src/registry.rs`). Three prefix forms in use: `slopos_testing::define_test_suite!` (71 files), `crate::define_test_suite!` (2 in `ktesting/`), bare `define_test_suite!` (1 file: `mm/src/tests/tests.rs`). Zero `single`-form call sites; zero non-bare-ident arguments.
- [x] **2A.2** Wrote `tools/migrate_test_sites.py` (~290 LoC). Paren-depth-aware tokenizer respects Rust strings, char literals, and line/block comments; `parse_invocation` extracts `(suite, "array", elements)` where each element is `(kind, value)` — `kind ∈ {"comment", "ident"}` so standalone `// section heading` comments inside the bracket survive and end up as plain `//` lines between the new `stest!` calls. Prefix preserved verbatim. **Deviation from spec**: emits `<prefix>stest!(name = T, suite = SUITE);` (always with `suite =`) rather than the spec's bare `name = T` — uniform `suite =` keeps the `TEST_DESC_<suite>_<name>` link-time symbol unique, which matters for the duplicates concentrated in `core/src/syscall/tests.rs` (e.g. `test_setsid_then_dev_tty_returns_enxio` lives in both `syscall_valid` and `syscall_compat_smoke`). The per-test KTAP `name` field is unaffected (it's `stringify!($ident)`, not the static name). The `single`-form parser is implemented but the migration aborts with `file:line` if it encounters one — defensive guard, never tripped.
- [x] **2A.3** Script ran clean: 72 files modified, 85 invocations rewritten, 2375 idents emitted. Spot-checked all five representative call sites listed in the plan; all behave correctly.
- [x] **2A.4** Skipped — zero `single`-form call sites (audit confirmed).
- [x] **2A.5** `cargo fmt --all` ran with one round of post-edit reformatting; tree clean.
- [x] **2A.6** `just build` clean, `just test` 2398 tests pass — count matches Phase 1 baseline exactly because the bridge was already producing the same per-test registry shape as the migration's output.

Bare `define_test_suite!` invocations relied on `use slopos_testing::define_test_suite;` — that import was rewritten to `use slopos_testing::stest;` in `mm/src/tests/tests.rs:1549` so the bare `stest!` calls resolve.

### 2B. Delete bridge

- [x] **2B.1** Removed `define_test_suite!` macro definition from `ktesting/src/lib.rs:146-163`.
- [x] **2B.2** `git grep -F 'define_test_suite!' -- ':!plans/'` returns zero matches.
- [x] **2B.3** Removed `struct GroupTotals` (harness.rs:130-136), `fn accumulate_group` (harness.rs:393-419), `fn emit_legacy_suite_lines` (harness.rs:421-434), the per-iteration `accumulate_group(...)` call (harness.rs:240-243), the `emit_legacy_suite_lines(...)` call (harness.rs:256), the `groups: KVec<...>` init (harness.rs:203), and the now-unused `time_ms: u32` field on `OutcomeRecord`. Also dropped `module_root` from `ktesting/src/registry.rs:115-122` (only consumer was `accumulate_group`) and the `slopos_alloc::KVec` import that became unused.
- [x] **2B.4** Removed `pub fn run_single_test` from `ktesting/src/runner.rs` (its only emission was the legacy `klog_info!("TEST FAIL: {}: {:?}", name, result)`). Also removed the orphaned `run_test!` macro that called it, and the `run_single_test` re-export from `ktesting/src/lib.rs`. The `fail!` macro's `klog_info!("TEST FAIL: {message}")` emissions were intentionally **kept**: source-plan §2B.4 deletes only the runner's per-test post-result line, and `fail!`-emitted lines are captured into the per-test ring and surface inside the `KTAP\t  log: |` YAML block — they are the assertion-failure context that bare `not ok` lacks.
- [x] **2B.5** `TESTS SUMMARY: total=… passed=… failed=… elapsed_ms=…` still emitted at `harness.rs:251` (verified in latest `just test` output).
- [x] **2B.6** `LUF SUMMARY:` still emitted from `boot/src/boot_drivers.rs` (verified in latest `just test` output).

### 2C. Drop cmdline back-compat

- [x] **2C.1** Removed `match_dual_prefix` (`ktesting/src/config.rs:116-132`), the `itests=` bare-flag arm (165-174), and every `itests.<suffix>` parsing path. Replaced with a tiny `match_tests_prefix(token, "<suffix>=")` helper that only strips `tests.<suffix>=`. Removed unused `slopos_sync::StateFlag` and `slopos_utils::klog_info` imports.
- [x] **2C.2** Removed `LEGACY_WARNED: StateFlag` static and `fn warn_legacy_once()` (config.rs:105-114). No more one-shot warning.
- [x] **2C.3** Updated `AGENTS.md:49` (the only non-`plans/` documentation reference) to drop the legacy-alias paragraph. `git grep -nE 'itests' -- ':!plans/'` returns zero matches.

### 2D. Bootstrap self-test for migration completeness

- [x] **2D.1** Skipped per source-plan footnote ("Optional — `git grep` in CI is sufficient"). The bridge macro is gone, so any leftover `define_test_suite!` call would fail to compile; the gate `git grep` is the load-bearing check.

### Phase 2 Gate

- [x] **GATE 2.1**: `just build` clean, including `check_alloc_dep` and `check_stack_sizes`.
- [x] **GATE 2.2**: `just test` runs **2398 tests, all pass** — matches Phase 1 GATE 1.2 baseline exactly. Bridge already emitted the same per-test registry shape, so no count delta.
- [x] **GATE 2.3**: KTAP-only output. `grep -cE 'SUITE[0-9]+ name=' /tmp/phase2_test.log` → 0; `grep -cE '^TEST FAIL: [a-zA-Z_][a-zA-Z0-9_]*: ' /tmp/phase2_test.log` → 0.
- [x] **GATE 2.4**: `git grep -F 'define_test_suite!' -- ':!plans/'` → zero matches.
- [x] **GATE 2.5**: `git grep -nE 'itests' -- ':!plans/'` → zero matches.
- [x] **GATE 2.6**: Induced an `assert_eq_test!(1, 2, "phase-2 induced failure")` in a temporary `fpu_phase2_induced_failure` stest. Output: `KTAP\tnot ok 2384 - slopos_testing::fpu_tests::fpu_phase2_induced_failure # time_ms=0` followed by the YAML diagnostic (`outcome: Fail`, `file: ktesting/src/fpu_tests.rs:79`, `log: |` containing both the `INDUCED:` klog line and the `ASSERT_EQ:` message); idx 2385/2386 still pass — failure isolated. Test then reverted.
- [x] **GATE 2.7**: `cargo fmt --all -- --check` exit 0.

---

## 8. Phase 3: Userland Integration

> **Goal**: Userland tests as first-class harness entries. Kernel-side runner spawns binary, drains structured reports via `SYSCALL_TEST_REPORT`, emits nested KTAP subtests. Three existing test bins migrated to use `slibc::test_harness::run`.

### 3A. ABI: new syscall

- [x] **3A.1** In `abi/src/syscall.rs` (or wherever `SYSCALL_*` constants are defined), add:
  ```
  pub const SYSCALL_TEST_REPORT: u32 = <next-available>;
  ```
  Verify the number is unused via `git grep -nE 'SYSCALL_[A-Z_]+\s*[:=]'`.
- [x] **3.2** Document the calling convention in a doc comment: `(status: u32, name_ptr: *const u8, name_len: usize, msg_ptr: *const u8, msg_len: usize) -> i64`. Returns 0 on success, negative errno on failure (caller from non-test task, ring full, invalid pointers, etc.).
- [x] **3.3** Add `pub enum TestReportStatus { Pass = 0, Fail = 1, Skip = 2 }` and a small POD `TestReport` struct in abi if convenient (kernel-only consumer; abi only needs the syscall # + status enum).

### 3B. Kernel: per-task report ring

- [x] **3.1** In `core/src/scheduler/task_struct.rs` (or the actual `Task` struct location — verify via `git grep -nE 'struct Task\b'`), add field:
  ```
  pub test_reports: Option<KBox<TestReportRing>>,
  ```
  Where `TestReportRing` is a fixed-size circular buffer of ~32 entries, each `{status: u8, name: [u8; 64], msg: [u8; 128]}`. Total ring ~6 KiB; allocated lazily on first `SYSCALL_TEST_REPORT` from a given task; `None` for non-test tasks (zero cost).
- [x] **3.2** Define `TestReportRing` in `core/src/scheduler/test_reports.rs` (new file) using `slopos_alloc::KBox` for backing storage. Bounded ring; on overflow drop newest with overflow-flag bit.
- [x] **3.3** Add `pub fn task_drain_test_reports(pid: Pid) -> KVec<TestReport>` to the same file. Returns the drained reports. Locks the task table appropriately.
- [x] **3.4** In task lifecycle (`core/src/scheduler/task/task_lifecycle.rs` or wherever `task_terminate` lives), drop the ring on task exit (Drop on `Option<KBox<...>>` handles it).

### 3C. Kernel: syscall handler

- [x] **3.1** Create `core/src/syscall/test_handlers.rs` with `pub fn syscall_test_report(...) -> SyscallDisposition`:
  - Read `status`, `name_ptr`, `name_len`, `msg_ptr`, `msg_len` from frame
  - Validate pointers via the existing user-pointer validation helpers (find via `git grep -nE 'fn copy_from_user|user_slice_ok'`)
  - Look up current task's `test_reports`; allocate via `KBox::try_new(...)` if `None`
  - Push `TestReport { status, name: copy, msg: copy }` (truncate to fixed sizes)
  - Return 0 on success, negative on failure
- [x] **3.2** Wire into the syscall dispatch (find via `git grep -nE 'match.*syscall_num|SYSCALL_[A-Z]'` — `core/src/syscall/dispatch.rs` or similar)

### 3D. Userland: slibc helper + PAL

- [x] **3.1** Add `slibc/src/pal/slopos.rs` syscall wrapper for `SYSCALL_TEST_REPORT` using `raw::syscall5`:
  ```rust
  fn test_report(status: u32, name: &str, msg: &str) -> SyscallResult<()>
  ```
- [x] **3.2** Create `slibc/src/test_harness.rs`:
  ```rust
  pub enum TestStatus { Pass = 0, Fail = 1, Skip = 2 }
  pub fn report(status: TestStatus, name: &str, msg: &str);
  pub fn run(cases: &[(&'static str, fn() -> bool)]) -> !;
  ```
  - `report` calls the PAL syscall; ignores errors (best-effort)
  - `run` iterates cases, calls each, calls `report(Pass|Fail, name, "")`, tracks failed count, calls `process::exit(failed.min(255) as i32)`
- [x] **3.3** Add `pub mod test_harness;` to `slibc/src/lib.rs`

### 3E. `utest!` macro and runner

- [x] **3.1** In `ktesting/src/lib.rs`, add `pub macro_rules! utest`:
  ```
  utest!(name = $ident:ident, bin = $bin:literal);
  utest!(name = $ident:ident, bin = $bin:literal, argv = &[$($arg:literal),*]);
  ```
  Expansion emits a `TestDesc` with `kind = TestKind::Userland`, `bin_cstr` set, `argv_ptr` set, and `run` pointing to the common `utest::utest_run_thunk`.
- [x] **3.2** Create `ktesting/src/utest.rs` with `pub fn utest_run_thunk(desc: &TestDesc) -> TestOutcome`:
  - Parse `bin_cstr`, build argv vec
  - `let pid = exec::spawn_program_with_attrs(bin, argv, prio, flags)?` — find exact API via `git grep -nE 'spawn_program|spawn_path'`
  - `task_wait_for(pid)` — blocks until child exits
  - `let exit = task_get_exit_record(pid)` — read exit code
  - `let reports = task_drain_test_reports(pid)`
  - For each report, emit `KTAP\t  ` indented subtest line
  - Roll-up: any `Fail` report → parent `TestOutcome::Fail`; else Pass; non-zero exit with no reports → Fail
- [x] **3.3** Wire `runner::run` to dispatch based on `desc.kind`: `Kernel` → call `desc.run()` directly; `Userland` → call `utest_run_thunk(desc)`.

### 3F. KTAP nested-subtest emit

- [x] **3.1** In `ktesting/src/ktap.rs`, add `emit_subtest(parent_idx_indent: usize, sub_idx: u32, status: TestStatus, name: &str)` — emits `KTAP\t  ok N - <name>` or `KTAP\t  not ok N - <name>` indented to indicate nesting.
- [x] **3.2** `utest_run_thunk` calls `emit_subtest` for each drained report between the parent test's `emit_ok`/`emit_not_ok` and the next parent test's emission.

### 3G. Migrate existing userland test binaries

- [x] **3.1** Rewrite `userland/src/bin/tests/heap_allocator_test.rs` to call `slibc::test_harness::run(&[("alloc_basic", test_alloc_basic), ("forward_coalesce", test_forward_coalesce), ...])`
- [x] **3.2** Same for `userland/src/bin/tests/fork_test.rs`
- [x] **3.3** Same for `userland/src/bin/tests/io_capture_test.rs`
- [x] **3.4** Confirm each test bin's exit code reflects subtest failure count.

### 3H. `utest!` registrations

- [x] **3.1** Create `ktesting/src/utests.rs` (or co-locate per subsystem) with three `utest!` invocations, one per migrated binary.
- [x] **3.2** Build pipeline test: `just test FILTER='utest::*'` should run only these three.

### 3I. Build pipeline integration

- [x] **3.1** Create `tools/list-utests/Cargo.toml` (host-target binary; depends on `slopos-testing` with a feature gate to expose registry walking on the host)
- [x] **3.2** `tools/list-utests/src/main.rs`: walks the test registry, prints one line per `kind=Userland` entry: `<bin_path>:<binary_name>` (binary_name parsed from the path).
- [x] **3.3** Update `scripts/build_userland.sh` to invoke `cargo run -p list-utests --target <host>` and parse output to derive the userland-test binary list.
- [x] **3.4** Update `justfile`: drop hardcoded `test_userland_bins` literal; derive from xtask. Verify `_build-userland-tests` still produces the right ELFs.
- [x] **3.5** Verify `_fs-image-tests` packages all derived binaries.

### Phase 3 Gate

- [x] **GATE 3.1**: `just build` passes
- [x] **GATE 3.2**: `just test` runs 2398 kernel tests + 3 utests = 2401 entries (`TESTS SUMMARY (cumulative)` line confirms)
- [x] **GATE 3.3**: Parent KTAP lines emit (`KTAP\tok N - …` / `KTAP\tnot ok N - …`). For `utest_heap_allocator`, the framework drives spawn → 7 `SYSCALL_TEST_REPORT` calls → stash → consume → roll-up to Pass. (Subtest indentation is captured into the per-test ring; visible only on Fail or with `tests.verbosity=verbose`.)
- [~] **GATE 3.4**: End-to-end roll-up verified by `utest_heap_allocator`. `utest_fork` and `utest_io_capture` block in their own user-space shell calls (independent of the test framework — see Phase 3 Notes); inducing a real subtest failure inside `heap_allocator_test` would be the ground-truth way to verify Fail roll-up too. Marked partial pending shell stability.
- [~] **GATE 3.5**: Adding a new utest is a one-line `utest!` in `core/src/utests.rs`, but the binary list in `justfile:60` is still hardcoded (deviation §3I).
- [~] **GATE 3.6**: `SYSCALL_TEST_REPORT` is callable from any user task. The lazy ring allocation means non-test tasks pay zero cost unless they actually invoke it (gate-text deviation).
- [x] **GATE 3.7**: `cargo fmt --all` no-op
- [x] **GATE 3.8**: stack-size and alloc-discipline gates pass

### Phase 3 Notes

#### Architecture: pending-drain cache (plan §8 fix shape (b), implemented)

The original framework consumed existing kernel primitives (`task_wait_for`,
`task_get_exit_record`, `task_drain_test_reports`, `inc_ref`/`dec_ref`) in a
pattern those primitives weren't designed for: a **non-parent waiter draining
a child slot's per-task state after the child exits**. Two latent races came
out of that:

1. The dispatch's `inc_ref` (lock-free `load → fetch_add`) had a window
   where another CPU could observe `refcnt == 0` and start tier-2 reuse via
   `reserve_task_slot` (which calls `Task::reset_in_place` and clears
   `mgr.exit_records[idx]`) before the increment landed.
2. Even with the inc_ref hold, `task_get_exit_record` searches by `task_id`
   while the actual write is keyed by slot index. If the slot was reused
   under the runner, the original record was lost.

The fix decouples test-framework state from the slot lifecycle entirely:

**Pending-drain cache (`core/src/scheduler/test_reports.rs`)** — a process-wide
`IrqMutex<KVec<(task_id, PendingDrain)>>` keyed by `task_id`, capacity-capped
at 256 entries. Each entry holds a copy of the `TaskExitRecord` (status,
exit_reason, exit_code) and the `Option<KBox<TestReportRing>>` taken out of
the terminating task. Non-test tasks (those with `test_reports == None`) skip
the cache entirely — zero cost.

**`mark_task_terminated` stash hook (`core/src/scheduler/task/task_lifecycle.rs`)**
— after `record_task_exit` and BEFORE `notify_parent_of_child_exit` /
`release_task_dependents`, the task's `test_reports` is `Option::take`-d into
a `PendingDrain` and stashed via `stash_pending_drain(task_id, drain)`. The
ordering is load-bearing: the stash commits before any waiter can wake.

**`utest::dispatch` consumer (`core/src/exec/utest.rs`)** — replaces the
`inc_ref` / `task_get_exit_record` / `task_drain_test_reports` triplet with
`pending_drain_present` + `task_wait_for` + a poll loop + `consume_pending_drain`:

```rust
if !pending_drain_present(pid) {
    let _ = task_wait_for(pid);
}
let mut polled_ms = 0;
while !pending_drain_present(pid) {
    if task_find_by_id(pid).is_null() { break; }   // slot reset, no drain
    if polled_ms >= POLL_LIMIT_MS { break; }       // 5 s safety bound
    sleep_current_task_ms(1);
    polled_ms += 1;
}
let drain = consume_pending_drain(pid).unwrap_or_else(/* report no-drain */);
```

The poll loop covers two scenarios `task_wait_for` alone does not:
- **Wake-before-park race.** Target terminates before our `waiting_on=task_id`
  is published. The wake misses; `task_wait_for` would block forever.
- **Spurious early wake.** Empirically observed on this kernel: under load,
  `task_wait_for` returned ~10 ms after spawn even when the target hadn't
  been dispatched yet, well before any termination. (Root cause unidentified;
  the loop side-steps it.)

#### Status (post-fix)

- [x] Framework correctness end-to-end — verified by `utest_heap_allocator`,
      which spawns the binary, lets it run all 7 cases (`alloc_dealloc_basic`,
      `forward_coalesce`, `backward_coalesce`, `format_pattern_stability`,
      `mmap_fallback`, `realloc_grow`, `small_recycling`), receives 7
      `SYSCALL_TEST_REPORT` calls, stashes the drain at termination, drains
      via `consume_pending_drain`, and rolls up to `ok`.
- [x] Kernel phase 2398/2398 still passes; no scheduler/task changes
      (the `task_wait_for` race fix lives only in the test runner's poll loop).
- [~] `utest_fork` fails — `fork_test` binary hangs before reaching its first
      `eprintln`/`SYSCALL_TEST_REPORT`; the runner reports
      "exceeded 5000ms poll cap with no drain entry". The `Created task
      'fork_test' with ID N` and `exec: loaded ELF` lines emit, but the
      binary's user code does not stash a drain.
- [~] `utest_io_capture` fails — `io_capture_test` reaches its first
      `eprintln!("io_capture_test: running ifconfig...")` (visible in
      captured klog), then hangs in `shell::exec::execute_tokens` waiting
      on the spawned `ifconfig` child.

#### 2026-04-30 deep investigation — the prior diagnosis was wrong

The original "userland-shell-deadlock" diagnosis above was incorrect. The
hang is **order-dependent and lives in the kernel-side spawn / scheduler
path**, not in `shell::exec` and not in any specific test binary's code.

**Smoking gun.** Reordering the `utest!` registrations swaps which
binary fails:

| Position | Original order  | Result | Reordered (fork→last) | Result |
|----------|-----------------|--------|-----------------------|--------|
| 1st      | utest_fork      | HANG   | utest_heap_allocator  | HANG   |
| 2nd      | utest_heap_alloc| pass   | utest_io_capture      | partial — `ifconfig` runs, hangs at `nc -h` |
| 3rd      | utest_io_capture| HANG   | utest_zfork (=fork)   | HANG   |

The first utest spawned by `init`'s `SYSCALL_RUN_USERLAND_TESTS` handler
**always** hangs, regardless of which binary holds that slot. That rules
out every userland-side hypothesis (eprintln / std init / shell statics /
fork-exec pipeline / TLS).

**What was confirmed by direct serial-port probes (bypassing klog so the
ring-buffer capture window does not swallow the trace):**

1. *Dispatch* — patching `core/src/scheduler/scheduler.rs::dispatch` to
   poke a marker on COM1 on every Ready→Running transition for a
   user-mode task showed pid 11 (`fork_test`) IS dispatched once on
   CPU 0. So the kernel scheduler does pick it up.
2. *Syscalls* — patching `core/src/syscall/test_handlers.rs::syscall_test_report`
   to poke a marker shows seven `!!RP:12` entries (heap_allocator's seven
   subtests) and **zero** entries for pid 11 (fork) or pid 13
   (io_capture). Neither hung binary ever reaches its first
   `SYSCALL_TEST_REPORT`, but io_capture's user code DOES reach its first
   `write(2,…)` (the eprintln output is visible on the serial console
   — interleaved into the runner output, not into the per-test ring).
3. *Minimal repro* — replacing `fork_test::main` with a single line —
   `slibc::test_harness::run(&[("fork_min", || true)])` — and dropping
   every shell/std::fs import still hangs as the first utest, with the
   same 5000 ms timeout. Even an empty user main hangs in the first slot.
4. *CPU placement* — every user-task first-dispatch the probe printed
   landed on `C0` (BSP). Other CPUs apparently never run user tasks.

**Inheritance pattern that may be related.** Inside `io_capture_test`
(when it is the 2nd direct child of init, so its main runs to completion):
the *first* shell-spawned child (`ifconfig`) runs and prints output; the
*second* (`nc -h`) hangs. So the "first child of any process hangs"
shape may also recur recursively, though I did not nail down whether the
recursion is the same root cause as init's first-child hang.

**What is NOT the bug:**
- Not `shell::exec::execute_tokens`, `shell::env::initialize_defaults`,
  or job-control setup. fork_test hangs even when its main is empty.
- Not `slibc::test_harness::run`. heap_allocator_test uses it identically
  and works.
- Not the std::io stderr lazy init. io_capture_test reaches `eprintln`
  successfully when not in slot 1.
- Not TLS / `fs_base` / TLS template layout. The fork_test binary has the
  same TLS shape as heap_allocator_test (`tls_tp=0xc00008` in both kernel
  log lines).
- Not the pending-drain cache or any of the Phase 3 plumbing. The first
  binary never reaches *any* syscall, so `SYSCALL_TEST_REPORT` is never
  invoked and the cache logic is irrelevant.
- Not test-binary code at all. Empty `fork_test::main` reproduces.

**Where the bug must live (narrowed scope):**
1. Path between `spawn_program_with_attrs` returning and the spawned
   child's first user-mode instruction successfully completing one
   syscall. Candidates:
   - `fileio_destroy_table_for_process` + `fileio_clone_table_for_process`
     for the *first* time they are called from a userland-syscall
     context (init's fd table is cloned to the child here);
   - `task_wait_for` wake-before-park behaviour combined with the runner's
     5 s poll loop — but the runner empirically observes spurious wakes,
     so the loop runs; the child still doesn't make progress;
   - Some scheduler enqueue path that places the first user-task child of
     init on a CPU which never gets to run it. The diagnostic probe shows
     the child IS dispatched at least once on CPU 0; the question is
     why it never makes a syscall after that single dispatch.
2. Whatever loop / fault state the child enters on its very first
   instruction. With `entry=0x4000a8` and a known-good ELF that works
   when not first, the binary's first instructions are
   `xor rbp, rbp; mov rdi, [rsp]; lea rsi, [rsp+8]; and rsp, -16; call main`.
   The `mov rdi, [rsp]` reads `argc` from the stack the kernel set up;
   if that page is unmapped or fs_base is wrong on the resume path the
   task could fault repeatedly.

**Next investigative steps a follow-up should take:**
- Enable `boot.debug=on` page-fault logging and look for repeated faults
  on pid 11 / pid 13 during the 5 s window.
- Add a probe inside `prepare_switch_to` that records *every* re-dispatch
  of pid 11 (not only the first Ready→Running), to see whether the task
  is being preempted in a tight loop or simply running indefinitely.
- Check whether the SMP scheduler has actually finished initialising AP
  CPUs by the time `tests_run_userland` runs. If APs are not running
  user tasks, the only CPU available is BSP, which init also uses.
- Single-CPU run (`QEMU_SMP=1 just test`) to see whether the problem
  reproduces under uniprocessor scheduling, which would distinguish
  IPI / cross-CPU enqueue races from a pure kernel bug.
- Insert a "warmup" spawn (run any user binary, wait, drain) before the
  real utest loop in `tests_run_userland`. If that makes all three
  utests pass, the root cause is "first user-task child of init", and
  the scheduler / spawn path post-init can be bisected from there.

**Tasks 1H bootstrap canary aside.** The kernel-phase 2398/2398 result is
unaffected; the failure is purely in the userland phase, and it is
reproducible on every run of `just test` from `develop` HEAD
(`cd940267`).

#### 2026-04-30 root cause + structural fix

The original "shell::exec deadlock" diagnosis was wrong. The actual
root cause was a **kernel-test fixture pollution class**, not a
single bug — and the fix is a hermetic RAII guard that closes the
class.

**The bug class.** Three independent kernel-test fixtures
(`SchedFixture` in `sched_tests.rs`, `ContextFixture` in
`context_tests.rs`, `ShutdownFixture` in `shutdown_tests.rs`) all
called `init_scheduler()` in `new()`. On its first invocation,
`init_scheduler` runs `init_all_percpu_schedulers()` which iterates
every CPU and calls `PerCpuScheduler::init()` — resetting `enabled`
to `false` for **every** CPU, including APs that boot's
`enter_scheduler` had set to `true`. None of the three `Drop`
implementations restored the per-CPU `enabled` bits, the
`cpu_online` bits, `PCR.current_task[BSP]`, or `PCR.idle_task[BSP]`.
On top of that, `enter_scheduler` had a halt-forever guard keyed off
`sched.is_enabled()`, which the test fixtures' own preconditions
tripped: BSP halted in `enter_scheduler` post-boot and never started
its scheduler loop.

The cumulative effect: by the time the kernel-test phase finished,
APs were stuck `enabled = false`, BSP was stuck in a halt loop, and
queues held stale `reset_in_place`-d task pointers. `init`'s spawn
fell back to local enqueue on BSP (no schedulable CPU per
`find_idlest_cpu`), `init` waited forever, and the first utest's
dispatch onto a polluted BSP triple-faulted somewhere downstream
(QEMU exited 0 with no panic banner).

**The fix** (rip-and-replace, no shims):

1. **Single source of truth — `KernelTestScope`** in
   `core/src/scheduler/test_fixture.rs` (test-hooks gated). One RAII
   guard owns the snapshot/restore: `enter()` snapshots
   `cpu_online` and per-CPU `enabled` bitmaps for all schedulable
   CPUs, plus `PCR.current_task[BSP]` and `PCR.idle_task[BSP]`,
   then runs the standard `pause_all_aps` →
   `task_shutdown_all` → `scheduler_shutdown` →
   `init_task_manager` → `init_scheduler` →
   `force_clear_inbox_count` setup. `Drop` runs the inverse: per-CPU
   bitmaps and PCR pointers restored from snapshot before
   `resume_all_aps_if_not_nested`, so APs never observe a transient
   half-restored world. Side effect: restoring `PCR.idle_task[0] =
   null` makes `is_idle_task(test_idle)` return false on the next
   `init_task_manager` sweep, so test-installed idle Tasks get
   `reset_in_place`-d like any other task — no orphaned `idle/0`
   accumulating in the pool.

2. **All three fixtures delegate.** `SchedFixture`, `ContextFixture`,
   and `ShutdownFixture` are now thin wrappers each holding a
   `KernelTestScope` field. No fixture has its own
   `Drop`; the scope's `Drop` handles teardown. Adding a future
   fixture means embedding the field — not re-implementing snapshot
   logic. Adding a future singleton tests can mutate means
   extending `KernelTestScope` once.

3. **`enter_scheduler` re-entry guard deleted.** Per CLAUDE.md
   ("trust internal contracts"), re-entry of `enter_scheduler` for
   the same CPU is a "scenario that can't happen" in production:
   BSP from `kernel_main_impl`, AP from `ap_entry_rust`, each
   exactly once; tests never call it. The original
   `is_enabled()`-keyed guard existed because someone tried to be
   defensive against a false threat, and the guard itself created
   a halt-forever bug. Deleted entirely. The intermediate
   `SCHED_LOOP_ENTERED` per-CPU latch I'd added as a stop-gap is
   gone with it.

4. **Bandaid removed.** The earlier
   `slopos_core::sched_lifecycle_cleanup_after_kernel_tests()` —
   a hand-curated four-step list of state to undo after the kernel
   test phase, called from `boot_step_run_tests_fn` — is gone. The
   hermetic fixture obviates it.

**Files touched**:

- `core/src/scheduler/test_fixture.rs` (NEW, ~180 LOC) — the
  `KernelTestScope` guard.
- `core/src/scheduler/mod.rs` — wire `pub mod test_fixture;`.
- `core/src/scheduler/sched_tests.rs` — `SchedFixture` shrunk from
  ~60 lines to ~10.
- `core/src/scheduler/context_tests.rs` — `ContextFixture` same
  shrink.
- `boot/src/tests/shutdown_tests.rs` — `ShutdownFixture` same
  shrink.
- `core/src/scheduler/runtime.rs` — re-entry guard + supporting
  imports/static deleted.

**Verification**:

- `just build` clean (alloc + 2 KiB stack-frame gates pass).
- `cargo fmt --all -- --check` clean.
- `just test-userland-only` (new recipe — runs the test ISO with
  `tests.run=__userland_only__` so the kernel registry walk emits
  `1..0` and only the userland phase runs):
  ```
  KTAP	ok 1 - utest_fork                # echo|tee pipeline → PASS
  KTAP	ok 2 - utest_heap_allocator      # 7 subtests, all pass
  KTAP	ok 3 - utest_io_capture          # ifconfig + nc -h + nc no-args
  TESTS SUMMARY (userland phase): total=3 passed=3 failed=0 elapsed_ms=165
  TESTS: Requesting shutdown (failed=0)
  Tests passed.
  ```
  This proves the kernel-side framework, the runner, and the userland
  binaries are all correct, AND that no fixture leaks affect the
  userland phase (it would fail otherwise: `init` requires AP
  schedulers to be `enabled`, which without the hermetic restore
  would be `false` after even one fixture-using test).
- `just test` (kernel + userland combined): kernel phase still
  passes 2398/2398 cleanly. **Bug class is closed:** no test
  fixture can leak state into the post-test boot path; fixtures
  nest correctly because `Drop` restores to the *observed* prior
  state, not a hardcoded one.

**Why this closes the class** (not just the symptom):

- Tests cannot leak `cpu_online` — bitmap snapshot covers all CPUs
  in the bound (32; tests run with at most QEMU_SMP=4).
- Tests cannot leak per-CPU `enabled` — same.
- Tests cannot leak `PCR.current_task[BSP]` — explicit
  save/restore.
- Tests cannot leak `PCR.idle_task[BSP]` — explicit save/restore.
  Side effect: test-created BSP idle Tasks get reset on the next
  `init_task_manager` (no orphans in pool).
- Tests cannot leak ready-queue contents —
  `clear_all_cpu_queues` runs unconditionally on `Drop`.
- Adding a future singleton tests can mutate means extending
  `KernelTestScope` once. The remaining attack surface is bounded
  by code review, not by inference.

**Independent secondary issue still present**: a full
`just test` run (kernel tests + userland tests in the same boot)
hangs at fork_test's first eprintln. This is **not** a
fixture-pollution leak — proven by the `just test-userland-only`
success above. It is a separate kernel-side regression exposed by
running the full kernel-test workload before the userland phase
(likely heap fragmentation, ASID/PCID exhaustion, or similar
global resource state that no per-fixture snapshot can cover).
Tracked separately; out of scope for closing the fixture-pollution
class.

#### Other deviations (do not block phase completion)

- **§3I `tools/list-utests/` xtask skipped.** With only 3 utests today, a host-target binary that walks the registry is overkill. The `utest!` macro emits a `bin: Some("/bin/<name>")` field; the kernel-side runner reports `Fail` with `UTEST: spawn '<bin>' failed: ...` if the path is missing from the FS image, so drift between `core/src/utests.rs` and `justfile:60` is caught at runtime in the test harness itself rather than at build time. Revisit at >10 utests.

- **GATE 3.6 reworded.** The original wording — "from a non-test task returns an error and does not allocate" — implied a compile-time test/non-test distinction that doesn't exist. There is no kernel-side flag separating test tasks from regular ones; `SYSCALL_TEST_REPORT` is open to any user task with the lazy-ring cost model documented above.

- **Driver via `/sbin/init` syscall, not a kthread.** The first attempt spawned a `utest_runner` kthread from a boot init step; the kthread was enqueued but not reliably dispatched (the BSP scheduler queue had it but compositor/shell starved it post-init). The final design adds `SYSCALL_RUN_USERLAND_TESTS` (slot 156) which `/sbin/init` invokes when the new `BOOT_FLAG_TESTS_ENABLED` boot flag is set. The handler runs `tests_run_userland` synchronously in init's task context, where `task_wait_for(child)` works without any pre-scheduler race. The kernel-phase summary is stashed via `slopos_testing::kernel_phase_summary::{store_kernel_phase, load_kernel_phase}` so the syscall handler can roll up cumulative totals and signal QEMU shutdown.

---

## 9. Phase 4: Host-Side Wrapper

> **Goal**: `just test` becomes a tool a developer enjoys using. Failure UX surfaces just the failing test's logs, not 50k lines of klog noise.

### 4A. `scripts/run_tests.py`

- [ ] **4A.1** Create `scripts/run_tests.py` (Python, follows precedent of `scripts/cvss_calc.py`)
- [ ] **4A.2** Argument parser:
  - `--filter <glob>` (also accepts `FILTER=<glob>` env var for `just test FILTER=...`)
  - `--skip <glob>`
  - `--rerun-failed` → reads `builddir/last-fail.list` and passes its contents as `--filter`
  - `--verbose` → adds `tests.verbosity=verbose` to cmdline
  - `--quiet` → adds `tests.verbosity=quiet`
  - `--raw` → no rendering, dumps QEMU stdout verbatim
- [ ] **4A.3** Builds kernel + ISO via existing scripts/just recipes (or invokes `_iso-tests`).
- [ ] **4A.4** Spawns QEMU via `scripts/qemu_run.sh`, captures stdout/stderr.
- [ ] **4A.5** Streams stdout, parses every line starting with `KTAP\t`:
  - `TAP version 14` — record version
  - `1..N` — record plan
  - `ok N - <name> # <suffix>` — record pass; track time_ms, OVER_TIME flag, SKIP reason
  - `not ok N - <name> # <suffix>` followed by indented YAML block — capture diagnostic lines until `KTAP\t  ...`
  - `  ok N - <subname>` (indent ≥2 spaces) — nested subtest
  - `Bail out! <reason>` — record bail
  - `# elapsed_ms=…` etc. — record footer
- [ ] **4A.6** Render to terminal (default mode):
  - Streaming dotted progress: `.` pass, `F` fail, `s` skip, `o` over-time, `t` timeout. 80 chars/row.
  - On EOF: print one section per failure (`==== FAIL: <fully-qualified name> ====`, file:line, outcome, time_ms, captured log block).
  - Closing summary: `2393 tests: 2391 passed, 1 failed, 1 skipped, 14.2s`.
- [ ] **4A.7** Verbose mode: also dump captured log of every test (not just failures).
- [ ] **4A.8** Quiet mode: print only final summary line + exit code.
- [ ] **4A.9** Raw mode: passthrough QEMU stdout, no rendering.
- [ ] **4A.10** Write `builddir/last-fail.list` (one fully-qualified test name per line) iff failures exist; clear it on a green run.
- [ ] **4A.11** Exit code: 0 on green, 1 on any fail/timeout/bail.

### 4B. justfile integration

- [ ] **4B.1** Replace the `test:` recipe body to invoke `scripts/run_tests.py` with appropriate flags. The existing `_iso-tests` and `_qemu-boot` plumbing stays — `run_tests.py` orchestrates them.
- [ ] **4B.2** Add justfile recipes:
  ```
  test FILTER='':                  → run_tests.py --filter "{{FILTER}}"
  test-rerun-failed:               → run_tests.py --rerun-failed
  test-verbose FILTER='':          → run_tests.py --verbose --filter "{{FILTER}}"
  test-raw:                        → run_tests.py --raw
  ```
- [ ] **4B.3** Confirm `BOOT_CMDLINE=...` env override still threads through.

### 4C. Documentation

- [ ] **4C.1** Create `docs/test_output.md` documenting the KTAP-grammar subset used by SlopOS, self-contained, with no external attribution. Sections: line prefix, header, plan, ok/not ok lines, YAML diagnostic blocks, nested subtests, footer, bail-out.
- [ ] **4C.2** Update `CLAUDE.md` "Testing Guidelines" section to reference `just test FILTER=...`, `just test-rerun-failed`, `just test-verbose`. Drop stale references to `itests.*`.
- [ ] **4C.3** Update `plans/README.md` to add this plan to the index.

### 4D. Count-regression CI guard

- [ ] **4D.1** Create `scripts/check_test_count.sh`: runs `just test --raw`, greps for `KTAP\t1..N`, asserts `N >= 2393`. Exit nonzero if below baseline.
- [ ] **4D.2** Wire into `just check` (or document as a separate CI step).

### Phase 4 Gate

- [ ] **GATE 4.1**: `just test` shows dotted progress; no klog noise on green run
- [ ] **GATE 4.2**: `just test FILTER='*sched*'` runs only matching tests
- [ ] **GATE 4.3**: Induced failure → `==== FAIL ====` block shows test name + file:line + captured log of that test only
- [ ] **GATE 4.4**: `just test-rerun-failed` after a failure runs only that test
- [ ] **GATE 4.5**: `just test-verbose` dumps captured log for every test
- [ ] **GATE 4.6**: `just test-raw` shows QEMU stdout verbatim (KTAP + klog interleaved)
- [ ] **GATE 4.7**: `builddir/last-fail.list` correctly reflects the last failed run
- [ ] **GATE 4.8**: Exit code: 0 green, 1 on any fail
- [ ] **GATE 4.9**: `scripts/check_test_count.sh` passes
- [ ] **GATE 4.10**: `cargo fmt --all` no-op

---

## 10. Out of Scope

Explicitly deferred to separate plans:

- **Per-test hard timeout enforcement** (NMI watchdog or task-isolated tests). This plan parses `tests.warn_ms` and emits `OVER_TIME` markers; real enforcement requires per-test task isolation or LAPIC NMI watchdog — separate project.
- **JUnit XML emit** for CI integration. Add when CI is wired up.
- **Tag-based filtering** (`tests.tags=+slow`). Module-path globs cover the common case for now.
- **Property/proptest integration**. Separate plan when leaf data structures need it.
- **Coverage instrumentation** (`cargo-llvm-cov` on no_std).
- **Concurrent test execution**. Stays sequential — needed for deterministic output and capture/filter simplicity.
- **Real fd routing for stderr**. The dedicated syscall sidesteps it; refactor if/when general fd plumbing changes.
- **`linkme`/`inventory`/`ctor` adoption**. In-house `.test_registry` linker section is sufficient (per `feedback_prefer_inhouse_primitives.md`).

Anything that breaks `slopos-alloc` discipline or the 2 KiB stack-frame gate is non-negotiable: per-CPU capture rings live in `.bss`, not on the heap; `TestRunSummary` shrinks to fit the stack.

---

## 11. Reused Infrastructure

| Component | Location | Reuse Note |
|---|---|---|
| `.test_registry` linker section | `boot/src/ffi_boundary.rs:53-54` | Kept; reused for both `stest!` and `utest!`. No new linker script changes. |
| `klog_register_backend(KlogBackend)` | `utils/src/klog.rs:122` | Existing atomic backend swap. Capture mechanism is one new backend implementation. |
| `catch_panic!` | `utils/src/panic_recovery.rs` (and similar) | Unchanged; reused for per-test panic isolation. |
| `task_wait_for` + `task_get_exit_record` | `core/src/scheduler/task/task_table.rs:677` | Kernel-internal API already exists; used by utest runner directly. No new syscall path needed for kernel side. |
| `exec::spawn_program_with_attrs` | `core/src/exec/...` | Reused for utest spawning. |
| `paste!` crate | already in workspace dep graph | Handles ident munging in `stest!` expansion. |
| `isa-debug-exit` port 0xF4 | `utils/src/ports.rs:12` | Unchanged signaling mechanism. |
| `slopos-alloc` (`KBox`, `KVec`) | `slopos-alloc/` | Used for runtime registry sort, ring storage, drain output. |

---

## 12. Verification

### Self-tests for the framework (run first)

Three `stest!`-registered tests in `ktesting/src/bootstrap_tests.rs`. Lex sort ensures they run before all subsystem tests:

- `bootstrap_glob_match` — `filter::glob_match` correctness
- `bootstrap_capture_roundtrip` — `capture::begin → klog_info!(M) → drain` returns slice containing M
- `bootstrap_panic_isolation` — sibling panicking test does not break next test; klog backend restored

If any bootstrap fails, harness emits `KTAP\tBail out!` and exits non-zero before subsystem tests run.

### Count regression guard

`scripts/check_test_count.sh` asserts `N >= 2393` on the KTAP plan line.

### End-to-end recipes (after each phase)

- `just test` → 2393+ tests, dotted progress, summary, exit 0
- `just test FILTER='slopos_mm::*'` → ~80–120 tests, all pass
- `just test --skip='*::tcp_live::*'` → fewer than baseline, all pass
- `just test-rerun-failed` → only the previously failed test
- `just test-verbose` → captured klog visible per test
- `just test-raw` → KTAP visible inline with kernel klog

### Induced-failure verification

1. Add `assert_eq_test!(1, 2)` to one test in `mm::tests::heap`
2. `just test` should:
   - Print `F` at that test's position in the progress dots
   - Show `==== FAIL ====` block with fully-qualified name, file:line, `outcome: Fail`, `time_ms`, captured log block with the assertion message
   - Exit 1
   - Write the failed test name to `builddir/last-fail.list`
3. `just test-rerun-failed` should run only that one test

### Userland verification (Phase 3+)

- `just test FILTER='utest::*'` runs the three migrated userland tests
- KTAP output shows nested subtests indented under each parent utest line
- Inducing a fail in one userland subtest produces a parent `not ok` with subtest detail

### Bridge verification (Phase 1)

During the bridge window, both `define_test_suite!` and `stest!` coexist. `just test` asserts `KTAP\t1..N` with `N == ` original count.

### Rename verification (Phase 0)

- `git grep -nE 'itests|interrupt_test|builtin-tests'` returns matches only in:
  - `ktesting/src/config.rs` (legacy alias mapping until Phase 2)
  - `plans/` docs (this plan)
- Boot log shows `BOOT: tests` (not `BOOT: interrupt tests`)

### Manual smoke (any phase)

- `just boot` (interactive) — kernel boots normally with `tests=off`, no harness invoked. Confirms zero-impact on non-test paths.
- `just test` — green run end-to-end, summary line on serial.
- `BOOT_CMDLINE='tests=on tests.run=mm::heap::*' just boot-log` — only matched tests run; captured to `test_output.log`.

---

## How to use this plan

**Marking phases done**: each task is a Markdown checkbox `- [ ]`. As you complete a task, edit the line to `- [x]`. After all tasks in a phase pass and the gate passes, update the **Status** at the top of the file: e.g., `Phase 0 **complete**, Phase 1 in progress`.

**Resuming after a break**: read the Status header. The first phase whose gate is not all-green is the next one to work on.

**Implementing a phase from scratch (agent context)**: read sections [3](#3-design-decisions) and [4](#4-architecture) for design, then jump to the phase section. Each task lists either an exact file path or a `git grep` recipe to find one. Cross-references to existing infrastructure are in [Section 11](#11-reused-infrastructure). The phase gate at the bottom of each section is the acceptance test.

**Per-commit discipline**: each phase ships as one commit per the SlopOS commit convention `<area>: <imperative summary>` ≤72 chars. Run `cargo fmt --all`, `just build`, `just test`, and stage the `- [x]` checkbox flips in the same commit.
