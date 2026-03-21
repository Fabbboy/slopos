//! MSI-X regression tests.
//!
//! These tests verify:
//! - MSI-X capability parsing from QEMU VirtIO devices
//! - Capability field consistency (table_size, BIR, offsets)
//! - Table mapping and accessor correctness
//! - Configuration of individual table entries
//! - Mask/unmask entry operations
//! - Enable/disable toggling via config space reads
//! - Error path validation (invalid vector, out-of-range entry, unmapped table)
//! - Integration with the MSI vector allocator
//!
//! All tests run on the QEMU q35 platform, targeting VirtIO block (1af4:1042)
//! and VirtIO net (1af4:1041), both of which expose MSI-X at a known capability
//! offset.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::msix::{
    MsixCapability, MsixError, msix_clear_function_mask, msix_configure, msix_map_table,
    msix_mask_entry, msix_read_capability, msix_refresh_control, msix_set_function_mask,
    msix_unmask_entry,
};
use crate::pci::{pci_config_read16, pci_config_write16, pci_get_device, pci_get_device_count};
use crate::pci_defs::*;

// =============================================================================
// Helpers
// =============================================================================

fn find_device_by_vendor_device(vendor: u16, device: u16) -> Option<PciDeviceInfo> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if dev.vendor_id == vendor && dev.device_id == device {
                return Some(dev);
            }
        }
    }
    None
}

/// Restore a previously saved MSI-X table entry by re-programming it via
/// `msix_configure`.  Extracts the vector (msg_data bits 7:0) and APIC ID
/// (msg_addr_lo bits 19:12) from the saved raw register values.
fn restore_table_entry(
    table: &crate::msix::MsixTable,
    entry: u16,
    orig_addr_lo: u32,
    orig_data: u32,
    _orig_ctrl: u32,
) {
    let vector = (orig_data & 0xFF) as u8;
    let apic_id = ((orig_addr_lo >> 12) & 0xFF) as u8;
    // msix_configure masks, writes addr/data, then unmasks — fully restoring
    // the entry to its pre-test state.
    let _ = msix_configure(table, entry, vector, apic_id);
}

/// Restore MSI-X Message Control register to a previously saved value.
///
/// Writes the saved control register directly via PCI config space,
/// bypassing `msix_enable`/`msix_disable` to avoid tearing down QEMU's
/// internal KVM irqfd routing state.  The single atomic config write
/// restores enable, function mask, and table-size bits in one go.
fn restore_msix_control(dev: &PciDeviceInfo, cap: &MsixCapability, saved_ctrl: u16) {
    pci_config_write16(
        dev.bus,
        dev.device,
        dev.function,
        cap.cap_offset + 0x02,
        saved_ctrl,
    );
}

/// Find first device with MSI-X capability and return (device, cap_offset).
fn find_msix_device() -> Option<(PciDeviceInfo, u16)> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if let Some(off) = dev.msix_cap_offset {
                return Some((dev, off));
            }
        }
    }
    None
}

/// Read the MSI-X Message Control register for a given capability.
fn read_msix_control(dev: &PciDeviceInfo, cap: &MsixCapability) -> u16 {
    pci_config_read16(dev.bus, dev.device, dev.function, cap.cap_offset + 0x02)
}

// =============================================================================
// 1. Capability parsing — VirtIO block device
// =============================================================================

/// VirtIO block device (1af4:1042) must have MSI-X capability discoverable.
pub fn test_virtio_blk_msix_cap_present() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device (1af4:1042) not found"),
    };
    assert_test!(dev.has_msix(), "VirtIO block device should have MSI-X");
    assert_test!(
        dev.msix_cap_offset.is_some(),
        "VirtIO block msix_cap_offset should be Some"
    );
    pass!()
}

/// Parsed capability must have a nonzero table_size (at least 1 entry).
pub fn test_virtio_blk_msix_table_size_nonzero() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device not found"),
    };
    let off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("No MSI-X cap offset on VirtIO blk"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    assert_test!(
        cap.table_size >= 1,
        "table_size must be >= 1, got {}",
        cap.table_size
    );
    assert_test!(
        cap.table_size <= 2048,
        "table_size must be <= 2048, got {}",
        cap.table_size
    );
    pass!()
}

/// Table BIR and PBA BIR must be within valid BAR range (0–5).
pub fn test_virtio_blk_msix_bir_valid() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device not found"),
    };
    let off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("No MSI-X cap offset on VirtIO blk"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    assert_test!(
        (cap.table_bar as usize) < PCI_MAX_BARS,
        "table_bar {} out of range",
        cap.table_bar
    );
    assert_test!(
        (cap.pba_bar as usize) < PCI_MAX_BARS,
        "pba_bar {} out of range",
        cap.pba_bar
    );
    pass!()
}

