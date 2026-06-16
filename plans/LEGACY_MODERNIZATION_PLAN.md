# SlopOS Legacy Modernization Plan

> **Status**: In Progress — Phase 0 (Timer Modernization) **complete**, Phase 0E (PIT Deprecation) complete, Phase 1 (FPU/SIMD State Modernization) **complete**, Phase 2 (Spinlock Modernization) **complete** (2C MCS deferred, `spin` crate fully removed), Phase 3A (PCI Capability List Parsing) **complete**, Phase 3B (MSI Support) **complete**, Phase 3C (MSI-X Support) **complete**, Phase 3D (VirtIO MSI-X Integration) **complete**, Phase 3E (Interrupt-Driven VirtIO Completion) **complete**, Phase 4A (ACPI MCFG Table Parsing) **complete**, Phase 4B (ECAM MMIO Config Access) **complete**, Phase 4C (Extended Config Space Usage) **complete**, Phase 4D (ECAM-Only Long-Term Migration) **complete**, Phase 5A (TCP State Machine) **complete**, Phase 5B (TCP Data Transfer) **complete**, Phase 5C (Socket Abstraction Layer) **complete**, Phase 5D (Async Network I/O & NAPI-Style Completion) **complete**, Phase 5E (UDP Datagram Socket Completion) **complete**
> **Target**: Replace all legacy/outdated hardware interfaces and patterns with modern equivalents as SlopOS approaches MVP
> **Scope**: Timers, FPU state, interrupts, spinlocks, PCI, networking, and beyond

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Phase 0: Timer Modernization](#3-phase-0-timer-modernization)
4. [Phase 1: FPU / SIMD State Modernization](#4-phase-1-fpu--simd-state-modernization)
5. [Phase 2: Spinlock Modernization](#5-phase-2-spinlock-modernization)
6. [Phase 3: MSI/MSI-X Interrupt Routing](#6-phase-3-msimsi-x-interrupt-routing)
7. [Phase 4: PCIe ECAM Configuration Space](#7-phase-4-pcie-ecam-configuration-space)
8. [Phase 5: Network Stack Completion](#8-phase-5-network-stack-completion)
9. [Phase 6: PCID / TLB Optimization](#9-phase-6-pcid--tlb-optimization)
10. [Phase 7: Long-Horizon Hardware](#10-phase-7-long-horizon-hardware)
11. [Dependency Graph](#11-dependency-graph)
12. [Blocked Features Reference](#12-blocked-features-reference)

---

## 1. Executive Summary

SlopOS has a remarkably modern foundation — Limine boot, APIC/IOAPIC interrupts, SYSCALL/SYSRET, buddy+slab allocators, higher-half kernel, 4-level paging with NX, SMP TLB shootdown, and proper Ring 0/3 separation. The legacy 8259 PIC has been removed from driver/client ownership; OSTD retains only the ACPI-required boot-time init-disable primitive.

However, several subsystems still rely on hardware designs from the 1980s–1990s:

| Legacy Component | Era | Modern Replacement | Impact |
|---|---|---|---|
| **PIT (8254)** | 1981 | HPET + LAPIC timer | Scheduler precision, power |
| **FXSAVE/FXRSTOR** | 1999 (Pentium III) | XSAVE/XRSTOR | Blocks AVX, wastes state space |
| **Test-and-set spinlock** | 1970s concept | Ticket / queued locks | SMP starvation, cache thrashing |
| **PCI port I/O (0xCF8/0xCFC)** | 1992 (PCI 2.0) | ECAM (MMIO) | 256B limit, slow, no PCIe |
| **Legacy IRQ lines** | 1981 (ISA) | MSI/MSI-X | Shared IRQs, no per-device vectors |
| **No TCP** | — | TCP state machine | No real networking |
| **PCID detected but unused** | — | Active PCID TLB tagging | Unnecessary TLB flushes |

This plan has **8 phases**, ordered by impact and dependency:

- **Phase 0**: Timer modernization (HPET + LAPIC timer replace PIT) — **highest impact**
- **Phase 1**: XSAVE/XRSTOR replaces FXSAVE — unlocks AVX
- **Phase 2**: Ticket/queued spinlocks — fixes SMP fairness
- **Phase 3**: MSI/MSI-X for VirtIO and PCI devices
- **Phase 4**: PCIe ECAM configuration space via MCFG
- **Phase 5**: Network stack completion (TCP + UDP sockets + DNS)
- **Phase 6**: PCID-accelerated TLB management
- **Phase 7**: Long-horizon items (USB/xHCI, virtio-gpu, RTC)

Phases 0–2 are self-contained kernel changes. Phase 3–4 build on each other. Phase 5 is independent. Phase 6–7 are stretch goals.

---

## 2. Current State Assessment

### What's Already Modern

| Component | Implementation | Files |
|---|---|---|
| Boot | Limine v8.7.0 | `limine.conf`, `boot/src/limine_protocol.rs` |
| Interrupts | LAPIC + IOAPIC (PIC driver removed; OSTD boot-disables dual 8259 when ACPI requires it) | `drivers/src/apic/`, `drivers/src/ioapic/`, `slopos-ostd/src/io/pic.rs` |
| Syscalls | SYSCALL/SYSRET | `core/src/syscall/`, `boot/src/gdt.rs` |
| Memory | Buddy + slab + per-CPU cache | `mm/src/page_alloc.rs`, `mm/src/kernel_heap.rs` |
| Paging | 4-level, NX, higher-half | `mm/src/paging/`, `link.ld` |
| TLB | SMP shootdown via IPI | `mm/src/tlb.rs` |
| Context | Full register save + FPU | `core/context_switch.s` |
| VirtIO | Block + Network drivers | `drivers/src/virtio_blk.rs`, `drivers/src/virtio_net.rs` |
| ACPI | RSDP → XSDT → MADT | `acpi/src/` |
| Userland | Ring 3, fork, exec, pipes, shm, futexes | `core/src/`, `userland/src/` |

### What's Legacy

| Component | Current | File(s) | Why It's Legacy |
|---|---|---|---|
| System timer | PIT at 100Hz | `drivers/src/pit.rs` | 1981 chip, imprecise, wastes IRQs |
| ~~FPU save~~ | ~~FXSAVE (512B fixed)~~ | `core/context_switch.s` | **Modernized** — XSAVE/XRSTOR (mandatory), FXSAVE removed |
| ~~Spinlocks~~ | ~~CAS loop, no fairness~~ | `lib/src/spinlock.rs` | **Modernized** — Ticket lock (FIFO fairness), proportional backoff; MCS locks optional stretch |
| ~~PCI config~~ | ~~Port I/O 0xCF8/0xCFC~~ | `drivers/src/pci.rs`, `lib/src/ports.rs` | **Modernized** — ECAM MMIO is mandatory, full 4096-byte config space, legacy port I/O removed |
| IRQ routing | Legacy IOAPIC lines only | `drivers/src/irq.rs` | No MSI/MSI-X, shared IRQ lines |
| Network | UDP/ICMP/ARP only | `drivers/src/net/` | No TCP = no real networking |
| TLB | PCID detected, not used | `mm/src/tlb.rs` | Unnecessary full TLB flushes |
| Input | PS/2 keyboard+mouse | `drivers/src/ps2/` | 1987, but needed for QEMU |
| Serial | UART 16550 on COM1 | `drivers/src/serial.rs` | 1987, but still the standard |

### What Stays (Legacy But Correct)

- **PS/2**: QEMU q35 emulates PS/2 natively. No practical replacement without USB/xHCI (Phase 7). Keep.
- **Serial UART 16550**: Industry standard for kernel debug. Every modern Rust kernel uses it. Keep.
- **8259 PIC lifecycle**: not a driver surface. OSTD owns the boot-only ACPI `PCAT_COMPAT` init-disable path; runtime interrupt routing stays LAPIC + IOAPIC/MSI/MSI-X.

---

## 3. Phase 0: Timer Modernization

> **The single highest-impact change. Replaces the oldest hardware dependency.**
> **Kernel changes required**: Yes — new HPET driver, LAPIC timer scheduler integration
> **Difficulty**: Medium-High
> **Depends on**: Nothing (self-contained)

### Background

The PIT (Programmable Interval Timer, Intel 8254) is a **1981 chip** running at 1.193182 MHz. SlopOS uses it for:
1. Scheduler preemption ticks (`pit_init(100)` → 100Hz)
2. Busy-wait delays (`pit_poll_delay_ms()`)
3. Sleep implementation (`pit_sleep_ms()`)

The LAPIC timer is already initialized in `drivers/src/apic/mod.rs` (lines 192–214) with `init_timer()`, `set_initial_count()`, `timer_set_divisor()`, and periodic mode support. It's just never wired to the scheduler.

**Problem**: The LAPIC timer's frequency is unknown — it runs off the CPU bus clock, which varies per machine. You need a **reference timer** (HPET or PIT) to calibrate it once at boot.

### 0A: HPET Driver

Parse the ACPI HPET table and implement a minimal HPET driver for system time and LAPIC calibration.

- [x] **0A.1** Add HPET table parsing to `acpi/src/hpet.rs`:
  - Find HPET table signature (`"HPET"`) in XSDT
  - Parse base address (MMIO), minimum tick period, comparator count
  - Export `HpetInfo { base_phys: u64, period_fs: u32, num_comparators: u8 }`
- [x] **0A.2** Create `drivers/src/hpet.rs`:
  - Map HPET MMIO registers via `MmioRegion::map()` (same pattern as IOAPIC)
  - Define register offsets: `GENERAL_CAP` (0x00), `GENERAL_CONFIG` (0x10), `MAIN_COUNTER` (0xF0), `TIMER_N_CONFIG` (0x100+0x20*N), `TIMER_N_COMPARATOR` (0x108+0x20*N)
  - Read capability register to get period (femtoseconds per tick) and number of timers
- [x] **0A.3** Implement `hpet::init()`:
  - Disable legacy replacement mode (clear bit 1 of GENERAL_CONFIG)
  - Enable main counter (set bit 0 of GENERAL_CONFIG)
  - Log: `"HPET: base 0x{:x}, period {} fs, {} comparators"`
- [x] **0A.4** Implement `hpet::read_counter() -> u64`:
  - Read MAIN_COUNTER register (64-bit monotonic)
  - This is the primary precision time source
- [x] **0A.5** Implement `hpet::nanoseconds(ticks: u64) -> u64`:
  - Convert ticks to nanoseconds using period from capability register
  - `ns = ticks * period_fs / 1_000_000`
- [x] **0A.6** Implement `hpet::delay_ns(ns: u64)` and `hpet::delay_ms(ms: u32)`:
  - Spin-wait on main counter for the specified duration
  - Replaces `pit_poll_delay_ms()` for calibration and early-boot delays
- [x] **0A.7** Wire into boot sequence (`boot/src/boot_drivers.rs`):
  - Add `BOOT_STEP_HPET_SETUP` after IOAPIC setup
  - Call `hpet::hpet_init()` using ACPI-discovered base address
- [x] **0A.8** Verify: boot prints HPET info, `hpet_read_counter()` returns increasing values, regression test suite passes (10 tests in `drivers/src/hpet_tests.rs`)

### 0B: LAPIC Timer Calibration

Use HPET (or PIT as fallback) to measure the LAPIC timer frequency.

- [x] **0B.1** Implement `calibrate_lapic_timer() -> u64` in `drivers/src/apic/`:
  - Set LAPIC timer to one-shot mode with a large initial count (e.g., 0xFFFF_FFFF)
  - Set divisor to 16 (already used: `LAPIC_TIMER_DIV_16`)
  - Wait exactly 10ms using `hpet_delay_ns(10_000_000)` (or `pit_poll_delay_ms(10)` as fallback)
  - Read remaining count: `elapsed = initial - current_count`
  - Calculate frequency: `freq_hz = elapsed * 100` (since we measured 10ms)
  - Store in static: `LAPIC_TIMER_FREQ_HZ: AtomicU64`
- [x] **0B.2** Implement `lapic_timer_set_periodic_ms(ms: u32)`:
  - Calculate initial count: `count = freq_hz * ms / 1000`
  - Set periodic mode with the scheduler vector
  - This replaces PIT for scheduling ticks
- [x] **0B.3** Call calibration during boot after HPET init:
  - Log: `"APIC: Timer calibrated: {} Hz (via HPET)"` or `"(via PIT fallback)"`
- [x] **0B.4** Verify: calibration produces a reasonable frequency (~62 MHz with div-16 on QEMU); 7 regression tests in `drivers/src/apic_timer_tests.rs` (425 total tests pass)

### 0C: Scheduler Migration to LAPIC Timer

Replace PIT-driven scheduling ticks with LAPIC timer ticks.

- [x] **0C.1** In `boot/src/boot_drivers.rs`, after LAPIC calibration:
  - Call `lapic_timer_set_periodic_ms(10)` (100Hz, same as current PIT)
  - The LAPIC timer interrupt already routes through the IDT; ensure the scheduler tick handler is called
- [x] **0C.2** Update `drivers/src/irq.rs`:
  - The LAPIC timer fires on a local vector (not through IOAPIC)
  - Ensure the timer ISR calls `scheduler_timer_tick()` (same as PIT currently does)
  - Each CPU gets its own LAPIC timer interrupt — no shared IRQ line
- [x] **0C.3** Disable PIT scheduling role:
  - Stop calling `pit_init()` / `pit_enable_irq()` during boot
  - Keep PIT driver code for fallback calibration only
  - Remove PIT IRQ route from IOAPIC setup
- [x] **0C.4** Update `pit_sleep_ms()` callers:
  - Replace with `hpet_delay_ns()` or a new `timer_sleep_ms()` that uses the scheduler's sleep queue
  - Audit all callers: `pit_sleep_ms`, `pit_poll_delay_ms` across the codebase
- [x] **0C.5** Per-CPU LAPIC timer setup for APs:
  - Each AP must calibrate or inherit the BSP's calibrated frequency
  - Call `lapic_timer_set_periodic_ms()` during AP startup in `boot/src/smp.rs`
- [x] **0C.6** Verify: `just test` passes with LAPIC timer driving scheduling. PIT no longer receives IRQs.

### 0D: High-Resolution System Clock

Expose a monotonic nanosecond clock to the kernel and userland.

- [x] **0D.1** Create `lib/src/clock.rs`:
  - `clock_monotonic_ns() -> u64` — reads HPET main counter, converts to nanoseconds
  - `clock_uptime_ms() -> u64` — wraps monotonic, converts to milliseconds
  - Replaces the tick-counting approach in `irq_get_timer_ticks()`
- [x] **0D.2** Update `SYSCALL_GET_TIME_MS` (39) to use `clock_uptime_ms()` instead of PIT tick counting
- [x] **0D.3** Expose `SYSCALL_CLOCK_GETTIME` (new) for nanosecond precision:
  - `rdi` = clock ID (0 = MONOTONIC)
  - `rsi` = pointer to `{ seconds: u64, nanoseconds: u64 }`
- [x] **0D.4** Update userland `time` command and `uptime` to use nanosecond clock for better precision
- [x] **0D.5** Verify: `uptime` shows correct elapsed time, `time ls` shows sub-millisecond precision

### 0E: PIT Deprecation

Reduce PIT to a calibration-only fallback.  HPET + LAPIC timer are now **mandatory**.

- [x] **0E.1** Make HPET + LAPIC timer mandatory at boot:
  - `boot_step_hpet_setup_fn()` panics if HPET unavailable
  - `boot_step_lapic_calibration_fn()` panics if calibration returns 0 Hz
  - `boot_step_lapic_timer_start_fn()` panics if periodic mode fails
  - PIT retained only as LAPIC calibration polled-delay fallback (dead path)
- [x] **0E.2** Remove `pit_enable_irq()` / `pit_disable_irq()` from the default boot path
- [x] **0E.3** Remove PIT IRQ routing — `enable_pit_timer_fallback()` and handler deleted
- [x] **0E.4** Update `TODO.md` to mark the LAPIC timer item as complete
- [x] **0E.5** Add deprecation comment to `drivers/src/pit.rs`:
  ```rust
  //! Legacy PIT (Intel 8254) driver — **calibration-only fallback**.
  //! The HPET + LAPIC timer is the primary timing source since Phase 0.
  ```
- [x] **0E.6** Remove `delay_ms_or_pit_fallback()` from `hpet.rs` — all callers use `delay_ms()`
- [x] **0E.7** Remove PIT fallback from PlatformServices (`boot_impl.rs`)
- [x] **0E.8** Replace `pit_get_frequency()` in `input_event.rs` with HPET-based timestamp

### Phase 0 Gate

- [x] **GATE**: HPET driver discovers and initializes the timer from ACPI
- [x] **GATE**: LAPIC timer calibrated against HPET (or PIT fallback)
- [x] **GATE**: Scheduler runs on LAPIC timer, not PIT
- [x] **GATE**: Each CPU has its own LAPIC timer tick (no shared IRQ)
- [x] **GATE**: `clock_monotonic_ns()` provides nanosecond precision
- [x] **GATE**: PIT no longer receives interrupts in the default boot path
- [x] **GATE**: `just test` passes
- [x] **GATE**: `just boot` boots and schedules correctly

---

## 4. Phase 1: FPU / SIMD State Modernization

> **Unlocks AVX/AVX-512 support and future-proofs context switching.**
> **Kernel changes required**: Yes — context switch assembly, target JSON, task struct
> **Difficulty**: Medium
> **Depends on**: Nothing (self-contained)

### Background

SlopOS currently uses `fxsave64`/`fxrstor64` (6 occurrences in `core/context_switch.s`) to save/restore FPU state. This saves exactly 512 bytes covering x87, MMX, and SSE registers.

The `XSAVE`/`XRSTOR` family (available since Intel Nehalem, 2008) dynamically sizes the save area based on which CPU features are enabled in `XCR0`. This is **required** for AVX (256-bit), AVX-512, AMX, and any future SIMD extensions.

The target JSON (`targets/x86_64-slos.json`) currently disables AVX with `"-mmx,-avx,-avx2"` — partly because FXSAVE can't handle it.

### 1A: XSAVE Feature Detection

- [x] **1A.1** Add CPUID leaf 0x0D detection to `lib/src/cpu/cpuid.rs`:
  - Query `CPUID.0x0D.0:EBX` for XSAVE area size (with current XCR0)
  - Query `CPUID.0x0D.0:ECX` for maximum XSAVE area size (all features)
  - Query `CPUID.0x0D.0:EAX` for supported XCR0 feature bits
  - Export: `xsave_area_size() -> usize`, `xsave_max_size() -> usize`, `xcr0_supported() -> u64`
- [x] **1A.2** Define XCR0 feature bits in `lib/src/cpu/control_regs.rs`:
  - Bit 0: x87 (always set)
  - Bit 1: SSE (MXCSR + XMM0-15)
  - Bit 2: AVX (YMM0-15 upper halves)
  - Bit 5-7: AVX-512 (opmask, ZMM upper, ZMM16-31)
- [x] **1A.3** Implement `xcr0_read() -> u64` and `xcr0_write(val: u64)`:
  - Uses `xgetbv` / `xsetbv` instructions
  - Only callable after `CR4.OSXSAVE` is set
- [x] **1A.4** Detect XSAVE support: `CPUID.1:ECX[bit 26]` (XSAVE) and `CPUID.1:ECX[bit 27]` (OSXSAVE)
  - Also detect XSAVEC (compact format): `CPUID.0x0D.1:EAX[bit 1]`
  - Prefer XSAVEC when available (smaller save area, no gaps)

### 1B: Enable XSAVE in Boot

- [x] **1B.1** During boot (`boot/src/boot_drivers.rs`):
  - Set `CR4.OSXSAVE` (bit 18) — already defined as `CR4_OSXSAVE` in `lib/src/cpu/control_regs.rs`
  - Write XCR0 with desired feature mask: `x87 | SSE` minimum, `| AVX` if supported
  - Log: `"XSAVE: enabled, area size {} bytes, features 0x{:x}"`
- [x] **1B.2** Store the active XSAVE area size in a global static:
  - `XSAVE_AREA_SIZE: AtomicUsize` — queried by task creation code
  - Minimum 512 (FXSAVE compat), typically 832 with AVX, 2688+ with AVX-512
- [x] **1B.3** Each AP must also set `CR4.OSXSAVE` and write `XCR0` during `ap_entry()` in `boot/src/smp.rs`

### 1C: Update Task FPU State

- [x] **1C.1** Replace fixed `FPU_STATE_SIZE = 512` in `core/src/scheduler/task_struct.rs`:
  - Compile-time maximum of 2688 bytes (covers FXSAVE, XSAVE+AVX, XSAVE+AVX-512)
  - Runtime validation via `validate_fpu_state_size()` panics at boot if hardware exceeds the compile-time max
  - `FXSAVE_AREA_SIZE = 512` constant retained for fallback reference
- [x] **1C.2** Update `FpuState::new()`:
  - XSAVE header (bytes 512–575) explicitly zeroed: `XSTATE_BV` = 0 (init values), `XCOMP_BV` = 0
  - Legacy region defaults unchanged: FCW=0x037F, MXCSR=0x1F80
  - XSAVE header constants defined: `XSAVE_HEADER_OFFSET`, `XSTATE_BV_OFFSET`, `XCOMP_BV_OFFSET`
- [x] **1C.3** Ensure 64-byte alignment for XSAVE area (required by hardware):
  - `#[repr(C, align(64))]` on `FpuState` (was `align(16)`)
  - Compile-time assertions: `align_of::<FpuState>() >= 64`, `FPU_STATE_SIZE % align == 0`
  - Assembly `FPU_STATE_OFFSET` unchanged at 0xD0 (compiler padding naturally satisfied)

### 1D: Update Context Switch Assembly

- [x] **1D.1** Replace all `fxsave64` with `xsave64` (or `xsavec`) in `core/context_switch.s`:
  - `xsave64` / `xrstor64` take an EDX:EAX mask specifying which components to save/restore
  - Set `EDX:EAX = XCR0` value (save all enabled components)
  - 6 sites updated (3 save, 3 restore) via `FPU_SAVE`/`FPU_RESTORE` GAS macros
  - `ACTIVE_XCR0` static exposed to assembly via `#[unsafe(no_mangle)]`
  - [x] **1D.2** ~~Implement lazy XSAVE optimization~~ — **Skipped per plan recommendation**:
  - Eager save/restore is simpler and modern CPUs make XSAVE fast
  - Can be revisited as a future optimisation if profiling shows FPU save/restore overhead
  - [x] **1D.3** ~~Fallback path~~ — **FXSAVE fallback intentionally removed**:
  - XSAVE is now a hard boot requirement — `init()` panics if CPUID reports no XSAVE
  - `XSAVE_ENABLED: AtomicBool` removed — `is_enabled()` unconditionally returns `true`
  - `FPU_SAVE`/`FPU_RESTORE` macros use unconditional `xsave64`/`xrstor64` (no branch)
  - Every x86-64 CPU since 2008 supports XSAVE; QEMU always exposes it

### 1E: Update Target JSON

- [x] **1E.1** Update `targets/x86_64-slos.json`:
  - Removed `-avx` and `-avx2` from disabled features, added `+xsave`
  - Kept `-mmx` disabled (MMX is truly legacy and conflicts with x87)
  - New features string: `"-mmx,+xsave"`
  - The kernel itself doesn't emit AVX instructions, but the compiler is no longer forbidden from using them
- [x] **1E.2** Update userland target `targets/x86_64-slos-userland.json`:
  - Enabled AVX/AVX2 and XSAVE for userland: `"-mmx,+xsave,+avx,+avx2"`
  - Userland code can now use `__m256` intrinsics and AVX instructions without #UD

### Phase 1 Gate

- [x] **GATE**: XSAVE area size detected at boot via CPUID
- [x] **GATE**: `CR4.OSXSAVE` set, `XCR0` configured on BSP and all APs
- [x] **GATE**: Context switch uses `xsave64`/`xrstor64` (or `xsavec`/`xrstorc`)
- [x] **GATE**: Task FPU state area sized dynamically (or compile-time max)
- [x] **GATE**: ~~Fallback to FXSAVE if XSAVE unavailable~~ — FXSAVE fallback removed; XSAVE is a hard boot requirement
- [x] **GATE**: AVX no longer disabled in target JSON
- [x] **GATE**: `just test` passes (437 passed, 0 failed — includes 13 XSAVE regression tests)
- [x] **GATE**: Userland programs can use AVX instructions without #UD


### Phase 1 Test Coverage

A dedicated `xsave` test suite (`tests/src/xsave_tests.rs`, 13 tests) was added to guard
against regressions in the XSAVE/FPU/SIMD modernization:

| # | Test | What It Catches |
|---|------|-----------------|
| 1 | `test_xsave_enabled_matches_cpuid` | XSAVE flag out of sync with CPUID |
| 2 | `test_xsave_area_size_sane` | Save-area size below 512 or exceeding CPUID max |
| 3 | `test_xsave_xcr0_mandatory_bits` | Missing x87/SSE bits in active XCR0 |
| 4 | `test_xsave_features_consistency` | XsaveFeatures struct internally inconsistent |
| 5 | `test_cr4_osxsave_set` | CR4.OSXSAVE not set despite XSAVE enabled |
| 6 | `test_xcr0_matches_active` | Live XCR0 register ≠ xsave::active_xcr0() |
| 7 | `test_xcr0_avx_consistent` | AVX enabled in XCR0 but unsupported by CPU |
| 8 | `test_sse_xsave_xrstor_roundtrip` | SSE register corruption through XSAVE/XRSTOR |
| 9 | `test_avx_xsave_xrstor_roundtrip` | **AVX upper-YMM loss** (would fail under FXSAVE) |
| 10 | `test_sse_multi_register_isolation` | Cross-register bleed across XMM0–XMM7 |
| 11 | `test_xsave_area_size_matches_cpuid` | Runtime vs CPUID area size divergence |
| 12 | `test_xsave_area_size_covers_avx` | Area too small for AVX state components |
| 13 | `test_xsave_variant_flags_consistent` | XSAVEC/XSAVEOPT flags mismatch |

---

## 5. Phase 2: Spinlock Modernization

> **Fixes SMP fairness and eliminates cache line thrashing.**
> **Kernel changes required**: Yes — replace spinlock implementation
> **Difficulty**: Low-Medium
> **Depends on**: Nothing (self-contained)

### Background

The `IrqMutex` in `lib/src/spinlock.rs` previously used a simple `compare_exchange_weak` loop (test-and-set).
On SMP (SlopOS runs with `QEMU_SMP=2`), this had two problems:
1. **No fairness**: CPU 0 can acquire the lock 1000 times while CPU 1 starves
2. **Cache line bouncing**: Every `compare_exchange` invalidates the cache line on the other core, even if the lock is held — wasting memory bandwidth

### 2A: Ticket Lock Implementation

The simplest fair lock. Linux used ticket locks from 2008–2015.

- [x] **2A.1** Implement ticket lock in `IrqMutex` (`lib/src/spinlock.rs`):
  - Replaced `lock: AtomicBool` with `next_ticket: AtomicU16` + `now_serving: AtomicU16`
  - `lock()`: `my_ticket = next_ticket.fetch_add(1, Relaxed)`, spin on `now_serving.load(Acquire) == my_ticket`
  - `unlock()` (Drop): `now_serving.fetch_add(1, Release)`
  - `try_lock()`: CAS `next_ticket` from `now_serving` value to +1 (only succeeds if lock is free)
  - `force_unlock()` / `poison_unlock()`: snap `now_serving` to `next_ticket` (releases lock entirely)
  - `is_locked()`: `next_ticket != now_serving`
  - **FIFO guaranteed** — whoever takes a ticket first gets served first
  - [x] **2A.2** Proportional backoff in the spin loop:
  - `core::hint::spin_loop()` (PAUSE instruction on x86) used in all spin paths
  - Proportional backoff: `distance = my_ticket.wrapping_sub(serving)` → pause 1× per ticket of distance, capped at 64
  - Reduces cache-line traffic dramatically when multiple CPUs are queued
  - [x] **2A.3** Replaced the `IrqMutex` internal lock with ticket lock:
  - The `IrqMutex` API (IRQ disable + preemption disable + lock) stays the same
  - Only the inner locking mechanism changed (`AtomicBool` → `AtomicU16` pair)
  - All existing callers (211 lock sites across 29 files) are unaffected — zero API changes
- [x] **2A.4** Audited all 211 lock sites across 29 files for hold-time:
  - **All lock holds are short-lived critical sections** — no lock is held across blocking I/O, sleep, or wait
  - `PIPE_STATE` / `FILEIO_STATE` (fs/fileio.rs): correctly scoped — lock acquired in `{}` block, released before any blocking wait
  - **Post-Phase 5C stability fix (2026-02-27)**: replaced `FileioState` storage that used `MaybeUninit` + `mem::transmute` with fully typed initialized fields (`kernel: FileTableSlot`, `processes: [FileTableSlot; MAX_PROCESSES]`) to remove UB-prone alias/reinterpretation in `with_tables`/`ensure_initialized`; this resolved observed 64-byte slab corruption during process file-table lifecycle
  - `KERNEL_HEAP` (mm/kernel_heap.rs): held during kmalloc/kfree; classic pattern, unavoidable. Per-CPU slab cache already mitigates contention
  - `PAGE_ALLOCATOR` (mm/page_alloc.rs): held during page frame allocation; per-CPU page cache (PCP) already mitigates hot-path contention
  - `VM_MANAGER` (mm/process_vm.rs): held during process VM operations (page table walks but no blocking I/O)
  - **Future `IrqRwLock` candidates** (read-heavy, write-rare): `CONTEXT` (compositor, 17 sites), `INPUT_MANAGER` (input, 18 sites)

### 2A+: Ancillary Lock Cleanup

- [x] **2A+.1** Replace `KLOG_LOCK` test-and-set `AtomicBool` in `drivers/src/serial.rs` with
  PCR-independent ticket lock (`AtomicU16` pair + `cli`/`sti`, no `PreemptGuard` dependency).
  The klog backend must work during AP boot when PCR is unavailable, so `IrqMutex` cannot
  be used — but the old CAS spinlock had no fairness.
- [x] **2A+.2** Upgrade `IrqRwLock` to **writer-preferring** design:
  - Added `writer_waiting: AtomicU32` counter to `IrqRwLock` struct
  - `write()` increments `writer_waiting` before spinning, decrements after acquiring
  - `read()` and `try_read()` yield when `writer_waiting > 0` — prevents writer starvation
  - `try_write()` unchanged (non-blocking, doesn't signal intent)
  - Zero API changes — all existing callers unaffected
- [x] **2A+.3** Remove the external `spin` crate entirely:
  - Implemented native `OnceLock<T>` in `lib/src/once_lock.rs` (124 lines, `AtomicU8` state machine: UNINIT→RUNNING→COMPLETE)
  - Replaced `spin::Once` with `OnceLock` in `drivers/src/apic/mod.rs`, `core/src/scheduler/runtime.rs`, `drivers/src/random.rs`
  - Replaced `spin::Mutex` with `IrqMutex` in `core/src/scheduler/per_cpu.rs` (queue_lock) and `drivers/src/random.rs`
  - Removed `spin` dependency from all 7 Cargo.toml files (workspace root + 6 crate manifests)
  - Zero `spin::` references remain in the codebase — all locking is now kernel-native

### 2B: Test-and-Test-and-Set Optimization

Even before ticket locks, the spin loop can be improved.

- [x] **2B.1** Test-and-test-and-set (TTAS) pattern:
  - The ticket lock inherently has this property: spinning on `now_serving` is a **read-only** operation
  - No CAS in the spin loop — only a `load(Acquire)` that hits the local cache line until the holder releases
  - The `IrqRwLock` spin loops also already use the TTAS pattern (read-only pre-check before CAS)

### 2C: MCS / Queued Locks (Deferred)

Linux's current lock since 2015. Per-CPU queue node eliminates all cache line bouncing.

> **Status**: Explicitly deferred.  Ticket locks + writer-preferring `IrqRwLock` are
> sufficient for SlopOS's current 2-CPU SMP target.  MCS locks should only be
> revisited when (a) SlopOS targets 4+ CPUs, AND (b) profiling shows spinlock
> contention as a measurable bottleneck.  At 2 CPUs with per-CPU page/slab caches
> already absorbing the hot paths, MCS adds complexity with no measurable benefit.

- [x] **2C.1** ~~Implement `McsLock<T>`~~ — **Deferred**: ticket lock sufficient for 2-CPU SMP
- [x] **2C.2** ~~Identify candidates~~ — **Deferred**: per-CPU caches already mitigate contention
- [x] **2C.3** ~~Benchmark~~ — **Deferred**: no contention data warrants MCS complexity

### Phase 2 Gate

- [x] **GATE**: `IrqMutex` uses ticket lock internally (`AtomicU16` pair replaces `AtomicBool`)
- [x] **GATE**: FIFO fairness guaranteed by ticket lock design — `fetch_add` serializes acquisition order
- [x] **GATE**: `spin_loop()` (PAUSE) used in all spin paths with proportional backoff
- [x] **GATE**: No legacy test-and-set `AtomicBool` locks remain anywhere in the codebase
- [x] **GATE**: `IrqRwLock` is writer-preferring — writers cannot be starved by continuous readers
- [x] **GATE**: `KLOG_LOCK` (serial.rs) replaced with PCR-independent ticket lock
- [x] **GATE**: Phase 2C (MCS) explicitly deferred with documented rationale
- [x] **GATE**: `just test` passes
- [x] **GATE**: No deadlocks or livelocks under SMP boot (2 CPUs, full test harness)

---

## 6. Phase 3: MSI/MSI-X Interrupt Routing

> **Eliminates shared IRQ lines, enables per-device interrupt vectors.**
> **Kernel changes required**: Yes — PCI capability parsing, MSI programming
> **Difficulty**: High
> **Depends on**: Phase 4 benefits from this but is not required

### Background

All current interrupt routing goes through IOAPIC with legacy IRQ line numbers:
```rust
// drivers/src/irq.rs
program_ioapic_route(LEGACY_IRQ_TIMER);
program_ioapic_route(LEGACY_IRQ_KEYBOARD);
program_ioapic_route(LEGACY_IRQ_MOUSE);
program_ioapic_route(LEGACY_IRQ_COM1);
```

MSI (Message Signaled Interrupts) writes a message directly to the LAPIC — no IOAPIC involved. Benefits:
- **No shared IRQs**: Each device gets its own vector(s)
- **Lower latency**: Direct write to LAPIC, no IOAPIC redirection table lookup
- **PCIe requirement**: MSI is mandatory for PCIe devices per spec
- **Multi-queue**: MSI-X supports up to 2048 vectors per device (critical for VirtIO multi-queue)

### 3A: PCI Capability List Parsing

MSI/MSI-X are discovered through PCI capability structures.

- [x] **3A.1** Implement PCI capability list walking in `drivers/src/pci.rs`:
  - Read `Status Register` (offset 0x06) bit 4: capabilities list present
  - Read `Capabilities Pointer` (offset 0x34): points to first capability
  - Walk linked list: each entry has `{ cap_id: u8, next_ptr: u8, ... }`
  - Return capability offset for a given ID
  - Implemented as `PciCapabilityIter` struct with `Iterator` trait — idiomatic Rust linked-list traversal
  - Guard counter (48 iterations, matching Linux `PCI_FIND_CAP_TTL`) protects against malformed lists
  - Bottom 2 bits of pointers masked per PCI spec (DWORD alignment)
- [x] **3A.2** Define capability IDs in `drivers/src/pci_defs.rs`:
  - `PCI_CAP_ID_MSI = 0x05`
  - `PCI_CAP_ID_MSIX = 0x11`
  - `PCI_CAP_ID_PCIE = 0x10` (added for Phase 4 readiness)
  - `PCI_CAP_ID_VNDR = 0x09` (already existed, used by VirtIO)
  - `PciCapability { offset: u8, id: u8 }` struct for iterator yield type
- [x] **3A.3** Implement `pci_find_capability(bus, dev, func, cap_id) -> Option<u8>`:
  - Wraps `PciCapabilityIter` with idiomatic `.find().map()` chain
  - Convenience methods on `PciDeviceInfo`: `find_capability()`, `capabilities()`, `has_msi()`, `has_msix()`
  - Used by MSI, MSI-X, and VirtIO modern device detection
- [x] **3A.4** Store discovered capabilities in `PciDeviceInfo`:
  - Added `msi_cap_offset: Option<u8>` and `msix_cap_offset: Option<u8>`
  - Populated during PCI enumeration via single capability list walk
  - All capabilities logged during enumeration: `CAP: 0x{id} ({name}) at offset 0x{off}`
  - `pci_get_msi_capable_devices()` helper for downstream MSI/MSI-X consumers

### 3B: MSI Support

Program basic MSI (1–32 vectors per device).

- [x] **3B.1** Parse MSI capability structure:
  ```
  Offset+0: Cap ID (0x05) | Next
  Offset+2: Message Control (enable, multi-message capable/enable, 64-bit, per-vector masking)
  Offset+4: Message Address (low 32 bits)
  Offset+8: Message Address (high 32 bits, if 64-bit capable)
  Offset+C: Message Data
  ```
- [x] **3B.2** Implement `msi_configure(device, vector, cpu) -> Result<(), Error>`:
  - Message Address: `0xFEE00000 | (cpu_apic_id << 12)`
  - Message Data: `vector | (edge_trigger << 15) | (fixed_delivery << 8)`
  - Set enable bit in Message Control
  - Disable INTx via PCI command register when MSI is active
- [x] **3B.3** Allocate interrupt vectors for MSI:
  - Lock-free bitmap allocator in `core/src/irq.rs` using `AtomicU64` CAS
  - Vectors 32–47 reserved for legacy IOAPIC, 48–223 for MSI allocation (176 vectors)
  - `msi_alloc_vector() -> Option<u8>`, `msi_free_vector(vector: u8)`
- [x] **3B.4** Register MSI handler in IDT:
  - 176 assembly stubs generated via macro in `boot/idt_handlers.s`
  - Address table exported to Rust via `msi_vector_table` in `.rodata`
  - IDT entries installed in `idt_init()` (skipping SYSCALL_VECTOR 0x80)
  - MSI dispatch integrated into `irq_dispatch()` with handler table + EOI + scheduler handoff

### 3C: MSI-X Support

MSI-X is the preferred mechanism (more vectors, per-vector masking, table-based).

- [x] **3C.1** Parse MSI-X capability structure:
  - `msix_read_capability(bus, dev, func, cap_offset) -> MsixCapability`
  - Parses Message Control (table size, function mask, enable), Table Offset/BIR, PBA Offset/BIR
  - `MsixCapability` struct: `cap_offset`, `control`, `table_size` (1–2048), `table_bar`, `table_offset`, `pba_bar`, `pba_offset`
  - Implemented in `drivers/src/msix.rs`
- [x] **3C.2** Map MSI-X table:
  - `msix_map_table(device: &PciDeviceInfo, cap: &MsixCapability) -> Result<MsixTable, MsixError>`
  - Reads BIR to find which BAR contains the table; maps via `MmioRegion::map()`
  - Maps both MSI-X table (16 bytes × table_size) and PBA (⌈table_size/64⌉ × 8 bytes)
  - `MsixTable` struct with `read_vector_control()` and `is_pending()` accessors
  - Each table entry: `{ addr_low: u32, addr_high: u32, data: u32, vector_control: u32 }`
- [x] **3C.3** Implement `msix_configure(table, entry_idx, vector, target_apic_id) -> Result<(), MsixError>`:
  - Masks entry → writes LAPIC message address/data → unmasks entry
  - Same x86 message format as MSI (0xFEE00000 base + APIC ID shift)
  - `msix_mask_entry()` / `msix_unmask_entry()` for per-vector masking
  - `MsixError` enum: `InvalidVector`, `InvalidEntry`, `BarNotAvailable`, `MappingFailed`, `TableNotMapped`
- [x] **3C.4** Implement `msix_enable(device)` / `msix_disable(device)`:
  - `msix_enable()`: sets MSI-X enable bit, clears function mask, disables legacy INTx
  - `msix_disable()`: clears MSI-X enable bit, re-enables legacy INTx
  - `msix_set_function_mask()` / `msix_clear_function_mask()` for atomic bulk reconfiguration
  - `msix_refresh_control()` to re-read Message Control register

### Phase 3C Test Coverage

25 tests in `drivers/src/msix_tests.rs` covering:
- Capability parsing from QEMU VirtIO block (1af4:1042) and net (1af4:1041) devices
- Field consistency: table_size range (1–2048), BIR range (0–5), DWORD-aligned offsets
- Deterministic parsing across multiple reads
- Table mapping: MMIO region creation, accessor bounds checking (read_vector_control, is_pending)
- Entry configuration: valid vector programming, InvalidVector/InvalidEntry error paths
- Mask/unmask operations: per-entry masking with vector control bit verification, out-of-range rejection
- Enable/disable: config-space enable bit toggling, function mask toggling, refresh_control
- MsixCapability helper methods: is_enabled(), is_function_masked()
- Sweep of all MSI-X devices for field validity
- Negative test: SATA controller (8086:2922) correctly reports no MSI-X

### 3D: VirtIO MSI-X Integration

Wire MSI-X into the existing VirtIO drivers.

- [x] **3D.1** Update `drivers/src/virtio_blk.rs`:
  - During probe: check for MSI-X capability
  - If available: allocate vectors, configure MSI-X table entries
  - Map virtqueue interrupts to MSI-X vectors instead of legacy IRQ
- [x] **3D.2** Update `drivers/src/virtio_net.rs`:
  - Same treatment — MSI-X enables separate vectors for RX and TX queues
  - Multi-queue: each queue pair gets its own vector (enables parallel processing)
- [x] **3D.3** MSI-X required, MSI as minimum fallback; legacy polling removed
  - `InterruptMode::None` variant eliminated — probe panics if neither MSI-X nor MSI available
  - `setup_interrupts` returns `Result` — callers handle the error explicitly
  - `irq_mode` field removed from driver state structs (redundant with `msix_state`)

### 3E: Interrupt-Driven VirtIO Completion

Replace busy-wait polling with real MSI-X interrupt-driven I/O completion.

- [x] **3E.1** Remove dead ISR capability code:
  - `VIRTIO_PCI_CAP_ISR_CFG` constant, `isr_cfg` field, PCI cap parsing arm, debug log references
- [x] **3E.2** Add `QueueEvent` completion primitive (`drivers/src/virtio/mod.rs`):
  - `AtomicBool`-backed signal/reset/consume
  - `wait_timeout_ms()` using HPET deadline + x86 `cli; sti; hlt` wakeup pattern
  - Prevents lost-wakeup race: `sti` defers interrupt delivery until after `hlt`
- [x] **3E.3** Wire MSI-X IRQ handlers to signal `QueueEvent`:
  - `virtio-blk`: `BLK_QUEUE_EVENT` static, handler calls `.signal()`
  - `virtio-net`: `NET_RX_EVENT` + `NET_TX_EVENT` statics, handler uses queue-index ctx
- [x] **3E.4** Replace polling with event waits:
  - `poll_used()` removed from `queue.rs`
  - `pop_used()` → non-blocking `try_pop_used()` + `advance_used()`
  - `REQUEST_TIMEOUT_SPINS` (arbitrary spin count) → `REQUEST_TIMEOUT_MS` (5s real-time via HPET)
- [x] **3E.5** Expose `hpet::period_fs()` for deadline computation

### Phase 3 Gate

- [x] **GATE**: PCI capability list walking implemented
- [x] **GATE**: MSI can be configured for at least one device
- [x] **GATE**: MSI-X table mapped and entries programmable
- [x] **GATE**: VirtIO block device works with MSI-X (MSI minimum, legacy polling removed)
- [x] **GATE**: Vector allocator manages the MSI vector space
- [x] **GATE**: `just test` passes (533/533, including 25 MSI-X regression tests + 18 VirtIO MSI-X integration tests + 13 interrupt-driven completion tests)
- [x] **GATE**: Legacy IOAPIC routing still works for PS/2, serial (verified: 533/533 tests pass including IOAPIC-routed keyboard/serial tests)

---

## 7. Phase 4: PCIe ECAM Configuration Space

> **Replaces 1992 port I/O with memory-mapped PCIe config access.**
> **Kernel changes required**: Yes — ACPI MCFG parsing, ECAM MMIO driver
> **Difficulty**: Medium
> **Depends on**: Phase 3 benefits from this (extended config space for MSI-X)

### Background

Current PCI config access uses port I/O:
```rust
PCI_CONFIG_ADDRESS.write(addr);  // 0xCF8
PCI_CONFIG_DATA.read()           // 0xCFC
```

This gives access to only 256 bytes of config space per function. PCIe extended config space is 4096 bytes — needed for:
- Advanced Error Reporting (AER)
- Extended capabilities (some MSI-X features)
- SR-IOV, ACS, ATS, etc.

ECAM (Enhanced Configuration Access Mechanism) maps the entire config space into MMIO. The base address comes from the ACPI MCFG table.

### 4A: ACPI MCFG Table Parsing

- [x] **4A.1** Add MCFG table parsing to `acpi/src/mcfg.rs`:
  - Finds `"MCFG"` signature via `AcpiTables::find_table()` (same pattern as HPET/MADT)
  - Parses raw `RawMcfgTable` (44-byte header: 36-byte SDT + 8 reserved) and variable-length `RawMcfgEntry` array (16 bytes each)
  - Exports `McfgEntry { base_phys: u64, segment: u16, bus_start: u8, bus_end: u8 }`
  - `Mcfg` struct with `from_tables()`, `entries()`, `find_entry(segment, bus)`, `primary_entry()` methods
  - `McfgEntry::region_size()` computes ECAM MMIO size; `ecam_offset(bus, dev, func)` computes BDF offset
  - Validates base address non-zero and bus range ordering; caps at 16 entries
  - Module declared in `acpi/src/lib.rs`
- [x] **4A.2** Store MCFG data in `drivers/src/pci.rs`:
  - `EcamState` struct with `IrqMutex`-protected array of up to 16 `McfgEntry`s
  - `ECAM_BASE: AtomicU64` — lock-free cached base for segment 0
  - `ECAM_ENTRY_COUNT: AtomicU8` — lock-free availability check
  - `pci_discover_mcfg()` called at start of `pci_init()` — mandatory in Phase 4D (panic if MCFG absent/invalid)
  - Public API: `pci_ecam_available()`, `pci_ecam_base()`, `pci_ecam_entry_count()`, `pci_ecam_entry(index)`, `pci_ecam_find_entry(segment, bus)`
  - QEMU q35 discovers ECAM at 0xe0000000 covering segment 0 buses 0–255 (256MB)

### Phase 4A Test Coverage

21 tests in `drivers/src/ecam_tests.rs` covering:
- MCFG discovery sanity: `pci_ecam_available()` returns true, entry count > 0 on QEMU q35
- Primary entry validation: non-zero base address, 4 KiB page-aligned, covers bus 0
- Entry field validity: all entries have non-zero base_phys, bus_start ≤ bus_end
- `McfgEntry::region_size()`: correct formula `(bus_end - bus_start + 1) * 256 * 4096`, full-range = 256 MiB
- `McfgEntry::ecam_offset()`: zero-BDF returns 0, known BDF matches `(bus<<20)|(dev<<15)|(func<<12)`
- Boundary rejection: bus below/above range returns None, device ≥ 32 returns None, function ≥ 8 returns None
- `pci_ecam_find_entry()`: finds segment 0 bus 0, returns None for nonexistent segment 0xFFFF
- Lock-free vs mutex consistency: `pci_ecam_base()` matches primary entry `base_phys`, `entry_count` matches indexable range
- Deterministic reads: consecutive reads of all ECAM state return identical results

### 4B: ECAM MMIO Config Access

- [x] **4B.1** Map the ECAM region via `MmioRegion::map()`:
  - Each MCFG entry's MMIO region mapped during `pci_discover_mcfg()`
  - Primary segment (segment 0) cached in lock-free atomics (`ECAM_PRIMARY_VIRT`, `ECAM_PRIMARY_SIZE`, `ECAM_PRIMARY_BUS_START`, `ECAM_PRIMARY_BUS_END`) for fast-path access
  - Multi-segment fallback via `ECAM_STATE` mutex for rare cases
  - QEMU q35 maps segment 0 buses 0–255 (256 MiB) at 0xE0000000
- [x] **4B.2** Implement `pci_ecam_read32/read16/read8(bus, dev, func, offset: u16) -> Option<T>`:
  - Full 4096-byte config space access via volatile MMIO reads
  - `ecam_virt_addr()` helper centralizes address computation and bounds checking
  - Dual-path: lock-free primary segment + mutex fallback for multi-segment
- [x] **4B.3** Implement `pci_ecam_write32/write16/write8(bus, dev, func, offset: u16, value) -> Option<()>`:
  - Same address pattern for writes, volatile MMIO writes
- [x] **4B.4** Create transitional `PciConfigBackend` abstraction (removed in Phase 4D):
  ```rust
  enum PciConfigBackend {
      LegacyPortIo,        // 0xCF8/0xCFC (fallback)
      Ecam,                // MMIO
  }
  ```
  - `pci_config_read32()` / `pci_config_write32()` etc. initially dispatched to ECAM when active, falling back to port I/O
  - Original port I/O implementations renamed to private `pci_pio_*` functions
  - 88 existing call sites across 6 files transparently use ECAM — zero API changes
### 4C: Extended Config Space Usage

- [x] **4C.1** Update PCI enumeration to scan extended capabilities (offset 0x100+):
  - `PciExtCapabilityIter` struct with `Iterator` trait — mirrors `PciCapabilityIter` pattern
  - Extended capability list starts at offset 0x100 (PCIe extended config space)
  - Each entry: 32-bit DWORD — `{ cap_id: u16 [15:0], version: u4 [19:16], next_offset: u12 [31:20] }`
  - Only accessible via ECAM (not through port I/O) — iterator yields nothing if ECAM inactive
  - Guard counter (48 iterations, matching `PciCapabilityIter::MAX_CAPS`) protects against malformed lists
  - Terminates on `next_offset == 0`, header `0x00000000`, or header `0xFFFFFFFF`
  - Next offset validated: must be ≥ 0x100 and DWORD-aligned, else treated as end-of-list
  - `PciExtCapability { offset: u16, id: u16, version: u8 }` struct in `pci_defs.rs`
  - 15 extended capability ID constants defined (`PCI_EXT_CAP_ID_AER`, `PCI_EXT_CAP_ID_SRIOV`, etc.)
- [x] **4C.2** Implement `pci_find_ext_capability(bus, dev, func, cap_id) -> Option<u16>`:
  - Wraps `PciExtCapabilityIter` with idiomatic `.find().map()` chain
  - Convenience methods on `PciDeviceInfo`: `find_ext_capability()`, `ext_capabilities()`
- [x] **4C.3** Log extended capabilities during PCI enumeration:
  - `"    EXT_CAP: 0x{:04x} ({name}) v{version} at offset 0x{:03x}"`
  - `pci_ext_cap_id_name()` maps 15 known extended capability IDs to human-readable names

### 4D: ECAM-Only Long-Term Migration

- [x] **4D.1** Remove all legacy PCI port I/O fallback code from `drivers/src/pci.rs`:
  - Removed `PciConfigBackend` enum, `pci_config_backend()`, and `pci_ecam_is_active()`
  - Removed all private `pci_pio_*` implementations (0xCF8/0xCFC path)
  - `pci_config_read*/write*` now route ECAM-only and fail fast on invalid access
- [x] **4D.2** Make ECAM a hard boot requirement:
  - `pci_discover_mcfg()` now panics if ACPI RSDP/MCFG is absent or invalid
  - ECAM segment mapping failures panic during PCI init
  - Primary segment (segment 0) mapping is required for enumeration
- [x] **4D.3** Widen PCI config offsets from `u8` to `u16` across the stack:
  - `pci_config_read*/write*` public signatures now use `offset: u16`
  - All PCI config register offset constants in `pci_defs.rs` are now `u16`
  - Capability offsets in `PciCapability`, `PciDeviceInfo`, MSI/MSI-X structs updated to `u16`
  - Downstream call sites updated (`msi.rs`, `msix.rs`, `virtio/pci.rs`, tests)
- [x] **4D.4** Remove obsolete legacy constants:
  - Deleted `PCI_CONFIG_ADDRESS` and `PCI_CONFIG_DATA` from `lib/src/ports.rs`
  - No remaining in-tree users of 0xCF8/0xCFC PCI config access

### Phase 4 Gate

- [x] **GATE**: MCFG table parsed from ACPI (21 regression tests in `ecam_tests.rs`)
- [x] **GATE**: ECAM MMIO region mapped and functional (17 Phase 4B regression tests)
- [x] **GATE**: PCI config reads work through ECAM for the full 4096-byte space
- [x] **GATE**: Extended capability list scanned during enumeration (Phase 4C)
- [x] **GATE**: Legacy 0xCF8/0xCFC PCI config path removed; ECAM is mandatory at boot
- [x] **GATE**: All PCI config offset APIs widened to `u16` and call sites updated
- [x] **GATE**: `just test` passes (571/571 including 38 ECAM regression tests)
- [x] **GATE**: `just boot` — PCI devices discovered correctly through ECAM

---

## 8. Phase 5: Network Stack Completion

> **TCP makes the network stack actually useful.**
> **Kernel changes required**: Yes — TCP state machine, socket layer
> **Difficulty**: Very High
> **Depends on**: Nothing (builds on existing VirtIO net + IPv4/UDP)

### Background

The current network stack has:
- VirtIO network driver (`drivers/src/virtio_net.rs`)
- Ethernet frame handling
- ARP (address resolution)
- IPv4 (packet parsing/construction)
- UDP (connectionless datagrams)
- ICMP (ping)
- DHCP client

Missing: **TCP** — the protocol that powers HTTP, SSH, DNS over TCP, and nearly all "real" networking.

### 5A: TCP State Machine

- [x] **5A.1** Create `drivers/src/net/tcp.rs`:
  - `TcpHeader` struct with all 10 fields, parse/construct via `parse_header()`/`write_header()`
  - Flag helpers: `is_syn()`, `is_ack()`, `is_fin()`, `is_rst()`, `is_psh()`, `is_urg()`, `is_syn_ack()`, `is_fin_ack()`
  - MSS option parsing/writing: `parse_mss_option()`, `write_mss_option()`
  - TCP checksum with IPv4 pseudo-header: `tcp_checksum()`, `verify_checksum()`
  - One's-complement accumulator with trailing odd-byte handling
  - Added `IPPROTO_TCP = 6` to `net/mod.rs`
- [x] **5A.2** Implement the TCP state machine (RFC 793 + RFC 7413):
  - 11 states: `Closed`, `Listen`, `SynSent`, `SynReceived`, `Established`, `FinWait1`, `FinWait2`, `CloseWait`, `Closing`, `LastAck`, `TimeWait`
  - `TcpState` enum with `name()`, `is_open()`, `is_closing()` helpers
  - `tcp_input()` dispatches to per-state processors: `process_listen()`, `process_syn_sent()`, `process_syn_received()`, `process_established_and_closing()`, `process_time_wait()`
  - Sequence number arithmetic: `seq_lt()`, `seq_le()`, `seq_gt()`, `seq_ge()` with wrapping comparison (RFC 793 §3.3)
  - ISN generator: monotonic counter incremented by 64000 per connection
  - Ephemeral port allocator: 49152–65535 range
- [x] **5A.3** Implement the TCP connection table:
  - `TcpConnectionTable` with 64-slot `[TcpConnection; MAX_CONNECTIONS]` array
  - `TcpTuple` four-tuple key with wildcard matching for listen sockets
  - Two-pass lookup: exact match first, then wildcard listen sockets
  - `port_in_use()` for bind conflict detection
  - Global `TCP_TABLE: IrqMutex<TcpConnectionTable>` for thread-safe access
  - `tcp_reset_all()` for test isolation
- [x] **5A.4** Implement three-way handshake (active open):
  - `tcp_connect()`: allocates slot, generates ISN, returns `(idx, SYN segment)`
  - `process_syn_sent()`: validates ACK range, handles RST, processes SYN+ACK → ESTABLISHED
  - MSS negotiation from SYN+ACK options
  - Bad ACK detection with RST response
  - Simultaneous open support (SYN without ACK → SYN_RECEIVED)
- [x] **5A.5** Implement three-way handshake (passive open / listen):
  - `tcp_listen()`: binds port with duplicate detection (`AddrInUse` error)
  - `process_listen()`: creates child connection in SYN_RECEIVED, sends SYN+ACK
  - `process_syn_received()`: validates ACK range → ESTABLISHED
  - Listen socket persists independently of accepted connections
  - ACK to LISTEN socket correctly generates RST
- [x] **5A.6** Implement connection teardown:
  - `tcp_close()`: ESTABLISHED→FIN_WAIT_1, CLOSE_WAIT→LAST_ACK
  - Active close: FIN_WAIT_1 → FIN_WAIT_2 → TIME_WAIT
  - Passive close: ESTABLISHED → CLOSE_WAIT → LAST_ACK → CLOSED
  - Simultaneous close: FIN_WAIT_1 → CLOSING → TIME_WAIT
  - FIN+ACK fast path: FIN_WAIT_1 → TIME_WAIT (when FIN+ACK acks our FIN)
  - `tcp_abort()`: sends RST and releases immediately
  - `tcp_timer_tick()`: expires TIME_WAIT after 2×MSL (60s)
  - Retransmitted FIN in TIME_WAIT correctly re-ACKed

### Phase 5A Test Coverage

61 tests in `drivers/src/tcp_tests.rs` covering:

| Category | Count | What It Catches |
|---|---|---|
| Header parsing | 6 | Malformed headers, short buffers, invalid data_offset, all flags |
| Header construction | 3 | Write/parse roundtrip, buffer overflow, options area zeroing |
| MSS options | 5 | MSS parse/write, NOP padding, missing MSS, buffer overflow |
| Checksum | 5 | Zero/non-zero payload, odd-length, wrong IP detection, determinism |
| Sequence arithmetic | 4 | Wrapping comparison correctness for lt/le/gt/ge |
| Connection table | 10 | Create/find/release, table full, port conflict, abort, close-not-found |
| Active handshake | 4 | Full handshake, RST in SYN_SENT, bad ACK, MSS negotiation |
| Passive handshake | 3 | Full handshake, RST in SYN_RECEIVED, ACK-to-LISTEN RST |
| Teardown | 3 | Active close, passive close, simultaneous close |
| TIME_WAIT | 2 | 2×MSL expiry, retransmitted FIN re-ACK |
| RST handling | 3 | RST in ESTABLISHED, RST to unknown, SYN in ESTABLISHED |
| Misc | 8 | No-connection RST, ephemeral ports, state helpers, tuple matching |
| Integration | 4 | Wildcard find, simultaneous open, multiple connections, defaults |

### 5B: TCP Data Transfer

- [x] **5B.1** Implement send buffer:
  - Ring buffer per connection (e.g., 16KB)
  - Track `SND.UNA`, `SND.NXT`, `SND.WND` per RFC 793
  - Segment outgoing data into MSS-sized chunks
- [x] **5B.2** Implement receive buffer:
  - Ring buffer per connection
  - Track `RCV.NXT`, `RCV.WND`
  - Handle out-of-order segments (simple: drop and let sender retransmit)
- [x] **5B.3** Implement acknowledgment:
  - Delayed ACK (200ms timer or every other segment)
  - Cumulative ACK
- [x] **5B.4** Implement retransmission:
  - Retransmission timeout (RTO) with exponential backoff
  - Start with fixed 1s RTO, later implement Karn/Partridge algorithm
- [x] **5B.5** Implement flow control:
  - Window size advertisement in ACK segments
  - Respect remote window size when sending
  - Zero window probing

### 5C: Socket Abstraction Layer

- [x] **5C.1** Define socket syscall interface in `abi/src/syscall.rs`:
  - `SYSCALL_SOCKET` — create a socket (AF_INET, SOCK_STREAM / SOCK_DGRAM)
  - `SYSCALL_BIND` — bind to local address/port
  - `SYSCALL_LISTEN` — mark socket as listening
  - `SYSCALL_ACCEPT` — accept incoming connection
  - `SYSCALL_CONNECT` — initiate TCP connection
  - `SYSCALL_SEND` / `SYSCALL_RECV` — transfer data
  - `SYSCALL_CLOSE` — close socket (reuse existing close syscall)
- [x] **5C.2** Implement socket file descriptors:
  - Sockets are file descriptors (POSIX model)
  - `read()` / `write()` on a socket FD maps to `recv()` / `send()`
  - Integrate with `poll()` for event-driven I/O
- [x] **5C.3** Implement kernel socket structures in `drivers/src/net/socket.rs`:
  - `SocketEntry { domain, sock_type, state, tcp_idx, local_addr, remote_addr, ... }`
  - Global socket table with FD integration (socket_idx field on FileDescriptor)
- [x] **5C.4** Add userland wrappers in `userland/src/syscall/`:
  - `socket()`, `bind()`, `listen()`, `accept()`, `connect()`, `send()`, `recv()`
  - These mirror the POSIX socket API

### 5D: Async Network I/O & NAPI-Style Completion

> Replace the synchronous, one-packet-at-a-time VirtIO net path and stubbed socket
> readiness with a proper interrupt→poll→wakeup pipeline modeled on Linux's NAPI +
> socket wait queue architecture. Eliminates the hardcoded `REQUEST_TIMEOUT_MS = 5000`
> safety timeout in `drivers/src/virtio_net.rs` with a real event-driven design.

#### 5D.1: NAPI-Style Receive Pipeline

- [x] **5D.1a** Implement `NapiContext` in `drivers/src/net/napi.rs`:
  - Budget-limited polling: process up to N packets per poll cycle (default 64)
  - State machine: `Idle` → `Scheduled` → `Polling` → `Idle`
  - `napi_schedule()`: disables virtqueue callbacks, marks scheduled, triggers softirq/deferred work
  - `napi_complete()`: re-enables virtqueue callbacks, returns to idle
- [x] **5D.1b** Refactor RX path in `drivers/src/virtio_net.rs`:
  - MSI-X RX handler calls `napi_schedule()` instead of `NET_RX_EVENT.signal()`
  - Remove `poll_one_rx_frame()` / `poll_one_rx_frame_timeout()` synchronous paths
  - New `virtnet_poll(budget) -> processed_count` drains used ring in a batch
  - Pre-post multiple RX buffers (ring of 256) instead of posting one at a time
  - Refill RX ring after each poll cycle
- [x] **5D.1c** Wire poll cycle into the existing timer tick / deferred work mechanism:
  - If no softirq infrastructure exists, use a per-CPU deferred work queue or piggyback on timer tick
  - Ensure poll runs on the same CPU that received the interrupt (cache locality)

#### 5D.2: Asynchronous TX Completion

- [x] **5D.2a** Make TX fire-and-forget:
  - `submit_tx()` returns immediately after posting descriptor (no `wait_timeout_ms`)
  - Remove `NET_TX_EVENT` blocking wait
  - Track in-flight TX buffers in a completion ring
- [x] **5D.2b** Lazy TX cleanup:
  - `virtnet_poll_cleantx()`: free completed TX buffers during RX poll or on next TX
  - Reclaim pages from used ring without blocking the sender
- [x] **5D.2c** TX queue backpressure:
  - If TX ring is full, return `-EAGAIN` to caller (or block in socket layer, not driver)
  - Track available TX descriptors, wake blocked senders when space freed

#### 5D.3: Socket Wait Queues

- [x] **5D.3a** Implement `WaitQueue` primitive in `lib/src/waitqueue.rs`:
  - `WaitQueue { head: IrqMutex<WaitQueueHead> }`
  - `wait_event(wq, condition)`: add current task to queue, set `Blocked`, yield to scheduler
  - `wake_one(wq)` / `wake_all(wq)`: move tasks from wait queue to run queue
  - Interruptible variant: `wait_event_interruptible()` that respects signals
- [x] **5D.3b** Add per-socket wait queues to `KernelSocket`:
  - `recv_wq: WaitQueue` — woken when data arrives in TCP receive buffer
  - `accept_wq: WaitQueue` — woken when a new connection completes handshake
  - `send_wq: WaitQueue` — woken when TX window opens / send buffer drains
- [x] **5D.3c** Wire TCP data delivery to socket wakeup:
  - `tcp_input()` → data queued in receive buffer → `recv_wq.wake_one()`
  - `tcp_input()` → handshake complete (new ESTABLISHED child) → `accept_wq.wake_one()`
  - TX completion / window update → `send_wq.wake_one()`

#### 5D.4: Blocking Socket Syscalls

- [x] **5D.4a** Implement blocking `recv()`:
  - If receive buffer empty and socket is blocking: `recv_wq.wait_event(|| has_data())`
  - Respect `SO_RCVTIMEO` (if set) as the wait timeout instead of hardcoded 5s
  - Non-blocking (`O_NONBLOCK`): return `-EAGAIN` immediately (current behavior)
- [x] **5D.4b** Implement blocking `accept()`:
  - If no pending connections: `accept_wq.wait_event(|| has_pending())`
  - Non-blocking: return `-EAGAIN` (current behavior, preserved)
- [x] **5D.4c** Implement blocking `send()` with backpressure:
  - If send buffer full: `send_wq.wait_event(|| has_space())`
  - Partial writes: send what fits, return bytes written
- [x] **5D.4d** Implement `SO_RCVTIMEO` / `SO_SNDTIMEO` socket options:
  - `setsockopt()` / `getsockopt()` syscalls (or simplified kernel-internal API)
  - Pass timeout to `wait_event_timeout()` instead of blocking indefinitely

#### 5D.5: Real `poll()` / Readiness Notification

- [x] **5D.5a** Implement `socket_poll()` in `drivers/src/net/socket.rs`:
  - Check actual socket state: data in receive buffer → `POLLIN`, send space → `POLLOUT`
  - Pending accept → `POLLIN` on listening socket
  - Connection refused / reset → `POLLERR` / `POLLHUP`
  - Replace the current stub that returns "always ready"
- [x] **5D.5b** Wire socket wait queues into `poll()` syscall:
  - `poll()` adds calling task to each socket's wait queue
  - When any socket becomes ready, task is woken and `poll()` re-checks all FDs
  - Remove task from all wait queues on return
- [x] **5D.5c** Extend `poll()` to handle mixed FD types:
  - Pipes, files, and sockets in the same `poll()` call
  - Each FD type implements a `poll_check() -> PollFlags` method

#### 5D.6: Remove Legacy Synchronous Paths

- [x] **5D.6a** Remove `REQUEST_TIMEOUT_MS` constant and `QueueEvent`-based TX/RX blocking
- [x] **5D.6b** Remove `NET_TX_EVENT` / `NET_RX_EVENT` statics (replaced by NAPI + wait queues; `DHCP_RX_EVENT` retained for boot-time DHCP)
- [x] **5D.6c** Audit DHCP path — kept polling for boot-time simplicity (`poll_one_rx_frame_timeout` retained)
- [x] **5D.6d** Audit ARP path — scan operations now use NAPI pipeline with `virtnet_poll()`
- [x] **5D.6e** Removed `virtio_transport.rs` dependency — all TX/RX now goes through batched NAPI + `submit_tx()`

#### Phase 5D Test Coverage

- [x] **5D.T1** NAPI poll: verify budget limiting (NapiContext state machine tested in `napi_tests.rs`)
- [x] **5D.T2** TX fire-and-forget: verify `submit_tx` returns immediately, cleanup happens lazily
- [x] **5D.T3** Wait queue: unit test `wake_one` / `wake_all` / generation counter
- [x] **5D.T4** Blocking recv: verify task sleeps when no data, wakes on timeout (EAGAIN)
- [x] **5D.T5** Blocking accept: verify task sleeps on empty backlog, wakes on timeout (EAGAIN)
- [x] **5D.T6** Socket poll: verify correct `POLLIN`/`POLLOUT`/`POLLERR`/`POLLHUP` flags for each socket state
- [x] **5D.T7** Non-blocking preserved: verify `O_NONBLOCK` still returns `-EAGAIN`
- [x] **5D.T8** Timeout: verify `SO_RCVTIMEO` wakes recv after deadline
- [x] **5D.T9** Backpressure: verify send blocks when buffer full, resumes when drained
- [x] **5D.T10** Regression: all existing TCP/socket tests still pass (708/708)

### 5E: UDP Datagram Socket Completion

> The socket layer accepts `SOCK_DGRAM` but every operation beyond `create()`
> and `bind()` either returns `EPROTONOSUPPORT` or silently requires a TCP
> connection index.  UDP RX packets are discarded in `dispatch_rx_frame()`.
> This phase completes the UDP datagram path end-to-end, unblocking DNS (5F)
> and every future connectionless protocol (NTP, mDNS, game networking).

#### 5E.1: Per-Socket UDP Receive Buffer

- [x] **5E.1a** Define `UdpDatagram` struct in `drivers/src/net/socket.rs`:
  - `{ src_ip: [u8; 4], src_port: u16, len: u16, data: [u8; UDP_DGRAM_MAX] }`
  - `UDP_DGRAM_MAX_PAYLOAD = 1472` (MTU 1500 − IPv4 header 20 − UDP header 8)
- [x] **5E.1b** Add `UdpReceiveQueue` per socket:
  - Fixed ring of 16 `UdpDatagram` slots (statically sized, no heap allocation)
  - `push()` drops oldest on overflow (UDP is unreliable by design)
  - `pop() -> Option<&UdpDatagram>` for consumer
  - `is_empty()` / `len()` for poll readiness

#### 5E.2: UDP RX Dispatch

- [x] **5E.2a** Implement UDP header parsing in `drivers/src/net/mod.rs`:
  - `parse_udp_header(payload: &[u8]) -> Option<(u16, u16, &[u8])>` — returns (src_port, dst_port, udp_payload)
  - Validate minimum 8-byte header, length field ≤ payload length
- [x] **5E.2b** Wire `IPPROTO_UDP` arm in `dispatch_rx_frame()` (`virtio_net.rs`):
  - Replace the no-op `let _ = (src_ip, dst_ip, ip_payload)` with real dispatch
  - Parse UDP header from `ip_payload`
  - Look up bound socket by `(dst_ip, dst_port)` in socket table
  - Enqueue `UdpDatagram { src_ip, src_port, data }` into socket's receive queue
  - Wake socket's `recv_wq` wait queue
- [x] **5E.2c** Add UDP port→socket lookup to `SocketTable`:
  - `find_udp_socket(dst_ip: [u8; 4], dst_port: u16) -> Option<u32>` (socket index)
  - Wildcard match: bound to `0.0.0.0` matches any local IP
  - Only searches `SOCK_DGRAM` sockets in `Bound` or `Connected` state

#### 5E.3: Generic UDP Transmit

- [x] **5E.3a** Factor `transmit_dhcp_packet()` into generic `transmit_udp_packet()` in `virtio_net.rs`:
  - `pub fn transmit_udp_packet(src_ip: [u8; 4], dst_ip: [u8; 4], src_port: u16, dst_port: u16, payload: &[u8]) -> bool`
  - Builds Ethernet + IPv4 + UDP headers (same frame layout as current DHCP path)
  - ARP-resolves destination MAC via existing ARP table (gateway MAC for non-local destinations, broadcast for `255.255.255.255`)
  - Rewrite `transmit_dhcp_packet()` as a thin wrapper calling `transmit_udp_packet()`
- [x] **5E.3b** Add UDP checksum calculation in `net/mod.rs`:
  - `udp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], udp_header: &[u8], payload: &[u8]) -> u16`
  - IPv4 pseudo-header + UDP header + payload (same one's-complement accumulator as TCP/IPv4)
  - DHCP currently sends checksum=0 (optional per RFC 768); real UDP sockets should compute it

#### 5E.4: `sendto()` / `recvfrom()` Syscalls

- [x] **5E.4a** Define `SYSCALL_SENDTO` and `SYSCALL_RECVFROM` in `abi/src/syscall.rs`:
  - `SYSCALL_SENDTO`: `(sock_idx, buf_ptr, len, flags, dest_addr: *const SockAddrIn)` → bytes sent or negative errno
  - `SYSCALL_RECVFROM`: `(sock_idx, buf_ptr, len, flags, src_addr: *mut SockAddrIn)` → bytes received or negative errno
- [x] **5E.4b** Implement `socket_sendto()` in `drivers/src/net/socket.rs`:
  - Validate socket is `SOCK_DGRAM` and bound (or auto-bind to ephemeral port 49152–65535)
  - Build UDP packet via `transmit_udp_packet()` with destination from `SockAddrIn`
  - Return payload length on success, `ERRNO_ENETUNREACH` if NIC not ready
- [x] **5E.4c** Implement `socket_recvfrom()` in `drivers/src/net/socket.rs`:
  - Dequeue from socket's `UdpReceiveQueue`
  - If empty and blocking: `recv_wq.wait_event(|| !queue.is_empty())`
  - If empty and non-blocking: return `ERRNO_EAGAIN`
  - Respect `SO_RCVTIMEO` via `wait_event_timeout()`
  - Fill `src_addr` with sender's IP and port
  - Return payload length
- [x] **5E.4d** Wire handlers in `core/src/syscall/net_handlers.rs`:
  - Dispatch `SYSCALL_SENDTO` → `socket_sendto()`
  - Dispatch `SYSCALL_RECVFROM` → `socket_recvfrom()`
- [x] **5E.4e** Add userland wrappers in `userland/src/syscall/`:
  - `sendto(sock, buf, len, flags, addr) -> isize`
  - `recvfrom(sock, buf, len, flags, addr) -> isize`

#### 5E.5: UDP-Aware `send()` / `recv()` / `connect()`

- [x] **5E.5a** Extend `socket_connect()` for `SOCK_DGRAM`:
  - Remove the `if sock.sock_type != SOCK_STREAM` early return
  - Set `remote_ip` / `remote_port` as default destination (POSIX semantics)
  - State → `Connected` (allows `send()`/`recv()` without specifying address)
  - No handshake, no packets sent — purely local state change
- [x] **5E.5b** Extend `socket_send()` for connected UDP:
  - If `sock_type == SOCK_DGRAM && state == Connected`: send to stored `remote_ip:remote_port`
  - Delegate to `transmit_udp_packet()`
- [x] **5E.5c** Extend `socket_recv()` for UDP:
  - If `sock_type == SOCK_DGRAM`: dequeue from `UdpReceiveQueue` (discard source address)
  - Connected UDP only accepts datagrams from the connected peer (POSIX filtering)

#### 5E.6: `poll()` Readiness for UDP Sockets

- [x] **5E.6a** Extend `socket_poll()` for `SOCK_DGRAM`:
  - `POLLIN`: UDP receive queue non-empty
  - `POLLOUT`: always ready (UDP has no send backpressure)
  - `POLLERR` / `POLLHUP`: socket error state

#### Phase 5E Test Coverage

- [x] **5E.T1** UDP receive buffer: push/pop, overflow drops oldest, empty returns None
- [x] **5E.T2** UDP RX dispatch: packet to bound port delivered, unbound port silently dropped
- [x] **5E.T3** Generic UDP TX: construct frame, verify IPv4/UDP headers, checksum validation
- [x] **5E.T4** sendto/recvfrom roundtrip: send datagram, receive response, verify src_addr populated
- [x] **5E.T5** Connected UDP: `connect()` sets peer, `send()` uses default dest, `recv()` filters by peer
- [x] **5E.T6** poll() readiness: `POLLIN` set after enqueue, clear after dequeue
- [x] **5E.T7** Non-blocking: `recvfrom()` returns `EAGAIN` on empty queue
- [x] **5E.T8** Auto-bind: `sendto()` without prior `bind()` assigns ephemeral port
- [x] **5E.T9** DHCP regression: DHCP still works after `transmit_udp_packet()` refactor
- [x] **5E.T10** Regression: all existing TCP/socket tests still pass (718/718)

### 5F: DNS Client

> Implements DNS name resolution on top of the UDP datagram infrastructure from
> Phase 5E.  The resolver lives in-kernel (matching the DHCP client pattern) with
> a `SYSCALL_RESOLVE` interface for userland.  Future-compatible: can migrate to a
> userspace libc `getaddrinfo()` once SlopOS's C runtime matures.

#### 5F.1: DNS Wire Protocol

- [x] **5F.1a** Create `drivers/src/net/dns.rs`:
  - `DnsHeader` struct (12 bytes): `id`, `flags`, `qdcount`, `ancount`, `nscount`, `arcount`
  - `DnsFlags`: QR (query/response), RD (recursion desired), RA (recursion available), RCODE
  - `DnsType` enum: `A = 1`, `CNAME = 5` (AAAA deferred — SlopOS is IPv4-only)
  - `DnsClass::IN = 1`
  - `DnsRcode` enum: `NoError = 0`, `ServFail = 2`, `NXDomain = 3`, `Refused = 5`
- [x] **5F.1b** Implement DNS name encoding:
  - `dns_encode_name(hostname: &[u8], buf: &mut [u8]) -> Option<usize>`
  - Length-prefixed labels: `"example.com"` → `[7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0]`
  - Validate: no empty labels, each label ≤ 63 bytes, total name ≤ 253 bytes
- [x] **5F.1c** Implement DNS query construction:
  - `dns_build_query(id: u16, hostname: &[u8], qtype: DnsType, buf: &mut [u8]) -> Option<usize>`
  - Header: QR=0, OPCODE=0 (standard query), RD=1 (recursion desired), QDCOUNT=1
  - Question section: encoded name + QTYPE + QCLASS(IN)

#### 5F.2: DNS Response Parsing

- [x] **5F.2a** Implement DNS name decoding with compression pointer support:
  - `dns_decode_name(packet: &[u8], offset: usize, out: &mut [u8]) -> Option<(usize, usize)>`
  - Label types: regular (0x00–0x3F length prefix), compression pointer (0xC0 high bits)
  - Pointer loop detection: cap pointer follows at 16 to prevent infinite loops
  - Returns `(decoded_name_len, wire_bytes_consumed)`
- [x] **5F.2b** Implement DNS answer section parsing:
  - `dns_parse_response(packet: &[u8], expected_id: u16) -> Option<DnsResponse>`
  - Validate: QR=1, ID matches query, RCODE == NoError
  - Parse answer RRs: skip name, read TYPE, CLASS, TTL, RDLENGTH, RDATA
  - Extract A records: RDATA = 4-byte IPv4 address
  - Chase CNAME records: if answer is CNAME, follow the canonical name to its A record (max 8 hops)
  - `DnsResponse { addr: [u8; 4], ttl: u32 }`

#### 5F.3: Resolver

- [x] **5F.3a** Implement `dns_resolve(hostname: &[u8]) -> Option<[u8; 4]>`:
  - Get DNS server IP from `VirtioNetState.dns` (DHCP-provided)
  - Check cache first (`dns_cache_lookup()`)
  - Build A-record query via `dns_build_query()`
  - Send via `transmit_udp_packet()` (src port = ephemeral, dst port = 53)
  - Wait for response with timeout via `DNS_RX_EVENT: QueueEvent` (same pattern as DHCP)
  - Parse response via `dns_parse_response()`
  - Cache result via `dns_cache_insert()`
  - Retry once on timeout, then return `None`
- [x] **5F.3b** Wire DNS RX in `dispatch_rx_frame()`:
  - When UDP src_port == 53: deliver payload to `DNS_RX_EVENT` + stash in `DNS_RX_BUF`
  - Follows the existing `DHCP_RX_EVENT` pattern
- [x] **5F.3c** Implement DNS cache:
  - 16-entry array: `DnsCacheEntry { hostname_hash: u32, addr: [u8; 4], expiry_ms: u64 }`
  - TTL-based expiry via `clock::uptime_ms()`
  - LRU eviction when full (track last-used timestamp)
  - `dns_cache_lookup(hostname) -> Option<[u8; 4]>`
  - `dns_cache_insert(hostname, addr, ttl_secs)`
  - `dns_cache_flush()` for test isolation

#### 5F.4: Syscall Interface

- [x] **5F.4a** Define `SYSCALL_RESOLVE` in `abi/src/syscall.rs`:
  - `rdi` = hostname pointer (null-terminated `*const u8`)
  - `rsi` = result pointer (`*mut [u8; 4]`)
  - Returns 0 on success, negative errno on failure (`EHOSTUNREACH`, `ETIMEDOUT`)
- [x] **5F.4b** Implement handler in `core/src/syscall/net_handlers.rs`:
  - Copy hostname from user memory (validate pointer, cap at 253 bytes)
  - Call `dns_resolve()`, copy resolved address back to user pointer
- [x] **5F.4c** Add userland wrapper: `resolve(hostname: &[u8]) -> Option<[u8; 4]>` in `userland/src/syscall/`
- [x] **5F.4d** Add `resolve` userland command:
  - Usage: `resolve example.com`
  - Calls `SYSCALL_RESOLVE`, prints `example.com -> 93.184.216.34` or error message

#### Phase 5F Test Coverage

- [x] **5F.T1** DNS name encoding: valid hostnames, empty label rejection, max-length enforcement
- [x] **5F.T2** DNS query construction: header flags, question section, wire format roundtrip
- [x] **5F.T3** DNS name decoding: regular labels, compression pointers, loop detection cutoff
- [x] **5F.T4** DNS response parsing: valid A record, CNAME chasing, RCODE error handling, ID mismatch rejection
- [x] **5F.T5** DNS cache: insert/lookup hit, TTL expiry miss, LRU eviction, flush
- [x] **5F.T6** Resolver integration: resolve known hostname via QEMU user-net DNS (10.0.2.3)
- [x] **5F.T7** Resolver timeout: unreachable DNS server returns `None` within timeout
- [x] **5F.T8** Regression: all existing TCP/socket/network tests still pass

### Phase 5 Gate

- [ ] **GATE**: TCP three-way handshake works (connect to a remote server)
- [ ] **GATE**: TCP data transfer works (send/receive payloads)
- [x] **GATE**: Socket syscalls implemented (socket, connect, send, recv, close)
- [ ] **GATE**: Simple TCP echo test passes (connect, send, receive echo, close)
- [x] **GATE**: `just test` passes
- [ ] **GATE**: Can connect to a TCP server from QEMU guest (e.g., netcat)
- [x] **GATE**: VirtIO net uses NAPI-style batched RX (no per-packet blocking)
- [x] **GATE**: TX is fire-and-forget (no `REQUEST_TIMEOUT_MS` blocking)
- [x] **GATE**: `recv()`/`accept()` properly sleep via wait queues
- [x] **GATE**: `poll()` returns real socket readiness (not stubbed)
- [x] **GATE**: UDP sockets work end-to-end (`sendto`/`recvfrom` on `SOCK_DGRAM`)
- [x] **GATE**: `dispatch_rx_frame()` delivers incoming UDP packets to bound sockets
- [x] **GATE**: DHCP still works after `transmit_udp_packet()` refactor
- [ ] **GATE**: DNS resolver can resolve a hostname via QEMU user-net (10.0.2.3)
- [ ] **GATE**: `resolve` command works from the userland shell

---

## 9. Phase 6: PCID / TLB Optimization — **SHIPPED**

> **Status**: landed as a ground-up MMU redesign that supersedes the
> original 4096-PCID-bitmap sketch below. See §9.1 for what actually
> shipped; the pre-implementation checklist is kept only for historical
> reference.

### 9.1 What landed

The simple per-process-PCID-bitmap sketch in §9.3 was replaced with a
Linux-style per-CPU ASID dance, Amit-style shootdowns, LUF, and a
typed MMU surface. The work lives under `mm/src/mmu/`, `mm/src/tlb.rs`,
and touchpoints across `boot/`, `core/`, `karch/`, `sync/`, `tests/`.

**Supervisor hardening**

- `CR4.PGE`, `CR4.SMEP`, `CR4.SMAP` enabled on BSP + every AP
  (`karch/src/cpu/security.rs`).
- Kernel leaf PTEs stamped with the `GLOBAL` bit
  (`paging_mark_kernel_global`).
- `raw_usercopy` wraps `rep movsb` with `stac`/`clac`.

**Typed CR3 primitives** (`mm/src/mmu/cr3.rs`)

- `Cr3Value` newtype, `Pcid` (12-bit), `MmContextId` (64-bit, monotonic,
  never reused). Every `cpu::write_cr3` call routed through
  `mmu::write_cr3_value`.

**Per-CPU ASID dance** (`mm/src/mmu/asid.rs`)

- 16 slots/CPU, Linux-style `(ctx_id, tlb_gen, loaded_mm)` keyed off
  the stable 64-bit `MmContextId`. Hot-path context switch writes
  CR3 with `NOFLUSH=1` when the ASID is still loaded; miss rotates
  slots and issues `INVPCID type 1`.
- `CR4.PCIDE` enabled at boot via `mmu::init_bsp` / `mmu::init_ap`.

**Amit-style shootdown** (`mm/src/tlb.rs`)

- Early ACK in `handle_shootdown_ipi` (ack before flush).
- Concurrent local flush on the initiator, overlapping the IPI
  round-trip.

**LUF (Lazy Unmap Flush)** (`mm/src/mmu/luf.rs`)

- Per-CPU 64-entry ring of `(vaddr, phys, ctx_id, pcid)`.
- Global `LUF_NONEMPTY_MASK` → fast-path skip when no CPU has
  pending entries.
- `LUF_DRAIN_IPI_VECTOR = 0xFA` cross-CPU drain-by-phys, Amit-style
  early ACK. Targets only CPUs with bits in the mask.
- `alloc_page_frame` calls `drain_if_reusing_frame(phys)`; `sys_munmap`
  calls `drain_local()` before sysret.
- Legacy `ProcessTlbInfo::tlb_gen` / `exit_lazy_tlb` catch-up /
  `flush_page_local_for_process` **ripped**.

**Typed kernel mappings** (`mm/src/mmu/mapping.rs`)

- `#[must_use]` `KernelMapping` with owned/borrowed frame semantics,
  Drop-safe unmap + TLB flush + conditional frame free.

**Typed teardown guard** (`mm/src/paging/tables.rs`)

- `MmTeardownGuard::begin(page_dir)` issues one
  `flush_all_for_process` + `unregister_process_tlb`, then
  `unmap_page` on the guard skips LUF / per-page coherence by type
  construction. `teardown_inner_mappings` uses it; closed a shutdown
  regression where LUF overflows during destroy re-enabled IRQs.

**Errata + scaffolding**

- `mmu::errata` — Alder/Raptor Lake INVLPG+PCID blacklist.
- `mmu::rar` — `trait ShootdownBackend`, `SoftwareIpi` concrete,
  `IntelRar::detect` stub.
- `mmu::kpti` + `mm/src/mmu/trampoline.s` — dual-PML4 scaffolding,
  CR3-swap template; `KPTI_ENABLED` defaults `false`, activation is
  a one-file change once the trampoline is linked into the IDT /
  SYSCALL MSR targets.

**Also landed along the way**

- `karch/src/pcr.rs` AP-count race: `init_ap_pcr` uses
  `fetch_max` instead of load+compare+store, fixing sysmon reporting
  fewer cores than online when APs came up out of order.

### 9.2 Gate

- [x] PCID allocated per context (per-CPU, not per-process bitmap) —
  delivered via ASID dance.
- [x] Context switch writes PCID-aware CR3 with `NOFLUSH` bit.
- [x] TLB entries survive context switches (hot-path CR3 writes set
  `NOFLUSH=1`).
- [x] TLB shootdown uses INVPCID when available (`mmu::asid::flush_pcid`
  → `INVPCID type 1`, fallback CR3 reload otherwise).
- [x] Fallback to non-PCID path works on errata-affected /
  unsupported CPUs (`mmu::errata::should_disable_pcid`).
- [x] `just test` passes — 2393/2393 green with live LUF counters.
- [x] No TLB-related crashes or stale mapping bugs under the
  exercised workloads.

### 9.3 Original sketch (historical)

Kept for reference — the pre-implementation plan, superseded by §9.1.

~~- **6A.1** Create PCID allocator in `mm/src/tlb.rs`:
  - 12-bit PCID → 4096 possible values (0 is reserved for kernel)
  - Simple bitmap allocator: `pcid_bitmap: [AtomicU64; 64]` (4096 bits)
  - `alloc_pcid() -> Option<u16>`, `free_pcid(pcid: u16)`~~ (replaced
  by per-CPU 16-slot pool keyed off `MmContextId`, matching Linux's
  model; a global bitmap across 4096 PCIDs has worse steady-state
  behaviour than a small per-CPU pool with generation-based reuse).

~~- **6A.2** Assign PCID to each process~~ — replaced by `MmContextId`
  (64-bit, stable, never reused). Hardware PCID is a per-CPU slot
  binding, not a per-process attribute.

~~- **6A.3** Handle PCID exhaustion~~ — the per-CPU 16-slot pool
  rotates via `next_asid`; `INVPCID type 1` flushes the evicted slot.

~~- **6B.*** — replaced by `mmu::write_cr3_value(Cr3Value)` choke
  point and the ASID dance in `mmu::select_cr3`.

~~- **6C.*** — replaced by LUF cross-CPU drain-by-phys IPI
  (`LUF_DRAIN_IPI_VECTOR`) plus Amit-style shootdown in `mm::tlb`.

---

## 10. Phase 7: Long-Horizon Hardware

> **Major new subsystems that complete the hardware story.**
> **These are massive efforts — each could be its own plan document.**
> **Depends on**: Phases 0–4 as foundation

### 7A: USB / xHCI Stack

Replaces PS/2 as the input mechanism and enables USB mass storage, USB networking, etc.

- [ ] **7A.1** Implement xHCI (USB 3.x) host controller driver:
  - Discover xHCI via PCI (class 0x0C, subclass 0x03, progif 0x30)
  - Map MMIO registers (capability, operational, runtime, doorbell)
  - Initialize: reset controller, set up device context base array, configure interrupter
  - Command ring + event ring + transfer ring management
- [ ] **7A.2** Implement USB device enumeration:
  - Address assignment, device descriptor reading
  - Configuration descriptor parsing
  - Interface and endpoint descriptor handling
- [ ] **7A.3** Implement USB HID driver (keyboard + mouse):
  - HID report descriptor parsing (boot protocol as minimum)
  - Interrupt IN endpoint for key events
  - Replace PS/2 keyboard/mouse as primary input
- [ ] **7A.4** Implement USB mass storage driver (optional):
  - Bulk-Only Transport (BOT) protocol
  - SCSI command set (INQUIRY, READ, WRITE)
  - Integrate with VFS as block device

**Estimated effort**: Very High (months of work). Consider as post-MVP.

### 7B: VirtIO GPU (Replace Raw Framebuffer)

- [ ] **7B.1** Implement VirtIO GPU driver:
  - Resource management (create, attach backing, transfer)
  - Scanout configuration
  - Cursor support
  - 2D acceleration (blit, fill)
- [ ] **7B.2** Integrate with compositor:
  - Replace direct framebuffer writes with VirtIO GPU commands
  - Enables hardware-accelerated compositing
  - Double buffering without manual page flipping

### 7C: Hardware RTC (Real-Time Clock)

- [ ] **7C.1** Parse ACPI FADT for RTC info (or use CMOS RTC at ports 0x70/0x71)
- [ ] **7C.2** Read current date/time on boot
- [ ] **7C.3** Combine with HPET monotonic clock for wall-clock time
- [ ] **7C.4** Implement `SYSCALL_CLOCK_GETTIME` with `CLOCK_REALTIME`

### 7D: Power Management

- [ ] **7D.1** Parse ACPI FADT for PM1a/PM1b control block addresses
- [ ] **7D.2** Implement proper ACPI S5 (soft-off) shutdown:
  - Currently uses hardcoded QEMU ports (`0x604`, `0xB004`, `0x4004`)
  - Should parse SLP_TYP from `_S5_` ACPI object and write to PM1a_CNT
- [ ] **7D.3** Implement ACPI S3 (sleep) support (stretch)

### 7E: `pic.rs` Cleanup

Framekernel/Asterinas interpretation: the 8259 PIC is a **sensitive
system interrupt controller**, not a driver-owned device.  ACPI MADT
`PCAT_COMPAT` reports whether a PC-AT-compatible dual-8259 setup exists;
when present, ACPI requires those vectors to be disabled (masked) while
enabling APIC operation.  SlopOS therefore keeps exactly one OSTD-owned,
boot-only init-disable primitive and removes all driver/public PIC
ownership.

- [x] **7E.1** Move the 8259 init-disable lifecycle into OSTD behind a BSP-token boot primitive
- [x] **7E.2** Delete `drivers/src/pic.rs` entirely
- [x] **7E.3** Remove `pub mod pic;` from `drivers/src/lib.rs`
- [x] **7E.4** Remove PIC ports from the client `IoPortRegistry` allowlist; PIC access is OSTD-internal only

---

## 11. Dependency Graph

```
Phase 0: Timers ─────────────┐
  (HPET + LAPIC timer)       │
                              │
Phase 1: XSAVE ──────────────┤ (all independent)
  (FPU/SIMD modernization)   │
                              │
Phase 2: Spinlocks ───────────┤
  (ticket locks)              │
                              │
Phase 3: MSI/MSI-X ──────────┤──→ Phase 4: ECAM ──→ Phase 7A: USB/xHCI
  (PCI cap parsing first)    │     (MCFG parsing)    (needs ECAM + MSI)
                              │
Phase 5: Network ────────────┤ (independent, builds on existing VirtIO net)
  5A-5C: TCP state machine,  │   5D: NAPI + wait queues + async I/O
  data transfer, sockets      │   5E: UDP sockets   5F: DNS resolver
                              │
Phase 6: PCID ────────────────┘ (independent, best after Phase 0+2)

Phase 7: Long-horizon
  7A: USB/xHCI      ← needs Phase 3 (MSI) + Phase 4 (ECAM)
  7B: VirtIO GPU    ← needs Phase 3 (MSI)
  7C: RTC           ← independent
  7D: Power Mgmt    ← independent (ACPI parsing exists)
  7E: pic.rs cleanup ← independent (trivial)
```

### Recommended Execution Order

| Order | Phase | Rationale |
|---|---|---|
| 1st | Phase 0 (Timers) | Highest impact, removes biggest legacy dependency |
| 2nd | Phase 2 (Spinlocks) | Low effort, immediate SMP improvement |
| 3rd | Phase 1 (XSAVE) | Medium effort, unlocks AVX for userland |
| 4th | Phase 4 (ECAM) | Foundation for Phase 3 (extended config space) |
| 5th | Phase 3 (MSI/MSI-X) | Requires PCI cap parsing, benefits from ECAM |
| 6th | Phase 6 (PCID) | Performance optimization, best after scheduler is stable |
| 7th | Phase 5 (TCP) | Massive effort, independent track |
| 8th | Phase 7 (Long-horizon) | Post-MVP work |

---

## 12. Blocked Features Reference

Features that **cannot be implemented** until specific phases complete:

| Feature | Blocked By | Phase Required |
|---|---|---|
| Precise sleep/timeout | No high-resolution clock | Phase 0 (HPET) |
| Per-CPU scheduler ticks | PIT is single-IRQ shared | Phase 0 (LAPIC timer) |
| AVX in userland programs | FXSAVE can't save YMM regs | Phase 1 (XSAVE) |
| AVX-512 | FXSAVE + target JSON | Phase 1 (XSAVE) |
| Fair lock acquisition on SMP | Test-and-set has no fairness | Phase 2 (Ticket locks) |
| Per-device interrupt vectors | Legacy shared IRQ lines | Phase 3 (MSI) |
| VirtIO multi-queue networking | Needs per-queue MSI-X vectors | Phase 3 (MSI-X) |
| Full PCIe config space (4KB) | Port I/O limited to 256B | Phase 4 (ECAM) |
| HTTP, SSH, any TCP protocol | No TCP state machine | Phase 5 (TCP) |
| NTP, mDNS, any UDP protocol | No UDP socket send/receive | Phase 5E (UDP Sockets) |
| DNS hostname resolution | No DNS resolver | Phase 5F (DNS) |
| TLB-efficient context switch | ✓ shipped via per-CPU ASID dance + LUF | Phase 6 ✓ |
| USB keyboard/mouse | No xHCI driver | Phase 7A (USB) |
| Hardware-accelerated graphics | No VirtIO GPU driver | Phase 7B |
| Wall-clock time / date | No RTC integration | Phase 7C |
| Proper async `recv()`/`accept()` | Per-packet blocking in VirtIO net | Phase 5D (Async I/O) |
| Real `poll()` readiness on sockets | `poll()` stubbed to always-ready | Phase 5D (Async I/O) |
| Clean ACPI shutdown (non-QEMU) | Hardcoded port addresses | Phase 7D |

---

## Progress Tracking

| Phase | Status | Tasks | Done | Blocked |
|---|---|---|---|---|
| **Phase 0**: Timer Modernization | **Complete** | 31 | 31 | — |
| **Phase 1**: XSAVE/XRSTOR | **Complete** | 14 | 14 | — |
| **Phase 2**: Spinlock Modernization | **Complete** (2C MCS deferred, `spin` removed) | 12 | 12 | — |
| **Phase 3**: MSI/MSI-X | **Complete** (3A, 3B, 3C, 3D, 3E all done) | 27 | 27 | — |
| **Phase 4**: PCIe ECAM | **Complete** | 9 | 9 | — |
| **Phase 5**: Network Stack | 5A+5B+5C+5D+5E **Complete**, 5F next | 97 | 73 | — |
| **Phase 6**: PCID / TLB | **Complete** (ground-up MMU redesign: PGE/SMEP/SMAP, typed CR3, per-CPU ASID dance, Amit shootdown, LUF with cross-CPU drain, MmTeardownGuard, KPTI scaffold) | 9 | 9 | — |
| **Phase 7**: Long-Horizon | Not Started | 16 | 0 | Phases 0–4 |
| **Total** | | **196** | **141** | |
