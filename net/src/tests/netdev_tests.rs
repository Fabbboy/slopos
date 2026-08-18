//! Tests for NetDevice trait, NetDeviceStats, NetDeviceFeatures, DeviceHandle,
//! and NetDeviceRegistry.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_REGISTRY;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::netdev::*;
use crate::packetbuf::PacketBuf;
use crate::pool::{PACKET_POOL, PacketPool};
use crate::types::*;

/// A minimal in-memory network device for testing the registry and handle.
struct MockNetDevice {
    mac_addr: MacAddr,
    dev_mtu: u16,
    feats: NetDeviceFeatures,
    stats: SpinLock<NetDeviceStats>,
    tx_count: SpinLock<u64>,
    poll_tx_count: SpinLock<u64>,
    is_up: SpinLock<bool>,
}

impl MockNetDevice {
    fn new(mac: MacAddr, mtu: u16) -> Self {
        Self {
            mac_addr: mac,
            dev_mtu: mtu,
            poll_tx_count: SpinLock::new(
                0,
                lock_class!("test.netdev_dev.poll_tx_count", LOCK_LEVEL_RESOURCE),
            ),
            feats: NetDeviceFeatures::empty(),
            stats: SpinLock::new(
                NetDeviceStats::new(),
                lock_class!("test.netdev_dev.stats", LOCK_LEVEL_RESOURCE),
            ),
            tx_count: SpinLock::new(
                0,
                lock_class!("test.netdev_dev.tx_count", LOCK_LEVEL_RESOURCE),
            ),
            is_up: SpinLock::new(
                true,
                lock_class!("test.netdev_dev.is_up", LOCK_LEVEL_RESOURCE),
            ),
        }
    }

    fn with_features(mut self, feats: NetDeviceFeatures) -> Self {
        self.feats = feats;
        self
    }
}

impl NetDevice for MockNetDevice {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        // Models the `set_down` contract: a downed device rejects sends.
        if !*self.is_up.lock() {
            return Err(NetError::NetworkUnreachable);
        }
        let mut count = self.tx_count.lock();
        *count += 1;
        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        Ok(())
    }

    fn poll_tx(&self) {
        *self.poll_tx_count.lock() += 1;
    }

    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }

    fn set_up(&self) {
        *self.is_up.lock() = true;
    }

    fn set_down(&self) {
        *self.is_up.lock() = false;
    }

    fn mtu(&self) -> u16 {
        self.dev_mtu
    }

    fn mac(&self) -> MacAddr {
        self.mac_addr
    }

    fn stats(&self) -> NetDeviceStats {
        *self.stats.lock()
    }

    fn features(&self) -> NetDeviceFeatures {
        self.feats
    }
}

fn ensure_pool_init() {
    PACKET_POOL.init();
}

pub fn test_netdev_stats_default_zeroed() -> TestResult {
    let stats = NetDeviceStats::default();
    assert_eq_test!(stats.rx_packets, 0, "rx_packets starts at 0");
    assert_eq_test!(stats.tx_packets, 0, "tx_packets starts at 0");
    assert_eq_test!(stats.rx_bytes, 0, "rx_bytes starts at 0");
    assert_eq_test!(stats.tx_bytes, 0, "tx_bytes starts at 0");
    assert_eq_test!(stats.rx_errors, 0, "rx_errors starts at 0");
    assert_eq_test!(stats.tx_errors, 0, "tx_errors starts at 0");
    assert_eq_test!(stats.rx_dropped, 0, "rx_dropped starts at 0");
    assert_eq_test!(stats.tx_dropped, 0, "tx_dropped starts at 0");
    pass!()
}

pub fn test_netdev_stats_new_equals_default() -> TestResult {
    let from_new = NetDeviceStats::new();
    let from_default = NetDeviceStats::default();
    assert_eq_test!(from_new, from_default, "new() == default()");
    pass!()
}