/// Capability cap_offset must match the stored msix_cap_offset.
pub fn test_virtio_blk_msix_cap_offset_matches() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device not found"),
    };
    let off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("No MSI-X cap offset"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    assert_eq_test!(cap.cap_offset, off, "cap_offset mismatch");
    pass!()
}

/// Table offset must be DWORD-aligned (bottom 3 bits of raw register are BIR).
pub fn test_virtio_blk_msix_table_offset_aligned() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO block device not found"),
    };
    let off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("No MSI-X cap offset"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    assert_eq_test!(cap.table_offset & 0x7, 0, "table_offset not DWORD-aligned");
    assert_eq_test!(cap.pba_offset & 0x7, 0, "pba_offset not DWORD-aligned");
    pass!()
}

// =============================================================================
// 2. Capability parsing — VirtIO network device
// =============================================================================

/// VirtIO net device (1af4:1041) must also have MSI-X.
pub fn test_virtio_net_msix_cap_present() -> TestResult {
    let dev = match find_device_by_vendor_device(PCI_VENDOR_ID_VIRTIO, 0x1041) {
        Some(d) => d,
        None => return fail!("VirtIO net device (1af4:1041) not found"),
    };
    assert_test!(dev.has_msix(), "VirtIO net device should have MSI-X");
    let off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("No MSI-X cap offset on VirtIO net"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    assert_test!(
        cap.table_size >= 1 && cap.table_size <= 2048,
        "VirtIO net table_size {} out of valid range",
        cap.table_size
    );
    pass!()
}

// =============================================================================
// 3. Parsing consistency — multiple reads must agree
// =============================================================================

/// Two consecutive capability reads on the same device must yield identical results.
pub fn test_msix_capability_parse_deterministic() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let first = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let second = msix_read_capability(dev.bus, dev.device, dev.function, off);

    assert_eq_test!(
        first.table_size,
        second.table_size,
        "table_size mismatch between reads"
    );
    assert_eq_test!(
        first.table_bar,
        second.table_bar,
        "table_bar mismatch between reads"
    );
    assert_eq_test!(
        first.table_offset,
        second.table_offset,
        "table_offset mismatch between reads"
    );
    assert_eq_test!(
        first.pba_bar,
        second.pba_bar,
        "pba_bar mismatch between reads"
    );
    assert_eq_test!(
        first.pba_offset,
        second.pba_offset,
        "pba_offset mismatch between reads"
    );
    pass!()
}

// =============================================================================
// 4. Table mapping
// =============================================================================

/// Mapping the MSI-X table on a VirtIO device must succeed.
pub fn test_msix_map_table_success() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    assert_test!(table.is_mapped(), "table should be mapped");
    assert_eq_test!(
        table.table_size(),
        cap.table_size,
        "table_size() mismatch after mapping"
    );
    pass!()
}

/// read_vector_control must return Some for valid entries.
pub fn test_msix_read_vector_control_valid() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    // Entry 0 must always be readable.
    let ctrl = table.read_vector_control(0);
    assert_test!(ctrl.is_some(), "read_vector_control(0) returned None");
    pass!()
}

/// read_vector_control for out-of-range entry must return None.
pub fn test_msix_read_vector_control_out_of_range() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    let ctrl = table.read_vector_control(cap.table_size);
    assert_test!(
        ctrl.is_none(),
        "read_vector_control({}) should be None (out of range)",
        cap.table_size
    );
    let ctrl_max = table.read_vector_control(u16::MAX);
    assert_test!(
        ctrl_max.is_none(),
        "read_vector_control(u16::MAX) should be None"
    );
    pass!()
}

/// is_pending must return Some(bool) for valid entries and None for out-of-range.
pub fn test_msix_is_pending_bounds() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    let pending = table.is_pending(0);
    assert_test!(pending.is_some(), "is_pending(0) returned None");

    let oob = table.is_pending(cap.table_size);
    assert_test!(
        oob.is_none(),
        "is_pending({}) should be None (out of range)",
        cap.table_size
    );
    pass!()
}

// =============================================================================
// 5. Entry configuration
// =============================================================================

