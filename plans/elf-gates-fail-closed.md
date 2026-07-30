# Make the build gates fail closed

A gate that can silently pass is worse than no gate: it converts an unverified
build into a verified-looking one, and it does so exactly when the environment
is degraded — a fresh runner, a toolchain bump, a renamed section. Both
post-link ELF gates currently have that property, and the kernel that runs the
test suite is exempt from both.

## What is wrong

### `check_kernel_softfloat.sh` reports OK when it disassembles nothing

Three independent fail-open paths compose into one:

```sh
OBJDUMP="$SYSROOT/lib/rustlib/$HOST/bin/llvm-objdump"
if [ ! -x "$OBJDUMP" ]; then
    OBJDUMP="objdump"                                    # 1. silent fallback
fi
COUNT="$("$OBJDUMP" -d --no-show-raw-insn "$ELF" 2>/dev/null \
    | grep -cE '%?(x|y|z)mm[0-9]' || true)"              # 2. errors discarded
                                                         # 3. `|| true` -> "0"
if [ "$COUNT" -ne 0 ]; then ... exit 1; fi
echo "check_kernel_softfloat: OK — kernel ELF is vector-free (+soft-float)"
```

`scripts/check_kernel_softfloat.sh:42-61`. If the sysroot tool is absent and
plain `objdump` is not installed, `COUNT` is `0` and the script emits its
strongest possible pass message. The same happens if the disassembler errors on
the input, or if a future llvm-objdump renames its register syntax so the regex
stops matching.

This gate is load-bearing. Its own header explains why: a single kernel vector
instruction in a fault or IRQ path clobbers the interrupted user task's live
XMM/YMM, because syscall and exception entry do not save FPU state.

### `check_stack_sizes.sh` reports OK on an ELF with no `.stack_sizes`

This one fails *closed* on a missing tool — `scripts/check_stack_sizes.sh:41-48`
exits 2 when `llvm-readobj` cannot be found, which is correct and is the model
the softfloat gate should copy. The hole is different: there is no check that
the parse produced any records at all. `offender_count` is computed from the
offenders file (`:148`) and, when the `.stack_sizes` section is absent or the
readobj output format changes, that file is empty and the script prints OK at
`:167`.

The section is populated by `-Zemit-stack-sizes`, injected in
`scripts/build_kernel.sh`. Anything that drops that flag — a profile change, a
RUSTFLAGS override, a cargo behaviour change — silently disarms the gate rather
than failing the build.

### The tests kernel is exempt from both, and the release kernel is never built

`just test` builds `builddir/slop-tests.iso` with the `tests` feature, and both
ELF gates skip that build:

```
check_stack_sizes: skipped (kernel/tests feature enabled)
check_kernel_softfloat: skipped (kernel/tests feature enabled)
```

The softfloat skip has a real reason: `slopos-ostd/src/test_support/cpu_state.rs`
carries deliberate named-register XMM/AVX asm for the xsave conformance tests.
The skip is a whole-binary exemption for a handful of symbols.

Nothing in CI builds a release-profile kernel, so the stack-frame ceiling — the
load-bearing enforcement of Inv. 5' — is only ever measured against debug
codegen, which inlines differently and produces different frames.

## What to build

### 1. Fail closed on tool and input

Resolve the disassembler the way `check_stack_sizes.sh` already resolves
`llvm-readobj`: prefer the sysroot tool, fall back to `command -v`, and exit 2
with a message naming `llvm-tools-preview` when neither exists. Drop
`2>/dev/null` and `|| true`; let a disassembler error be a build failure.

Both gates then need an input sanity check, because a working tool on the wrong
input is the remaining hole:

- softfloat: require the disassembly to be non-empty and to contain at least one
  recognisable instruction line before trusting a zero match count.
- stack sizes: require the record count to exceed a floor. The current kernel
  yields 44,219 records, so a floor of 1,000 is far below any plausible real
  build and far above zero. Print the count on success so a collapse is visible
  in the log even when it stays above the floor.

### 2. Give every gate a known-bad fixture

`scripts/check_task_ownership.sh --self-test` already does this, and it is the
right pattern: the gate proves it can still reject before it is trusted to
accept. Generalise it.

