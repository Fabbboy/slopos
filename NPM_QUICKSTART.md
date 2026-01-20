# SlopOS - npm Quick Start

## What is this?

SlopOS is now available on npm! You can install and run a full kernel operating system using npm/npx.

## Prerequisites

Before installing, make sure you have:

```bash
# On Debian/Ubuntu
sudo apt install qemu-system-x86 xorriso e2fsprogs

# On Arch Linux
sudo pacman -S qemu-full xorriso e2fsprogs

# Install rustup (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Installation Methods

### Method 1: Using npx (Recommended for first-time)

Try SlopOS without installing:

```bash
# This will download, build, and boot SlopOS
npx @slopos/kernel install && npx @slopos/kernel boot-video
```

### Method 2: Global Installation

Install globally for easier access:

```bash
# Install
npm install -g @slopos/kernel

# Build everything
slopos install

# Boot with display
slopos boot-video
```

### Method 3: Local Project Installation

Add to a project:

```bash
# Create a new directory
mkdir my-slopos && cd my-slopos

# Initialize npm
npm init -y

# Install SlopOS
npm install @slopos/kernel

# Run commands
npx slopos install
npx slopos boot-video
```

## Available Commands

```bash
slopos install      # Complete setup + build (does everything)
slopos setup        # Install Rust toolchain only
slopos build        # Build kernel and ISO
slopos iso          # Build ISO only
slopos boot         # Boot (headless, serial only)
slopos boot-video   # Boot with graphical display
slopos boot-log     # Boot with timeout and logging
slopos test         # Run test harness
slopos clean        # Clean build artifacts
slopos help         # Show help
```

## Quick Examples

```bash
# Boot with GPU acceleration
VIRGL=1 VIDEO=1 slopos boot

# Boot with debug output
DEBUG=1 slopos boot-video

# Boot with more CPU cores (must be power of 2)
QEMU_SMP=4 slopos boot-video

# Save boot log to file
slopos boot-log
cat test_output.log
```

## What Gets Installed?

When you install `@slopos/kernel`, you get:
- Full SlopOS kernel source code (Rust)
- Build scripts and Makefile
- Limine bootloader integration
- All necessary configuration files
- The sacred lore documents

After running `slopos install` or `slopos build`, you'll have:
- A bootable ISO image at `builddir/slop.iso`
- Kernel ELF binary at `builddir/kernel.elf`
- Userland binaries (shell, roulette, compositor)

## Package Size

- Source package: ~3-4 MB
- Built artifacts: ~50-100 MB
- Full installation with dependencies: ~200-300 MB

## Troubleshooting

**Command not found after global install:**
```bash
# Make sure npm global bin is in your PATH
npm config get prefix
# Add to PATH: export PATH="$(npm config get prefix)/bin:$PATH"
```

**Build fails:**
```bash
# Make sure you have all dependencies
slopos help  # Shows requirements
```

**QEMU doesn't start:**
```bash
# Check if QEMU is installed
which qemu-system-x86_64

# Try with just serial output
slopos boot
```

## Uninstallation

```bash
# Global
npm uninstall -g @slopos/kernel

# Local
npm uninstall @slopos/kernel
```

## Why npm?

Because installing an entire operating system via `npm install` is peak SlopOS energy. The Wheel of Fate respects no package manager boundaries.

## Next Steps

1. Read the lore: `lore/GENESIS.md` → `lore/THE_SLOPOCALYPSE.md` → `lore/THE_COOKED.md`
2. Check the README: `README.md`
3. Explore the source: `boot/`, `kernel/`, `mm/`, etc.
4. Spin the Wheel of Fate: `slopos boot-video`

**May your builds succeed and your boots be blessed by the Wheel.** 🎰
