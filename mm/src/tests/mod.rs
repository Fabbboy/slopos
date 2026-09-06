// lib.rs gates this entire module behind #[cfg(feature = "test-hooks")].

pub mod cache_census_tests;
pub mod kernel_mapping_tests;
pub mod mmio_tests;
pub mod quiesce_tests;
pub mod test_fixtures;
pub mod tests;
pub mod tests_cow_edge;
pub mod tests_demand;
pub mod tests_exec_boundary;
pub mod tests_filemap_vma;
pub mod tests_map_ownership;
pub mod tests_oom;
pub mod tests_pcid;
pub mod tests_quota_heap;
pub mod tests_quota_pages;
pub mod tests_reclaim;
pub mod tests_vm_space_contention;
pub mod tlb_tests;
