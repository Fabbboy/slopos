// Test modules for core.
// The parent lib.rs gates this entire module behind #[cfg(feature = "itests")].

pub mod helpers;
pub mod irq_tests;
pub mod msi_tests;