pub fn test_netdev_stats_accumulation() -> TestResult {
    let mut stats = NetDeviceStats::new();

    stats.rx_packets += 100;
    stats.tx_packets += 50;
    stats.rx_bytes += 102400;
    stats.tx_bytes += 51200;
    stats.rx_errors += 3;
    stats.tx_errors += 1;
    stats.rx_dropped += 7;
    stats.tx_dropped += 2;

    assert_eq_test!(stats.rx_packets, 100, "rx_packets after increment");
    assert_eq_test!(stats.tx_packets, 50, "tx_packets after increment");
    assert_eq_test!(stats.rx_bytes, 102400, "rx_bytes after increment");
    assert_eq_test!(stats.tx_bytes, 51200, "tx_bytes after increment");
    assert_eq_test!(stats.rx_errors, 3, "rx_errors after increment");
    assert_eq_test!(stats.tx_errors, 1, "tx_errors after increment");
    assert_eq_test!(stats.rx_dropped, 7, "rx_dropped after increment");
    assert_eq_test!(stats.tx_dropped, 2, "tx_dropped after increment");

    assert_eq_test!(stats.total_packets(), 150, "total_packets = rx + tx");
    assert_eq_test!(stats.total_bytes(), 153600, "total_bytes = rx + tx");
    assert_eq_test!(stats.total_errors(), 4, "total_errors = rx + tx");
    assert_eq_test!(stats.total_dropped(), 9, "total_dropped = rx + tx");
    pass!()
}

pub fn test_netdev_stats_copy() -> TestResult {
    let mut original = NetDeviceStats::new();
    original.rx_packets = 42;
    original.tx_bytes = 1024;

    let copy = original;
    assert_eq_test!(copy.rx_packets, 42, "copy preserves rx_packets");
    assert_eq_test!(copy.tx_bytes, 1024, "copy preserves tx_bytes");
    assert_eq_test!(original, copy, "original == copy");
    pass!()
}

pub fn test_features_empty() -> TestResult {
    let feats = NetDeviceFeatures::empty();
    assert_test!(feats.is_empty(), "empty features has no flags set");
    assert_test!(
        !feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "empty has no CHECKSUM_TX"
    );
    assert_test!(
        !feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "empty has no CHECKSUM_RX"
    );
    pass!()
}

pub fn test_features_individual() -> TestResult {
    let tx = NetDeviceFeatures::CHECKSUM_TX;
    assert_test!(
        tx.contains(NetDeviceFeatures::CHECKSUM_TX),
        "has CHECKSUM_TX"
    );
    assert_test!(
        !tx.contains(NetDeviceFeatures::CHECKSUM_RX),
        "no CHECKSUM_RX"
    );
    assert_test!(!tx.contains(NetDeviceFeatures::TSO), "no TSO");
    assert_test!(!tx.contains(NetDeviceFeatures::VLAN_TAG), "no VLAN_TAG");
    pass!()
}

pub fn test_features_combination() -> TestResult {
    let feats = NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX;
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "combined has CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "combined has CHECKSUM_RX"
    );
    assert_test!(!feats.contains(NetDeviceFeatures::TSO), "combined no TSO");
    assert_test!(!feats.is_empty(), "combined is not empty");
    pass!()
}

pub fn test_features_all() -> TestResult {
    let feats = NetDeviceFeatures::all();
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "all has CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "all has CHECKSUM_RX"
    );
    assert_test!(feats.contains(NetDeviceFeatures::TSO), "all has TSO");
    assert_test!(
        feats.contains(NetDeviceFeatures::VLAN_TAG),
        "all has VLAN_TAG"
    );
    pass!()
}

pub fn test_features_default_is_empty() -> TestResult {
    let feats = NetDeviceFeatures::default();
    assert_test!(feats.is_empty(), "default features is empty");
    assert_eq_test!(feats, NetDeviceFeatures::empty(), "default == empty");
    pass!()
}

pub fn test_registry_register_and_enumerate() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    assert_eq_test!(registry.device_count(), 0, "empty registry has 0 devices");
    assert_test!(
        registry.enumerate().is_empty(),
        "enumerate on empty is empty"
    );

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let dev1 = KArc::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc");
    let handle1 = match registry.register(dev1) {
        Some(h) => h,
        None => return slopos_testing::fail!("register should succeed"),
    };

    assert_eq_test!(handle1.index(), DevIndex(0), "first device gets index 0");
    assert_eq_test!(handle1.mac(), mac1, "handle.mac() matches");
    assert_eq_test!(handle1.mtu(), 1500, "handle.mtu() matches");
    assert_eq_test!(registry.device_count(), 1, "1 device registered");

    let enumerated = registry.enumerate();
    assert_eq_test!(enumerated.len(), 1, "enumerate returns 1 entry");
    assert_eq_test!(enumerated[0].0, DevIndex(0), "enum index 0");
    assert_eq_test!(enumerated[0].1, mac1, "enum mac matches");
    assert_eq_test!(enumerated[0].2, true, "enum is_up=true");
    pass!()
}

