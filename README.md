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
# Debian/Ubuntu
sudo apt install qemu-system-x86 xorriso e2fsprogs

# Arch (btw)
sudo pacman -S qemu-full xorriso e2fsprogs

# Then:
make setup          # installs rust nightly
make boot VIDEO=1   # spins the wheel
```

<br/>

## Line Count (scc)

You can install scc by using the standard Go toolchain (Go >= 1.25).

```bash
go install github.com/boyter/scc/v3@latest
```

<!-- scc-table-start -->
```text
───────────────────────────────────────────────────────────────────────────────
Language            Files       Lines    Blanks  Comments       Code Complexity
───────────────────────────────────────────────────────────────────────────────
Rust                  221      59,878     8,037     5,542     46,299      7,180
C Header              138       9,516     1,990     1,089      6,437        217
C++                   114      21,791     3,381     1,362     17,048      2,834
Markdown               21       4,649     1,216         0      3,433          0
TOML                   14         362        56         1        305          1
JSON                    5         621         2         0        619          0
Assembly                3         889       151         0        738         21
JavaScript              2         215        18         2        195         10
Meson                   2          73         9         0         64          0
Python                  2         311        56        14        241         41
Shell                   2          93        18         4         71          9
INI                     1          18         4         0         14          0
License                 1         674       121         0        553          0
Makefile                1         449        27         1        421         60
Plain Text              1           4         0         0          4          0
YAML                    1          90        18         0         72          0
───────────────────────────────────────────────────────────────────────────────
Total                 529      99,633    15,104     8,015     76,514     10,373
───────────────────────────────────────────────────────────────────────────────
Estimated Cost to Develop (organic) $2,567,575
Estimated Schedule Effort (organic) 19.68 months
Estimated People Required (organic) 11.59
───────────────────────────────────────────────────────────────────────────────
Processed 3164202 bytes, 3.164 megabytes (SI)
───────────────────────────────────────────────────────────────────────────────
```
<!-- scc-table-end -->

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
VIRGL=1 VIDEO=1 make boot         # GPU acceleration
QEMU_DISPLAY=gtk make boot VIDEO=1    # Force GTK
QEMU_DISPLAY=sdl make boot VIDEO=1    # Force SDL
DEBUG=1 make boot VIDEO=1             # Debug logging
```

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
