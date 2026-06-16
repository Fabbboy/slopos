# SlopOS Plans

This directory contains working notes, historical analysis, and temporary
roadmaps. It is not the public documentation surface.

Stable public documentation now lives in the sibling docs repo.

Treat a plan as historical unless it has been refreshed against the current
workspace and test results. In particular, older plans may contain stale paths,
completed phases, or obsolete test counts.

## Current Files

| Document | Status |
|----------|--------|
| `ANALYSIS_SLOPOS_VS_LINUX_REDOX.md` | Historical comparative analysis |
| `FRAMEKERNEL_PLAN.md` | Historical implementation plan; public verification docs are now in `slopos-docs` |
| `KNOWN_ISSUES.md` | Working notes; verify before using as source of truth |
| `LEGACY_MODERNIZATION_PLAN.md` | Historical unless refreshed; contains old path references |
| `microtransactions.md` | Proposed: kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `RAW_PTR_TO_KARC_MIGRATION.md` | Working migration notes |
| `WILLBLOCK_SCHEDULER_STATUS.md` | Historical scheduler status notes |
| `desktop-ui.md` | UI working notes |
| `panic-reliable-abort-core.md` | Panic/reliability working notes |
| `resource-lifetime-redesign.md` | Resource lifetime working notes |
| `terminal-split.md` | Terminal working notes |
| `widget-toolkit-spec.md` | Widget toolkit working specification |

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