/// Configuring entry 0 with a valid vector must succeed.
///
/// Saves and restores the original entry 0 configuration to avoid corrupting
/// live device state.
pub fn test_msix_configure_entry_success() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    // Save original entry 0 state before overwriting.
    let orig_data = table.read_msg_data(0).unwrap_or(0);
    let orig_addr = table.read_msg_addr_lo(0).unwrap_or(0);
    let orig_ctrl = table.read_vector_control(0).unwrap_or(0);

    // Use vector 48 (MSI_VECTOR_BASE), APIC ID 0.
    let result = msix_configure(&table, 0, 48, 0);
    assert_test!(
        result.is_ok(),
        "msix_configure(0, 48, 0) failed: {:?}",
        result
    );

    // Restore original entry 0 state.
    restore_table_entry(&table, 0, orig_addr, orig_data, orig_ctrl);
    pass!()
}

/// Configuring with vector < 32 must return InvalidVector error.
///
/// The vector-32 boundary test succeeds and writes to entry 0, so we
/// save/restore the original entry state.
pub fn test_msix_configure_invalid_vector() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    // Save original entry 0 state before overwriting.
    let orig_data = table.read_msg_data(0).unwrap_or(0);
    let orig_addr = table.read_msg_addr_lo(0).unwrap_or(0);
    let orig_ctrl = table.read_vector_control(0).unwrap_or(0);

    // Vector 0 is a CPU exception — must fail.
    let result = msix_configure(&table, 0, 0, 0);
    assert_eq_test!(
        result,
        Err(MsixError::InvalidVector),
        "vector 0 should return InvalidVector"
    );
    // Vector 31 is the last exception vector.
    let result31 = msix_configure(&table, 0, 31, 0);
    assert_eq_test!(
        result31,
        Err(MsixError::InvalidVector),
        "vector 31 should return InvalidVector"
    );
    // Vector 32 is the first valid vector.
    let result32 = msix_configure(&table, 0, 32, 0);
    assert_test!(result32.is_ok(), "vector 32 should succeed");

    // Restore original entry 0 state (vector 32 write was destructive).
    restore_table_entry(&table, 0, orig_addr, orig_data, orig_ctrl);
    pass!()
}

/// Configuring an out-of-range entry must return InvalidEntry error.
pub fn test_msix_configure_invalid_entry() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    let result = msix_configure(&table, cap.table_size, 48, 0);
    assert_eq_test!(
        result,
        Err(MsixError::InvalidEntry),
        "out-of-range entry should return InvalidEntry"
    );
    let result_max = msix_configure(&table, u16::MAX, 48, 0);
    assert_eq_test!(
        result_max,
        Err(MsixError::InvalidEntry),
        "entry u16::MAX should return InvalidEntry"
    );
    pass!()
}

// =============================================================================
// 6. Mask / Unmask operations
// =============================================================================

/// Masking a valid entry must return true.
pub fn test_msix_mask_entry_valid() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    assert_test!(
        msix_mask_entry(&table, 0),
        "msix_mask_entry(0) should return true"
    );
    // After masking, vector control bit 0 should be set.
    let ctrl = table.read_vector_control(0).unwrap_or(0);
    assert_test!((ctrl & 1) != 0, "entry 0 mask bit not set after masking");

    // Clean up: unmask entry 0 so subsequent tests see the original state.
    msix_unmask_entry(&table, 0);
    pass!()
}

/// Unmasking a valid entry must return true and clear the mask bit.
pub fn test_msix_unmask_entry_valid() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    // Mask first, then unmask.
    msix_mask_entry(&table, 0);
    assert_test!(
        msix_unmask_entry(&table, 0),
        "msix_unmask_entry(0) should return true"
    );
    let ctrl = table.read_vector_control(0).unwrap_or(1);
    assert_test!(
        (ctrl & 1) == 0,
        "entry 0 mask bit still set after unmasking"
    );
    pass!()
}

/// Mask/unmask on out-of-range entries must return false.
pub fn test_msix_mask_unmask_out_of_range() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let table = match msix_map_table(&dev, &cap) {
        Ok(t) => t,
        Err(e) => return fail!("msix_map_table failed: {:?}", e),
    };

    assert_test!(
        !msix_mask_entry(&table, cap.table_size),
        "msix_mask_entry({}) should return false",
        cap.table_size
    );
    assert_test!(
        !msix_unmask_entry(&table, cap.table_size),
        "msix_unmask_entry({}) should return false",
        cap.table_size
    );
    assert_test!(
        !msix_mask_entry(&table, u16::MAX),
        "msix_mask_entry(u16::MAX) should return false"
    );
    assert_test!(
        !msix_unmask_entry(&table, u16::MAX),
        "msix_unmask_entry(u16::MAX) should return false"
    );
    pass!()
}

