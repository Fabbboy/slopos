project('slopos', 'rust',
  version : '0.1.0',
  default_options : [
    'warning_level=0',
    'werror=false',
    'buildtype=debug'
  ]
)

cargo = find_program('cargo', required: true)
rust_target = meson.current_source_dir() / 'targets' / 'x86_64-slos.json'
cargo_target_dir = meson.current_build_dir() / 'target'

build_env = [
  'CARGO_TARGET_DIR=' + cargo_target_dir,
]

kernel = custom_target('kernel',
  output : 'kernel.elf',
  command : [
    'env',
    'CARGO_TARGET_DIR=' + cargo_target_dir,
    cargo,
    'build',
    '-Zbuild-std=core,alloc',
    '-Zunstable-options',
    '--target', rust_target,
    '--package', 'kernel',
    '--bin', 'kernel',
    '--out-dir', meson.current_build_dir()
  ],
  build_by_default : true,
  console : true
)

summary({
  'Target': 'x86_64-unknown-none',
  'Boot Protocol': 'Limine Native',
  'Kernel Type': 'Rust freestanding',
  'Output': 'kernel.elf'
}, section: 'SlopOS Kernel Build (Rust)')
