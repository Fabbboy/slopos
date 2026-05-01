# SlopOS Test Output Format

This document describes the wire format that the SlopOS kernel test harness
emits over its serial console, plus the JSONL event schema that
`builddir/run_tests` (built from `tools/run_tests/`) writes when given
`--json <path>`. Both are stable contracts for tooling.

## 1. Overview

When the kernel boots with `tests=on` on its Limine cmdline, two test
phases run in order:

- **Phase 1 — kernel**: every entry registered with `stest!` whose
  `<module>::<name>` matches the active filter. Runs from the boot
  pipeline, before `init` is launched.
- **Phase 2 — userland**: every entry registered with `utest!`.
  Triggered from `/sbin/init` via a syscall after the filesystem is
  mounted; each entry spawns a `/bin/<name>` binary, waits for it to
  exit, and drains the per-task test-report ring.

Both phases emit a self-contained KTAP-grammar stream over the serial
console (`-serial stdio` in QEMU). Every line emitted by the harness is
prefixed with the literal seven bytes `KTAP` + ASCII tab (`0x09`). Lines
without that prefix are ordinary kernel klog and may interleave freely;
the host parser ignores them outside log-literal blocks.

When the userland phase finishes, the kernel writes to the
`isa-debug-exit` port (`0xf4`) with status `0` (all phases green) or `1`
(any failure), which causes QEMU to exit `1` or `3` respectively.

## 2. Header

Each phase opens with two lines:

```
KTAP	TAP version 14
KTAP	1..N
```

`N` is the number of tests that pass the active filter for this phase
(not the total registered count). Successive phases re-emit `TAP version
14`, so the parser keys phase boundaries off either header line.

## 3. Result lines

Pass:

```
KTAP	ok 17 - slopos_mm::tests::heap::test_heap_kzalloc_zeroed # time_ms=3
```

Pass + slow (when `time_ms > tests.warn_ms`):

```
KTAP	ok 51 - slopos_drivers::tests::msi::test_msi_hot_plug # time_ms=7321 OVER_TIME
```

Pass + expected panic (used by the bootstrap panic-isolation canary):

```
KTAP	ok 1 - bootstrap_panic_isolation # time_ms=3 EXPECTED_PANIC
```

Skip (the test fn returned `TestResult::Skipped`):

```
KTAP	ok 18 - slopos_net::tests::tcp_live::test_loopback_handshake # SKIP test returned Skipped
```

Fail / panic (the diagnostic block below the line is mandatory for
fails):

```
KTAP	not ok 42 - slopos_core::tests::sched::test_priority_inversion # time_ms=11
```

The number after `ok` / `not ok` is the 1-based index *within the
current phase*; it matches the phase's `1..N` plan.

## 4. YAML diagnostic blocks

A `not ok` line is always followed by a YAML diagnostic block:

```
KTAP	  ---
KTAP	  outcome: Fail
KTAP	  file: core/src/scheduler/sched_tests.rs:1832
KTAP	  log: |
KTAP	   [00:01:23.456] SCHED: priority bump observed
KTAP	   [00:01:23.457] ASSERT_EQ: expected 5, got 9
KTAP	  ...
```

In **verbose mode** (`tests.verbosity=verbose`), passing tests may also
emit a diagnostic block — but only with the `log: |` field, no
`outcome:` / `file:`:

```
KTAP	ok 1 - mod::ok # time_ms=2
KTAP	  ---
KTAP	  log: |
KTAP	   trace line 1
KTAP	   trace line 2
KTAP	  ...
```

### Diagnostic fields

- `outcome:` — the kernel-side `TestResult` enum debug-formatted
  (`Fail`, `Panic`, etc.). Failures only.
- `file:` — `path:line` of the test fn. Failures only.
- `log: |` — opens a YAML literal block. The captured klog of the
  test follows, one entry per line, indented `KTAP\t   ` (three spaces
  after the tab). The block ends with `KTAP\t  ...`.

### Truncation markers inside `log:`

The kernel caps the emitted log at 4096 bytes per failure. If the
captured log exceeds that, the head is dropped and the harness inserts:

```
KTAP	   [head trimmed: 12345 bytes]
```

If the per-CPU klog ring overflowed during the test, the tail is
truncated:

```
KTAP	   [tail trimmed: 200 bytes lost to ring overflow]
```

### Multi-CPU verbose logs

Verbose mode may also emit other-CPU klog rings as additional sections
inside the same `log: |` block:

```
KTAP	  log: |
KTAP	   <cpu0 lines>
KTAP	   --- cpu1 ---
KTAP	   <cpu1 lines>
KTAP	  ...
```

These are content lines (3-space indent) — the parser does not need to
treat them specially.

## 5. Nested subtests

Userland tests (Phase 2) run as separate processes. Each `slibc::test_harness::report` call inside a userland binary becomes a
**subtest** line emitted by the kernel-side runner with a 2-space indent
*before* the `ok`/`not ok` parent line:

