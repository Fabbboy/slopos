//! VirtIO MSI-X integration regression tests.
//!
//! Run after PCI enumeration and VirtIO probe have completed on QEMU q35.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_fs::blockdev::BlockDeviceIndex;

use crate::pci::{pci_config_read16, pci_get_device, pci_get_device_count};
use crate::virtio_blk;
use crate::virtio_net;

/// Mirrors the vector range `msi_alloc_vector` allocates from.
const MSI_VECTOR_BASE: u8 = 48;
const MSI_VECTOR_MAX: u8 = 223;

fn disk0() -> Option<virtio_blk::DevHandle> {
    virtio_blk::blk_device_by_index(BlockDeviceIndex(0))
}

fn find_device(vendor: u16, device: u16) -> Option<crate::pci::PciDeviceInfo> {
    for i in 0..pci_get_device_count() {
        if let Some(dev) = pci_get_device(i) {
            if dev.vendor_id == vendor && dev.device_id == device {
                return Some(dev);
            }
        }
    }
    None
}

/// The MSI-X Message Control register sits at `cap_offset + 0x02`.
fn msix_control_bits(dev: &crate::pci::PciDeviceInfo, cap_offset: u16) -> (bool, bool) {
    let ctrl = pci_config_read16(dev.bus, dev.device, dev.function, cap_offset + 0x02);
    let enabled = (ctrl & (1 << 15)) != 0;
    let fmask = (ctrl & (1 << 14)) != 0;
    (enabled, fmask)
}

pub fn test_virtio_blk_ready() -> TestResult {
    assert_test!(
        disk0().is_some_and(virtio_blk::blk_is_ready),
        "virtio-blk should be ready after probe"
    );
    pass!()
}

pub fn test_virtio_blk_has_msix_state() -> TestResult {
    let state = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None — unexpected on q35"),
    };
    assert_test!(
        state.num_queues >= 1,
        "virtio-blk should have at least 1 queue with MSI-X"
    );
    pass!()
}

pub fn test_virtio_blk_vector_in_range() -> TestResult {
    let state = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None"),
    };
    let vec = state.queue_vectors[0];
    assert_test!(vec != 0, "virtio-blk queue 0 vector should be allocated");
    assert_test!(
        vec >= MSI_VECTOR_BASE && vec <= MSI_VECTOR_MAX,
        "virtio-blk queue 0 vector {} not in range {}-{}",
        vec,
        MSI_VECTOR_BASE,
        MSI_VECTOR_MAX
    );
    pass!()
}

/// The vector sits in bits 7:0 of the entry's Message Data field.
pub fn test_virtio_blk_table_entry_matches_vector() -> TestResult {
    let state = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None"),
    };
    let expected_vec = state.queue_vectors[0];
    let msg_data = match state.table.read_msg_data(0) {
        Some(d) => d,
        None => return fail!("failed to read MSI-X table entry 0 data"),
    };
    let actual_vec = (msg_data & 0xFF) as u8;
    assert_eq_test!(
        actual_vec,
        expected_vec,
        "MSI-X table entry 0 vector mismatch"
    );
    pass!()
}

pub fn test_virtio_blk_table_entry_targets_bsp() -> TestResult {
    let state = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None"),
    };
    let addr_lo = match state.table.read_msg_addr_lo(0) {
        Some(a) => a,
        None => return fail!("failed to read MSI-X table entry 0 addr"),
    };
    // Destination ID is in bits 19:12.
    let dest_id = (addr_lo >> 12) & 0xFF;
    assert_eq_test!(
        dest_id,
        0,
        "MSI-X table entry 0 should target APIC ID 0 (BSP)"
    );
    pass!()
}

/// The per-entry mask is bit 0 of the vector control word.
pub fn test_virtio_blk_entry_unmasked() -> TestResult {
    let state = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None"),
    };
    let ctrl = match state.table.read_vector_control(0) {
        Some(c) => c,
        None => return fail!("failed to read MSI-X table entry 0 vector control"),
    };
    assert_test!(
        (ctrl & 1) == 0,
        "virtio-blk MSI-X entry 0 should be unmasked"
    );
    pass!()
}

