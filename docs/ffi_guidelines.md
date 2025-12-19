# SlopOS FFI / `extern "C"` Guidelines

The kernel is now almost entirely Rust, and `extern "C"` is reserved for the few ABI boundaries that truly need it. To keep this maintainable:

- **Only use `extern "C"` in dedicated FFI modules:**
  - `boot/src/ffi_boundary.rs`, `sched/src/ffi_boundary.rs`: assembly entry/exit and IDT/GDT glue.
  - `drivers/src/irq.rs`: IRQ handler ABI surface used by the low-level interrupt stubs.
  - `mm/src/symbols.rs`: linker-provided section symbols (kernel/user bounds) wrapped in safe helpers.
  - `kernel/src/ffi.rs`: boot entry pointer for the Limine trampoline.
  - `boot/src/limine_protocol.rs`: Limine/UEFI ABI exports.
  - `lib/src/user_syscall.rs`, `lib/src/lib.rs` (`cpuid_ffi`, `cpu_read_msr_ffi`): syscall ABI to userland and minimal CPU helpers.
  - `third_party/limine/limine.h`: upstream Limine headers.

- **Everywhere else:** prefer normal Rust functions, crates, and callback structs. If you think you need `extern "C"`, add it to the allowlisted FFI module instead, or rethink the dependency.

- **Suggested CI guard:** run `rg 'extern "C"'` and fail if matches are found outside the allowlist above. This mimics a `#![forbid(extern "C")]` policy even though rustc doesn’t provide that lint.

- **Symbols access:** if you need linker symbols (e.g., section bounds), import them via `mm::symbols::*` rather than declaring new `extern "C"` statics.

This keeps unsafe/ABI edges contained and makes it easy to audit any future FFI usage.
