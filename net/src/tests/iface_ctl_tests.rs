//! Tests for the interface control plane: what an administrative up or down
//! actually does to the rest of the stack.
//!
//! These need a *real* device registered in the global registry, because the
//! whole point of the sequence under test is that it calls into a driver and
//! edits three other tables. So each test registers its own mock, exercises it,
//! and then unregisters and detaches — the tree is left exactly as it was
//! found. Nothing here clears a global table: the kernel these run inside has a
//! live NIC whose configuration later tests depend on.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::net::NET_MAX_IFACES;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec, lock_class};

use crate::iface::{self, AddrOrigin, AddrScope, IfaceAddr, IfaceError, IfaceKind, OperState};
use crate::iface_ctl;
use crate::neighbor::{NEIGHBOR_CACHE, NeighborSnapshot};
use crate::netdev::{DEVICE_REGISTRY, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::packetbuf::PacketBuf;
use crate::pool::{PACKET_POOL, PacketPool};
use crate::route::{ROUTE_TABLE, RouteEntry};
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

/// A device that records what the control plane did to it.
///
/// The counters are the point: "did the transition reach the driver, exactly
/// once" is not observable from the interface table alone.
struct AdminMock {
    mac: MacAddr,
    up_calls: AtomicU32,
    down_calls: AtomicU32,
    carrier: SpinLock<bool>,
}

impl AdminMock {
    fn new(mac: MacAddr) -> Self {
        Self {
            mac,
            up_calls: AtomicU32::new(0),
            down_calls: AtomicU32::new(0),
            carrier: SpinLock::new(
                true,
                lock_class!("test.admin_mock.carrier", LOCK_LEVEL_RESOURCE),
            ),
        }
    }
}

impl NetDevice for AdminMock {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        Ok(())
    }
    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }
    fn set_up(&self) {
        self.up_calls.fetch_add(1, Ordering::Relaxed);
    }
    fn set_down(&self) {
        self.down_calls.fetch_add(1, Ordering::Relaxed);
    }
    fn mtu(&self) -> u16 {
        1500
    }
    fn mac(&self) -> MacAddr {
        self.mac
    }
    fn stats(&self) -> NetDeviceStats {
        NetDeviceStats::new()
    }
    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }
    fn kind(&self) -> IfaceKind {
        IfaceKind::Ethernet
    }
    fn carrier(&self) -> bool {
        *self.carrier.lock()
    }
    fn carrier_detect(&self) -> bool {
        true
    }
}

/// One registered mock plus its interface, and the counters to assert on.
struct Fixture {
    dev: crate::types::DevIndex,
    ifindex: u32,
    mock: KArc<AdminMock>,
}

impl Fixture {
    /// Register a mock device and attach an interface for it, exactly as the
    /// real probe path does: register first, attach after it returns.
    fn new(kind: IfaceKind, mac: MacAddr) -> Option<Self> {
        let mock = KArc::try_new(AdminMock::new(mac)).ok()?;
        let dyn_dev: KArc<dyn NetDevice + Send + Sync> = mock.clone();
        let handle = DEVICE_REGISTRY.register(dyn_dev)?;
        let dev = handle.index();
        let ifindex = iface::attach(dev, kind, mac, 1500, true, true).ok()?;
        Some(Self { dev, ifindex, mock })
    }

    fn up_calls(&self) -> u32 {
        self.mock.up_calls.load(Ordering::Relaxed)
    }

    fn down_calls(&self) -> u32 {
        self.mock.down_calls.load(Ordering::Relaxed)
    }

    /// Put the tree back exactly as it was found.
    fn teardown(self) {
        ROUTE_TABLE.remove_device_routes(self.dev);
        drop(NEIGHBOR_CACHE.flush_device(self.dev));
        iface::detach(self.dev);
        DEVICE_REGISTRY.unregister(self.dev);
    }
}

/// Routes belonging to one device, so an assertion never counts another's.
fn routes_for(dev: crate::types::DevIndex) -> usize {
    ROUTE_TABLE
        .all_routes()
        .iter()
        .filter(|r| r.dev == dev)
        .count()
}