pub fn test_virtio_blk_msix_enabled_in_config() -> TestResult {
    let dev = match find_device(0x1af4, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO-blk device (1af4:1042) not found"),
    };
    let cap_off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("VirtIO-blk device has no MSI-X capability"),
    };
    let (enabled, fmask) = msix_control_bits(&dev, cap_off);
    assert_test!(enabled, "MSI-X should be enabled for virtio-blk");
    assert_test!(!fmask, "MSI-X function mask should be clear for virtio-blk");
    pass!()
}

pub fn test_virtio_net_ready() -> TestResult {
    assert_test!(
        virtio_net::virtio_net_is_ready(),
        "virtio-net should be ready after probe"
    );
    pass!()
}

pub fn test_virtio_net_has_msix_state() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None — unexpected on q35"),
    };
    assert_eq_test!(
        state.num_queues,
        2,
        "virtio-net should have 2 queue vectors (RX + TX)"
    );
    pass!()
}

pub fn test_virtio_net_vectors_in_range() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };
    for q in 0..2u8 {
        let vec = state.queue_vectors[q as usize];
        assert_test!(
            vec != 0,
            "virtio-net queue {} vector should be allocated",
            q
        );
        assert_test!(
            vec >= MSI_VECTOR_BASE && vec <= MSI_VECTOR_MAX,
            "virtio-net queue {} vector {} not in range {}-{}",
            q,
            vec,
            MSI_VECTOR_BASE,
            MSI_VECTOR_MAX
        );
    }
    pass!()
}

pub fn test_virtio_net_vectors_distinct() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };
    let rx_vec = state.queue_vectors[0];
    let tx_vec = state.queue_vectors[1];
    assert_test!(
        rx_vec != tx_vec,
        "RX vector ({}) and TX vector ({}) should be distinct",
        rx_vec,
        tx_vec
    );
    pass!()
}

pub fn test_virtio_net_table_entries_match_vectors() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };
    for q in 0..2u16 {
        let expected = state.queue_vectors[q as usize];
        let msg_data = match state.table.read_msg_data(q) {
            Some(d) => d,
            None => return fail!("failed to read MSI-X table entry {} data", q),
        };
        let actual = (msg_data & 0xFF) as u8;
        assert_test!(
            actual == expected,
            "MSI-X table entry {} vector mismatch",
            q
        );
    }
    pass!()
}

pub fn test_virtio_net_table_entries_target_bsp() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };
    for q in 0..2u16 {
        let addr_lo = match state.table.read_msg_addr_lo(q) {
            Some(a) => a,
            None => return fail!("failed to read MSI-X table entry {} addr", q),
        };
        let dest_id = (addr_lo >> 12) & 0xFF;
        assert_test!(
            dest_id == 0,
            "MSI-X table entry {} should target APIC ID 0 (BSP)",
            q
        );
    }
    pass!()
}

pub fn test_virtio_net_entries_unmasked() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };
    for q in 0..2u16 {
        let ctrl = match state.table.read_vector_control(q) {
            Some(c) => c,
            None => return fail!("failed to read MSI-X entry {} vector control", q),
        };
        assert_test!(
            (ctrl & 1) == 0,
            "virtio-net MSI-X entry {} should be unmasked",
            q
        );
    }
    pass!()
}

pub fn test_virtio_net_msix_enabled_in_config() -> TestResult {
    let dev = match find_device(0x1af4, 0x1041) {
        Some(d) => d,
        None => return fail!("VirtIO-net device (1af4:1041) not found"),
    };
    let cap_off = match dev.msix_cap_offset {
        Some(o) => o,
        None => return fail!("VirtIO-net device has no MSI-X capability"),
    };
    let (enabled, fmask) = msix_control_bits(&dev, cap_off);
    assert_test!(enabled, "MSI-X should be enabled for virtio-net");
    assert_test!(!fmask, "MSI-X function mask should be clear for virtio-net");
    pass!()
}

