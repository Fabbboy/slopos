// Test modules for boot.
// The parent lib.rs gates this entire module behind #[cfg(feature = "itests")].

pub mod gdt_tests;
pub mod shutdown_tests;