/// Bringing an interface down must reach the driver exactly once, withdraw its
/// routes, and leave every other device's routes alone.
fn test_admin_down_reaches_the_driver_and_withdraws_routes() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 1])) else {
        return fail!("could not register a mock device");
    };
    let Some(other) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 2])) else {
        f.teardown();
        return fail!("could not register a second mock device");
    };

    // Give both a connected route, and the first a default route too.
    if iface_ctl::configure_ipv4(
        f.dev,
        Ipv4Addr([10, 9, 1, 5]),
        24,
        Ipv4Addr([10, 9, 1, 1]),
        AddrOrigin::Dhcp,
    )
    .is_err()
    {
        f.teardown();
        other.teardown();
        return fail!("configure_ipv4 failed");
    }
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr([10, 9, 2, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev: other.dev,
        metric: 0,
    });

    assert_eq_test!(
        routes_for(f.dev),
        2,
        "connected plus default route installed"
    );
    assert_eq_test!(
        routes_for(other.dev),
        1,
        "the other device has its own route"
    );

    let result = iface_ctl::set_admin_up(f.ifindex, false);
    let (before, after) = match result {
        Ok(pair) => pair,
        Err(e) => {
            f.teardown();
            other.teardown();
            return fail!("set_admin_up(false) failed: {:?}", e);
        }
    };

    assert_eq_test!(before, OperState::Up, "it was up beforehand");
    assert_eq_test!(after, OperState::Down, "and is down afterwards");
    assert_eq_test!(f.down_calls(), 1, "the driver was downed exactly once");
    assert_eq_test!(routes_for(f.dev), 0, "its routes were withdrawn");
    assert_eq_test!(
        routes_for(other.dev),
        1,
        "and no other device's routes were touched"
    );

    f.teardown();
    other.teardown();
    pass!()
}

/// The operator's own configuration survives a down; the lease does not.
///
/// A static address is not the lease's to discard, and an interface being
/// administratively down is exactly what invalidates a lease.
fn test_admin_down_keeps_static_drops_dhcp() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 3])) else {
        return fail!("could not register a mock device");
    };

    let _ = iface::add_addr(
        f.ifindex,
        IfaceAddr::permanent(
            Ipv4Addr([10, 9, 3, 5]),
            24,
            AddrScope::Global,
            AddrOrigin::Dhcp,
        ),
    );
    let _ = iface::add_addr(
        f.ifindex,
        IfaceAddr::permanent(
            Ipv4Addr([192, 168, 30, 7]),
            24,
            AddrScope::Global,
            AddrOrigin::Static,
        ),
    );

    if let Err(e) = iface_ctl::set_admin_up(f.ifindex, false) {
        f.teardown();
        return fail!("set_admin_up(false) failed: {:?}", e);
    }

    let Some(info) = iface::get(f.ifindex) else {
        f.teardown();
        return fail!("interface vanished");
    };
    assert_eq_test!(info.addrs().len(), 1, "exactly one address survives");
    assert_eq_test!(
        info.addrs()[0].addr.0,
        [192, 168, 30, 7],
        "and it is the static one"
    );
    assert_test!(
        info.admin_up == false,
        "administrative intent records the down"
    );

    f.teardown();
    pass!()
}

/// Bringing it back up must re-install the connected route for whatever
/// survived, and reach the driver.
fn test_admin_up_restores_connected_routes() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 4])) else {
        return fail!("could not register a mock device");
    };

    let _ = iface::add_addr(
        f.ifindex,
        IfaceAddr::permanent(
            Ipv4Addr([10, 9, 4, 5]),
            24,
            AddrScope::Global,
            AddrOrigin::Static,
        ),
    );

    let _ = iface_ctl::set_admin_up(f.ifindex, false);
    assert_eq_test!(routes_for(f.dev), 0, "down withdrew everything");

    if let Err(e) = iface_ctl::set_admin_up(f.ifindex, true) {
        f.teardown();
        return fail!("set_admin_up(true) failed: {:?}", e);
    }

    assert_eq_test!(f.up_calls(), 1, "the driver was brought up exactly once");
    assert_eq_test!(
        routes_for(f.dev),
        1,
        "the connected route for the surviving address is back"
    );
    // The default route is deliberately *not* restored: it belongs to whoever
    // learned it, and DHCP re-installs it when it re-binds.
    let has_default = ROUTE_TABLE
        .all_routes()
        .iter()
        .any(|r| r.dev == f.dev && r.prefix_len == 0);
    assert_test!(
        !has_default,
        "a default route is not resurrected by an administrative up"
    );

    f.teardown();
    pass!()
}

