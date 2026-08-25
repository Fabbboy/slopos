//! Tests for the Rust-typed XDP filter chain.
//!
//! Recording filters use atomics — never a `SpinLock` — because
//! `XdpFilter::execute` runs under a `NET_EPOCH` read guard where acquiring a
//! tracked lock is forbidden.
//!
//! [`crate::ingress::net_rx_inner`] reads [`XDP`] and no other chain, so a test
//! of the ingress path has to publish into the kernel-wide one. Every such test
//! holds [`Quiesced`] first: the physical NIC's frames reach the chain through
//! that same funnel, and a `Drop` verdict standing over the live NIC is a
//! disconnected machine for as long as the test runs. A test that only needs a
//! verdict runs its filters through [`LOCAL_CHAIN`] and publishes nothing.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_REGISTRY;

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KBox, KVec};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, pass};

use crate::ETH_HEADER_LEN;
use crate::ingress::{self, net_rx_injected};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::*;
use crate::packetbuf::PacketBuf;
use crate::pool::{PACKET_POOL, PacketPool};
use crate::types::*;
use crate::xdp::{PacketView, XDP, XdpAction, XdpFilter, XdpHookChain, xdp_filter};

// Each `#[xdp_filter]` fn generates an upper-cased `'static` instance.

#[xdp_filter]
fn xdp_test_drop_all(_pkt: &mut PacketView<'_>) -> XdpAction {
    XdpAction::Drop
}

#[xdp_filter]
fn xdp_test_pass_all(_pkt: &mut PacketView<'_>) -> XdpAction {
    XdpAction::Pass
}

#[xdp_filter]
fn xdp_test_tx_all(_pkt: &mut PacketView<'_>) -> XdpAction {
    XdpAction::Tx
}

static ORDER_LOG: AtomicU32 = AtomicU32::new(0);

fn order_push(nibble: u32) {
    // Single-threaded stest phase: load+store is sufficient.
    let prev = ORDER_LOG.load(Ordering::Relaxed);
    ORDER_LOG.store((prev << 4) | (nibble & 0xF), Ordering::Relaxed);
}

struct RecA;
impl XdpFilter for RecA {
    fn execute(&self, _pkt: &mut PacketView<'_>) -> XdpAction {
        order_push(0xA);
        XdpAction::Pass
    }
}
static REC_A: RecA = RecA;

struct RecB;
impl XdpFilter for RecB {
    fn execute(&self, _pkt: &mut PacketView<'_>) -> XdpAction {
        order_push(0xB);
        XdpAction::Pass
    }
}
static REC_B: RecB = RecB;

static REDIRECT_TARGET: AtomicUsize = AtomicUsize::new(0);

struct RedirectFilter;
impl XdpFilter for RedirectFilter {
    fn execute(&self, _pkt: &mut PacketView<'_>) -> XdpAction {
        XdpAction::Redirect(DevIndex(REDIRECT_TARGET.load(Ordering::Relaxed)))
    }
}
static REDIRECT_FILTER: RedirectFilter = RedirectFilter;

struct MockNetDevice {
    mac_addr: MacAddr,
    dev_mtu: u16,
    feats: NetDeviceFeatures,
    stats: SpinLock<NetDeviceStats>,
    is_up: SpinLock<bool>,
}

impl MockNetDevice {
    fn new(mac: MacAddr) -> Self {
        Self {
            mac_addr: mac,
            dev_mtu: 1500,
            feats: NetDeviceFeatures::empty(),
            stats: SpinLock::new(
                NetDeviceStats::new(),
                lock_class!("test.xdp_dev.is_up", LOCK_LEVEL_RESOURCE),
            ),
            is_up: SpinLock::new(
                false,
                lock_class!("test.xdp_dev.tx_count", LOCK_LEVEL_RESOURCE),
            ),
        }
    }
}

