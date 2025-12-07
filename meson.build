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

builtin_tests_enabled = get_option('enable_builtin_tests')

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
    '-Wno-unused-parameter',
    '-Wno-unused-command-line-argument',
    '-DENABLE_BUILTIN_TESTS=' + (builtin_tests_enabled ? '1' : '0'),
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
  'boot/cpu_verify.c',
  'boot/kernel_panic.c',
  'boot/idt.c',
  'boot/gdt.c',
  'boot/safe_stack.c',
  'boot/shutdown.c',
  'boot/limine_protocol.c'
)

# Memory management directory
mm_dir = meson.current_source_dir() / 'mm'
mm_sources += files(
  'mm/paging.c',
  'mm/page_alloc.c',
  'mm/process_vm.c',
  'mm/kernel_heap.c',
  'mm/memory_reservations.c',
  'mm/memory_init.c',
  'mm/phys_virt.c',
  'mm/user_copy.c'
)

# Video/framebuffer directory
video_dir = meson.current_source_dir() / 'video'
video_sources += files(
  'video/framebuffer.c',
  'video/graphics.c',
  'video/font.c',
  'video/splash.c',
  'video/roulette_core.c',
  'video/roulette_user.c'
)

# Lib directory - utility functions
lib_dir = meson.current_source_dir() / 'lib'
lib_sources += files(
  'lib/memory.c',
  'lib/string.c',
  'lib/stacktrace.c',
  'lib/numfmt.c',
  'lib/klog.c',
  'lib/kdiag.c'
)

# Drivers directory
drivers_dir = meson.current_source_dir() / 'drivers'
drivers_sources += files(
  'drivers/serial.c',
  'drivers/pci.c',
  'drivers/pit.c',
  'drivers/apic.c',
  'drivers/ioapic.c',
  'drivers/pic_quiesce.c',
  'drivers/irq.c',
  'drivers/keyboard.c',
  'drivers/tty.c',
  'drivers/interrupt_test.c',
  'drivers/interrupt_test_config.c',
  'drivers/random.c',
  'drivers/wl_currency.c',
  'drivers/syscall.c'
)

# Scheduler directory
sched_dir = meson.current_source_dir() / 'sched'
sched_sources += files(
  'sched/scheduler.c',
  'sched/kthread.c',
  'sched/task.c',
  'sched/context_switch.s',
  'sched/test_tasks.c'
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
  'fs/fileio.c'
)

# Optional in-kernel test sources
if builtin_tests_enabled
  mm_sources += files('mm/test_process_vm.c', 'mm/test_kernel_heap.c')
  sched_sources += files('sched/test_tasks.c')
  fs_sources += files('fs/test_ramfs.c')
endif

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