```
KTAP	  ok 1 - alloc_basic
KTAP	  ok 2 - alloc_huge
KTAP	  not ok 3 - alloc_zero # zero-size returned null
KTAP	ok 5 - utest_heap_allocator # time_ms=234
```

Subtests appear *before* their parent line because the parent's
`time_ms` is only known after the userland binary exits. The host
parser must buffer subtests and attach them to the next top-level
result.

Subtest formats:

```
KTAP	  ok M - <name>
KTAP	  not ok M - <name> [# <diagnostic>]
KTAP	  ok M - <name> # SKIP
```

## 6. Footer

Each phase ends with a single footer line:

```
KTAP	# elapsed_ms=14238 pass=2391 fail=1 skip=0 over_time=14
```

The kernel also emits a non-KTAP companion line (regular klog) for
backwards compatibility with raw-serial readers:

```
TESTS SUMMARY (kernel phase): total=2392 passed=2391 failed=1 elapsed_ms=14238
```

## 7. Bail out

If a test whose name begins with `bootstrap_` fails, the harness aborts
the current phase immediately and emits:

```
KTAP	Bail out! bootstrap_glob_match
```

No further tests in that phase run. The `1..N` plan number is *not*
amended; the host parser detects the discrepancy via the missing
expected results.

## 8. Multi-phase streams

A typical full run on the wire (abridged):

```
[boot prose…]
KTAP	TAP version 14
KTAP	1..2422
KTAP	ok 1 - bootstrap_glob_match # time_ms=1
…
KTAP	ok 2422 - slopos_drivers::tests::tty::test_zzz # time_ms=2
KTAP	# elapsed_ms=12300 pass=2422 fail=0 skip=0 over_time=14
TESTS SUMMARY (kernel phase): total=2422 passed=2422 failed=0 elapsed_ms=12300
[init starts, /sbin/init invokes SYSCALL_RUN_USERLAND_TESTS]
KTAP	TAP version 14
KTAP	1..3
KTAP	  ok 1 - alloc_basic
KTAP	  ok 2 - alloc_huge
KTAP	ok 1 - utest_heap_allocator # time_ms=234
…
KTAP	# elapsed_ms=434 pass=3 fail=0 skip=0 over_time=0
TESTS SUMMARY (userland phase): total=3 passed=3 failed=0 elapsed_ms=434
[QEMU exits via isa-debug-exit port 0xf4]
```

## 9. Cmdline knobs

Threaded via Limine `cmdline:` (which `scripts/build_iso.sh` injects from
its third positional argument, controlled by the justfile `test_cmdline`
constant or the `TEST_CMDLINE` env override):

| Key | Values | Effect |
|---|---|---|
| `tests` | `on` / `off` | master enable; `off` short-circuits the harness |
| `tests.shutdown` | `on` / `off` | write to `isa-debug-exit` after the run |
| `tests.verbosity` | `quiet` / `summary` / `verbose` | per-test emit policy |
| `tests.warn_ms` | integer | mark tests slower than this as `OVER_TIME` |
| `tests.run` | comma-separated globs | only run tests matching at least one |
| `tests.skip` | comma-separated globs | skip tests matching any |

Globs match against `<module_path>::<test_fn>`. Supported wildcards:
`*` (any sequence) and `?` (single char).

## 10. JSONL event log (`--json <path>`)

`builddir/run_tests --json events.jsonl` writes one JSON object per
line. Schema (each event has a `t` discriminator):

| `t` | Other fields |
|---|---|
| `phase_start` | `idx` (1-based), `name` |
| `plan` | `phase_idx`, `n` |
| `test` | `phase`, `phase_idx`, `idx`, `name`, `outcome` (`pass`/`fail`/`skip`/`bail`/`not_run`/`timeout`), `time_ms`, `over_time`, `expected_panic`, `skip_reason`, `fail_file`, `fail_outcome_kind`, `log`, `subtests`, `pre_fail_klog_tail` |
| `phase_end` | `idx`, `name`, `elapsed_ms`, `pass`, `fail`, `skip`, `over_time` |
| `bail` | `phase_idx`, `reason` |
| `run_end` | `wall_ms`, `exit`, `qemu_status`, `user_aborted`, `timed_out`, `truncated`, `phases` (per-phase summaries) |

The schema is stable enough to drive a JUnit XML converter, a
test-history regression detector, or a CI dashboard without requiring
parser changes.

## 11. Exit codes

`builddir/run_tests` exits:

- `0` — all phases green
- `1` — any `not ok`, `Bail out!`, timeout, or truncated stream
- `2` — `qemu_run.sh` returned an unexpected status (kernel didn't
  reach `isa-debug-exit`)
- `64` — flag parse error or missing prerequisite (ISO/fs image)
- `130` — interrupted by SIGINT (does not overwrite
  `builddir/last-fail.list`)

The underlying `scripts/qemu_run.sh test` translates the
`isa-debug-exit` status `0` / `1` (which QEMU itself surfaces as raw exit
`1` / `3`) into host exit `0` / `1`. Anything else means the kernel
never reached the exit port (hang, panic loop, hard fault).
