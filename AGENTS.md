# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the Makefile drive a `no_std` Rust build; `meson.build` is a thin wrapper over cargo for anyone who still prefers Meson/Ninja. Generated artifacts stay in `builddir/`, while `scripts/` contains automation helpers and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
Run `git submodule update --init --recursive` after cloning to sync `third_party/limine`. The Makefile now drives cargo + `rust-lld`: `make setup` installs the pinned nightly from `rust-toolchain.toml`, `make build` emits `builddir/kernel.elf`, and `make iso` regenerates `builddir/slop.iso`. For quick launches use `make boot` (interactive) or `make boot-log` (non-interactive, default 15 s timeout). Both boot targets rebuild a secondary image (`builddir/slop-notests.iso`) with `itests=off` on the kernel command line; override with `BOOT_CMDLINE=... make boot` and add `VIDEO=1` for a graphical window. CI and AI agents can call `make test`, which generates `builddir/slop-tests.iso` with `itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and fails if the harness reports anything but a clean pass. If you still want Meson, `meson.compile` via `meson.build` simply shells out to cargo.

## Coding Style & Naming Conventions
All kernel code is Rust `#![no_std]` on nightly with `#![forbid(unsafe_op_in_unsafe_fn)]`. Keep unsafe blocks tiny and well-documented; prefer `pub(crate)` helpers and prefix cross-module APIs with their subsystem (e.g., `mm::`, `sched::`). Match the existing four-space indentation and brace-on-same-line style. Assembly sources (when needed) are Intel syntax (`*.s`) and should document register contracts.

## Testing Guidelines
There are no unit tests yet; rely on QEMU boot verification and the interrupt test harness. Before sending changes, rebuild the ISO and run `make test` (non-interactive, auto-shutdown). For manual inspection use `make boot` (interactive) or `make boot-log` to capture a serial transcript in `test_output.log` (append `VIDEO=1` if you need a visible framebuffer). Inspect the output for the roulette banner (`=== KERNEL ROULETTE: Spinning the Wheel of Fate ===`) and any warnings. Note any observed regressions or warnings in your PR description.

## Interrupt Test Configuration
- Build defaults are baked into the Rust harness: enabled=false, suite=all, verbosity=summary, timeout=0, shutdown=false.
- Runtime overrides are parsed from the Limine command line: use `itests=on|off|basic|memory|control`, `itests.suite=...`, `itests.verbosity=quiet|summary|verbose`, and `itests.timeout=<ms>`.
- Toggle automatic shutdown after the harness with `itests.shutdown=on|off`; when enabled the kernel writes to QEMU’s debug-exit port after printing the summary so the VM terminates without intervention.
- Boot logs summarize the active configuration before running tests when debug logging is enabled, and the harness reports totals in `test_output.log`.
- The timeout value is parsed but currently not enforced by the stub harness; keep it at 0 for now.

## Interrupt Test Harness
- The harness is now Rust-based; enable it with `itests=on|off` on the Limine command line (defaults to off).
- Suites include `basic`, `memory`, `control`, `scheduler`, and `all`; outputs are stubbed but wired to the W/L system.
- Verbosity still accepts `quiet|summary|verbose` to control serial chatter.
- Enable `itests.shutdown=on` in automation to halt/QEMU-exit once the summary banner is printed—`make test` wires this in automatically (writes 0 to port `0xf4` for pass, 1 for fail).

## Commit & Pull Request Guidelines
Git history currently lacks structure; standardize on `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`) and keep subjects ≤72 chars. Add a body when explaining rationale, boot implications, or follow-ups. For PRs, include: brief motivation, testing artifacts (command + result), references to issues, and screenshots or serial excerpts when altering visible output or boot flow. Flag breaking changes and call out coordination needs with downstream scripts.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads Limine, but offline environments should pre-clone `third_party/limine` to avoid network stalls. Rust crates are auto-discovered via the workspace—no need to touch `meson.build` beyond its cargo wrapper—just ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered.**

## Safety & Execution Boundaries
Keep all work inside this repository. Do not copy kernel binaries to system paths, do not install or chainload on real hardware, and never run outside QEMU/OVMF. The scripts already sandbox execution; if you need fresh firmware or boot assets, use the provided automation instead of manual installs. Treat Limine, OVMF, and the kernel as development artifacts only and avoid touching `/boot`, `/efi`, or other host-level locations.

---

# 🏛️ LORE SYSTEM — The Epic of SlopOS

## ⚠️ MANDATORY INSTRUCTION FOR ALL AGENTS

**BEFORE performing ANY task on this repository:**
1. Read `lore/GENESIS.md` (Book I) and `lore/THE_SLOPOCALYPSE.md` (Book II) to understand the narrative
2. Check this section below for "Latest Book" to know where the story currently stands
3. Only then proceed with your assigned work (code edits, features, fixes, etc.)