pub fn test_registry_register_multiple() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mac2 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
    let mac3 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x03]);

    let h1 = registry.register(KArc::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc"));
    let h2 = registry.register(KArc::try_new(MockNetDevice::new(mac2, 9000)).expect("test alloc"));
    let h3 = registry.register(KArc::try_new(MockNetDevice::new(mac3, 1500)).expect("test alloc"));

    assert_test!(h1.is_some(), "register #1 succeeds");
    assert_test!(h2.is_some(), "register #2 succeeds");
    assert_test!(h3.is_some(), "register #3 succeeds");

    let h1 = h1.unwrap();
    let h2 = h2.unwrap();
    let h3 = h3.unwrap();

    assert_eq_test!(h1.index(), DevIndex(0), "dev 1 at index 0");
    assert_eq_test!(h2.index(), DevIndex(1), "dev 2 at index 1");
    assert_eq_test!(h3.index(), DevIndex(2), "dev 3 at index 2");

    assert_eq_test!(h2.mtu(), 9000, "dev 2 has mtu 9000");
    assert_eq_test!(registry.device_count(), 3, "3 devices registered");

    let enumerated = registry.enumerate();
    assert_eq_test!(enumerated.len(), 3, "enumerate returns 3 entries");
    pass!()
}

pub fn test_registry_unregister() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));

    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xAA]);
    let _handle =
        registry.register(KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc"));
    assert_eq_test!(registry.device_count(), 1, "1 device before unregister");

    let removed = registry.unregister(DevIndex(0));
    assert_test!(removed, "unregister returns true for occupied slot");
    assert_eq_test!(registry.device_count(), 0, "0 devices after unregister");
    assert_test!(
        registry.enumerate().is_empty(),
        "enumerate is empty after unregister"
    );

    let removed_again = registry.unregister(DevIndex(0));
    assert_test!(!removed_again, "double unregister returns false");
    pass!()
}

/// A retained handle outlives unregistration, so it is the one path that can
/// still reach a retired device; its send failing proves `set_down` ran.
pub fn test_registry_unregister_calls_set_down() -> TestResult {
    ensure_pool_init();
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xBB]);
    let dev = MockNetDevice::new(mac, 1500);
    dev.set_up();

    let handle = registry
        .register(KArc::try_new(dev).expect("test alloc"))
        .expect("register");
    let pkt = PacketBuf::alloc().expect("alloc pkt");
    assert_test!(handle.tx(pkt).is_ok(), "a live device accepts a send");

    let removed = registry.unregister(DevIndex(0));
    assert_test!(removed, "unregister succeeded");

    let pkt = PacketBuf::alloc().expect("alloc pkt");
    assert_test!(
        handle.tx(pkt).is_err(),
        "a retained handle must not drive a retired device"
    );
    pass!()
}

/// The registry stops resolving a device the moment retirement begins, so an
/// index-addressed send cannot reach one that is going away.
pub fn test_registry_unregister_rejects_tx_by_index() -> TestResult {
    ensure_pool_init();
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xBC]);
    let _handle = registry
        .register(KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc"))
        .expect("register");

    registry.unregister(DevIndex(0));

    let pkt = PacketBuf::alloc().expect("alloc pkt");
    assert_test!(
        registry.tx_by_index(DevIndex(0), pkt).is_err(),
        "an unregistered index must not transmit"
    );
    assert_test!(
        registry.mac_by_index(DevIndex(0)).is_none(),
        "an unregistered index resolves to nothing"
    );
    pass!()
}

