// Test modules for core.
// The parent lib.rs gates this entire module behind #[cfg(feature = "test-hooks")].

pub mod event_bus_tests;
pub mod helpers;
pub mod irq_tests;
pub mod msi_tests;
pub mod ostd_arc_tests;
pub mod process_identity_tests;
pub mod rcu_qs_tests;