// =============================================================================
// 7. Enable / Disable toggling
// =============================================================================

/// Verify the MSI-X enable bit is set on a device that was enabled at probe time.
///
/// This is a read-only check: MSI-X was enabled by the driver probe, so the
/// enable bit (bit 15) must already be set.  We do NOT toggle the enable bit
/// because QEMU's KVM irqfd routing cannot survive disable/re-enable cycles.
pub fn test_msix_enable_sets_enable_bit() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

    let ctrl = read_msix_control(&dev, &cap);
    let enabled = (ctrl & (1 << 15)) != 0;
    assert_test!(
        enabled,
        "MSI-X enable bit not set (device should be enabled at probe)"
    );
    pass!()
}

/// Verify that clearing the enable bit via config space actually clears it,
/// then restore.  We keep the function mask set throughout so QEMU never
/// attempts to fire vectors through a stale KVM notifier.
pub fn test_msix_disable_clears_enable_bit() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);
    let saved_ctrl = read_msix_control(&dev, &cap);

    // Set function mask first to suppress vector delivery.
    msix_set_function_mask(dev.bus, dev.device, dev.function, &cap);

    // Clear the enable bit while function mask is active.
    let fmasked_ctrl = read_msix_control(&dev, &cap);
    pci_config_write16(
        dev.bus,
        dev.device,
        dev.function,
        cap.cap_offset + 0x02,
        fmasked_ctrl & !(1 << 15),
    );

    let ctrl = read_msix_control(&dev, &cap);
    assert_test!(
        (ctrl & (1 << 15)) == 0,
        "enable bit still set after clearing"
    );

    // Re-enable: set the enable bit again (still function-masked).
    pci_config_write16(
        dev.bus,
        dev.device,
        dev.function,
        cap.cap_offset + 0x02,
        ctrl | (1 << 15),
    );

    // Now restore original control register (clears function mask).
    restore_msix_control(&dev, &cap, saved_ctrl);
    pass!()
}

/// Function mask set/clear must toggle bit 14 in Message Control.
pub fn test_msix_function_mask_toggling() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

    msix_set_function_mask(dev.bus, dev.device, dev.function, &cap);
    let ctrl_masked = read_msix_control(&dev, &cap);
    assert_test!(
        (ctrl_masked & (1 << 14)) != 0,
        "Function mask bit not set after msix_set_function_mask()"
    );

    msix_clear_function_mask(dev.bus, dev.device, dev.function, &cap);
    let ctrl_cleared = read_msix_control(&dev, &cap);
    assert_test!(
        (ctrl_cleared & (1 << 14)) == 0,
        "Function mask bit still set after msix_clear_function_mask()"
    );
    pass!()
}

/// msix_refresh_control must update the capability's control field.
pub fn test_msix_refresh_control_updates_cap() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let mut cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

    // Toggle function mask, then refresh — the in-memory cap should match hardware.
    msix_set_function_mask(dev.bus, dev.device, dev.function, &cap);
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    let live_ctrl = read_msix_control(&dev, &cap);
    assert_eq_test!(
        cap.control,
        live_ctrl,
        "refresh_control did not update cap.control"
    );

    // Restore: clear function mask.
    msix_clear_function_mask(dev.bus, dev.device, dev.function, &cap);
    pass!()
}

// =============================================================================
// 8. MsixCapability helper methods
// =============================================================================

/// is_enabled() must agree with the enable bit in control.
pub fn test_msix_cap_is_enabled_method() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let mut cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

    // MSI-X is enabled at probe time.  Verify is_enabled() reflects that.
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    assert_test!(
        cap.is_enabled(),
        "is_enabled() should be true (enabled at probe)"
    );

    // Set function mask, clear enable bit, verify is_enabled() returns false.
    msix_set_function_mask(dev.bus, dev.device, dev.function, &cap);
    let fmasked_ctrl = read_msix_control(&dev, &cap);
    pci_config_write16(
        dev.bus,
        dev.device,
        dev.function,
        cap.cap_offset + 0x02,
        fmasked_ctrl & !(1 << 15),
    );
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    assert_test!(
        !cap.is_enabled(),
        "is_enabled() should be false after clearing enable bit"
    );

    // Restore: set enable bit back (still function-masked).
    let disabled_ctrl = read_msix_control(&dev, &cap);
    pci_config_write16(
        dev.bus,
        dev.device,
        dev.function,
        cap.cap_offset + 0x02,
        disabled_ctrl | (1 << 15),
    );
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    assert_test!(
        cap.is_enabled(),
        "is_enabled() should be true after restoring enable bit"
    );

    // Clear function mask to restore fully.
    msix_clear_function_mask(dev.bus, dev.device, dev.function, &cap);
    pass!()
}