impl NetDevice for MockNetDevice {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        self.stats.lock().tx_packets += 1;
        Ok(())
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

fn make_test_handle(mac: MacAddr) -> DeviceHandle {
    let registry = KBox::leak(
        KBox::try_new(NetDeviceRegistry::new(lock_class!(
            "test.xdp_registry",
            LOCK_LEVEL_REGISTRY
        )))
        .expect("test alloc"),
    );
    let dev = KArc::try_new(MockNetDevice::new(mac)).expect("test alloc");
    registry.register(dev).expect("register must succeed")
}

fn build_frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> KVec<u8> {
    let mut f = KVec::<u8>::with_capacity(ETH_HEADER_LEN + payload.len()).expect("test alloc");
    f.extend_from_slice(&dst).expect("test alloc");
    f.extend_from_slice(&src).expect("test alloc");
    f.extend_from_slice(&ethertype.to_be_bytes())
        .expect("test alloc");
    f.extend_from_slice(payload).expect("test alloc");
    f
}

fn build_ipv4_header(proto: u8, src: [u8; 4], dst: [u8; 4], payload_len: usize) -> [u8; 20] {
    let total_len = (20 + payload_len) as u16;
    let mut hdr = [0u8; 20];
    hdr[0] = 0x45;
    hdr[2..4].copy_from_slice(&total_len.to_be_bytes());
    hdr[8] = 64;
    hdr[9] = proto;
    hdr[12..16].copy_from_slice(&src);
    hdr[16..20].copy_from_slice(&dst);
    let csum = crate::checksum::internet_checksum(&hdr);
    hdr[10..12].copy_from_slice(&csum.to_be_bytes());
    hdr
}

fn build_arp_request(
    dst_mac: [u8; 6],
    sender_mac: [u8; 6],
    sender_ip: [u8; 4],
    target_ip: [u8; 4],
) -> KVec<u8> {
    let mut arp = [0u8; 28];
    arp[0..2].copy_from_slice(&1u16.to_be_bytes()); // htype = Ethernet
    arp[2..4].copy_from_slice(&EtherType::Ipv4.as_u16().to_be_bytes()); // ptype = IPv4
    arp[4] = 6; // hlen
    arp[5] = 4; // plen
    arp[6..8].copy_from_slice(&1u16.to_be_bytes()); // oper = request
    arp[8..14].copy_from_slice(&sender_mac);
    arp[14..18].copy_from_slice(&sender_ip);
    arp[24..28].copy_from_slice(&target_ip);
    build_frame(dst_mac, sender_mac, EtherType::Arp.as_u16(), &arp)
}

fn build_tcp_frame(dst_mac: [u8; 6], src_mac: [u8; 6], src_port: u16, dst_port: u16) -> KVec<u8> {
    let mut tcp = [0u8; 20];
    tcp[0..2].copy_from_slice(&src_port.to_be_bytes());
    tcp[2..4].copy_from_slice(&dst_port.to_be_bytes());
    tcp[12] = 5 << 4; // data offset = 5 words
    let ipv4 = build_ipv4_header(
        IpProtocol::Tcp.as_u8(),
        [10, 0, 0, 1],
        [10, 0, 0, 2],
        tcp.len(),
    );
    let mut payload: KVec<u8> = KVec::new();
    payload.extend_from_slice(&ipv4).expect("test alloc");
    payload.extend_from_slice(&tcp).expect("test alloc");
    build_frame(dst_mac, src_mac, EtherType::Ipv4.as_u16(), &payload)
}

fn chain_of(filters: &[&'static dyn XdpFilter]) -> Option<KVec<&'static dyn XdpFilter>> {
    let mut v: KVec<&'static dyn XdpFilter> = KVec::new();
    for &f in filters {
        v.push(f).ok()?;
    }
    Some(v)
}

/// A chain nothing else executes, for the tests that assert on a verdict rather
/// than on what the ingress path did with one.
static LOCAL_CHAIN: XdpHookChain = XdpHookChain::new();

/// Shuts the physical NIC's ingress funnel for as long as it lives, so a filter
/// published in [`XDP`] below can only ever be reached by this test's own
/// injected frame.
struct Quiesced;

impl Quiesced {
    fn enter() -> Self {
        ingress::quiesce_begin();
        Self
    }
}

impl Drop for Quiesced {
    fn drop(&mut self) {
        ingress::quiesce_end();
    }
}

/// A kernel-wide install that cannot outlive the test that made it: an
/// `assert_*` returning early must not leave a verdict standing over every
/// device in the machine.
struct GlobalChain;

impl GlobalChain {
    fn install(filters: &[&'static dyn XdpFilter]) -> Option<Self> {
        XDP.install(chain_of(filters)?).ok()?;
        Some(Self)
    }
}

impl Drop for GlobalChain {
    fn drop(&mut self) {
        XDP.clear();
    }
}

/// The one cache key a test needs absent beforehand and gone afterwards,
/// established without emptying the table the live NIC's gateway entry is in.
///
/// The index is not private: `make_test_handle`'s private registry hands out
/// `DevIndex(0)`, which in the global cache this writes to is loopback. Safe
/// only because nothing else claims these addresses there.
struct ScopedNeighbor {
    dev: DevIndex,
    ip: Ipv4Addr,
}

impl ScopedNeighbor {
    fn cleared(dev: DevIndex, ip: Ipv4Addr) -> Self {
        let scoped = Self { dev, ip };
        scoped.remove();
        scoped
    }

    fn remove(&self) {
        drop(NEIGHBOR_CACHE.remove(self.dev, self.ip));
    }
}

impl Drop for ScopedNeighbor {
    fn drop(&mut self) {
        self.remove();
    }
}

const DEV_MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const SENDER_MAC: [u8; 6] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
const BROADCAST: [u8; 6] = [0xff; 6];

pub fn test_xdp_empty_chain_passes() -> TestResult {
    ensure_pool_init();
    XDP.clear();

    assert_test!(XDP.is_empty(), "cleared chain is empty");
    let frame = build_arp_request(BROADCAST, SENDER_MAC, [192, 168, 1, 42], [192, 168, 1, 1]);
    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    let verdict = XDP.execute(&mut PacketView::new(&mut pkt));
    assert_eq_test!(verdict, XdpAction::Pass, "empty chain returns Pass");
    pass!()
}

pub fn test_xdp_filter_drop_drops_packet() -> TestResult {
    ensure_pool_init();
    let _quiesced = Quiesced::enter();
    let handle = make_test_handle(MacAddr(DEV_MAC));
    let sender_ip = Ipv4Addr([192, 168, 1, 42]);
    let _neighbor = ScopedNeighbor::cleared(handle.index(), sender_ip);

    let Some(_chain) = GlobalChain::install(&[&XDP_TEST_DROP_ALL]) else {
        return slopos_testing::fail!("xdp install");
    };

    let frame = build_arp_request(BROADCAST, SENDER_MAC, sender_ip.0, [192, 168, 1, 1]);
    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    net_rx_injected(&handle, pkt);

    assert_test!(
        NEIGHBOR_CACHE.lookup(handle.index(), sender_ip).is_none(),
        "drop filter suppresses stack dispatch"
    );
    pass!()
}

pub fn test_xdp_filter_pass_falls_through() -> TestResult {
    ensure_pool_init();
    let _quiesced = Quiesced::enter();
    let handle = make_test_handle(MacAddr(DEV_MAC));
    let sender_ip = Ipv4Addr([192, 168, 1, 43]);
    let _neighbor = ScopedNeighbor::cleared(handle.index(), sender_ip);

    let Some(_chain) = GlobalChain::install(&[&XDP_TEST_PASS_ALL]) else {
        return slopos_testing::fail!("xdp install");
    };

    let frame = build_arp_request(BROADCAST, SENDER_MAC, sender_ip.0, [192, 168, 1, 1]);
    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    net_rx_injected(&handle, pkt);

    match NEIGHBOR_CACHE.lookup(handle.index(), sender_ip) {
        Some(mac) => assert_eq_test!(mac, MacAddr(SENDER_MAC), "learned sender MAC"),
        None => return slopos_testing::fail!("pass filter should let the stack run"),
    }
    pass!()
}

/// Ordering is a property of `execute` walking a chain, so it is asserted
/// against a chain of this test's own. `ORDER_LOG` records one entry per
/// filter run: published in [`XDP`], every frame the live NIC received would
/// append to it and the recorded order would be somebody else's traffic.
pub fn test_xdp_filter_chain_order() -> TestResult {
    ensure_pool_init();

    let Some(first) = chain_of(&[&REC_A, &REC_B, &XDP_TEST_DROP_ALL]) else {
        return slopos_testing::fail!("test alloc");
    };
    if LOCAL_CHAIN.install(first).is_err() {
        return slopos_testing::fail!("xdp install");
    }
    ORDER_LOG.store(0, Ordering::Relaxed);
    let frame = build_arp_request(BROADCAST, SENDER_MAC, [192, 168, 1, 44], [192, 168, 1, 1]);
    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    let verdict = LOCAL_CHAIN.execute(&mut PacketView::new(&mut pkt));
    assert_eq_test!(verdict, XdpAction::Drop, "first non-Pass (Drop) wins");
    assert_eq_test!(
        ORDER_LOG.load(Ordering::Relaxed),
        0xAB,
        "filters ran in registration order A then B"
    );

    let Some(second) = chain_of(&[&XDP_TEST_DROP_ALL, &REC_A]) else {
        return slopos_testing::fail!("test alloc");
    };
    if LOCAL_CHAIN.install(second).is_err() {
        return slopos_testing::fail!("xdp install");
    }
    ORDER_LOG.store(0, Ordering::Relaxed);
    let mut pkt2 = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    let verdict2 = LOCAL_CHAIN.execute(&mut PacketView::new(&mut pkt2));
    assert_eq_test!(verdict2, XdpAction::Drop, "leading Drop wins");
    assert_eq_test!(
        ORDER_LOG.load(Ordering::Relaxed),
        0,
        "filter after a Drop never runs"
    );

    LOCAL_CHAIN.clear();
    pass!()
}

pub fn test_xdp_tx_action() -> TestResult {
    ensure_pool_init();
    let _quiesced = Quiesced::enter();
    let handle = make_test_handle(MacAddr(DEV_MAC));

    let Some(_chain) = GlobalChain::install(&[&XDP_TEST_TX_ALL]) else {
        return slopos_testing::fail!("xdp install");
    };

    let frame = build_arp_request(BROADCAST, SENDER_MAC, [192, 168, 1, 45], [192, 168, 1, 1]);
    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };
    net_rx_injected(&handle, pkt);

    assert_eq_test!(
        handle.stats().tx_packets,
        1,
        "Tx verdict re-transmits via the ingress device"
    );
    pass!()
}

pub fn test_xdp_redirect_action() -> TestResult {
    ensure_pool_init();
    let _quiesced = Quiesced::enter();
    let ingress_handle = make_test_handle(MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x10]));

    // Target must live in the global registry: net_rx's Redirect arm transmits
    // via DEVICE_REGISTRY.tx_by_index.
    let target_mac = MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, 0x11]);
    let target = KArc::try_new(MockNetDevice::new(target_mac)).expect("test alloc");
    let target_handle = match DEVICE_REGISTRY.register(target) {
        Some(h) => h,
        None => return slopos_testing::fail!("register target in global registry"),
    };
    REDIRECT_TARGET.store(target_handle.index().0, Ordering::Relaxed);

    let Some(_chain) = GlobalChain::install(&[&REDIRECT_FILTER]) else {
        DEVICE_REGISTRY.unregister(target_handle.index());
        return slopos_testing::fail!("xdp install");
    };

    let frame = build_arp_request(BROADCAST, SENDER_MAC, [192, 168, 1, 46], [192, 168, 1, 1]);
    let pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => {
            DEVICE_REGISTRY.unregister(target_handle.index());
            return slopos_testing::fail!("from_raw_copy should succeed");
        }
    };
    net_rx_injected(&ingress_handle, pkt);

    let tx = target_handle.stats().tx_packets;
    DEVICE_REGISTRY.unregister(target_handle.index());

    assert_eq_test!(tx, 1, "Redirect transmits via the target device");
    pass!()
}