/// A learned default route does not survive a down/up cycle.
///
/// `realise` derives the connected route back from an address that survived;
/// the default route was *learned* rather than derived, so only a DHCP re-bind
/// installs it again.
///
/// The `assert_test!` below is non-vacuous by construction: a default route is
/// really installed first, so the assertion is about a route that existed and
/// is gone, not about one that was never there.
fn test_admin_up_does_not_restore_the_default_route() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 12])) else {
        return fail!("could not register a mock device");
    };

    // A static address, so the address itself survives the down and the test is
    // isolated to what happens to the two routes derived from it.
    if iface_ctl::configure_ipv4(
        f.dev,
        Ipv4Addr([10, 9, 12, 5]),
        24,
        Ipv4Addr([10, 9, 12, 1]),
        AddrOrigin::Static,
    )
    .is_err()
    {
        f.teardown();
        return fail!("configure_ipv4 failed");
    }

    let default_before = has_default_route(f.dev);
    let routes_before = routes_for(f.dev);

    let down = iface_ctl::set_admin_up(f.ifindex, false);
    let up = iface_ctl::set_admin_up(f.ifindex, true);

    let default_after = has_default_route(f.dev);
    let routes_after = routes_for(f.dev);
    let addrs_after = iface::get(f.ifindex).map(|i| i.addrs().len());

    f.teardown();

    assert_test!(down.is_ok(), "admin down must succeed");
    assert_test!(up.is_ok(), "admin up must succeed");
    assert_test!(
        default_before,
        "the default route really was installed, so the assertion below is not vacuous"
    );
    assert_eq_test!(routes_before, 2, "connected plus default beforehand");
    assert_eq_test!(
        addrs_after,
        Some(1),
        "the static address survived the cycle"
    );
    assert_eq_test!(
        routes_after,
        1,
        "only the connected route is derived back from that address"
    );
    assert_test!(
        !default_after,
        "the learned default route is gone until a DHCP re-bind installs it"
    );
    pass!()
}

/// Whether a default route is installed for `dev`.
fn has_default_route(dev: DevIndex) -> bool {
    ROUTE_TABLE
        .all_routes()
        .iter()
        .any(|r| r.dev == dev && r.prefix_len == 0)
}

/// A concurrent transition is refused rather than interleaved with a
/// half-applied one.
fn test_admin_guard_refuses_a_second_entrant() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 5])) else {
        return fail!("could not register a mock device");
    };

    // Claim the guard by hand to model a transition already in flight.
    if let Err(e) = iface::try_begin_admin(f.ifindex) {
        f.teardown();
        return fail!("could not claim the guard: {:?}", e);
    }
    assert_eq_test!(
        iface_ctl::set_admin_up(f.ifindex, false),
        Err(IfaceError::Busy),
        "a transition already in flight must refuse a second"
    );
    assert_eq_test!(
        f.down_calls(),
        0,
        "and the refusal must not have touched the driver"
    );
    iface::end_admin(f.ifindex);

    assert_test!(
        iface_ctl::set_admin_up(f.ifindex, false).is_ok(),
        "the guard is reclaimable once released"
    );

    f.teardown();
    pass!()
}

/// A down flushes the interface's neighbours, and the packets those entries had
/// queued go back to the pool rather than leaking.
///
/// The free happens in `iface_ctl`, outside the cache lock, because
/// `PacketBuf::drop` takes the packet pool's lock — which is exactly why
/// `flush_device` hands the packets back instead of dropping them itself. The
/// pool's free count is how that shows up from outside.
fn test_admin_down_flushes_neighbours_and_frees_packets() -> TestResult {
    PACKET_POOL.init();

    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 6])) else {
        return fail!("could not register a mock device");
    };

    let pool_before = PACKET_POOL.available();
    let Some(pkt) = PacketBuf::from_raw_copy(&[0xAA_u8; 64]) else {
        f.teardown();
        return fail!("packet pool has no capacity");
    };
    // An absent neighbour creates an `Incomplete` entry and queues the packet in
    // it. The ARP request the outcome asks for is never sent, which is fine —
    // the entry and its queued packet are the state under test.
    drop(NEIGHBOR_CACHE.resolve(f.dev, Ipv4Addr([10, 9, 6, 9]), pkt));

    let pool_queued = PACKET_POOL.available();
    let queued = queued_pkts_for(f.dev);

    let result = iface_ctl::set_admin_up(f.ifindex, false);

    let pool_after = PACKET_POOL.available();
    let neigh_after = neighbours_for(f.dev);

    f.teardown();

    assert_test!(result.is_ok(), "admin down must succeed");
    // Deterministic half: this device's cache entries are nobody else's.
    assert_eq_test!(
        queued,
        Some(1),
        "the resolve created one entry holding one packet"
    );
    assert_eq_test!(neigh_after, 0, "the down flushed the neighbour entry");
    assert_test!(
        pool_queued < pool_before,
        "the queued packet is held out of the pool"
    );
    // The pool is shared with live NIC receive, so an exact count here would be
    // a flake waiting for a SLIRP frame to land mid-test. `>=` is the assertion
    // that actually catches the defect: a leaked vector returns nothing, giving
    // `pool_after == pool_queued`.
    assert_test!(
        pool_after >= pool_queued + 1,
        "the queued packet was returned to the pool"
    );
    pass!()
}