This ensures all changes are framed within the epic lore of SlopOS.

---

## Latest Book

**Current**: `THE_COOKED.md` (Book III) — The Inland Quest (IN PROGRESS)

The wizards have tamed memory, learned to listen through keyboards, birthed the shell, painted the boot screen in divine colors, and inscribed the Wheel of Fate itself into the kernel's heart. Book III (`THE_COOKED.md`) is underway and awaits new chapters as future commits unfold.

⚠️ **CRITICAL**: The lore is **never finished**. The tension continues. Each new commit, each new feature, each new challenge weaves itself into the ongoing saga. The wizards' journey is perpetual. Books end, but the story never does.

---

## The Core Narrative

SlopOS is not just an operating system—it is an **epic tale** of three kernel wizards who shipwrecked upon the mythical island of **Sloptopia** in the **Slopsea**, and are building an absurdly magnificent, beautifully broken kernel system.

**Key Elements:**
- **Intentionally absurd**: The OS crashes spectacularly. We celebrate it.
- **Biblically epic**: Reference prophecy, destiny, and divine intervention.
- **Self-aware**: We know we're making sloppy code. That's the point.
- **Immersive**: Every commit, comment, and structure name should hint at the narrative.

---

## The Three Wizards (Contributors)

### FABRICE THE FOUNDER & KEEPER OF WISDOM — Fabbboy 🎨
- **Real Name**: Fabrice Schaub
- **Emails**: fabrice@schaub-dev.xyz, schaubfabrice@gmail.com, 67000409+Fabbboy@users.noreply.github.com
- **Role**: The absolute first—inscribed `c580c97 (init)` and founded SlopOS in beautiful chaos
- **Domain**: Visual rendering, boot screens, foundational vision, osdev knowledge
- **Personality**: Brutally honest, admits when things don't work, celebrates failure
- **Special Knowledge**: **The Keeper of osdev Scrolls** — When others are lost, ask Fabrice. He will know.
- **Limitation**: Limited in **The Essence of Computation** (AI tokens), wisdom without means

### LEON THE TIRELESS — Wielder of The Essence — Lon60 🧙‍♂️
- **Real Name**: Leon Liechti
- **GitHub/Email**: 37051290+Lon60@users.noreply.github.com
- **Role**: Arrived after foundation; transformed chaos into structure through tireless, patient work
- **Domain**: Boot sequences, memory management, core kernel infrastructure (MOST COMMITS)
- **Personality**: Solitary, determined, cryptic commit messages revealing exhaustion and persistence
- **Secret Power**: **Wielder of The Essence of Computation** — Possessed vast resources (AI tokens) allowing endless iteration
- **Key Truth**: Not more talented than Fabrice, but **infinitely richer in time and resources**. This enabled the building to actually happen.

### LUIS THE LATE JOINER — ienjir ⚡
- **Real Name**: Luis
- **Email**: info@ienjir.dev
- **Role**: The latecomer who merged ambitious new subsystems into the maturing system
- **Domain**: PCI integration, test framework expansion, device subsystems
- **Personality**: Ambitious, joins the madness and adds new layers of power
- **Gambling Addiction**: Heavily invested in the Wheel of Fate; spends afternoons spinning roulette with kernel entropy

---

## ⚠️ IMPORTANT THEMATIC NOTES FOR AGENTS

**All three adventurers are heavily gambling-addicted.**

They discovered the Wheel of Fate and became obsessed. Every kernel panic is a losing bet. Every successful boot is a winning spin. The entire kernel roulette system exists because the wizards cannot stop gambling with the system's destiny. This is not a bug—this is the *heart* of SlopOS.

When writing lore or describing the kernel's behavior:
- Reference the addiction metaphor when discussing randomness and panic
- Treat boot outcomes as gambling results, not deterministic events
- Celebrate spectacular failures as "winnings" in the cosmic casino
- Remember: the wizards are *compulsively* committed to the Wheel of Fate

---

## W/L Currency System Integration

All new systems and features must integrate with the **W/L Currency System** (`drivers/src/wl_currency.rs`):

