# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17 (original); last reviewed 2026-07-27.
Method: repository-wide static review (`grep`, `ast-grep`, targeted source inspection), plus NVD CVE lookups via `curl` + `jq`.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no backwards-compatibility or audit-trail obligations, so this ledger tracks **open findings only**. When a finding is resolved it is **removed** from this file (not retained as a `fixed` historical record). Internal IDs stay stable for findings that remain open, so gaps in the numbering are expected.

## Scoring Method

- CVSS version: 3.1 Base Score
- Formula used: Base score derived from Impact + Exploitability subscores with scope-aware rounding up to one decimal
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`
- Reusable scorer script: `scripts/cvss_calc.py`

### Reusable scorer

```bash
python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
```

## Open SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0006
- Title: ext2 inode/group descriptor size trust can panic on out-of-bounds slicing
- Status: open
- Confidence: 70 (evidence 35, exploitability 15 — depends on whether SlopOS mounts untrusted images, reproducibility 20) — below the 80 threshold for a guaranteed CVSS-scored issue.
- Evidence: untrusted on-disk `inode_size` / derived offsets used in slice indexing without validating `within + size <= block_size`. `fs/src/ext2/ondisk.rs` (`Inode::parse`, `GroupDesc::parse`, `effective_inode_size` clamps `inode_size == 0` up to 128 but does not bound it from above against `block_size`) and the inode-table offset math in `fs/src/ext2/inode.rs` / `fs/src/ext2/mod.rs`.
- Impact: malformed-image-triggered out-of-bounds slice index. Because `fs/` is `#![forbid(unsafe_code)]`, this is a bounded-slice **panic** (DoS), never memory unsafety/UB.
- CVSS vector/score: not assigned because confidence is below 80.
- Remediation (proposed): validate `effective_inode_size() as u32 <= block_size` and bound each `within + size` against the containing block before slicing.

## Relevant NVD CVE Analogs (fetched)

Retrieved using NVD API pattern:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Selected analogs:

| CVE | Vector | Score | Severity | Why relevant |
|---|---|---:|---|---|
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | Filesystem metadata parsing / ext* class |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening analog |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in kernel path |

## Priority Remediation Plan

1. Guard all ext2 slice constructions with explicit bounds checks before indexing, including `effective_inode_size() <= block_size` (SLOPOS-2026-0006).

> **Residual note from the closed RCU finding.** The tick and QS-IPI sites now
> decline to report while a read-side section is open, so a grace period can no
> longer complete *because of a false quiescent state*. `synchronize_rcu`'s
> 500 ms stall path is unchanged and still declares a grace period complete
> after warning about a holdout CPU — an escape hatch that predates this
> finding and is a liveness/policy decision rather than part of it. It is far
> longer than any legitimate read-side section (which holds a `PreemptGuard`
> and cannot block), so it is not a practical exposure, but it is the one way
> the guarantee can still be violated and it is written down here rather than
> assumed away.
