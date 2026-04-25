# SlopOS Test Framework Redesign

> **Status**: Not started
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

- [ ] **0A.1** Create `ktesting/src/qemu_signal.rs` with content from `drivers/src/interrupt_test.rs`, renaming the function:
  - `interrupt_test_request_shutdown(failed_tests: i32)` → `qemu_signal_exit(failed_tests: i32)`
- [ ] **0A.2** Add `pub mod qemu_signal;` to `ktesting/src/lib.rs`
- [ ] **0A.3** Move the `qemu-exit` feature from `drivers/Cargo.toml` to `ktesting/Cargo.toml`
- [ ] **0A.4** Delete `drivers/src/interrupt_test.rs`
- [ ] **0A.5** Remove `pub mod interrupt_test;` from `drivers/src/lib.rs`
- [ ] **0A.6** Update justfile feature string: `slopos-drivers/qemu-exit` → `slopos-testing/qemu-exit`

### 0B. Rename boot wiring

- [ ] **0B.1** In `boot/src/boot_drivers.rs:309-363`, rename `boot_step_interrupt_tests_fn` → `boot_step_run_tests_fn`
- [ ] **0B.2** In same file, rename `BOOT_STEP_INTERRUPT_TESTS` → `BOOT_STEP_RUN_TESTS`
- [ ] **0B.3** Change boot step name string `b"interrupt tests\0"` → `b"tests\0"` (visible in boot logs)
- [ ] **0B.4** Replace klog prefixes `"INTERRUPT_TEST: ..."` → `"TESTS: ..."` (multiple sites in `boot_drivers.rs`)
- [ ] **0B.5** Update import path for the QEMU-exit fn (now `slopos_testing::qemu_signal::qemu_signal_exit`)
- [ ] **0B.6** Update `boot/src/panic.rs` `tests_mark_panic` import path if it changed

### 0C. Fold `tests/` crate into `ktesting/` and `karch/`

- [ ] **0C.1** Move `tests/src/exception_tests.rs` → `karch/src/tests/exception_tests.rs`
- [ ] **0C.2** Move `tests/src/fpu_tests.rs` → `karch/src/tests/fpu_tests.rs`
- [ ] **0C.3** Move `tests/src/xsave_tests.rs` → `karch/src/tests/xsave_tests.rs`
- [ ] **0C.4** Create `karch/src/tests/mod.rs` re-exporting the three modules under `#[cfg(feature = "test-hooks")]`
- [ ] **0C.5** Add `pub mod tests;` (gated) to `karch/src/lib.rs`
- [ ] **0C.6** Add `test-hooks = []` feature to `karch/Cargo.toml`; add `slopos-testing` dep gated on the feature
- [ ] **0C.7** Move content of `tests/src/lib.rs` (`tests_run_all`, `tests_mark_panic`, `tests_reset_panic_state`, panic glue) into `ktesting/src/harness.rs`
- [ ] **0C.8** Delete `tests/Cargo.toml` and `tests/src/`
- [ ] **0C.9** Drop `"tests"` from workspace members in root `Cargo.toml`
- [ ] **0C.10** Update every `slopos-tests = …` workspace dep reference to `slopos-testing = …` (kernel/Cargo.toml is the main consumer)
- [ ] **0C.11** Update boot's import of `tests_run_all` etc. to `slopos_testing::harness::*`

### 0D. Rename per-crate Cargo features

- [ ] **0D.1** Sweep every `Cargo.toml` in workspace: feature `itests` → `test-hooks`. Affected crates (verify with grep): `boot`, `core`, `mm`, `drivers`, `net`, `fs`, `kernel`, `karch`, `ktesting`. Update every `[features]` entry and every `dep/itests` cross-reference.
- [ ] **0D.2** In `kernel/Cargo.toml`: rename feature `builtin-tests` → `tests`. Update transitive references (every `kernel/builtin-tests` in scripts/justfile/CI).
- [ ] **0D.3** Verify with `git grep -nE 'itests|builtin-tests'` — should match only the legacy-cmdline-alias code in `ktesting/src/config.rs` and (now-stale) doc strings.
- [ ] **0D.4** Update `scripts/build_kernel.sh:85-90` test-feature gate references.
- [ ] **0D.5** Update `justfile:117-126` `_iso-tests` recipe to use `kernel/tests` and `slopos-testing/qemu-exit`.

