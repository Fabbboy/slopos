# SlopOS Vulnerability Audit and CVSS Scoring

**No findings are currently open.** Last swept 2026-09-06 (Phase 5: the ext2
disk as the default, writable root — the seal, the exec path, `/bin` write
protection, disk exhaustion, and what "the root is now writable by userland"
newly reaches).

That sweep found three defects, all fixed inside the same unreleased change
and therefore not ledger entries. They are recorded as method because each is
a class, not an instance:

- **A seal on the contents is not a seal on the name.** Every grant-path
  binary carried `EXT2_IMMUTABLE_FL`, so `/bin/halt` could not be overwritten
  — but `/bin` itself was an ordinary directory. `rename("/bin", "/x")`,
  `mkdir("/bin")`, plant a `halt`, and `exec::grants` confers `TASK_FLAG_POWER`
  on whatever was planted, because program identity is a path and the table is
  keyed on it (`core/src/exec/grants.rs`). Confidence 95 — evidence 40 (the
  grant table and the unsealed inode were both read directly), exploitability
  30 (three syscalls, any unprivileged process on the disk root; the RAM root
  had the same hole because ramfs never checked a parent's seal at all),
  reproducibility 25 (`grant_directories_are_sealed` in `spawn_privilege_test`
  was confirmed to fail on both roots with the fix reverted). Would have
  scored `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H` — **7.8 HIGH**. The
  fix seals `/bin` and `/sbin` on both roots and makes ramfs enforce a
  parent's seal on create, unlink and rename as ext2 already did. The general
  rule: a privilege keyed on a path must seal every component of that path,
  because the attack that replaces the leaf and the attack that replaces its
  parent reach the same grant.
- **A reserve on one table is not a reserve on the other.** ext2's
  `s_r_blocks_count` stops an unprivileged writer filling the *block* pool,
  but ext2 has no `s_r_inodes_count`: a process creating empty files exhausts
  the inode table with every block still free, and `/sbin/init` gets `ENOSPC`
  on its next create. Confidence 90 — the allocator was read, the fixture
  image has 4 096 inodes against 4 096 blocks, and
  `test_ext2_reserve_refuses_unprivileged_allocation` pins the inode half.
  Would have scored `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H` — **5.5
  MEDIUM**. The fix applies the block reserve's ratio to the inode table.
- **A blocking primitive re-entered by its own wake.** Not a security finding
  — no attacker chooses when a virtio completion lands — but recorded here
  because it was found by this sweep's method: the scheduler's deferred
  reschedule and trap-exit paths skipped a `Blocked` current task and not a
  `Ready` one, so a wake landing between the Blocked-CAS and the deschedule
  let `schedule()` dequeue the caller as its own successor and spin on its own
  `on_cpu` forever. A quarter of disk-root runs stalled on it. Reverting the
  fix reproduced 3 stalls in 10; with it, 0 in 21.

The previous sweep (2026-09-05, Phase 4) found two unprivileged denial-of-service defects
(`CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H`, 5.5 MEDIUM each) and one
cross-file disclosure race, all three introduced *and* fixed inside the same
unreleased change, so none is a ledger entry under the open-findings-only
policy below. They are recorded here as method rather than as findings, because
what produced them generalises: a new error classification that decides a
mount-wide policy must be checked against **every** producer of the errors it
names, in the direction of "can an unprivileged caller induce this on demand".
Both DoS defects were one overloaded error variant — `Ext2Error::InvalidBlock`
meaning both "the image is damaged" and "your offset is out of range" — and the
fix was to split the caller-argument case into `InvalidRange`, which
`is_corruption` deliberately excludes. `fs/src/tests.rs`'s
`test_ext2_corruption_latches_but_caller_errors_do_not` pins both directions
and was confirmed to fail against the vulnerable code.

This file is the living ledger of open security findings. It is empty of
findings by design, not by neglect: every entry recorded since 2026-03-17 has
been fixed and removed under the pre-alpha policy below. What remains is the
method and the tooling, so the next sweep has a shape to follow.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no backwards-compatibility
> or audit-trail obligations, so this ledger tracks **open findings only**. When a
> finding is resolved it is **removed** from this file, not retained as a `fixed`
> historical record. Internal IDs stay stable for findings that remain open, so
> gaps in the numbering are expected and IDs are never reused. The git history is
> the audit trail; `git log -- CVSS.md` recovers any entry that was here.

The highest ID issued so far is **SLOPOS-2026-0053**. The next finding is
`SLOPOS-2026-0054`.

## Method

A sweep is three phases, and the third is what makes the output trustworthy:

1. **Reference briefs.** Before reading SlopOS, read what mature kernels
   settled on for the subsystem in question — including designs they tried and
   abandoned, which is usually the more informative half.
2. **Per-subsystem auditors.** One pass per subsystem (syscall entry, memory
   management, filesystems, drivers, net, scheduler, authority model),
   producing candidate findings with exact paths and lines.
3. **Adversarial verification.** One verifier per candidate, instructed to
   default to **REFUTED** and to locate the guard that would disprove the
   claim. Two independent lenses for anything scoring High or above. A
   candidate that survives with a concrete attacker trigger becomes a finding;
   everything else is discarded or filed under `plans/` as an engineering
   defect.

Tools: repository-wide static review (`grep`, `ast-grep`, targeted source
inspection), plus NVD CVE lookups via `curl` + `jq` for prior-art analogs.

### Required cadence

1. Run a security sweep after each major milestone and before any release or PR
   handoff.
2. Re-scan subsystems touched by recent commits — at minimum syscall paths,
   memory management, filesystems, and drivers.

### Triage workflow (strict order)

1. **List all findings first** in a raw triage section. Do not score as CVSS yet.
2. Assign each a **confidence score (0-100)**:
   - Evidence quality (0-40): direct code proof, exact path and line references
   - Exploitability clarity (0-30): realistic attacker path and impact
   - Reproducibility (0-30): deterministic repro or strong step-by-step plausibility
3. Only findings with **confidence >= 80** are **guaranteed issues**.
4. Only guaranteed issues get a CVSS vector and score.
5. Use `scripts/cvss_calc.py` so vectors are computed identically across agents.

A finding below 80 still belongs in this file if it has an attacker-reachable
trigger — it is recorded without a CVSS vector, and its confidence line says
what evidence would raise it.

**Findings with no attacker-reachable trigger are deliberately absent even at
high confidence**, because the non-negotiable rule below forbids presenting
speculative issues as scored vulnerabilities. Real engineering defects of that
kind are tracked in `plans/` instead.

### Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- Confidence-gated, evidence-backed findings only.
- **Verify the claim before fixing it.** A ledger entry can be wrong: entries
  have described bounds that were already total, and the real defect at the
  named site turned out to be different and worse. Read the code, not the
  entry.
- A fix lands with a test that **fails without it**. Confirm that by reverting
  the fix and re-running, never by assertion.

## Entry format

Every finding carries:

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps
- Remediation: the shape of the fix, not just "add a check"
```

### Repro requirements

1. Add a minimal repro for each guaranteed issue when technically feasible.
2. A repro may be a syscall sequence, a malformed input artifact, or concise
   PoC steps.
3. If no safe repro is possible, document why and give the nearest
   deterministic validation method.

## Scoring method

- CVSS version: 3.1 Base Score
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`

SlopOS has no credential model, so "unprivileged local attacker" means any
process that can execute code. Those are scored `AV:L/PR:L` by the usual
convention.

A panic in a `#![forbid(unsafe_code)]` crate is an **availability** impact,
never a memory-safety one, and is scored accordingly — several findings that
read as alarming are correctly `A:H` with `C:N/I:N`. Note that overflow checks
are on in the dev and tests kernels and off in release, so the same arithmetic
defect is a panic in one build and a silent wrong value in another; say which
when scoring.

### Reusable scorer

```bash
python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
```

### Prior-art lookup

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Analogs worth citing when they match a finding's shape:

| CVE | Vector | Score | Severity | Shape |
|---|---|---:|---|---|
| CVE-2016-5696 | CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:L/I:L/A:N | 4.8 | MEDIUM | Global counter side channel in a network stack |
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | ext* metadata parsing: a record individually legal but inconsistent with the walker's own arithmetic |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in a kernel path |

## Open findings at a glance

| ID | Title | Score | Severity |
|---|---|---|---|
| — | none open | — | — |

## Open SlopOS findings

None. SLOPOS-2026-0053 (verity trailer unreachable when the image is not a
whole number of sectors) was closed on 2026-09-03: `gen_verity.py` pads the
image to a sector multiple, `build_verified` refuses a trailer it cannot use
instead of failing open, `verity=require` turns an unverified disk into a
failed boot step, and `test_verity_artifact_*` mounts the shipped artifact
through a real virtio capacity under `just test`. The same sweep looked at
what the fix newly exposed — a trailer forged inside the filesystem's own
data blocks, the `verity=require` arms that did not reach the trailer parse,
a kernel-internal `VfsHandle` writing through a read-only mount, and `EROFS`
collapsed to `ENOENT`/`EINVAL` on two syscall funnels — and closed each in
the same change.

## Structural invariants the closed findings left behind

Worth knowing before writing a new finding in these areas, because each one
changes what a defect there would have to look like:

- **`Ext2Geometry` is the only constructor of `GroupIdx`.** Superblock geometry
  is validated once at mount, so an unbounded group index is not a value the
  ext2 code can construct. A new ext2 caller inherits the bounds rather than
  re-deriving them.
- **`FdContainment` is total over `FileKind`.** A new descriptor kind must
  state whether it can own other descriptions before it compiles, and only
  `Leaf` kinds cross an ancillary transfer. This is what makes an SCM_RIGHTS
  reference cycle unrepresentable rather than collected.
- **`CursorUnmapHook::select_cr3` is the only path to a PCID.** The tag and the
  right to skip the TLB flush are one decision, taken by the per-CPU pool that
  owns the binding.
- **`process_vm_reset_for_exec` and `create_process_vm_for` share one
  definition of a fresh address space**, so exec is an address-space boundary
  and a new region kind is added in one place.
- **The per-tier aging backstop bounds how long a runnable tier can be passed
  over**, so strict priority cannot starve a lower tier indefinitely.
- **A verity trailer is recognised only beyond the filesystem's own extent,
  and a verified device is write-protected.** `build_verified` takes the
  superblock's block count, so bytes a user can write into a file are never a
  trailer, and a trailer that is present but unusable refuses the mount rather
  than mounting unverified. Read-only-ness is one rule
  (`Ext2Fs::read_only_for`) and reaches userland as `EROFS` through
  `MOUNT_RDONLY`, checked at every `vfs::ops` mutation before the filesystem
  sees it.
