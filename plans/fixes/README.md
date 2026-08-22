# Fix proposals

Design documents for the CVSS findings closed in the 2026-08-24 pass, kept
because each records *why* a design was chosen and what was rejected — the part
a diff cannot carry.

Each was written by a research agent against the upstream reference, then put
through two rounds of adversarial review that verified every code claim against
the tree. The review caught a fabricated construction-site count, a check that
would have rejected valid images carrying deleted directory entries, and
several wrong line references; those corrections are recorded in each
document's revision log.

Read them as the reasoning behind the change, not as the specification of what
landed — implementation diverged where the tree disagreed with the plan, most
significantly for `SLOPOS-2026-0030`, where the proposed EEVDF calendar wheel
was implemented, found to destabilise signal delivery under load, and replaced
with a per-tier aging backstop. The `CVSS.md` entry for each finding records
what actually landed.