/// At capacity every device must be polled: one silently skipped by
/// `poll_tx_all` stalls its TX reclaim with nothing to report.
pub fn test_registry_poll_tx_all_visits_every_device() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mut devices = KVec::new();
    for i in 0..8u8 {
        let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, i]);
        let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
        let cloned = KArc::clone(&dev);
        registry.register(cloned).expect("register");
        devices.push(dev).expect("test alloc");
    }
    assert_eq_test!(registry.device_count(), 8, "registry at capacity");

    registry.poll_tx_all();

    for dev in devices.iter() {
        assert_eq_test!(*dev.poll_tx_count.lock(), 1, "every device polled once");
    }
    pass!()
}

pub fn test_registry_slot_reuse() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));

    let mac1 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    let mac2 = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);

    let h1 = registry.register(KArc::try_new(MockNetDevice::new(mac1, 1500)).expect("test alloc"));
    assert_test!(h1.is_some(), "first register succeeds");
    let h1 = h1.unwrap();
    assert_eq_test!(h1.index(), DevIndex(0), "first device at index 0");

    registry.unregister(DevIndex(0));

    let h2 = registry.register(KArc::try_new(MockNetDevice::new(mac2, 1500)).expect("test alloc"));
    assert_test!(h2.is_some(), "re-register succeeds");
    let h2 = h2.unwrap();
    assert_eq_test!(h2.index(), DevIndex(0), "reuses slot 0");
    assert_eq_test!(h2.mac(), mac2, "new device's mac");
    pass!()
}

/// The slot must stay occupied until `set_down` returns; free it first and
/// `register` hands the same index to a new device while the old one is still
/// shutting down, after which an index-addressed send reaches the wrong NIC.
/// `set_down` re-enters the registry, the only vantage point that is visible
/// from.
static RETIRE_PROBE_REGISTRY: NetDeviceRegistry =
    NetDeviceRegistry::new(lock_class!("test.netdev_retire_probe", LOCK_LEVEL_REGISTRY));

/// Index handed out by a `register` racing the retirement, or `usize::MAX` if
/// the registry refused.
static RETIRE_PROBE_REISSUED: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Whether the retiring index still resolved to a device mid-retirement.
static RETIRE_PROBE_RESOLVED: AtomicBool = AtomicBool::new(true);

struct RetireProbeDevice;

impl NetDevice for RetireProbeDevice {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        Ok(())
    }

    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }

    fn set_up(&self) {}

    fn set_down(&self) {
        let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xEE]);
        let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
        let reissued = RETIRE_PROBE_REGISTRY
            .register(dev)
            .map(|h| h.index().0)
            .unwrap_or(usize::MAX);
        RETIRE_PROBE_REISSUED.store(reissued, Ordering::Relaxed);
        RETIRE_PROBE_RESOLVED.store(
            RETIRE_PROBE_REGISTRY.mac_by_index(DevIndex(0)).is_some(),
            Ordering::Relaxed,
        );
    }

    fn mtu(&self) -> u16 {
        1500
    }

    fn mac(&self) -> MacAddr {
        MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0xED])
    }

    fn stats(&self) -> NetDeviceStats {
        NetDeviceStats::new()
    }

    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }
}

pub fn test_registry_retiring_slot_is_not_reissued() -> TestResult {
    let probe = KArc::try_new(RetireProbeDevice).expect("test alloc");
    let handle = RETIRE_PROBE_REGISTRY.register(probe).expect("register");
    assert_eq_test!(handle.index(), DevIndex(0), "probe at index 0");

    RETIRE_PROBE_REGISTRY.unregister(DevIndex(0));

    assert_test!(
        RETIRE_PROBE_REISSUED.load(Ordering::Relaxed) != 0,
        "index 0 was reissued while its device was still shutting down"
    );
    assert_test!(
        !RETIRE_PROBE_RESOLVED.load(Ordering::Relaxed),
        "a retiring index must resolve to nothing"
    );
    pass!()
}

pub fn test_registry_unregister_out_of_range() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let removed = registry.unregister(DevIndex(999));
    assert_test!(!removed, "unregister out-of-range returns false");
    pass!()
}

pub fn test_handle_tx() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0xCA, 0xFE, 0x00, 0x00, 0x01]);
    let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => return slopos_testing::fail!("PacketBuf::alloc failed"),
    };

    let result = handle.tx(pkt);
    assert_test!(result.is_ok(), "tx should succeed");

    let stats = handle.stats();
    assert_eq_test!(stats.tx_packets, 1, "stats.tx_packets == 1 after TX");
    pass!()
}

