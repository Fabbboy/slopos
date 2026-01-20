<p align="center">
  <img src="https://img.shields.io/badge/status-it%20boots%20(sometimes)-brightgreen?style=for-the-badge" />
  <img src="https://img.shields.io/badge/vibes-immaculate-blueviolet?style=for-the-badge" />
  <img src="https://img.shields.io/badge/stability-the%20wheel%20decides-orange?style=for-the-badge" />
</p>

<h1 align="center">SlopOS</h1>

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

> **You need:** QEMU, xorriso, mkfs.ext2, and mass skill issue tolerance

```bash
# macOS
brew install qemu xorriso e2fsprogs

# Debian/Ubuntu
sudo apt install qemu-system-x86 xorriso e2fsprogs

# Arch (btw)
sudo pacman -S qemu-full xorriso e2fsprogs

# Then:
make setup          # installs rust nightly
make boot VIDEO=1   # spins the wheel
```

> **macOS Note:** The Cocoa display backend is automatically detected and used. If you see display errors, run `qemu-system-x86_64 -display help` to check available backends.

<br/>

|  | Command | What it does |
|:--:|---------|--------------|
| | `make boot VIDEO=1` | Boot with display window |
| | `make boot` | Headless boot (serial only) |
| | `make boot-log` | Boot with timeout, saves to `test_output.log` |
| | `make test` | Run the test harness |

<details>
<summary><b>Advanced Options</b></summary>

```bash
VIRGL=1 VIDEO=1 make boot             # GPU acceleration (not supported on macOS Cocoa)
QEMU_DISPLAY=cocoa make boot VIDEO=1  # Force Cocoa (macOS default)
QEMU_DISPLAY=sdl make boot VIDEO=1    # Force SDL (if installed)
DEBUG=1 make boot VIDEO=1             # Debug logging
```

**Note:** On macOS, GTK is not available. The Makefile automatically uses Cocoa display.

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

## Project Layout

```
slopos/
├── boot/       → GDT, IDT, TSS, early init, SYSCALL MSRs
├── core/       → scheduler, syscall handlers, task management  
├── mm/         → physical frames, virtual memory, ELF loader
├── drivers/    → PIT, PS/2, IOAPIC, VirtIO, PCI enumeration
├── video/      → framebuffer, graphics primitives, roulette wheel
├── fs/         → ext2 implementation
├── userland/   → shell, compositor, roulette, file manager
├── kernel/     → main entry point
└── lore/       → the sacred chronicles (worth reading)
```

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