Add `--self-test` to each gate, run from `check-framekernel-gates` immediately
before the real invocation:

- softfloat: assemble a three-line object containing one `movups` and assert the
  gate rejects it.
- stack sizes: synthesise a `.stack_sizes` section with one oversized record and
  assert rejection; separately assert that an ELF with no section is rejected.
- the source scanners (`check_unsafe_outside_ostd.sh`, `check_alloc_dep.sh`,
  `check_no_kernel_async.sh`, `check_drop_panic_free.sh`,
  `check_wait_predicate_purity.sh`): a fixture directory of files each gate must
  flag, plus files it must not (the cfg-gated and comment forms it deliberately
  accepts), so the accept side is tested too.

A gate whose self-test fails is a build failure. This is the single highest-value
item here: it converts "the gate looked green" into "the gate demonstrated it can
still go red".

### 3. Replace the whole-binary test exemptions with symbol allowlists

For softfloat, the exemption is a symbol set, not a binary. Restrict the scan to
exclude the known conformance-test symbols in
`slopos-ostd/src/test_support/cpu_state.rs`, capped at their measured count, and
run the gate on the tests kernel like any other. A new vector instruction
anywhere else in the tests kernel then fails, which is what the gate is for —
and the tests kernel is the binary that actually executes 2716 times per CI run.

For stack sizes, determine whether the tests build emits `.stack_sizes` at all.
If it does, run the gate on it with a separate allowlist. If it does not, that is
a build-flag gap to close rather than a reason to skip.

### 4. Build and gate the release kernel

Add a CI job that builds the release-profile kernel and runs both ELF gates on
it. It need not boot; the point is that the frame-size ceiling and the soft-float
guarantee are properties of the binary users would run, not of the debug binary.

Expect the stack allowlist to differ between profiles. Keep two allowlists rather
than taking the union — a union hides a regression in whichever profile is
looser.

### 5. Track the allowlist as a budget

`check_stack_sizes.sh` carries a measured allowlist where each entry is capped at
its observed size, which is the right design. What is missing is pressure: add
the entry count and the total allowlisted bytes to the success line, so growth is
visible in every build log rather than only to whoever edits the file.

## Sequencing

| Phase | Work | Done when |
|---|---|---|
| 1 | Fail closed on tool and input in both ELF gates; print record counts | A build with `llvm-objdump` removed from PATH and the sysroot fails, rather than passing |
| 2 | `--self-test` for both ELF gates, wired into `check-framekernel-gates` | Each gate demonstrably rejects a known-bad fixture on every CI run |
| 3 | `--self-test` fixtures for the five source scanners, covering accept and reject | A deliberately-planted violation of each discipline fails CI |
| 4 | Symbol allowlist for softfloat; run both gates on the tests kernel | `just test` no longer prints `skipped` for either gate |
| 5 | Release-kernel CI job with its own stack allowlist | A frame that is under 2 KiB in debug and over it in release fails CI |
| 6 | Allowlist budget reporting | Every build log states the allowlist size and total |

## Prior art

Linux treats this as settled. `objtool` is a hard build dependency that fails the
build when it cannot run, rather than skipping validation — the reasoning being
that an unvalidated object silently entering the kernel image is the failure the
tool exists to prevent. `CONFIG_FRAME_WARN` is the direct analogue of the stack
ceiling, though Linux only warns where SlopOS fails.

The self-test pattern is what compiler test suites call a *negative test*, and
what mutation testing generalises: a check that has never been observed to fail
has not been observed to work. It is also why `check_task_ownership.sh` grew its
`--self-test` flag, which makes the precedent in-tree rather than borrowed.

## Risks

- **A gate that newly fails closed will break someone's environment.** That is
  the point, but it should break with a message naming the missing component and
  the `rustup component add` that fixes it. `rust-toolchain.toml` already
  declares `llvm-tools-preview`, so a correctly provisioned runner is unaffected.
- **The record-count floor is a magic number.** Keep it far below the real value
  and state its provenance in the script, the way `TEST_COUNT_BASELINE` is
  handled: written down in one place, measured rather than guessed.
- **Running the gates on the tests kernel may surface existing violations.** If
  so, that is a finding, not a reason to restore the skip. Measure first, then
  decide whether each is a symbol to allowlist or a bug to fix.