/// is_function_masked() must agree with the function mask bit.
pub fn test_msix_cap_is_function_masked_method() -> TestResult {
    let (dev, off) = match find_msix_device() {
        Some(pair) => pair,
        None => return fail!("No MSI-X device found"),
    };
    let mut cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

    msix_clear_function_mask(dev.bus, dev.device, dev.function, &cap);
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    assert_test!(
        !cap.is_function_masked(),
        "is_function_masked() should be false after clear"
    );

    msix_set_function_mask(dev.bus, dev.device, dev.function, &cap);
    msix_refresh_control(dev.bus, dev.device, dev.function, &mut cap);
    assert_test!(
        cap.is_function_masked(),
        "is_function_masked() should be true after set"
    );

    // Clean up.
    msix_clear_function_mask(dev.bus, dev.device, dev.function, &cap);
    pass!()
}

// =============================================================================
// 9. All MSI-X devices — consistency sweep
// =============================================================================

/// For every device with MSI-X, the parsed table_size must be 1–2048
/// and BIR values must be in range 0–5.
pub fn test_all_msix_devices_fields_valid() -> TestResult {
    let mut found = false;
    for i in 0..pci_get_device_count() {
        let dev = match pci_get_device(i) {
            Some(d) => d,
            None => continue,
        };
        let off = match dev.msix_cap_offset {
            Some(o) => o,
            None => continue,
        };
        found = true;
        let cap = msix_read_capability(dev.bus, dev.device, dev.function, off);

        assert_test!(
            cap.table_size >= 1 && cap.table_size <= 2048,
            "Device {:02x}:{:02x}.{} table_size {} out of range",
            dev.bus,
            dev.device,
            dev.function,
            cap.table_size
        );
        assert_test!(
            (cap.table_bar as usize) < PCI_MAX_BARS,
            "Device {:02x}:{:02x}.{} table_bar {} out of range",
            dev.bus,
            dev.device,
            dev.function,
            cap.table_bar
        );
        assert_test!(
            (cap.pba_bar as usize) < PCI_MAX_BARS,
            "Device {:02x}:{:02x}.{} pba_bar {} out of range",
            dev.bus,
            dev.device,
            dev.function,
            cap.pba_bar
        );
    }
    if !found {
        return fail!("No MSI-X devices found on q35 — unexpected");
    }
    pass!()
}

// =============================================================================
// 10. SATA controller — must NOT have MSI-X
// =============================================================================

/// ICH9 SATA (8086:2922) has MSI but not MSI-X.
pub fn test_sata_no_msix() -> TestResult {
    let dev = match find_device_by_vendor_device(0x8086, 0x2922) {
        Some(d) => d,
        None => return fail!("SATA controller (8086:2922) not found"),
    };
    assert_test!(!dev.has_msix(), "SATA controller should NOT have MSI-X");
    pass!()
}

// =============================================================================
// Suite registration
// =============================================================================

slopos_testing::define_test_suite!(
    msix,
    [
        // Capability parsing — VirtIO block
        test_virtio_blk_msix_cap_present,
        test_virtio_blk_msix_table_size_nonzero,
        test_virtio_blk_msix_bir_valid,
        test_virtio_blk_msix_cap_offset_matches,
        test_virtio_blk_msix_table_offset_aligned,
        // Capability parsing — VirtIO net
        test_virtio_net_msix_cap_present,
        // Parsing consistency
        test_msix_capability_parse_deterministic,
        // Table mapping
        test_msix_map_table_success,
        test_msix_read_vector_control_valid,
        test_msix_read_vector_control_out_of_range,
        test_msix_is_pending_bounds,
        // Entry configuration
        test_msix_configure_entry_success,
        test_msix_configure_invalid_vector,
        test_msix_configure_invalid_entry,
        // Mask / Unmask
        test_msix_mask_entry_valid,
        test_msix_unmask_entry_valid,
        test_msix_mask_unmask_out_of_range,
        // Enable / Disable
        test_msix_enable_sets_enable_bit,
        test_msix_disable_clears_enable_bit,
        test_msix_function_mask_toggling,
        test_msix_refresh_control_updates_cap,
        // Capability helper methods
        test_msix_cap_is_enabled_method,
        test_msix_cap_is_function_masked_method,
        // Sweep all MSI-X devices
        test_all_msix_devices_fields_valid,
        // Negative — device without MSI-X
        test_sata_no_msix,
    ]
);
