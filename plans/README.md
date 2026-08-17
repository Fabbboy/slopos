# SlopOS Plans

This directory holds only live proposals and open work, written statelessly
against the current tree: a plan describes what to build and why, never what
was already built or how the plan evolved. When part of a plan lands, remove
that part and re-baseline the rest; when nothing remains, delete the plan.
Git history preserves the record. Stable public documentation lives in the
sibling docs repo.

Even live plans can carry stale paths after refactors — verify paths before
editing.

## Current Files

| Document | Scope |
|----------|-------|
| `KNOWN_ISSUES.md` | Working notes on open issues; verify before using as source of truth |
| `authority-model.md` | What authority is: a flat per-process capability set, total by compile-time construction, with rights on objects |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

`authority-model.md` builds on two things that have landed. The `Process` object owns
identity — `slopos_ostd::process`, with the address-space and descriptor tables keyed on
`Handle<Process>` rather than on a recycled pid — so a credential has a principal to belong
to. And per-principal accounting is complete: `slopos_ostd::process::quota` is the arena and
the linear `Charge` token, every `ResourceKind` is charged and enforced, `mm::reclaim` bounds
holding time as well as acquisition, and the numbers are published through `prlimit64`. The
authority plan can assume a live `Account` per process rather than proposing one.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
