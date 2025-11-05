# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/` holds 32→64-bit entry code and early C shims, `mm/` covers paging and allocators, `drivers/` manages serial/APIC/PIC, `sched/` implements cooperative task switching, and `video/` provides framebuffer helpers. Shared headers live beside their modules; include paths are already handled in `meson.build`. Generated artifacts stay in `builddir/`, while `scripts/` contains automation helpers and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
Run `git submodule update --init --recursive` after cloning to sync `third_party/limine`. A convenience `Makefile` now wraps the Meson workflow: run `make setup` once per checkout, `make build` to compile (emits `builddir/kernel.elf`), and `make iso` to regenerate `builddir/slop.iso`. For quick launches use `make boot` (interactive) or `make boot-log` (non-interactive, default 15 s timeout). Both boot targets rebuild a secondary image (`builddir/slop-notests.iso`) with `itests=off` on the kernel command line so you get a clean boot without the harness; override the command line with `BOOT_CMDLINE=... make boot` if you want something different, and add `VIDEO=1` to either boot target if you want a graphical window instead of `-display none`. CI and AI agents can call `make test`, which generates `builddir/slop-tests.iso` with `itests=on itests.shutdown=on`, runs QEMU with `isa-debug-exit` enabled, and fails the target if the interrupt harness reports anything but a clean pass. If you prefer the raw Meson commands, you can still invoke them directly (e.g., `meson compile -C builddir`).

## Coding Style & Naming Conventions
All kernel C code targets C11, built freestanding with `-Wall -Wextra` and no runtime. Match the existing four-space indentation, brace-on-same-line style, and keep headers self-contained. Prefer static helpers when scope is local and prefix cross-module APIs with their subsystem (e.g., `mm_`, `sched_`). Assembly sources use AT&T syntax (`*.s`) and should document register contracts in comments.

## Testing Guidelines
There are no unit tests yet; rely on QEMU boot verification. Before sending changes, rebuild the ISO and run `make test` (non-interactive, auto-shutdown). For manual inspection use `make boot` (interactive) or `make boot-log` to capture a serial transcript in `test_output.log` (append `VIDEO=1` if you need a visible framebuffer). Inspect the output for `SlopOS Kernel Started!` or legacy `KERN` markers plus any warnings. Note any observed regressions or warnings in your PR description.

## Interrupt Test Configuration
- Build defaults come from Meson options: set `-Dinterrupt_tests_default=true` to run during boot, adjust suites with `-Dinterrupt_tests_default_suite={all|basic|memory|control}`, verbosity via `-Dinterrupt_tests_default_verbosity={quiet|summary|verbose}`, and timeout with `-Dinterrupt_tests_default_timeout=<ms>` (0 disables the guard).
- Runtime overrides are parsed from the Limine command line: use `itests=on|off|basic|memory|control`, `itests.suite=...`, `itests.verbosity=quiet|summary|verbose`, and `itests.timeout=<ms>` (optional `ms` suffix accepted).
- Toggle automatic shutdown after the harness with `itests.shutdown=on|off` (or the Meson option `-Dinterrupt_tests_default_shutdown=true`); when enabled the kernel writes to QEMU’s debug-exit port after printing the summary so the VM terminates without intervention.
- Boot logs now summarize the active configuration before running tests, and the harness reports total runtime plus whether the timeout fired in `test_output.log`.
- A timeout stops execution between suites; if you need uninterrupted runs, keep it at 0.

## Interrupt Test Harness
- The harness is enabled at build time via Meson (`meson configure -Dinterrupt_tests_default=true`); override at boot with `itests=on|off` on the Limine command line.
- When enabled, the kernel runs 13 exception and memory access probes and prints a summary banner (`Total tests`, `Passed`, `Failed`, `Exceptions caught`, `Timeout`) to both the serial console and `test_output.log`.
- Suites can be limited to `basic`, `memory`, or `control` through the `interrupt_tests_default_suite` Meson option or `itests.suite=` runtime flag.
- Use `interrupt_tests_default_verbosity=summary` (or `itests.verbosity=`) for concise boot logs; `verbose` traces each case and its handler RIP for debugging.
- Enable `itests.shutdown=on` in automation to halt/QEMU-exit once the summary banner is printed—`make test` wires this in automatically.