pub fn test_xdp_packet_view_parses() -> TestResult {
    ensure_pool_init();

    let frame = build_tcp_frame(DEV_MAC, SENDER_MAC, 1234, 80);
    let mut pkt = match PacketBuf::from_raw_copy(&frame) {
        Some(p) => p,
        None => return slopos_testing::fail!("from_raw_copy should succeed"),
    };

    {
        let view = PacketView::new(&mut pkt);
        let eth = match view.ethernet() {
            Some(e) => e,
            None => return slopos_testing::fail!("ethernet parse"),
        };
        assert_eq_test!(eth.ethertype, EtherType::Ipv4.as_u16(), "ethertype IPv4");

        let ip = match view.ipv4() {
            Some(i) => i,
            None => return slopos_testing::fail!("ipv4 parse"),
        };
        assert_eq_test!(ip.protocol, IpProtocol::Tcp.as_u8(), "protocol TCP");

        let tcp = match view.tcp() {
            Some(t) => t,
            None => return slopos_testing::fail!("tcp parse"),
        };
        assert_eq_test!(tcp.src_port, 1234, "tcp src_port");
        assert_eq_test!(tcp.dst_port, 80, "tcp dst_port");
    }

    {
        let mut view = PacketView::new(&mut pkt);
        view.frame_mut()[0] = 0xEE;
        assert_eq_test!(view.frame()[0], 0xEE, "mutable frame write is visible");
    }

    pass!()
}

slopos_testing::stest!(name = test_xdp_empty_chain_passes, suite = xdp);
slopos_testing::stest!(name = test_xdp_filter_drop_drops_packet, suite = xdp);
slopos_testing::stest!(name = test_xdp_filter_pass_falls_through, suite = xdp);
slopos_testing::stest!(name = test_xdp_filter_chain_order, suite = xdp);
slopos_testing::stest!(name = test_xdp_tx_action, suite = xdp);
slopos_testing::stest!(name = test_xdp_redirect_action, suite = xdp);
slopos_testing::stest!(name = test_xdp_packet_view_parses, suite = xdp);
