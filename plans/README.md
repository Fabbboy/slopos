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
| `microtransactions.md` | Kernel microtransaction layer on W/L currency; Phase 1 = pay-to-boot gate |
| `persistent-storage.md` | Files that survive a reboot: per-inode durability, the ext2 write surface, crash consistency, the disk as `/` |
| `usb-xhci.md` | USB/xHCI stack: host controller, enumeration, HID input, mass storage |

The driver-framework base has **landed and its plan is retired**. One `Bus` trait
(`drivers/src/driver_core/bus.rs`) and one generic `probe_bus` matchmaker drive both the
PCI (`.driver_registry`) and platform/ACPI (`.platform_driver_registry`) registries; each
keeps its own `#[repr(C)]` entry type and enumerator, and shares the binding protocol,
the devres claim table and `BoundDevice<B>`. Every device driver binds declaratively —
`boot_init!` carries no device drivers. Read the code rather than a document:
`driver_core::bus` for the model, `drivers/src/pci.rs` and `drivers/src/platform_bus/` for
the two instances, and `drivers/src/tests/bus_generic.rs` for what the protocol guarantees.
Deferred-probe-to-fixpoint, unbind and hotplug were the plan's Phase 2 and are deliberately
not planned in the mid term; the `Deferred` outcome and the Binding-above-Devres slot order
are the seams they would build on.

The authority model has **landed and its plan is retired**. Authority is a flat
per-capability mask whose classification is total by compile-time construction:
`define_syscall!` takes a mandatory `cap(X)` clause and emits it into the dispatch table
through the handler, so a `const` histogram in `core/src/syscall/handlers.rs` asserts both
totality over all 177 slots and each capability's recorded entry-point count. Read the code
rather than a document: `slopos_ostd::authority` for the vocabulary and the witness,
`core/src/exec/grants.rs` for where authority enters, `slopos_ostd::seat` for the display
and input seats, and `verification/proofs/authority.rs` for the four machine-checked
obligations. `scripts/check_authority_reachability.sh` is what catches an unprivileged
syscall reaching a power primitive two calls away, which a slot-level gate cannot see.

## When To Promote A Plan

Promote durable content into the public docs repo when it describes a stable
public architecture, ABI, verification contract, or developer workflow.