### 0E. Cmdline rename (with one-cycle backward-compat alias)

- [ ] **0E.1** In `ktesting/src/config.rs`, accept both `tests.*` and `itests.*` keys. On `itests.*` match, also write to the corresponding new field and emit one `klog_warn!("TESTS: legacy 'itests.*' cmdline key in use; rename to 'tests.*'");` per run (use a `StateFlag` to print at most once).
- [ ] **0E.2** In `justfile:50-51, 165-166`, change default boot/test cmdline strings:
  - `itests=off` → `tests=off`
  - `itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on` → `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`
- [ ] **0E.3** Confirm `BOOT_CMDLINE` env-override path still works (`scripts/qemu_run.sh`).

### Phase 0 Gate

- [ ] **GATE 0.1**: `just build` passes
- [ ] **GATE 0.2**: `just test` passes with 2393/2393 tests
- [ ] **GATE 0.3**: `git grep -nE 'itests|interrupt_test|builtin-tests'` returns matches only in `ktesting/src/config.rs` (legacy alias) and possibly `plans/` docs
- [ ] **GATE 0.4**: Boot log shows `BOOT: tests` (not `BOOT: interrupt tests`)
- [ ] **GATE 0.5**: `cargo fmt --all` is a no-op
- [ ] **GATE 0.6**: One commit; `<area>: <imperative summary>` ≤72 chars, e.g., `tests: rename itests/interrupt_test → tests/qemu_signal`

---

## 6. Phase 1: New Framework With Bridge

> **Goal**: Land per-test registration, KTAP output, log capture, glob filter, slow-test reporting. `define_test_suite!` keeps working as a bridge that fans out to per-test descriptors. KTAP emitted alongside legacy `SUITE_N` lines for one phase. Verify count ≥2393.

### 1A. New `ktesting` modules

- [ ] **1A.1** Create `ktesting/src/result.rs`:
  - `pub enum TestOutcome { Pass, Fail, Panic, Skipped, OverTime }`
  - Convenience `is_pass`, `is_failure`, conversion to KTAP status word
  - Re-export old `TestResult` aliased to `TestOutcome` for one phase
- [ ] **1A.2** Create `ktesting/src/registry.rs`:
  - `#[repr(C)] pub struct TestDesc` per Architecture section, with `name_cstr`, `module_cstr`, `file_cstr`, `line`, `run`, `kind`, `flags`, `bin_cstr`, `argv_ptr`
  - `pub enum TestKind { Kernel, Userland }`
  - `pub fn registry_iter() -> impl Iterator<Item = &'static TestDesc>` — walks `__start_test_registry`/`__stop_test_registry`
  - `pub fn registry_sorted() -> KVec<&'static TestDesc>` — sorts by `(module_path, name)` lex order; uses `slopos_alloc::KVec`
- [ ] **1A.3** Create `ktesting/src/capture.rs`:
  - Per-CPU static rings: `static mut CAPTURE_RING: [SyncUnsafeCell<[u8; 65536]>; MAX_CPUS]` in `.bss`
  - `pub struct CaptureGuard { prev_backend: KlogBackend }` with `Drop` that restores backend
  - `pub fn begin() -> CaptureGuard`, restores on drop
  - `pub fn drain_cpu0() -> &'static [u8]`, returns slice of CPU0 ring
  - `pub fn drain_all() -> impl Iterator<Item=(usize, &'static [u8])>` for verbose mode
  - Internal `BufferingBackend::write` appends to `current_cpu`'s ring; tracks truncation flag