pub fn test_blk_and_net_vectors_disjoint() -> TestResult {
    let blk = match disk0().and_then(virtio_blk::blk_msix_state) {
        Some(s) => s,
        None => return fail!("virtio-blk MSI-X state is None"),
    };
    let net = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };

    for bq in 0..blk.num_queues as usize {
        let bv = blk.queue_vectors[bq];
        if bv == 0 {
            continue;
        }
        for nq in 0..net.num_queues as usize {
            let nv = net.queue_vectors[nq];
            assert_test!(
                bv != nv,
                "blk queue {} vector {} collides with net queue {} vector {}",
                bq,
                bv,
                nq,
                nv
            );
        }
    }
    pass!()
}

pub fn test_msix_preferred_over_msi_on_q35() -> TestResult {
    let blk_dev = match find_device(0x1af4, 0x1042) {
        Some(d) => d,
        None => return fail!("VirtIO-blk (1af4:1042) not found"),
    };
    assert_test!(
        blk_dev.msix_cap_offset.is_some(),
        "VirtIO-blk should have MSI-X capability"
    );

    let net_dev = match find_device(0x1af4, 0x1041) {
        Some(d) => d,
        None => return fail!("VirtIO-net (1af4:1041) not found"),
    };
    assert_test!(
        net_dev.msix_cap_offset.is_some(),
        "VirtIO-net should have MSI-X capability"
    );

    assert_test!(
        disk0().and_then(virtio_blk::blk_msix_state).is_some(),
        "virtio-blk should use MSI-X, not MSI fallback"
    );
    assert_test!(
        virtio_net::virtio_net_msix_state().is_some(),
        "virtio-net should use MSI-X, not MSI fallback"
    );
    pass!()
}

pub fn test_queue_msix_entry_helper() -> TestResult {
    let state = match virtio_net::virtio_net_msix_state() {
        Some(s) => s,
        None => return fail!("virtio-net MSI-X state is None"),
    };

    assert_eq_test!(state.queue_msix_entry(0), 0, "queue 0 entry should be 0");
    assert_eq_test!(state.queue_msix_entry(1), 1, "queue 1 entry should be 1");

    assert_eq_test!(
        state.queue_msix_entry(99),
        crate::virtio::VIRTIO_MSI_NO_VECTOR,
        "unassigned queue should return VIRTIO_MSI_NO_VECTOR"
    );
    pass!()
}

slopos_testing::stest!(name = test_virtio_blk_ready, suite = virtio_msix);
slopos_testing::stest!(name = test_virtio_blk_has_msix_state, suite = virtio_msix);
slopos_testing::stest!(name = test_virtio_blk_vector_in_range, suite = virtio_msix);
slopos_testing::stest!(
    name = test_virtio_blk_table_entry_matches_vector,
    suite = virtio_msix
);
slopos_testing::stest!(
    name = test_virtio_blk_table_entry_targets_bsp,
    suite = virtio_msix
);
slopos_testing::stest!(name = test_virtio_blk_entry_unmasked, suite = virtio_msix);
slopos_testing::stest!(
    name = test_virtio_blk_msix_enabled_in_config,
    suite = virtio_msix
);
slopos_testing::stest!(name = test_virtio_net_ready, suite = virtio_msix);
slopos_testing::stest!(name = test_virtio_net_has_msix_state, suite = virtio_msix);
slopos_testing::stest!(name = test_virtio_net_vectors_in_range, suite = virtio_msix);
slopos_testing::stest!(name = test_virtio_net_vectors_distinct, suite = virtio_msix);
slopos_testing::stest!(
    name = test_virtio_net_table_entries_match_vectors,
    suite = virtio_msix
);
slopos_testing::stest!(
    name = test_virtio_net_table_entries_target_bsp,
    suite = virtio_msix
);
slopos_testing::stest!(name = test_virtio_net_entries_unmasked, suite = virtio_msix);
slopos_testing::stest!(
    name = test_virtio_net_msix_enabled_in_config,
    suite = virtio_msix
);
slopos_testing::stest!(
    name = test_blk_and_net_vectors_disjoint,
    suite = virtio_msix
);
slopos_testing::stest!(
    name = test_msix_preferred_over_msi_on_q35,
    suite = virtio_msix
);
slopos_testing::stest!(name = test_queue_msix_entry_helper, suite = virtio_msix);
