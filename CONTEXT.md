# CONTEXT — interrupt-driven touchpad (WIP handoff)

> Temporary handoff doc. Delete once the work lands. Branch: `feat/touchpad-interrupt-driven`.

## Goal

Make the I²C-HID touchpad **interrupt-driven** (it was polling, which amplified a
latent scheduler race). Target machine: **Lenovo, Alder Lake-P**, Focaltech
touchpad `FTCS0038` (`2808:0101`) on `i2c-1` (LPSS DesignWare I²C #1, addr 0x38),
GpioInt on PCH pinctrl `INTC1055`.

## Current status: NOT working yet — one blocker left

Boot log on hardware shows:
```
touchpad: pinctrl setup failed for GpioInt 17; polling
```
The GpioInt pad **resolves to 17, should be 177**. Everything downstream (pinctrl
MMIO, IRQ cascade, waker thread, poll fallback) is built and compiles; the only
thing wrong is the pad number coming out of ACPI.

### Why 17 (the exact blocker)

The pad number is computed by firmware in `_INI`: `INT1 = GNUM(0x09080011)`, where
(verified against the decompiled DSDT):
- `GNUM(enc) = GNMB(enc) + GINF(GGRP(enc), 8)`
- `GNMB = enc & 0xFFFF = 17`, `GGRP = (enc>>16)&0xFF = 8`
- `GINF(g,f) = DerefOf(DerefOf(GPCL[g])[f])` over static `Name(GPCL, Package)`
- `GPCL[8][8] = 0xA0 = 160` → **`GNUM = 17 + 160 = 177`** ✓ (decode is correct)

The interpreter returns **17**, i.e. `GINF(8,8) = 0` → the `GPCL` package isn't
resolving (or the nested `Index` fails) on the real table.

### Known: it's NOT these
- Not stale build, not SSDT cross-blob (`GNUM/GINF/GPCL/GNMB/GGRP` are all in
  `dsdt` only — confirmed via `acpidump`).
- Decode is correct (`GPCL[8][8]=160` confirmed from the DSDT).
- Found one real bug + fixed it: `GPCL` is **both** a `Name(Package)` (in `_SB`)
  and an unrelated `Method(GPCL,0)` (in a PCI scope). The flat namespace collided
  them and `eval_name` called the method. Fix: a `Package`/`Buffer` global `Name`
  now preempts a same-named method (`acpi/src/aml/interp.rs::eval_name`). A
  synthetic test reproduces this collision and passes (`test_aml_method_package_eval`
  in `drivers/src/touchpad/mod.rs`) — **but the real `GPCL` still gives 0**, so
  there's a second cause not yet identified.

## NEXT STEP (do this first)

A **release ISO with `tp.debug` is already built** at `builddir/slop.iso`
(see "build" below to rebuild). Flash it, boot, and read:
```
cat /dev/kmsg | grep "aml:"
```
Two diagnostic lines (gated by `tp.debug`, temporary):
```
aml: idx methods=.. bodies=.. names=.. | GPCL=.. GNUM=.. GINF=.. GNMB=.. GGRP=..
aml: GPCL_pkg_len=Some(18)? GINF(8,8)=Some(160)? GNUM(0x09080011)=Some(177)?  (expect 18/160/177)
```
Interpret:
- `GPCL=false` → global `Name` index (`mod.rs` `IndexVisitor::name` /
  `Index.names`) isn't capturing the scoped `Name(GPCL)`. Check `walk_name`
  positions / scope descent.
- `GPCL_pkg_len=None` but `GPCL=true` → resolves but `eval_package` doesn't parse
  the real (18×9, multi-byte `PkgLength`) package. Check `eval_package` /
  `pkg_length`.
- `GPCL_pkg_len=Some(18)` but `GINF(8,8)=Some(0)` → `Index`/`DerefOf` nesting on
  real data (`eval_index`).
- All as expected but pin still 17 → the `_INI` buffer patch (`CreateWordField
  SBFG@0x17` overlay store) or `parse_gpio_int` read offset.

The debug lines + helpers (`resolve_pkg_len_for_test`, `invoke_for_test`,
`eval_method_u64_for_test`) are **temporary** — remove before final landing.

## Architecture of what's built

- **`drivers/src/pinctrl.rs`** (new) — minimal Intel PCH pinctrl. TGL-LP community-1
  layout; `SBREG_BAR=0xfd000000 + COMMUNITY1_OFFSET=0x6d0000` (documented SoC
  constants — P2SB is firmware-hidden, `SBRG` is in an OperationRegion the interp
  can't resolve, so they're irreducible; validated against silicon at runtime via
  PADBAR/PADCFG0). `init_for_pad(line,edge,active_low)` maps + configures pad,
  `pad_irq_mask/unmask`, `service_pending` (IRQ-side).
- **`drivers/src/touchpad/mod.rs`** — `try_interrupt_mode`: resolve pad via ACPI
  GpioInt → `pinctrl::init_for_pad` → `register_cascade` (IO-APIC GSI 14, level,
  active-low) → `spawn_kernel_io!("touchpad-irq")` parking on `TouchpadWaker`
  (IRQ-armed `WaitQueue`). Falls back to descheduling poll (`Deadline::AtMs(8)`)
  if anything fails or `tp.poll`. `GPIO_DEFAULT_GSI=14`.
- **`acpi/src/aml/`** — the interpreter was extended (the sound fix) to evaluate
  resource methods: method calls + call frames (`Arg`/`Local`), `Add`/`And`/shifts
  with Target, `Package`/`Index`/`DerefOf`, global `Name(Package)` resolution.
  `mod.rs` index gained `method_bodies` + `names`; `object.rs` gained
  `AmlVal::Package`. `AcpiI2cHid` carries `gpio_int_*`.
- **`boot/src/boot_drivers.rs`** — `tp.poll` cmdline knob; passes `force_poll`.

## Hardware facts (verified, machine-specific)
| | |
|---|---|
| Pad | `ISH_GP_4`, pinctrl pin **116**, GpioInt line **177**, Level/ActiveLow |
| Community 1 MMIO | `0xfd6d0000` (= `SBREG_BAR 0xfd000000 + 0x6d0000`), 64 KiB |
| gpp | `INTEL_GPP(2, 99,119, gpio_base 160)`, gpp_offset 17, padno 49 |
| Regs | PADBAR@0x0c, GPI_IS@0x100, GPI_IE@0x120, PADCFG0 (RXEVCFG[26:25], RXINV b23, GPIROUTIOXAPIC b20, GPIORXDIS b9) |
| Cascade | IO-APIC **GSI 14**, level, active-low |

Decompiled DSDT for reference: `~/Downloads/d.dsl` (or re-dump: `sudo acpidump -b`
in /tmp, `iasl -d dsdt.dat`). Key lines: `Name(GPCL` 12153, `Method(GINF` 12414,
`Method(GNUM` 12434, `Method(GPCL` 5480 (the collision), touchpad device ~98099.

## Build / test / flash
```
just build                                  # kernel + framekernel gates
just test '*touchpad*'                       # pinctrl + AML-eval stests (positional filter!)
just test                                    # full suite (2576 pass baseline)
BOOT_CMDLINE="tests=off roulette=skip tp.debug=on" KERNEL_RELEASE=1 just iso   # → builddir/slop.iso
```
QEMU can't test this path (no INTC1055 / I²C-HID) — bare-metal flash only.

## Unrelated WIP also in this branch
- **Terminal scroll/size** (`terminal-core/src/input.rs`, `userland/.../terminal/mod.rs`):
  temporary `]`/`\` scrollback keys + window sized to 80% of display — a debugging
  aid so the kernel log is readable on bare metal. Mark temporary; revert when done.
- **Scheduler dual-representation race** (separate, unfixed): the original reason
  the busy-wait poll thread was harmful. Interrupt-driving the touchpad removes the
  *amplifier*, not the latent lost-enqueue race. See the auto-memory note
  `project_sched_dual_representation_gap`.

Plan file: `/home/lon60/.claude/plans/functional-noodling-llama.md`.
