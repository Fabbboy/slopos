#!/usr/bin/env node

console.log(`
╔═══════════════════════════════════════════════════════════════╗
║              ✨ SlopOS Successfully Installed ✨              ║
╚═══════════════════════════════════════════════════════════════╝

The kernel source code is now on your system.
To build and run the bootable ISO:

  1️⃣  Setup the environment:
      $ slopos setup

  2️⃣  Build the kernel and ISO:
      $ slopos build

  3️⃣  Boot SlopOS:
      $ slopos boot-video

Or do it all at once:
  $ slopos setup && slopos build && slopos boot-video

Requirements:
  • QEMU (qemu-system-x86_64)
  • xorriso
  • mkfs.ext2 (e2fsprogs)
  • rustup

Install on Debian/Ubuntu:
  $ sudo apt install qemu-system-x86 xorriso e2fsprogs

Install on Arch:
  $ sudo pacman -S qemu-full xorriso e2fsprogs

The built ISO will be at:
  builddir/slop.iso

For help:
  $ slopos help

May the Wheel of Fate spin in your favor! 🎰

`);
