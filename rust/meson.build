# SlopOS Rust Graphics Subsystem Build

rust_target = 'x86_64-unknown-none'
rust_profile = get_option('buildtype') == 'release' ? 'release' : 'dev'
rust_profile_dir = rust_profile == 'release' ? 'release' : 'debug'

# Find cargo
cargo = find_program('cargo', required: true)

# Rust source files (for dependency tracking)
rust_sources = files(
    'graphics/src/lib.rs',
    'graphics/Cargo.toml',
    'graphics/build.rs',
    'graphics/cbindgen.toml',
    'bindings/src/lib.rs',
    'bindings/Cargo.toml',
    'bindings/build.rs',
    'bindings/wrapper.h',
    'Cargo.toml',
    'rust-toolchain.toml',
)

# Build the Rust graphics library
rust_target_dir = meson.current_source_dir() / 'target' / rust_target / rust_profile_dir

rust_graphics_build = custom_target('rust_graphics',
    output: 'rust_build.stamp',
    input: rust_sources,
    command: [
        'sh', '-c',
        cargo.full_path() + ' build ' +
        '--manifest-path ' + meson.current_source_dir() / 'Cargo.toml' + ' ' +
        '--package graphics ' +
        '--target ' + rust_target + ' ' +
        '--profile ' + rust_profile + ' ' +
        '&& touch @OUTPUT@'
    ],
    build_by_default: true,
    console: true,
    install: false,
    depend_files: rust_sources,
)

# Copy library to build directory
rust_graphics_lib = custom_target('copy_rust_graphics_lib',
    output: 'libgraphics.a',
    input: rust_graphics_build,
    command: [
        'cp',
        rust_target_dir / 'libgraphics.a',
        '@OUTPUT@'
    ],
    build_by_default: true,
)

# Copy header to build directory
rust_graphics_header = custom_target('copy_rust_graphics_header',
    output: 'rust_graphics.h',
    input: rust_graphics_build,
    command: [
        'sh', '-c',
        'find ' + rust_target_dir + ' -name graphics.h | head -1 | xargs -I{} cp {} @OUTPUT@'
    ],
    build_by_default: true,
)

# Create a dependency that other parts of the build can use
rust_graphics_dep = declare_dependency(
    link_with: rust_graphics_lib,
    include_directories: include_directories('.'),
    sources: rust_graphics_header,
)