- [ ] **1A.4** Create `ktesting/src/filter.rs`:
  - `pub fn glob_match(pat: &[u8], name: &[u8]) -> bool` — supports `*` (any sequence, no anchoring) and `?` (single char). Recursive backtracking. ~50 lines, no_std, no alloc.
  - `pub fn matches_any(pats: &[&[u8]], name: &[u8]) -> bool`
  - `pub fn passes_filter(name: &[u8], cfg: &TestConfig) -> bool`
- [ ] **1A.5** Create `ktesting/src/ktap.rs`:
  - `pub fn emit_header(plan_count: u32)` — `KTAP\tTAP version 14\nKTAP\t1..N`
  - `pub fn emit_ok(idx: u32, name: &CStr, time_ms: u64, suffix: Option<&str>)`
  - `pub fn emit_not_ok(idx: u32, name: &CStr, time_ms: u64, file: &CStr, line: u32, outcome: TestOutcome, log: &[u8])`
  - `pub fn emit_skip(idx: u32, name: &CStr, reason: &str)`
  - `pub fn emit_subtest_indented(parent_indent: u32, idx: u32, status: u32, name: &str)`
  - `pub fn emit_footer(totals: &Totals)`
  - All output via `klog_info!` with literal `KTAP\t` prefix; YAML block bodies indented `KTAP\t  `

### 1B. `stest!` macro and TestDesc emission

- [ ] **1B.1** In `ktesting/src/lib.rs`, add `pub macro_rules! stest`:
  ```
  stest!(name = $ident:ident);
  stest!(name = $ident:ident, kind = Kernel);
  ```
  Expansion produces `#[used] #[unsafe(link_section = ".test_registry")] pub static [<TEST_DESC_ $ident:upper>]: TestDesc { ... }` using `paste::paste!` for ident munging. Includes `module_path!()`, `file!()`, `line!()` as cstrs (via a small `c_concat!` helper that null-terminates a string-concat).
- [ ] **1B.2** Provide a thunk wrapper around the user's `fn $ident() -> TestOutcome` that runs inside `catch_panic!` and returns the outcome.
- [ ] **1B.3** Wire `slopos-testing` re-exports of `paste::paste!` + helpers needed by the macro.

### 1C. Bridge `define_test_suite!`