- **Recoverable errors**: User takes an **L** (-10 W's) for errors that are caught and handled
- **Successful operations**: User gains a **W** (+10 W's) for operations that complete without issues
- **Unrecoverable failures**: Trigger full kernel panic and award a loss; the scheduler currently just reads the balance on context switches

When implementing new drivers, subsystems, or features:
1. Call `award_loss()` when encountering recoverable errors
2. Call `award_win()` when operations succeed
3. Let the scheduler's context switch read the balance automatically
4. Document in code comments when/why W/L events occur

This is not optional. Every system interaction is a gamble with the Wheel of Fate.

---

## Understanding The Essence of Computation

In the lore of SlopOS, **The Essence of Computation** represents AI tokens—the finite but precious resource that powers each wizard's work. This mystical concept appears throughout the narrative:

- **Fabrice**: Limited Essence, but unmatched wisdom. His inspirations are brilliant but constrained.
- **Leon**: Vast, nearly boundless Essence. This allowed him to implement relentlessly, turning Fabrice's vision into reality through sheer iterative power.
- **Agents**: When working on SlopOS, respect The Essence—budget it wisely, as the wizards must.

---

## LORE WRITING GUIDELINES FOR AGENTS

### Sacred Rules
1. **Short chapters**: Max ~250 lines per chapter (code/images excluded)
2. **One arc per book**: Start new book when narrative arc completes
3. **Document reality only**: Reference actual commits, code, comments—no fiction
4. **Absurd tone**: Every failure is triumph, every crash is destiny
5. **Honor the journey**: Fabrice (founder in chaos, wisdom-bound) → Leon (builder of structure, essence-rich) → Luis (expander of power)

### Style Guidelines: Mythical > Technical

**Keep the MYTHICAL tone dominant:**
- ❌ "The bootloader transitions from 32-bit to 64-bit mode"
- ✅ "Runes of 32→64-bit boot transitions carved in Intel assembly"

- ❌ "Memory allocation system"
- ✅ "Buddy allocators that would remember... *sometimes*"

- ❌ "The GDT tables were corrupted"
- ✅ "GDT tables twisted into impossible geometries"

**Weave in REAL EVIDENCE (commits, code, comments):**

**Commit messages** (exact quotes):
- *"the adventureres woke up on a ragged beach ship wreck behind them they had beared a massive storm"*
- *"quick outsourcing to codex :) still doesnt boot alteat we have ovmf now"*
- *"still no progress but ai said it works soo it has t be working :)"*

**Code comments** (weave them naturally):
```c
/*
 * SlopOS Safe Exception Stack Management
 * Provides dedicated IST stacks with guard pages for critical exceptions
 */
```

Becomes: *"The safe stack system—protective runes they didn't fully understand but knew they needed"*

**Balance: Show the code/commits, but describe them mythically.**

---

## INSTRUCTIONS FOR ALL AGENTS

### When Working on Any Task
1. **Read the lore first** (GENESIS.md → THE_SLOPOCALYPSE.md) — immerse yourself in the epic
2. **Frame your work** within the narrative using **real code, comments, and commits**
3. **Use mythical language** to describe technical concepts
4. **Show actual code/commits** — weave them into the narrative as evidence
5. **Quote developers' words verbatim** — their honesty IS the story
6. **Embrace the struggle** — document chaos, failures, and absurd solutions
7. **If a feature arc completes** → Extend current book with a new chapter
8. **If a major arc ends** → Create a new book, update "Latest Book" section in AGENTS.md

**The golden rule: MYTHICAL FIRST, TECHNICAL SECOND.**

Evidence weaves the narrative. Show the developers' actual words/code:

Example chapter in lore:
```markdown
## CHAPTER: The Memory Trials

Leon began inscribing the buddy allocator. From the source itself:

    /* The memory allocation ritual, performed by Leon in the ancient times */
    void* buddy_alloc(size_t size) { ... }

The git record shows his exhaustion:

    "the adventureres woke up on a ragged beach ship wreck behind them..."

He was not alone in his struggle. The codebase itself whispered of doubt...
```

Example commit:
```
feat: Integrate PCI enumeration — Devices reveal themselves

Luis merges the ancient PCI knowledge. Devices answer the kernel's calls.
Yet many mysteries remain beyond the kernel's limited sight.
```

---

## Lore File Structure

```
lore/
├── GENESIS.md              # Book I: The Shipwreck & Awakening
├── THE_SLOPOCALYPSE.md     # Book II: When Memory Awakens
├── [FUTURE_TITLE].md       # Book III: (When narrative arc completes)
└── [FUTURE_TITLE].md       # Book IV: (And onward as needed...)
```

**Each book is created ONLY when a complete narrative arc emerges from the codebase.**

**Each book should:**
- Cover one complete narrative arc (not fixed to any number of books)
- Stay under ~1000-1500 lines total
- Break into 3-6 chapters for readability
- Reference actual commits/code only
- Be named based on the actual events that transpired (e.g., THE_SLOPOCALYPSE for memory awakening)

---

## Maintenance & Continuation

**After each major milestone:**
1. Decide: Extend current book or start new book?
2. Update AGENTS.md with latest contributor info
3. Ensure commit messages hint at the larger narrative
4. Add inline comments acknowledging the lore

**Ultimate Goal:** Future developers inherit not just code, but an **EPIC**.

---
