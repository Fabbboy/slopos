<p align="center">
  <img src="https://img.shields.io/badge/status-it%20boots%20(sometimes)-brightgreen?style=for-the-badge" />
  <img src="https://img.shields.io/badge/vibes-immaculate-blueviolet?style=for-the-badge" />
  <img src="https://img.shields.io/badge/stability-the%20wheel%20decides-orange?style=for-the-badge" />
</p>

<p align="center">
  <img src="assets/logo.png" alt="SlopOS" width="600" />
</p>

<p align="center">
  <i>Three kernel wizards shipwrecked on the island of Sloptopia.<br/>
  Armed with Rust, mass AI token consumption, and zero fear of <code>unsafe</code>,<br/>
  they built an operating system that boots—when the Wheel of Fate allows it.</i>
</p>

<p align="center">
  <b>Win the spin → enter the shell.<br/>
  Lose → reboot and try again.<br/>
  The house always wins. Eventually.</b>
</p>

---

<br/>

## Get It Running

> **You need:** QEMU, xorriso, mkfs.ext2, [`just`](https://github.com/casey/just), and mass skill issue tolerance

```bash
# macOS
brew install qemu xorriso e2fsprogs just

# Debian/Ubuntu
sudo apt install qemu-system-x86 xorriso e2fsprogs
cargo install just  # or: https://github.com/casey/just#installation

# Arch (btw)
sudo pacman -S qemu-full xorriso e2fsprogs just

# Then:
just setup          # installs rust nightly
just boot           # spins the wheel
```

> **macOS Note:** The Cocoa display backend is automatically detected and used. If you see display errors, run `qemu-system-x86_64 -display help` to check available backends.

<br/>

|  | Command | What it does |
|:--:|---------|--------------|
| | `just boot` | Boot with display window |
| | `just boot-headless` | Headless boot (serial only) |
| | `just boot-log` | Boot with timeout, saves to `test_output.log` |
| | `just test` | Run the test harness |
| | `just --list` | Show all available recipes |

<details>
<summary><b>Advanced Options</b></summary>

```bash
QEMU_DISPLAY=cocoa just boot           # Force Cocoa (macOS default)
QEMU_DISPLAY=sdl just boot             # Force SDL (if installed)
just show-qemu-resolution              # Show detected framebuffer mode
QEMU_FB_AUTO=0 just boot               # Disable auto-detection, use defaults
QEMU_FB_WIDTH=2560 QEMU_FB_HEIGHT=1440 just boot  # Manual override
QEMU_FB_AUTO_POLICY=max just boot      # Multi-monitor: pick largest display
QEMU_FB_AUTO_OUTPUT=DP-1 just boot     # Multi-monitor: pin specific output
DEBUG=1 just boot                      # Debug logging
just boot-log video=1                  # Timed boot with display window
just ports=7777 boot                   # Expose guest:7777 on host:7777
just ports=7777,8080 boot              # Expose multiple guest ports
```

**Note:** On macOS, GTK is not available. The justfile automatically uses Cocoa display.

</details>

<br/>

---

<br/>

## What's Inside

```
                          ┌─────────────────────────────────────┐
                          │            USERLAND (Ring 3)        │
                          │  ┌─────────┐ ┌────────┐ ┌─────────┐ │
                          │  │  Shell  │ │Roulette│ │Composit.│ │
                          │  └────┬────┘ └───┬────┘ └────┬────┘ │
                          └───────┼──────────┼──────────┼───────┘
                                  │ SYSCALL  │          │
                          ┌───────▼──────────▼──────────▼───────┐
                          │             KERNEL (Ring 0)         │
                          │  ┌────────┐ ┌────────┐ ┌──────────┐ │
                          │  │ Sched  │ │   MM   │ │  Video   │ │
                          │  └────────┘ └────────┘ └──────────┘ │
                          │  ┌────────┐ ┌────────┐ ┌──────────┐ │
                          │  │  VirtIO│ │  ext2  │ │  PS/2    │ │
                          │  └────────┘ └────────┘ └──────────┘ │
                          └─────────────────────────────────────┘
```

<br/>

| | Feature |
|:--:|---------|
| | Buddy allocator + demand paging |
| | Ring 0/3 with proper TSS isolation |
| | Preemptive scheduler |
| | SYSCALL/SYSRET fast path |
| | IOAPIC + LAPIC interrupts |
| | PS/2 keyboard & mouse |
| | ext2 on VirtIO block |
| | Framebuffer graphics |
| | The Wheel of Fate + W/L currency |

<br/>

---

<br/>

## Actually, The Slop Is Proven

Under the goofy exterior, SlopOS is a **framekernel**: every line of `unsafe`
in the entire kernel is confined to one trusted crate, `slopos-ostd`, held
under a **≤1% TCB budget**. Every other crate is `#![forbid(unsafe_code)]`,
so the compiler itself refuses to let unsafety leak out of the trusted core.

The load-bearing invariants of that core are **machine-checked with
[Verus](https://github.com/verus-lang/verus)**, an SMT-backed proof system
for Rust. Three subsystems on the memory-safety critical path carry formal
proofs:

- **Frame reference counts** — no double-free, no use-after-free.
- **Slab slot lifetimes** — a slot can never outlive the slab it came from.
- **Page-table mutation** — every walk stays well-formed; user mappings
  can't reach into sensitive kernel memory.

These are the spots where a bug would be genuine undefined behaviour rather
than a typed error, so they are proven instead of merely tested. The proofs
run in CI on every change and reproduce locally with `just verify`.

<br/>

---

<br/>

<p align="center">
  <sub>
    <i>"still no progress but ai said it works soo it has t be working :)"</i><br/>
    — from the sacred commit logs
  </sub>
</p>

<p align="center">
  <b>GPL-3.0-only</b>
</p>
