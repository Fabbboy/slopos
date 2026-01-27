#!/usr/bin/env node

const { execSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const REPO_ROOT = path.join(__dirname, '..');
const COMMANDS = {
  setup: 'make setup',
  build: 'make build',
  iso: 'make iso',
  boot: 'make boot',
  'boot-video': 'VIDEO=1 make boot',
  'boot-log': 'make boot-log',
  test: 'make test',
  clean: 'make clean',
  distclean: 'make distclean',
  install: null, // Special case - setup + build
  help: null, // Special case
};

function printBanner() {
  console.log(`
╔═══════════════════════════════════════════════════════════════╗
║                         🎰 SlopOS 🎰                          ║
║                                                               ║
║  Three kernel wizards shipwrecked on the island of Sloptopia ║
║  Armed with Rust, mass AI token consumption, and zero fear   ║
║  of unsafe, they built an OS that boots—when the Wheel of    ║
║  Fate allows it.                                              ║
║                                                               ║
║  Win the spin → enter the shell.                             ║
║  Lose → reboot and try again.                                ║
║  The house always wins. Eventually.                          ║
╚═══════════════════════════════════════════════════════════════╝
`);
}

function printHelp() {
  printBanner();
  console.log(`
Usage: slopos <command>

Commands:
  install       Complete setup + build (does setup and build in one command)
  setup         Install Rust nightly toolchain and dependencies
  build         Build the SlopOS kernel and ISO image
  iso           Build ISO only (requires kernel.elf from build)
  boot          Boot SlopOS in QEMU (headless, serial only)
  boot-video    Boot SlopOS with graphical display
  boot-log      Boot with timeout and save output to test_output.log
  test          Run the interrupt test harness
  clean         Clean build artifacts
  distclean     Remove all build artifacts and ISOs
  help          Show this help message

Environment Variables:
  VIDEO=1       Enable graphical display
  DEBUG=1       Enable debug logging
  QEMU_SMP=N    Number of CPU cores (must be power of 2, default: 2)

Examples:
  slopos install            # Complete install (setup + build)
  slopos setup              # First-time setup only
  slopos build              # Build kernel and ISO
  slopos boot-video         # Boot with display
  DEBUG=1 slopos boot       # Boot with debug output

Quick Start:
  npx @slopos/kernel install && npx @slopos/kernel boot-video

Requirements:
  - QEMU (qemu-system-x86_64)
  - xorriso
  - mkfs.ext2 (e2fsprogs)
  - rustup
  - Make

Documentation:
  https://github.com/Fabbboy/slopos

Read the lore: lore/GENESIS.md → lore/THE_SLOPOCALYPSE.md → lore/THE_COOKED.md
`);
}

function checkRequirements() {
  const required = {
    'qemu-system-x86_64': 'QEMU',
    'xorriso': 'xorriso',
    'mkfs.ext2': 'e2fsprogs',
    'rustup': 'rustup',
    'make': 'make',
  };

  const missing = [];

  for (const [cmd, pkg] of Object.entries(required)) {
    try {
      execSync(`which ${cmd}`, { stdio: 'ignore' });
    } catch {
      missing.push(`${cmd} (${pkg})`);
    }
  }

  if (missing.length > 0) {
    console.error('⚠️  Missing required dependencies:');
    missing.forEach(dep => console.error(`   - ${dep}`));
    console.error('\nInstall them first:');
    console.error('  Debian/Ubuntu: sudo apt install qemu-system-x86 xorriso e2fsprogs');
    console.error('  Arch: sudo pacman -S qemu-full xorriso e2fsprogs');
    console.error('  rustup: curl --proto \'=https\' --tlsv1.2 -sSf https://sh.rustup.rs | sh');
    process.exit(1);
  }
}

function runCommand(command) {
  console.log(`\n🎲 Spinning the Wheel of Fate...\n`);
  console.log(`Executing: ${command}\n`);

  try {
    execSync(command, {
      cwd: REPO_ROOT,
      stdio: 'inherit',
      env: { ...process.env },
    });
    console.log(`\n✅ Command completed successfully!`);
  } catch (error) {
    console.error(`\n❌ The Wheel of Fate has spoken: Command failed with exit code ${error.status}`);
    process.exit(error.status || 1);
  }
}

function main() {
  const args = process.argv.slice(2);
  const command = args[0];

  if (!command || command === 'help' || command === '--help' || command === '-h') {
    printHelp();
    return;
  }

  if (!COMMANDS.hasOwnProperty(command)) {
    console.error(`\n❌ Unknown command: ${command}`);
    console.error(`Run 'slopos help' for usage information.\n`);
    process.exit(1);
  }

  // Special case: install command runs setup + build
  if (command === 'install') {
    checkRequirements();
    console.log('\n🎲 Complete SlopOS installation starting...\n');
    console.log('Step 1/2: Setting up Rust toolchain...\n');
    runCommand('make setup');
    console.log('\n✅ Setup complete!\n');
    console.log('Step 2/2: Building kernel and ISO...\n');
    runCommand('make build');
    console.log('\n🎰 Installation complete! ISO ready at: builddir/slop.iso\n');
    console.log('Run "slopos boot-video" to launch!\n');
    return;
  }

  // Check requirements before running most commands (except help and distclean)
  if (!['help', 'distclean'].includes(command)) {
    checkRequirements();
  }

  runCommand(COMMANDS[command]);
}

main();