pub fn test_handle_poll_rx_empty() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0xDE, 0xAD, 0x00, 0x00, 0x01]);
    let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let pkts = handle.poll_rx(16, &PACKET_POOL);
    assert_test!(pkts.is_empty(), "mock poll_rx returns empty");
    pass!()
}

pub fn test_handle_stats() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x55]);
    let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let stats = handle.stats();
    assert_eq_test!(stats, NetDeviceStats::new(), "initial stats are zeroed");
    pass!()
}

pub fn test_handle_features() -> TestResult {
    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x66]);
    let dev = KArc::try_new(
        MockNetDevice::new(mac, 1500)
            .with_features(NetDeviceFeatures::CHECKSUM_TX | NetDeviceFeatures::CHECKSUM_RX),
    )
    .expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let feats = handle.features();
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_TX),
        "handle reports CHECKSUM_TX"
    );
    assert_test!(
        feats.contains(NetDeviceFeatures::CHECKSUM_RX),
        "handle reports CHECKSUM_RX"
    );
    assert_test!(!feats.contains(NetDeviceFeatures::TSO), "handle no TSO");
    pass!()
}

/// Structural: `tx()` is called with the registry lock already held, so it
/// would deadlock if it took that lock — `SpinLock` is non-reentrant.
pub fn test_handle_tx_does_not_acquire_registry_lock() -> TestResult {
    ensure_pool_init();

    let registry = NetDeviceRegistry::new(lock_class!("test.netdev_registry", LOCK_LEVEL_REGISTRY));
    let mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x77]);
    let dev = KArc::try_new(MockNetDevice::new(mac, 1500)).expect("test alloc");
    let handle = match registry.register(dev) {
        Some(h) => h,
        None => return slopos_testing::fail!("register failed"),
    };

    let _guard = registry.inner.lock();

    let pkt = match PacketBuf::alloc() {
        Some(p) => p,
        None => {
            drop(_guard);
            return slopos_testing::fail!("PacketBuf::alloc failed");
        }
    };

    let result = handle.tx(pkt);
    assert_test!(result.is_ok(), "tx succeeds while registry lock is held");

    drop(_guard);
    pass!()
}

slopos_testing::stest!(name = test_netdev_stats_default_zeroed, suite = netdev);
slopos_testing::stest!(name = test_netdev_stats_new_equals_default, suite = netdev);
slopos_testing::stest!(name = test_netdev_stats_accumulation, suite = netdev);
slopos_testing::stest!(name = test_netdev_stats_copy, suite = netdev);
slopos_testing::stest!(name = test_features_empty, suite = netdev);
slopos_testing::stest!(name = test_features_individual, suite = netdev);
slopos_testing::stest!(name = test_features_combination, suite = netdev);
slopos_testing::stest!(name = test_features_all, suite = netdev);
slopos_testing::stest!(name = test_features_default_is_empty, suite = netdev);
slopos_testing::stest!(name = test_registry_register_and_enumerate, suite = netdev);
slopos_testing::stest!(name = test_registry_register_multiple, suite = netdev);
slopos_testing::stest!(name = test_registry_unregister, suite = netdev);
slopos_testing::stest!(
    name = test_registry_unregister_calls_set_down,
    suite = netdev
);
slopos_testing::stest!(name = test_registry_slot_reuse, suite = netdev);
slopos_testing::stest!(
    name = test_registry_unregister_rejects_tx_by_index,
    suite = netdev
);
slopos_testing::stest!(
    name = test_registry_poll_tx_all_visits_every_device,
    suite = netdev
);
slopos_testing::stest!(
    name = test_registry_retiring_slot_is_not_reissued,
    suite = netdev
);
slopos_testing::stest!(name = test_registry_unregister_out_of_range, suite = netdev);
slopos_testing::stest!(name = test_handle_tx, suite = netdev);
slopos_testing::stest!(name = test_handle_poll_rx_empty, suite = netdev);
slopos_testing::stest!(name = test_handle_stats, suite = netdev);
slopos_testing::stest!(name = test_handle_features, suite = netdev);
slopos_testing::stest!(
    name = test_handle_tx_does_not_acquire_registry_lock,
    suite = netdev
);
