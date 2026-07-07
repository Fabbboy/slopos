# Kernel-Level Microtransactions — Task Plan

## Goal

Introduce a kernel-level microtransaction layer that **spends** W/L currency to
unlock kernel behaviour, built on top of the existing Wheel of Fate ledger
(`slopos-ostd/src/wl_currency.rs`). The layer grows in three stages:

1. **Phase 1 — Pay-to-boot / "on-boot W/L buy-in".** Poker semantics: you bring
   your chips to the table on the boot medium. At boot the kernel reads a W/L
   **buy-in** off the USB (a Limine module); if it's below the table minimum
   (`BOOT_THRESHOLD`) you don't get a seat — the kernel **refuses to boot**.
   This is the first order of business and the load-bearing proof that
   microtransactions can gate kernel behaviour.
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

### The persistence problem (solved by the buy-in)

`kernel_main_impl()` calls `wl_currency::reset()` unconditionally at the top
(`boot/src/early_init.rs:622`), zeroing the balance every boot. A boot gate that
reads a freshly-reset balance always sees `0`, so either the gate is trivially
unbootable or trivially bypassed. The balance has to come from *somewhere* the
kernel didn't just zero.

**Chosen mechanism — the on-boot buy-in (poker model).** Your chips live on the
boot medium (USB). At boot the kernel reads a **buy-in record** shipped as a
Limine module and seeds the ledger from it. The boot medium *is* the
persistence: no fs sector or UEFI-var backend is required for Phase 1. This also
matches the precedent already in the tree — the initramfs is a Limine module
read via `boot/src/limine_protocol.rs:382` (`initramfs()`), and the modules
response (`MODULES_REQUEST`) can carry more than one module.

Workstream 1.1 — buy-in reader:
1. Define the buy-in record format in `slopos-ostd` (the only crate that may
   own the parsing/`unsafe` if any): magic, version, `i64` chip count, and an
   integrity tag (see anti-cheat below). Fixed-size, little-endian, no alloc.
2. Add a `wl.chips` (name TBD) Limine module entry to `limine.conf` and a
   reader in `boot/src/limine_protocol.rs` that mirrors `initramfs()`:
   `fn buyin() -> Option<&'static [u8]>` finding the module by
   `cmdline() == "wl-buyin"`. The crate stays `#![forbid(unsafe_code)]` — the
   `limine` crate owns the `File::data` unsafe, exactly as `initramfs()` does.
3. Replace the unconditional `wl_currency::reset()` at `early_init.rs:622` with
   a **seed-from-buy-in** path: parse the module → `wl_currency` seed; absent or
   invalid module → seed `0` (which, with the gate on, means "no chips, no
   seat"). `reset()` stays available for tests and factory-reset.
4. *(Optional, later)* a `wl_currency::cash_out()` that writes the ending
   balance back so a session's winnings persist to the medium — only feasible on
   a writable medium (real USB), not the read-only Limine module mapping. Defer
   until a writable backend exists; for Phase 1 the buy-in is read-only chips.

### Anti-cheat: the buy-in must be verifiable ("counterfeit chips")

A file on a USB is trivially editable — nothing stops a user from writing
`chips = 1_000_000_000` and sitting at any table for free. In poker terms,
that's bringing counterfeit chips. The buy-in record therefore carries an
integrity tag the kernel verifies before seeding:
- minimum: a keyed MAC / checksum over `(magic, version, chips)` with a key
  baked into the kernel image (obfuscation-grade, not real crypto — fine for the
  joke, but call it out as such);
- a tampered or unverifiable record fails **closed** to seed `0`, never to a
  bypass or to the raw attacker-supplied number.
This forgery surface is a CVSS candidate (currency forgery → privilege/feature
escalation under Phase 2) and must be in the Phase 1 security sweep.

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
- buy-in record parses and seeds the ledger with the carried chip count;
- valid integrity tag accepted; tampered tag rejected → seeds `0`, not the
  attacker number;
- `can_afford_boot()` true/false either side of `BOOT_THRESHOLD`;
- absent/corrupted buy-in module fails closed to seed `0`, not to a bypass;
- gate is disabled under `tests=on`.

**Phase 1 exit criteria:** a dev image whose `wl-buyin` module carries chips
below `BOOT_THRESHOLD` (and `wl.boot_gate=on`) refuses to boot and halts with
the notice; an image whose buy-in is above threshold boots normally; a
hand-edited (forged) buy-in is rejected and treated as `0` chips; `just test` is
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
- **Re-buy (mock IAP):** a single-use "receipt" — either a cmdline
  `wl.topup=<amount>` or a second Limine module — that credits once and is
  consumed (replay-protected nonce → CVSS candidate). This is the poker re-buy:
  bring more chips to the table. The natural sibling of the Phase 1 buy-in, and
  the QEMU stand-in for an app-store purchase. Reuses the same verifiable-record
  + fail-closed discipline as the buy-in.

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

## Open decisions

- [ ] Buy-in module name (`wl-buyin` vs `wl.chips`) and exact record layout.
- [ ] Integrity-tag scheme (keyed checksum vs MAC) and where the key lives —
      remembering it's obfuscation-grade, not real anti-cheat.
- [ ] `BOOT_THRESHOLD` (table minimum) value and the dev/test seed buy-in.
- [ ] Which Phase 2 feature is the first real entitlement.
- [ ] Phase 3 top-up source ordering and whether real persistence is required
      before top-up ships.

## Touch list (current paths — verify before editing)

- `slopos-ostd/src/wl_currency.rs` — ledger; add buy-in seed, `try_spend`,
  `credit`, `BOOT_THRESHOLD`, `can_afford_boot`; buy-in record parse + verify.
- `slopos-ostd/src/boot_flags.rs` — pattern for a new gate flag if needed.
- `boot/src/limine_protocol.rs:382` — add `buyin()` mirroring `initramfs()`.
- `limine.conf` — declare the `wl-buyin` module entry.
- `boot/src/early_init.rs:622` — swap `reset()` → seed-from-buy-in; add the gate
  boot step; (later) `cash_out()` on shutdown for writable media.
- `sched/src/fate_api.rs` — Phase 3 jackpot credit (reuses sanctioned path).
- `abi/src/syscall/numbers.rs` — new syscall ids + `SYSCALL_TABLE_SIZE`.
- `core/src/syscall/handlers.rs` — new handlers; entitlement registry.
- `userland/src/syscall/`, `userland/src/apps/`, `userland/src/program_registry.rs`
  — wrappers + store app.
- `CVSS.md` — findings from each phase's security sweep.