/// Carrier is the physical link; `admin_up` is what the operator asked for.
/// Losing the first must not rewrite the second, or replugging a cable would
/// bring an interface back administratively down.
fn test_admin_intent_survives_carrier_loss() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 7])) else {
        return fail!("could not register a mock device");
    };

    let enabled = iface::is_enabled();
    let lost = iface::set_carrier(f.dev, false);
    let while_lost = iface::get(f.ifindex).map(|i| (i.admin_up, i.oper_state(enabled)));
    let regained = iface::set_carrier(f.dev, true);
    let after_regain = iface::get(f.ifindex).map(|i| (i.admin_up, i.oper_state(enabled)));
    let down_calls = f.down_calls();

    f.teardown();

    assert_test!(lost.is_some(), "carrier loss is a transition");
    assert_test!(regained.is_some(), "carrier return is a transition");
    // `LowerLayerDown`, not `Down`: RFC 2863 keeps "the operator took it down"
    // and "the cable is out" as distinct states, and collapsing them is what
    // makes a UI tell someone to check a setting when they should check a plug.
    assert_eq_test!(
        while_lost,
        Some((true, OperState::LowerLayerDown)),
        "intent survives carrier loss while the operational state follows the link"
    );
    assert_eq_test!(
        after_regain,
        Some((true, OperState::Up)),
        "the interface is operational again once carrier returns"
    );
    assert_eq_test!(
        down_calls,
        0,
        "carrier is observed, not commanded — it never calls the driver"
    );
    pass!()
}

// =============================================================================
// The master switch
// =============================================================================
//
// The switch acts on every admin-up non-loopback interface, so a naive test
// downs the real virtio NIC the rest of the suite is using. `LeasePolicy::Keep`
// keeps its address, but `realise` re-installs only *connected* routes on the
// way back up — a DHCP-learned default route is withdrawn and not restored,
// because re-learning it is the client's job. Off-link traffic would stay
// broken for the rest of the boot, failing the socket and DNS tests downstream.
//
// Parking is what makes the switch testable anyway. `iface::set_admin_intent`
// writes the intent flag and *only* the flag — no driver call, no route
// withdrawal, no address touched — so a parked interface drops out of the
// switch's target set ("non-loopback and admin-up") while nothing about it has
// actually changed. Unparking writes the flag back, leaving the switch acting
// on exactly the interfaces the test owns.
//
// Each test below gathers its observations *before* restoring global state and
// asserts *after*, so a failing assertion cannot leave networking switched off
// for every test that runs later.

/// Park every live non-loopback interface's admin intent, recording them in
/// `out` and returning how many.
fn park_live_ifaces(keep: &[u32], out: &mut [u32; NET_MAX_IFACES]) -> usize {
    let mut n = 0usize;
    // Collect under the table lock, mutate outside it: `set_admin_intent` takes
    // the same lock `for_each` is holding.
    iface::for_each(|i| {
        if matches!(i.kind, IfaceKind::Loopback) || !i.admin_up || keep.contains(&i.ifindex) {
            return;
        }
        if n < out.len() {
            out[n] = i.ifindex;
            n += 1;
        }
    });
    for ifindex in &out[..n] {
        let _ = iface::set_admin_intent(*ifindex, false);
    }
    n
}

fn unpark_live_ifaces(parked: &[u32]) {
    for ifindex in parked {
        let _ = iface::set_admin_intent(*ifindex, true);
    }
}

