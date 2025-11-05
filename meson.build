project('slopos', 'c',
  version : '0.1.0',
  default_options : [
    'c_std=c11',
    'warning_level=3',
    'werror=false',
    'b_staticpic=false',
    'b_lundef=false',
    'buildtype=debug'
  ]
)

interrupt_tests_default_enabled_opt = get_option('interrupt_tests_default')
interrupt_tests_default_suite_opt = get_option('interrupt_tests_default_suite')
interrupt_tests_default_timeout_opt = get_option('interrupt_tests_default_timeout')
interrupt_tests_default_verbosity_opt = get_option('interrupt_tests_default_verbosity')
interrupt_tests_default_shutdown_opt = get_option('interrupt_tests_default_shutdown')

interrupt_tests_default_enabled_define = interrupt_tests_default_enabled_opt ? '1' : '0'
interrupt_tests_default_suite_define = '"' + interrupt_tests_default_suite_opt + '"'
interrupt_tests_default_verbosity_define = '"' + interrupt_tests_default_verbosity_opt + '"'
interrupt_tests_default_timeout_define = interrupt_tests_default_timeout_opt.to_string()
interrupt_tests_default_shutdown_define = interrupt_tests_default_shutdown_opt ? '1' : '0'

add_project_arguments([
    '-ffreestanding',
    '-fno-stack-protector',
    '-fno-pic',
    '-mno-red-zone',
    '-mcmodel=kernel',
    '-mabi=sysv',
    # Disable SIMD/vector instructions to avoid "Invalid Opcode" exceptions
    # These instructions may not be supported by QEMU or cause issues with large structures
    '-mno-sse',
    '-mno-sse2',
    '-mno-mmx',
    '-mno-avx',
    '-mno-avx2',
    '-msoft-float',
    '-Wall',
    '-Wextra',
    '-Wno-unused-parameter',
    '-DINTERRUPT_TESTS_DEFAULT_ENABLED=' + interrupt_tests_default_enabled_define,
    '-DINTERRUPT_TESTS_DEFAULT_SUITE=' + interrupt_tests_default_suite_define,
    '-DINTERRUPT_TESTS_DEFAULT_TIMEOUT_MS=' + interrupt_tests_default_timeout_define,
    '-DINTERRUPT_TESTS_DEFAULT_VERBOSITY=' + interrupt_tests_default_verbosity_define,
    '-DINTERRUPT_TESTS_DEFAULT_SHUTDOWN=' + interrupt_tests_default_shutdown_define
  ],
  language: 'c')

# Source files organized by component
boot_sources = []
mm_sources = []
video_sources = []
drivers_sources = []
sched_sources = []
kernel_sources = []
lib_sources = []
fs_sources = []

# Boot directory - assembly and early C code
boot_dir = meson.current_source_dir() / 'boot'
boot_sources += files(
  'boot/limine_entry.s',
  'boot/idt_handlers.s',
  'boot/early_init.c',
  'boot/log.c',
  'boot/cpu_verify.c',
  'boot/kernel_panic.c',
  'boot/idt.c',
  'boot/gdt.c',
  'boot/safe_stack.c',
  'boot/debug.c',
  'boot/shutdown.c',
  'boot/limine_protocol.c'
)

# Memory management directory
mm_dir = meson.current_source_dir() / 'mm'
mm_sources += files(
  'mm/paging.c',
  'mm/memory_layout.c',
  'mm/mb2_parser.c',
  'mm/page_alloc.c',
  'mm/process_vm.c',
  'mm/kernel_heap.c',
  'mm/early_paging.c',
  'mm/uefi_memory.c',
  'mm/memory_reservations.c',
  'mm/buddy_alloc.c',
  'mm/vmem_regions.c',
  'mm/memory_init.c',
  'mm/phys_virt.c',
  'mm/test_process_vm.c',
  'mm/test_kernel_heap.c'
)

# Video/framebuffer directory
video_dir = meson.current_source_dir() / 'video'
video_sources += files(
  'video/framebuffer.c',
  'video/graphics.c',
  'video/font.c',
  'video/splash.c'
)

# Lib directory - utility functions
lib_dir = meson.current_source_dir() / 'lib'
lib_sources += files(
  'lib/memory.c',
  'lib/string.c',
  'lib/unit_test.c',
  'lib/stacktrace.c'
)

# Drivers directory
drivers_dir = meson.current_source_dir() / 'drivers'
drivers_sources += files(
  'drivers/serial.c',
  'drivers/pci.c',
  'drivers/pic.c',
  'drivers/pit.c',
  'drivers/apic.c',
  'drivers/irq.c',
  'drivers/keyboard.c',
  'drivers/tty.c',
  'drivers/interrupt_test.c',
  'drivers/interrupt_test_config.c',
  'drivers/exceptions.s',
  'drivers/exception_handlers.c',
  'drivers/random.c',
  'drivers/wl_currency.c'
)

# Scheduler directory
sched_dir = meson.current_source_dir() / 'sched'
sched_sources += files(
  'sched/scheduler.c',
  'sched/kthread.c',
  'sched/task.c',
  'sched/test_tasks.c',
  'sched/context_switch.s'
)

# Shell directory
shell_dir = meson.current_source_dir() / 'shell'
shell_sources = files(
  'shell/shell.c',
  'shell/builtins.c'
)

# File system directory
fs_dir = meson.current_source_dir() / 'fs'
fs_sources += files(
  'fs/ramfs.c',
  'fs/fileio.c',
  'fs/test_ramfs.c'
)

# Kernel main sources (root directory)
kernel_sources += files()  # Will be populated when source files exist

# Combine all source files
all_sources = boot_sources + mm_sources + video_sources + drivers_sources + sched_sources + shell_sources + fs_sources + kernel_sources + lib_sources

# Include directories
inc_dirs = include_directories('.', 'boot', 'mm', 'video', 'drivers', 'sched', 'shell', 'fs', 'lib')

# Custom target for assembly files (when they exist)
# This will be used for boot assembly code that needs special handling

# Main kernel executable
linker_script = meson.current_source_dir() / 'link.ld'

kernel_elf = executable('kernel',
  sources : all_sources,
  include_directories : inc_dirs,
  c_args : [
    '-O2',
    '-fno-builtin',
    '-mcmodel=kernel',
    '-fno-PIE'
  ],
  link_args : [
    '-nostdlib',
    '-static',
    '-Wl,-T,' + linker_script,
    '-Wl,-z,max-page-size=0x1000',
    '-Wl,-z,common-page-size=0x1000',
    '-Wl,--build-id=none',
    '-lgcc'
  ],
  link_depends : files('link.ld'),
  install : false,
  name_suffix : 'elf'
)

# Build summary
summary({
  'Target': 'x86_64-unknown-none',
  'Boot Protocol': 'Limine Native',
  'Kernel Type': 'Freestanding Higher-Half',
  'Linker Script': 'link.ld',
  'Output': 'kernel.elf'
}, section: 'SlopOS Kernel Build')
