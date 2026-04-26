// Test modules for drivers.
// The parent lib.rs gates this entire module behind #[cfg(feature = "test-hooks")].

pub mod apic_timer_tests;
pub mod ecam_tests;
pub mod hpet_tests;
pub mod msix_tests;
pub mod pci_cap_tests;
pub mod virtio_completion_tests;
pub mod virtio_msix_tests;
pub mod virtio_net_tests;
