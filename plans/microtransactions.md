# Kernel-Level Microtransactions — Task Plan

Status: **proposed** (not started). Working notes; verify paths before relying on them.

## Goal

Introduce a kernel-level microtransaction layer that **spends** W/L currency to
unlock kernel behaviour, built on top of the existing Wheel of Fate ledger
(`slopos-ostd/src/wl_currency.rs`). The layer grows in three stages:

1. **Phase 1 — Pay-to-boot.** If the persisted W/L balance is below a boot
   threshold, the kernel **refuses to boot**. This is the first order of
   business and the load-bearing proof that microtransactions can gate kernel
   behaviour.
2. **Phase 2 — Feature entitlements.** Generalise the spend path into an
   entitlement store: features (extra heap, faster scheduler quantum, IOAPIC
   priority, etc.) cost W/L and are unlocked per-boot or persistently.
3. **Phase 3 — Top-up.** Allow *acquiring* W/L currency (the "buy currency for
   features" direction) through a top-up source, so a starved system can climb
   back over the boot threshold.

## Architectural constraints (do not violate)

- **Unsafe surface.** Only `slopos-ostd` may use `unsafe`. All persistence,
  atomics, and any MMIO-touching code lives in `slopos-ostd`; every other
  kernel crate stays `#![forbid(unsafe_code)]`.
- **W/L mutation boundary.** Per the module doc in `wl_currency.rs`, the balance
  is mutated **only** at syscall boundaries (`SyscallContext::ok()`/`err()`) and
  by `fate_api::fate_apply_outcome`. Microtransactions add exactly **one** new
  sanctioned mutation surface — a debit/credit pair — and it must be as
  low-level and hard-to-misuse as `adjust_balance` already is. Drivers, fs
  internals, and boot steps must not reach in to spend.
- **Allocation discipline.** Any heap use routes through `KBox`/`KVec`/etc.; no
  `alloc::*`.
- **Stack frames** ≤ 2 KiB (`check_stack_sizes.sh`); ledger structs that grow
  must be built in place via `KBox::try_init`.
- **Security ledger.** A boot-gate and a currency-acquisition path are
  attack surface. Each phase ends with a security sweep per `CVSS.md` workflow
  (syscall validation, integer over/underflow on balance math, persistence
  tampering).

---

## Phase 1 — Pay-to-boot (first order of business)

**Outcome:** on boot, if the W/L balance is below `BOOT_THRESHOLD`, the kernel
prints a deadpan "insufficient funds" notice and halts instead of continuing
`kernel_main_impl`.

### The persistence problem (must solve first)

`kernel_main_impl()` calls `wl_currency::reset()` unconditionally at the top
(`boot/src/early_init.rs:622`), zeroing the balance every boot. A boot gate that
reads a freshly-reset balance always sees `0`, so either the gate is trivially
unbootable or trivially bypassed. **The balance must persist across boots before
the gate is meaningful.**

Workstream 1.1 — persisted ledger:
1. Add a W/L persistence backend in `slopos-ostd` (the only crate that may own
   the storage primitive). Minimum viable: a fixed-format record (magic,
   version, `i64` balance, checksum) read at early boot and written at
   shutdown. Candidate stores, simplest first:
   - **a)** a reserved disk sector / file via the existing fs path (durable,
     real microtransaction feel);
   - **b)** a UEFI variable (durable, no fs dependency at gate time);
   - **c)** a Limine/boot-module-supplied seed value on the cmdline
     (`wl.balance=…`) for QEMU/dev (non-durable but unblocks the gate today).
   Start with **(c)** behind a cmdline knob to land the gate, then graduate to
   (a) or (b) for durability. Decide and record the choice in this file.
2. Replace the unconditional `wl_currency::reset()` at `early_init.rs:622` with
   a **load** path: `wl_currency::load_persisted_or(default)`. `reset()` stays
   available for tests and explicit factory-reset.
3. Add `wl_currency::persist()` to the shutdown path (the reliable
   shutdown/reboot flow added in `6c355516`) so earned/spent balance survives.

Workstream 1.2 — the boot gate:
1. New boot threshold + check. Add `pub const BOOT_THRESHOLD: i64` to
   `wl_currency.rs` and a `wl_currency::can_afford_boot() -> bool`.
2. Add a boot step that runs **after** the ledger load but **before** the
   expensive bring-up (place it early in `kernel_main_impl`, after the persisted
   load, before SMP/driver init). On failure: emit the notice via the existing
   `fblog`/serial path and halt (do not `panic!` if a clean halt reads better;
   match the shutdown primitive from `6c355516`).
3. Add a `tests.*`-style cmdline escape hatch so `just test` / CI never gets
   wedged below threshold: e.g. `wl.boot_gate=off` (default `off` under
   `tests=on`, `on` otherwise). The gate must be a no-op for the test ISO or
   every CI run dies at the gate.

Workstream 1.3 — tests (`stest!`):
- balance load round-trips a written record;
- `can_afford_boot()` true/false either side of `BOOT_THRESHOLD`;
- corrupted/zeroed persistence record fails closed to the default seed, not to a
  bypass;
- gate is disabled under `tests=on`.

**Phase 1 exit criteria:** a dev image booted with `wl.balance=5
wl.boot_gate=on` (threshold > 5) refuses to boot and halts with the notice; the
same image with `wl.balance` above threshold boots normally; `just test` is
green (gate off on the test ISO).

---

## Phase 2 — Feature entitlements