/// The kernel's loopback interface index, if one is attached.
fn find_loopback() -> Option<u32> {
    let mut found = None;
    iface::for_each(|i| {
        if matches!(i.kind, IfaceKind::Loopback) && found.is_none() {
            found = Some(i.ifindex);
        }
    });
    found
}

/// An empty snapshot buffer. Eight entries is well past what any test here
/// creates, and small enough to stay clear of the 2 KiB stack-frame gate.
fn empty_neigh_snapshot() -> [NeighborSnapshot; 8] {
    [NeighborSnapshot {
        dev: DevIndex(0),
        ip: Ipv4Addr::UNSPECIFIED,
        mac: MacAddr::ZERO,
        state: 0,
        queued_pkts: 0,
        confirmed_ms_ago: 0,
    }; 8]
}

/// Neighbour-cache entries belonging to one device.
fn neighbours_for(dev: DevIndex) -> usize {
    let mut out = empty_neigh_snapshot();
    NEIGHBOR_CACHE.snapshot(Some(dev), &mut out).1
}

/// Packets queued on this device's single cache entry, or `None` if it does not
/// hold exactly one.
fn queued_pkts_for(dev: DevIndex) -> Option<u32> {
    let mut out = empty_neigh_snapshot();
    let (written, total) = NEIGHBOR_CACHE.snapshot(Some(dev), &mut out);
    (written == 1 && total == 1).then(|| out[0].queued_pkts)
}

/// Disabling networking unrealises interfaces without rewriting what the
/// operator asked for. `admin_up` is the memory the next enable reads, so a
/// disable that wrote it would destroy the very thing it needs.
fn test_disable_preserves_admin_intent() -> TestResult {
    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 8])) else {
        return fail!("could not register a mock device");
    };

    let mut parked = [0u32; NET_MAX_IFACES];
    let n_parked = park_live_ifaces(&[f.ifindex], &mut parked);

    iface_ctl::set_networking_enabled(false);
    let while_off = iface::get(f.ifindex).map(|i| (i.admin_up, i.is_realised(false)));
    let enabled_while_off = iface::is_enabled();
    let down_calls = f.down_calls();

    iface_ctl::set_networking_enabled(true);
    unpark_live_ifaces(&parked[..n_parked]);
    f.teardown();

    assert_test!(
        !enabled_while_off,
        "the master switch reads as off while it is off"
    );
    assert_eq_test!(
        while_off,
        Some((true, false)),
        "intent is preserved while the interface is unrealised"
    );
    assert_eq_test!(down_calls, 1, "the disable downed the device exactly once");
    pass!()
}

/// Loopback ignores the master switch entirely. Taking `127.0.0.1` away would
/// break AF_INET localhost IPC, which has nothing to do with networking being
/// switched off.
fn test_loopback_is_exempt_from_master_switch() -> TestResult {
    let Some(lo) = find_loopback() else {
        return fail!("no loopback interface is attached");
    };

    let mut parked = [0u32; NET_MAX_IFACES];
    let n_parked = park_live_ifaces(&[], &mut parked);

    let addrs_before = iface::get(lo).map(|i| i.addrs().len());

    iface_ctl::set_networking_enabled(false);
    let while_off = iface::get(lo).map(|i| (i.admin_up, i.is_realised(false), i.addrs().len()));

    iface_ctl::set_networking_enabled(true);
    unpark_live_ifaces(&parked[..n_parked]);

    assert_eq_test!(
        while_off,
        addrs_before.map(|n| (true, true, n)),
        "loopback stays up, realised and addressed while networking is off"
    );
    pass!()
}

