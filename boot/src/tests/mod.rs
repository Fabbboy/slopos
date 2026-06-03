// Test modules for boot.
// The parent lib.rs gates this entire module behind #[cfg(feature = "test-hooks")].

pub mod gdt_tests;
pub mod safestack_dstack_tests;
pub mod shutdown_tests;
