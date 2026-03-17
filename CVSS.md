# SlopOS Vulnerability Audit and CVSS Scoring

Date: 2026-03-17
Method: repository-wide static review (`grep`, `ast-grep`, targeted source inspection), plus NVD CVE lookups via `curl` + `jq`.

## Scoring Method

- CVSS version: 3.1 Base Score
- Formula used: Base score derived from Impact + Exploitability subscores with scope-aware rounding up to one decimal
- Severity mapping: `0.0 None`, `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`, `9.0-10.0 Critical`

## Candidate SlopOS Findings (for remediation)

These are **candidate CVE-style records** for internal tracking. They are not official CVE assignments.

### SLOPOS-2026-0001
- Title: Unchecked user pointer write in `syscall_input_poll`
- Evidence: `core/src/syscall/ui_handlers.rs:135`, `core/src/syscall/ui_handlers.rs:146`, `drivers/src/input_event.rs:341`
- Impact: `event_ptr` comes from userspace and is dereferenced directly (`*event_ptr = event`) without `UserPtr` / `copy_to_user`, enabling arbitrary kernel memory write or kernel panic.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H`
- Base score: `8.4` (High)

### SLOPOS-2026-0002
- Title: Unchecked user pointer write in `syscall_input_poll_batch`
- Evidence: `core/src/syscall/ui_handlers.rs:154`, `core/src/syscall/ui_handlers.rs:165`, `drivers/src/input_event.rs:326`, `drivers/src/input_event.rs:341`
- Impact: unvalidated `buffer_ptr` is passed to `input_drain_batch`, which writes `InputEvent` entries using raw pointer arithmetic and `write(event)`.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H`
- Base score: `8.4` (High)

### SLOPOS-2026-0003
- Title: Unchecked output pointer write in compositor window enumeration
- Evidence: `core/src/syscall/ui_handlers.rs:270`, `core/src/syscall/ui_handlers.rs:274`, `video/src/compositor_context.rs:516`, `video/src/compositor_context.rs:560`
- Impact: `out_buffer` is user-controlled and written directly in kernel context. This path is compositor-gated, but still provides arbitrary write if compositor context is compromised.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:H/UI:N/S:U/C:H/I:H/A:H`
- Base score: `6.7` (Medium)

### SLOPOS-2026-0004
- Title: Unchecked user pointer read in `syscall_surface_set_title`
- Evidence: `core/src/syscall/ui_handlers.rs:122`, `core/src/syscall/ui_handlers.rs:130`, `video/src/compositor_context.rs:716`
- Impact: `title_ptr` is dereferenced via `from_raw_parts` without user pointer validation. Can trigger kernel faults and may permit kernel memory disclosure depending on mapping/readability.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:H`
- Base score: `7.7` (High)

### SLOPOS-2026-0005
- Title: ext2 superblock sanity gaps allow divide-by-zero panic
- Evidence: `fs/src/ext2.rs:191`, `fs/src/ext2.rs:748`, `fs/src/ext2.rs:964`, `fs/src/ext2.rs:973`, `fs/src/ext2.rs:1002`, `fs/src/ext2.rs:1067`, `fs/src/ext2.rs:1068`
- Impact: attacker-controlled ext2 metadata (`inodes_per_group`, `blocks_per_group`) is not validated for zero before division/mod operations, causing kernel panic on malformed filesystem images.
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H`
- Base score: `5.5` (Medium)

### SLOPOS-2026-0006
- Title: ext2 inode/group descriptor size trust can panic on out-of-bounds slicing
- Evidence: `fs/src/ext2.rs:205`, `fs/src/ext2.rs:574`, `fs/src/ext2.rs:591`, `fs/src/ext2.rs:761`, `fs/src/ext2.rs:1072`, `fs/src/ext2.rs:1087`
- Impact: untrusted on-disk `inode_size` and derived offsets are used in slice indexing without validating `within + size <= block_size`, enabling malformed-image-triggered OOB panic (DoS).
- CVSS vector: `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:H`
- Base score: `6.1` (Medium)

## Relevant NVD CVE Analogs (fetched)

Retrieved using NVD API pattern:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=<CVE-ID>" | jq
```

Sample command requested and executed:

```bash
curl -s "https://services.nvd.nist.gov/rest/json/cves/2.0?cveId=CVE-2020-0001" | jq
```

Selected analogs:

| CVE | Vector | Score | Severity | Why relevant |
|---|---|---:|---|---|
| CVE-2010-3904 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H | 7.8 | HIGH | Unchecked user pointer / kernel memory corruption class |
| CVE-2022-0185 | CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H | 8.4 | HIGH | Local unprivileged kernel heap corruption class |
| CVE-2023-32233 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H | 7.8 | HIGH | Kernel privilege escalation boundary failure class |
| CVE-2025-37785 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:H | 7.1 | HIGH | Filesystem metadata parsing / ext* class |
| CVE-2024-26817 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Kernel allocation/validation hardening analog |
| CVE-2025-38665 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Local kernel DoS through insufficient validation |
| CVE-2025-39838 | CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H | 5.5 | MEDIUM | Null/invalid pointer handling in kernel path |
| CVE-2016-5195 | CVSS:3.1/AV:L/AC:H/PR:L/UI:N/S:U/C:H/I:H/A:H | 7.0 | HIGH | Local memory write integrity break analog |

## Priority Remediation Plan

1. Replace all raw syscall pointer dereferences in UI/input paths with `UserPtr`/`UserBytes` + `copy_from_user`/`copy_to_user`.
2. Add ext2 superblock invariant checks at init (`inodes_per_group > 0`, `blocks_per_group > 0`, `inode_size` bounds).
3. Guard all ext2 slice constructions with explicit bounds checks before indexing.
4. Add regression tests for malformed userspace pointers and malformed ext2 images.