**Outcome:** userland (and select kernel knobs) can spend W/L to unlock
features. Built on a sanctioned spend API, surfaced through new syscalls,
fronted by a userland store app.

Workstream 2.1 — spend API in `slopos-ostd`:
1. `wl_currency::try_spend(cost: i64) -> Result<i64, InsufficientFunds>`:
   atomic compare-and-debit (CAS loop on `BALANCE`), saturating, never goes
   negative for a spend. Returns new balance. This is the new sanctioned
   mutation surface alongside `adjust_balance` — document it in the module
   header's boundary section the same way.
2. Guard the balance math against overflow/underflow explicitly (i64 wrap is a
   currency-dup bug → CVSS candidate).

Workstream 2.2 — entitlement registry:
1. New module (in the syscall layer, e.g. `core/src/`, **not** ostd — semantics
   belong above the low-level ledger) holding an entitlement table:
   id, price, scope (per-boot vs persisted), granted-flag.
2. Wire granted entitlements to real effects, starting with one cheap, visible
   feature to prove the loop (candidate: unlock `BOOT_FLAG_ROULETTE_SKIP`
   behaviour, or a larger heap arena). Keep blast radius small.

Workstream 2.3 — syscalls:
1. Add `SYSCALL_WL_BALANCE`, `SYSCALL_WL_PURCHASE(entitlement_id)` to
   `abi/src/syscall/numbers.rs` (next free ids after `161`; bump
   `SYSCALL_TABLE_SIZE`). Mirror the existing roulette syscall plumbing
   (`SYSCALL_ROULETTE` = 4, `_RESULT` = 13, `_DRAW` = 24) end-to-end:
   abi number → `core/src/syscall/handlers.rs` → userland wrapper in
   `userland/src/syscall/`.
2. Validate args at the boundary; failures take the normal `err()` W/L loss.

Workstream 2.4 — userland store app:
- `userland/src/apps/store/` (mirror the `roulette` app structure): list
  entitlements + prices, show balance, purchase. Register in
  `userland/src/program_registry.rs`.

Workstream 2.5 — tests (`stest!` + `utest!`): spend success/insufficient-funds;
double-purchase idempotency; per-boot entitlement does not survive a simulated
reset; persisted one does.

**Phase 2 exit criteria:** from the store app, a user with sufficient balance
buys an entitlement, observes balance debited and the feature active; an
under-funded purchase fails cleanly with balance unchanged.

---

## Phase 3 — Top-up (buy W/L currency for features)

**Outcome:** a starved system can acquire W/L, including climbing back over the
Phase 1 boot threshold. This is the satirical "buy currency" direction.

Workstream 3.1 — top-up source (pick one, in joke-coherent order):
- **Wheel of Fate jackpot:** extend `fate_api` so a rare spin outcome credits a
  large W/L sum (reuses the sanctioned `fate_apply_outcome` mutation path — no
  new surface).
- **Grind credit:** N successful syscalls already earn `WL_DELTA` each; expose a
  "claim daily bonus" that credits a lump sum, rate-limited.
- **Mock IAP:** a cmdline/boot-module "receipt" (`wl.topup=<amount>`) that
  credits once and is consumed (single-use token to prevent replay → CVSS
  candidate). The QEMU stand-in for an app-store purchase.

Workstream 3.2 — credit API:
- `wl_currency::credit(amount: i64)` (saturating add, overflow-guarded) as the
  acquisition counterpart to `try_spend`. Document both in the boundary section.

Workstream 3.3 — anti-abuse / security:
- top-up receipts are single-use (consume-on-apply, persisted nonce);
- no path lets userland credit itself arbitrarily without going through a
  rate-limited / fate-gated / receipt-gated source;
- full security sweep: replay, persistence tampering, integer overflow on
  credit, and the interaction with the Phase 1 boot gate (cannot mint your way
  past the gate without a valid receipt).

Workstream 3.4 — tests: receipt applied once then rejected on replay; credit
overflow saturates; a below-threshold system + valid top-up receipt boots on the
next cycle.

**Phase 3 exit criteria:** a system parked below `BOOT_THRESHOLD` (Phase 1
refuses boot) can apply a valid top-up receipt and boot on the subsequent cycle;
replaying the receipt is rejected.

---

## Open decisions to record here as they're made

- [ ] Persistence backend for Phase 1 (fs sector / UEFI var / cmdline seed).
- [ ] `BOOT_THRESHOLD` value and default seed balance for a fresh install.
- [ ] Which Phase 2 feature is the first real entitlement.
- [ ] Phase 3 top-up source ordering and whether real persistence is required
      before top-up ships.

## Touch list (current paths — verify before editing)

- `slopos-ostd/src/wl_currency.rs` — ledger; add persist/load, `try_spend`,
  `credit`, `BOOT_THRESHOLD`, `can_afford_boot`.
- `slopos-ostd/src/boot_flags.rs` — pattern for a new gate flag if needed.
- `boot/src/early_init.rs:622` — swap `reset()` → load; add the gate boot step;
  add `persist()` to shutdown.
- `sched/src/fate_api.rs` — Phase 3 jackpot credit (reuses sanctioned path).
- `abi/src/syscall/numbers.rs` — new syscall ids + `SYSCALL_TABLE_SIZE`.
- `core/src/syscall/handlers.rs` — new handlers; entitlement registry.
- `userland/src/syscall/`, `userland/src/apps/`, `userland/src/program_registry.rs`
  — wrappers + store app.
- `CVSS.md` — findings from each phase's security sweep.
