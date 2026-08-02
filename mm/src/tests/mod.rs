// Test modules for mm.
// The parent lib.rs gates this entire module behind #[cfg(feature = "test-hooks")].

pub mod kernel_mapping_tests;
pub mod mmio_tests;
pub mod quiesce_tests;
pub mod test_fixtures;
pub mod tests;
pub mod tests_cow_edge;
pub mod tests_demand;
pub mod tests_oom;
pub mod tlb_tests;