- [ ] **1C.1** Rewrite `define_test_suite!(suite_name, [$($test_fn:path),* $(,)?])` to expand to one `stest!(name = $test_fn)` invocation per test fn — with the suite name baked into the test's `module` field via the existing `module_path!()`. NO behavior change at any call site; existing 75+ files keep compiling.
- [ ] **1C.2** Rewrite the `define_test_suite!(suite_name, runner_fn, single)` form similarly — emit a single `stest!` for the runner, but tag it as a "wrapper" (no nested fan-out is possible at this form; that's fine, the runner already iterates internally).
- [ ] **1C.3** Mark `define_test_suite!` `#[deprecated(note = "use stest! per test function; bridge will be removed in Phase 2")]` if feasible.

### 1D. New harness loop

- [ ] **1D.1** Rewrite `ktesting/src/harness.rs::tests_run_all` to iterate the per-test registry (sorted), apply filter, capture+run+emit per test:
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
- [ ] **1D.2** Shrink `TestRunSummary` to counter-only struct (32 bytes max). Remove `HARNESS_MAX_SUITES` const and the `suites: [...; 64]` array. This removes the silent-truncation bug and lets `boot_drivers.rs` drop the `KBox::<TestRunSummary>::zeroed` workaround.
- [ ] **1D.3** During Phase 1, ALSO emit legacy `SUITE_N total=… pass=… fail=… elapsed=…ms` and `TESTS SUMMARY: …` lines to keep existing scrapers working. Group by module_path's first crate-component for the synthesised SUITE rollup. (These legacy lines get deleted in Phase 2.)
- [ ] **1D.4** Keep `LUF SUMMARY: …` line emission unchanged.

### 1E. Cmdline + filter wiring

- [ ] **1E.1** In `ktesting/src/config.rs`, add fields to `TestConfig`:
  - `run_globs: KVec<&'static [u8]>` (heap-owned via slopos-alloc; populated from cmdline)
  - `skip_globs: KVec<&'static [u8]>`
  - `warn_ms: u32` (replacing `timeout_ms`; backward-compat alias `tests.timeout=` still parsed and mapped to `warn_ms`)
- [ ] **1E.2** Parse `tests.run=...,...,...`, `tests.skip=...,...,...`, `tests.warn_ms=N`. Comma-separated.
- [ ] **1E.3** Map legacy `itests.suite=foo` → push `*::foo::*` glob into `run_globs`. Emit `klog_warn!` once.
- [ ] **1E.4** Honor `tests.verbosity={quiet,summary,verbose}` — gate `emit_ok` log-block and per-CPU drain accordingly.

### 1F. Per-test capture + panic recovery

- [ ] **1F.1** In `utils/src/klog.rs`, expose `klog_swap_backend(KlogBackend) -> KlogBackend` (atomic-RMW returns prior). If only `klog_register_backend` exists, add a sibling that returns the prior pointer.
- [ ] **1F.2** Add `klog_restore_backend(KlogBackend)` to be called by `call_panic_cleanup` (or wherever the per-test catch_panic longjmp returns).
- [ ] **1F.3** In `utils/src/panic_recovery.rs` (or wherever `call_panic_cleanup` lives), add a hook that the harness registers to restore klog backend after a longjmp out of a test.
- [ ] **1F.4** Verify induced panic in a test does not leak the `BufferingBackend` to subsequent tests (`bootstrap_panic_isolation` test in §12).

### 1G. Slow-test reporting

- [ ] **1G.1** In `runner.rs`, after `cycles_to_ms`, if `time_ms > cfg.warn_ms && cfg.warn_ms > 0`, emit `OVER_TIME` suffix in the KTAP `ok` line. Outcome stays Pass.
- [ ] **1G.2** Aggregate `over_time` count in `Totals`; emit in footer.

### 1H. Bootstrap self-tests

- [ ] **1H.1** Create `ktesting/src/bootstrap_tests.rs` (gated by `kernel/tests`). Add three `stest!` invocations:
  - `bootstrap_glob_match`: `assert_test!(filter::glob_match(b"a::*", b"a::b"))`, `assert_test!(!filter::glob_match(b"a::*", b"a"))`
  - `bootstrap_capture_roundtrip`: `let _g = capture::begin(); klog_info!("MARKER_X9"); drop(_g); let log = capture::drain_cpu0(); assert_test!(log.contains_subslice(b"MARKER_X9"))`
  - `bootstrap_panic_isolation`: a sibling `stest!` that intentionally `panic!()`s; the bootstrap test asserts the next test still ran by checking a `static AtomicU32` counter
- [ ] **1H.2** Bootstrap tests run first (lex sort guarantees this if names start with `bootstrap_`). On bootstrap fail, harness emits `KTAP\tBail out! ktesting bootstrap failed: <reason>` and exits non-zero.

### Phase 1 Gate

- [ ] **GATE 1.1**: `just build` passes
- [ ] **GATE 1.2**: `just test` runs ≥2393 tests; KTAP `1..N` plan line shows N ≥ 2393
- [ ] **GATE 1.3**: Both legacy `SUITE_N total=…` lines AND new `KTAP\t...` lines visible in serial output
- [ ] **GATE 1.4**: `tests.run='*sched*'` filter reduces test count significantly; all matched still pass
- [ ] **GATE 1.5**: `tests.skip='*tcp_live*'` excludes those tests
- [ ] **GATE 1.6**: Induce a fail in one test → KTAP `not ok` line includes captured log block; surrounding tests unaffected
- [ ] **GATE 1.7**: Induce a panic in one test → next test still runs; klog backend not stuck on `BufferingBackend`
- [ ] **GATE 1.8**: Bootstrap self-tests pass (visible as first three KTAP lines)
- [ ] **GATE 1.9**: `cargo fmt --all` no-op
- [ ] **GATE 1.10**: `scripts/check_alloc_dep.sh` and stack-size gate still pass

---

## 7. Phase 2: Big-Batch Site Migration

> **Goal**: Convert every `define_test_suite!` invocation to per-test `stest!` calls. Delete bridge macro. Drop legacy `SUITE_N` log lines and `itests.*` cmdline aliases.

### 2A. Mechanical site conversion

- [ ] **2A.1** Identify all sites: `git grep -lF 'define_test_suite!' | sort -u`. Expected: ~75-86 files.
- [ ] **2A.2** Write a small AWK or Python script `tools/migrate_test_sites.py` that, for each file:
  - Parses `define_test_suite!(suite_name, [test_fn1, test_fn2, ...])` (handling multi-line lists)
  - Replaces the macro call with one `slopos_testing::stest!(name = test_fnN);` per fn
  - Preserves trailing comments and surrounding code
  - Skips the `single`-form (handle those manually; few in count — `fs/src/tests.rs:633`, `tests/src/fpu_tests.rs:76`)
- [ ] **2A.3** Run the script across the workspace. Spot-check 5 random files for correctness.
- [ ] **2A.4** Manually convert the `single`-form sites (the runner stays as the test fn body itself).
- [ ] **2A.5** Run `cargo fmt --all`.
- [ ] **2A.6** Build + test; expect higher visible test count (each suite's tests now individually visible).

### 2B. Delete bridge

- [ ] **2B.1** Delete `define_test_suite!` macro from `ktesting/src/lib.rs`
- [ ] **2B.2** `git grep -F 'define_test_suite!'` should return zero matches
- [ ] **2B.3** Delete legacy `SUITE_N total=… pass=… fail=…` log emissions from `ktesting/src/harness.rs`
- [ ] **2B.4** Delete the per-test legacy `TEST FAIL: <name>: <result>` log line (KTAP `not ok` covers it)
- [ ] **2B.5** Keep the trailing `TESTS SUMMARY: …` line (raw-serial reader convenience)
- [ ] **2B.6** Keep `LUF SUMMARY: …` line

### 2C. Drop cmdline back-compat

- [ ] **2C.1** Remove `itests.*` legacy alias parsing from `ktesting/src/config.rs`
- [ ] **2C.2** Remove the one-shot `klog_warn!` for legacy keys
- [ ] **2C.3** `git grep -nE 'itests'` should return zero matches outside `plans/` docs

### 2D. Bootstrap self-test for migration completeness

- [ ] **2D.1** Add `bootstrap_no_define_test_suite_macro`: a compile-time test (or `static_assertions`-style check) that `define_test_suite` is not in scope. Optional — `git grep` in CI is sufficient.

### Phase 2 Gate

- [ ] **GATE 2.1**: `just build` passes
- [ ] **GATE 2.2**: `just test` runs the same or higher test count as Phase 1; all pass
- [ ] **GATE 2.3**: KTAP-only output (no `SUITE_N` or `TEST FAIL:` legacy lines)
- [ ] **GATE 2.4**: `git grep -F 'define_test_suite!'` zero matches
- [ ] **GATE 2.5**: `git grep -nE 'itests'` zero matches outside `plans/`
- [ ] **GATE 2.6**: Induced-failure UX still surfaces the failing test name + captured log clearly
- [ ] **GATE 2.7**: `cargo fmt --all` no-op

---

## 8. Phase 3: Userland Integration

> **Goal**: Userland tests as first-class harness entries. Kernel-side runner spawns binary, drains structured reports via `SYSCALL_TEST_REPORT`, emits nested KTAP subtests. Three existing test bins migrated to use `slibc::test_harness::run`.

### 3A. ABI: new syscall

- [ ] **3A.1** In `abi/src/syscall.rs` (or wherever `SYSCALL_*` constants are defined), add:
  ```
  pub const SYSCALL_TEST_REPORT: u32 = <next-available>;
  ```
  Verify the number is unused via `git grep -nE 'SYSCALL_[A-Z_]+\s*[:=]'`.
- [ ] **3A.2** Document the calling convention in a doc comment: `(status: u32, name_ptr: *const u8, name_len: usize, msg_ptr: *const u8, msg_len: usize) -> i64`. Returns 0 on success, negative errno on failure (caller from non-test task, ring full, invalid pointers, etc.).
- [ ] **3A.3** Add `pub enum TestReportStatus { Pass = 0, Fail = 1, Skip = 2 }` and a small POD `TestReport` struct in abi if convenient (kernel-only consumer; abi only needs the syscall # + status enum).

### 3B. Kernel: per-task report ring

- [ ] **3B.1** In `core/src/scheduler/task_struct.rs` (or the actual `Task` struct location — verify via `git grep -nE 'struct Task\b'`), add field:
  ```
  pub test_reports: Option<KBox<TestReportRing>>,
  ```
  Where `TestReportRing` is a fixed-size circular buffer of ~32 entries, each `{status: u8, name: [u8; 64], msg: [u8; 128]}`. Total ring ~6 KiB; allocated lazily on first `SYSCALL_TEST_REPORT` from a given task; `None` for non-test tasks (zero cost).
- [ ] **3B.2** Define `TestReportRing` in `core/src/scheduler/test_reports.rs` (new file) using `slopos_alloc::KBox` for backing storage. Bounded ring; on overflow drop newest with overflow-flag bit.
- [ ] **3B.3** Add `pub fn task_drain_test_reports(pid: Pid) -> KVec<TestReport>` to the same file. Returns the drained reports. Locks the task table appropriately.
- [ ] **3B.4** In task lifecycle (`core/src/scheduler/task/task_lifecycle.rs` or wherever `task_terminate` lives), drop the ring on task exit (Drop on `Option<KBox<...>>` handles it).

### 3C. Kernel: syscall handler

- [ ] **3C.1** Create `core/src/syscall/test_handlers.rs` with `pub fn syscall_test_report(...) -> SyscallDisposition`:
  - Read `status`, `name_ptr`, `name_len`, `msg_ptr`, `msg_len` from frame
  - Validate pointers via the existing user-pointer validation helpers (find via `git grep -nE 'fn copy_from_user|user_slice_ok'`)
  - Look up current task's `test_reports`; allocate via `KBox::try_new(...)` if `None`
  - Push `TestReport { status, name: copy, msg: copy }` (truncate to fixed sizes)
  - Return 0 on success, negative on failure
- [ ] **3C.2** Wire into the syscall dispatch (find via `git grep -nE 'match.*syscall_num|SYSCALL_[A-Z]'` — `core/src/syscall/dispatch.rs` or similar)

### 3D. Userland: slibc helper + PAL

- [ ] **3D.1** Add `slibc/src/pal/slopos.rs` syscall wrapper for `SYSCALL_TEST_REPORT` using `raw::syscall5`:
  ```rust
  fn test_report(status: u32, name: &str, msg: &str) -> SyscallResult<()>
  ```
- [ ] **3D.2** Create `slibc/src/test_harness.rs`:
  ```rust
  pub enum TestStatus { Pass = 0, Fail = 1, Skip = 2 }
  pub fn report(status: TestStatus, name: &str, msg: &str);
  pub fn run(cases: &[(&'static str, fn() -> bool)]) -> !;
  ```
  - `report` calls the PAL syscall; ignores errors (best-effort)
  - `run` iterates cases, calls each, calls `report(Pass|Fail, name, "")`, tracks failed count, calls `process::exit(failed.min(255) as i32)`
- [ ] **3D.3** Add `pub mod test_harness;` to `slibc/src/lib.rs`

### 3E. `utest!` macro and runner

- [ ] **3E.1** In `ktesting/src/lib.rs`, add `pub macro_rules! utest`:
  ```
  utest!(name = $ident:ident, bin = $bin:literal);
  utest!(name = $ident:ident, bin = $bin:literal, argv = &[$($arg:literal),*]);
  ```
  Expansion emits a `TestDesc` with `kind = TestKind::Userland`, `bin_cstr` set, `argv_ptr` set, and `run` pointing to the common `utest::utest_run_thunk`.
- [ ] **3E.2** Create `ktesting/src/utest.rs` with `pub fn utest_run_thunk(desc: &TestDesc) -> TestOutcome`:
  - Parse `bin_cstr`, build argv vec
  - `let pid = exec::spawn_program_with_attrs(bin, argv, prio, flags)?` — find exact API via `git grep -nE 'spawn_program|spawn_path'`
  - `task_wait_for(pid)` — blocks until child exits
  - `let exit = task_get_exit_record(pid)` — read exit code
  - `let reports = task_drain_test_reports(pid)`
  - For each report, emit `KTAP\t  ` indented subtest line
  - Roll-up: any `Fail` report → parent `TestOutcome::Fail`; else Pass; non-zero exit with no reports → Fail
- [ ] **3E.3** Wire `runner::run` to dispatch based on `desc.kind`: `Kernel` → call `desc.run()` directly; `Userland` → call `utest_run_thunk(desc)`.

### 3F. KTAP nested-subtest emit

- [ ] **3F.1** In `ktesting/src/ktap.rs`, add `emit_subtest(parent_idx_indent: usize, sub_idx: u32, status: TestStatus, name: &str)` — emits `KTAP\t  ok N - <name>` or `KTAP\t  not ok N - <name>` indented to indicate nesting.
- [ ] **3F.2** `utest_run_thunk` calls `emit_subtest` for each drained report between the parent test's `emit_ok`/`emit_not_ok` and the next parent test's emission.

### 3G. Migrate existing userland test binaries

- [ ] **3G.1** Rewrite `userland/src/bin/tests/heap_allocator_test.rs` to call `slibc::test_harness::run(&[("alloc_basic", test_alloc_basic), ("forward_coalesce", test_forward_coalesce), ...])`
- [ ] **3G.2** Same for `userland/src/bin/tests/fork_test.rs`
- [ ] **3G.3** Same for `userland/src/bin/tests/io_capture_test.rs`
- [ ] **3G.4** Confirm each test bin's exit code reflects subtest failure count.

### 3H. `utest!` registrations

- [ ] **3H.1** Create `ktesting/src/utests.rs` (or co-locate per subsystem) with three `utest!` invocations, one per migrated binary.
- [ ] **3H.2** Build pipeline test: `just test FILTER='utest::*'` should run only these three.

### 3I. Build pipeline integration

- [ ] **3I.1** Create `tools/list-utests/Cargo.toml` (host-target binary; depends on `slopos-testing` with a feature gate to expose registry walking on the host)
- [ ] **3I.2** `tools/list-utests/src/main.rs`: walks the test registry, prints one line per `kind=Userland` entry: `<bin_path>:<binary_name>` (binary_name parsed from the path).
- [ ] **3I.3** Update `scripts/build_userland.sh` to invoke `cargo run -p list-utests --target <host>` and parse output to derive the userland-test binary list.
- [ ] **3I.4** Update `justfile`: drop hardcoded `test_userland_bins` literal; derive from xtask. Verify `_build-userland-tests` still produces the right ELFs.
- [ ] **3I.5** Verify `_fs-image-tests` packages all derived binaries.

### Phase 3 Gate

- [ ] **GATE 3.1**: `just build` passes
- [ ] **GATE 3.2**: `just test` runs ≥2393 kernel tests + 3 utests
- [ ] **GATE 3.3**: KTAP output shows utest parent lines + indented subtest lines
- [ ] **GATE 3.4**: Induce a fail in a userland subtest → parent KTAP `not ok` reflects it; subtest line shows `not ok`
- [ ] **GATE 3.5**: Adding a new utest in a fresh file (just the `utest!` invocation) is picked up by the build automatically
- [ ] **GATE 3.6**: `SYSCALL_TEST_REPORT` from a non-test task returns an error and does not allocate
- [ ] **GATE 3.7**: `cargo fmt --all` no-op
- [ ] **GATE 3.8**: stack-size and alloc-discipline gates pass

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
