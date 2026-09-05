# SlopOS Vulnerability Audit and CVSS Scoring

**No findings are currently open.** Last swept 2026-09-05 (Phase 4 crash
consistency: ext2 mount state, `errors=remount-ro`, orphan inodes, the VFS
open-inode reference table).

That sweep found two unprivileged denial-of-service defects
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