/// A device attached while networking is disabled comes up admin-up but
/// unrealised, and the next enable realises it.
///
/// This is the case a remembered-set implementation gets wrong: the interface
/// was in nobody's snapshot when the switch moved, so an enable that only
/// re-realises what the disable recorded leaves it dark forever.
fn test_attach_while_disabled_realises_on_enable() -> TestResult {
    let mut parked = [0u32; NET_MAX_IFACES];
    let n_parked = park_live_ifaces(&[], &mut parked);

    iface_ctl::set_networking_enabled(false);

    let Some(f) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 9])) else {
        iface_ctl::set_networking_enabled(true);
        unpark_live_ifaces(&parked[..n_parked]);
        return fail!("could not register a mock device");
    };

    let while_off = iface::get(f.ifindex).map(|i| (i.admin_up, i.is_realised(false)));
    let up_calls_while_off = f.up_calls();

    iface_ctl::set_networking_enabled(true);

    let after_enable = iface::get(f.ifindex).map(|i| (i.admin_up, i.is_realised(true)));
    let up_calls_after = f.up_calls();

    unpark_live_ifaces(&parked[..n_parked]);
    f.teardown();

    assert_eq_test!(
        while_off,
        Some((true, false)),
        "a device attached while disabled is admin-up but unrealised"
    );
    assert_eq_test!(
        up_calls_while_off,
        0,
        "nothing realised it while the switch was off"
    );
    assert_eq_test!(
        after_enable,
        Some((true, true)),
        "the enable realises the interface that was never in a snapshot"
    );
    assert_eq_test!(up_calls_after, 1, "the enable brought the device up once");
    pass!()
}

/// `disable` then `enable` leaves the administrative flags exactly as they
/// were, and puts back the addresses and connected routes it unrealised. The
/// switch is a gate in front of intent, not an edit of it.
fn test_disable_then_enable_is_identity() -> TestResult {
    let Some(down) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 10])) else {
        return fail!("could not register a mock device");
    };
    let Some(up) = Fixture::new(IfaceKind::Ethernet, MacAddr([2, 0, 0, 0, 9, 11])) else {
        down.teardown();
        return fail!("could not register a second mock device");
    };

    // A DHCP-origin address on the interface that stays up: the switch must keep
    // it, which is what distinguishes `LeasePolicy::Keep` from an admin down.
    let _ = iface::add_addr(
        up.ifindex,
        IfaceAddr::permanent(
            Ipv4Addr([10, 9, 11, 5]),
            24,
            AddrScope::Global,
            AddrOrigin::Dhcp,
        ),
    );
    // One of the pair is administratively down before the cycle, so the test
    // distinguishes "flags preserved" from "everything ended up up".
    let pre_down = iface_ctl::set_admin_up(down.ifindex, false);

    let mut parked = [0u32; NET_MAX_IFACES];
    let n_parked = park_live_ifaces(&[down.ifindex, up.ifindex], &mut parked);

    let before = (
        iface::get(down.ifindex).map(|i| i.admin_up),
        iface::get(up.ifindex).map(|i| i.admin_up),
    );
    let addrs_before = iface::get(up.ifindex).map(|i| i.addrs().len());
    let down_calls_before = down.down_calls();

    iface_ctl::set_networking_enabled(false);
    iface_ctl::set_networking_enabled(true);

    let after = (
        iface::get(down.ifindex).map(|i| i.admin_up),
        iface::get(up.ifindex).map(|i| i.admin_up),
    );
    let addrs_after = iface::get(up.ifindex).map(|i| i.addrs().len());
    let enabled_after = iface::is_enabled();
    let routes_after = routes_for(up.dev);
    let down_calls_after = down.down_calls();

    unpark_live_ifaces(&parked[..n_parked]);
    up.teardown();
    down.teardown();

    assert_test!(pre_down.is_ok(), "the pre-cycle down must succeed");
    assert_eq_test!(
        before,
        (Some(false), Some(true)),
        "the pair starts with different intent"
    );
    assert_eq_test!(after, before, "a disable/enable cycle preserves intent");
    assert_test!(enabled_after, "the master switch is back on");
    assert_eq_test!(
        addrs_after,
        addrs_before,
        "the switch keeps a lease it did not grant"
    );
    assert_eq_test!(
        routes_after,
        1,
        "the connected route for the kept address is re-installed"
    );
    assert_eq_test!(
        down_calls_after,
        down_calls_before,
        "an already-down interface is not handed to the driver again"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_admin_down_reaches_the_driver_and_withdraws_routes,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_down_keeps_static_drops_dhcp,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_up_restores_connected_routes,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_guard_refuses_a_second_entrant,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_down_flushes_neighbours_and_frees_packets,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_intent_survives_carrier_loss,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_disable_preserves_admin_intent,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_loopback_is_exempt_from_master_switch,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_attach_while_disabled_realises_on_enable,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_disable_then_enable_is_identity,
    suite = iface_ctl
);
slopos_testing::stest!(
    name = test_admin_up_does_not_restore_the_default_route,
    suite = iface_ctl
);
