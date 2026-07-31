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
| `process-identity.md` | Bounded, recycling process id + `Handle<ProcessVm>` as the identity token; fixes the 256-process wall |
| `privilege-model.md` | Spike: what authority should actually be, given no uid and no credential object |
| `rt-sigreturn-xrstor.md` | Validate the signal-return XSAVE image; make a ring-0 #GP survivable |
| `resource-accounting.md` | Spike: per-process limits. Inventory + the reclaim fixes that need no framework |
| `unsafe-enforcement-gap.md` | Macro-injected `unsafe` and safe-fn-with-prose-contract surface; correct the claim, then gate it |
| `elf-gates-fail-closed.md` | The post-link ELF gates pass when they verify nothing; self-tests for every gate |
| `deferred-work.md` | Per-CPU work list so RCU reclaim stops depending on CPU 0 being idle |
| `lockdep-effectiveness.md` | The lock-order validator exhausts its class table during mm init and turns itself off |
| `kernel-teardown-model.md` | Spike: `panic=unwind` and cross-CPU abandon contradict each other; invariant I8 is the tax |
| `driver-framework-base.html` | Driver-framework base: unified Bus model, platform/ACPI registry, deferred-probe/hotplug/unbind |
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