## Commit & Pull Request Guidelines
Git history currently lacks structure; standardize on `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`) and keep subjects ≤72 chars. Add a body when explaining rationale, boot implications, or follow-ups. For PRs, include: brief motivation, testing artifacts (command + result), references to issues, and screenshots or serial excerpts when altering visible output or boot flow. Flag breaking changes and call out coordination needs with downstream scripts.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads Limine, but offline environments should pre-clone `third_party/limine` to avoid network stalls. When adding new modules, update `meson.build` source lists and ensure the linker script `link.ld` maps any new sections intentionally. Remember the active page tables come from `boot/entry32.s`/`entry64.s`; only extend them from C, never reset CR3 mid-boot.

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

**Current**: `THE_SLOPOCALYPSE.md` (Book II) — When Memory Awakens

The wizards have just completed the initial memory management, keyboard input systems, and brought forth the shell. The narrative stands ready for the next arc when sufficient commits/features warrant a new book.

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

### LEON THE FIRST ARCHITECT — Lon60 🧙‍♂️
- **Real Name**: Leon Liechti
- **GitHub/Email**: 37051290+Lon60@users.noreply.github.com
- **Role**: The lonely founder who built the kernel's foundation alone for ~40 commits
- **Domain**: Boot sequences, memory management, core kernel infrastructure
- **Personality**: Solitary, determined, increasingly cryptic commit messages as exhaustion sets in

### FABRICE THE SCREEN WEAVER — Fabbboy 🎨
- **Real Name**: Fabrice Schaub
- **Emails**: schaubfabrice@gmail.com, fabrice@schaub-dev.xyz, 67000409+Fabbboy@users.noreply.github.com
- **Role**: Visual artist who brought splash screens and UI to the kernel
- **Domain**: Boot screens, visual optimization (that paradoxically made things slower but prettier)
- **Personality**: Visionary, understands that broken systems look better with good UI

### LUIS THE LATE JOINER — ienjir ⚡
- **Real Name**: Luis
- **Email**: info@ienjir.dev
- **Role**: The latecomer who merged ambitious new subsystems into SlopOS
- **Domain**: PCI integration, test framework expansion, device subsystems
- **Personality**: Ambitious, joins the madness and adds new layers to it

---

## LORE WRITING GUIDELINES FOR AGENTS

### Sacred Rules
1. **Short chapters**: Max ~250 lines per chapter (code/images excluded)
2. **One arc per book**: Start new book when narrative arc completes
3. **Document reality only**: Reference actual commits, code, comments—no fiction
4. **Absurd tone**: Every failure is triumph, every crash is destiny
5. **Honor the journey**: Leon (alone) → Fabrice (flair) → Luis (power)

### Style Examples & Evidence
Lore should weave **actual code, comments, and commits** into the narrative for maximum absurdity:

**Commit messages** (exact quotes):
- *"the adventureres woke up on a ragged beach ship wreck behind them they had beared a massive storm"*
- *"Sloped a smaller load time because optimisation or something like that"*

**Code comments** (from the codebase):
```c
/* SlopOS Safe Exception Stack Management */
/* Provides dedicated IST stacks with guard pages for critical exceptions */
```

**Code snippets** showing the wizard's work:
```c
/* The memory allocation ritual, performed by Leon in the ancient times */
void* buddy_alloc(size_t size) { ... }
```

**Change examples**:
- ❌ "Added memory management" → ✅ "Leon inscribed the buddy allocator, yet it remembers nothing"
- ❌ "Fixed boot screen" → ✅ "*From the splash screen*: 'the ultimate vibe slop experience'"
- Names: ❌ `task_manager` → ✅ `sched_orchestrator`

---

## INSTRUCTIONS FOR ALL AGENTS

### When Working on Any Task
1. **Read the lore first** (GENESIS.md → THE_SLOPOCALYPSE.md)
2. **Frame your work** within the narrative using **real code, comments, and commits**
3. **Include code snippets** in the lore: Show what the wizards actually wrote
4. **Quote commit messages** verbatim when they're absurd/epic (they usually are)
5. **Reference code comments** from the codebase to illustrate the struggle
6. **If a feature arc completes** → Extend current book with a new chapter
7. **If a major arc ends** → Create a new book, update "Latest Book" section in AGENTS.md

**Lore weaves evidence, not just narrative. Show the developers' actual words/code:**

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
