//! Tests that the real mutators announce themselves to the monitors, run
//! against a monitor opened on the kernel's own registry.
//!
//! Each test registers its own device and filters every assertion by that
//! device's `ifindex`, because the live NIC is in use. `NET_EV_GLOBAL_ENABLE`
//! stays unasserted: it acts on every registered device, so exercising it
//! would down the live NIC.

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_fs::fileio::FdTable;

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_abi::file_ops::FileOps;
use slopos_abi::io::KernelIoBuf;
use slopos_abi::net::{
    IFF_RUNNING, IFF_UP, NET_ADDR_ORIGIN_DHCP, NET_ADDR_SCOPE_GLOBAL, NET_EV_ADDR_ADDED,
    NET_EV_ADDR_REMOVED, NET_EV_IFACE_ADDED, NET_EV_IFACE_CHANGED, NET_EV_IFACE_REMOVED,
    NET_EV_ROUTE_ADDED, NET_EV_ROUTE_REMOVED, NET_EVENT_LEN, NET_MON_ADDR, NET_MON_DEFAULT,
    NET_MON_IFACE, NET_MON_ROUTE, NET_OPER_DOWN, NET_OPER_LOWERLAYERDOWN, NET_OPER_NOTPRESENT,
    NET_OPER_UP, NET_ROUTE_ORIGIN_DHCP, NET_ROUTE_ORIGIN_KERNEL, NetEvent,
};
use slopos_abi::syscall::POLLIN;
use slopos_ostd::{KArc, KVec};

use crate::iface::{self, AddrOrigin, IfaceKind};
use crate::iface_ctl;
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::{DEVICE_REGISTRY, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::netmon::NETMON_TABLE;
use crate::netmon_file_ops::NETMON_FILE_OPS;
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::route;
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

/// The kernel's own table, rather than a synthetic pid no process could hold.
const TEST_OWNER: FdTable = FdTable::Kernel;

struct CarrierMock {
    mac: MacAddr,
    link_up: AtomicBool,
}

impl NetDevice for CarrierMock {
    fn tx(&self, _pkt: PacketBuf) -> Result<(), NetError> {
        Ok(())
    }
    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }
    fn set_up(&self) {}
    fn set_down(&self) {}
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
        self.link_up.load(Ordering::Acquire)
    }
    fn carrier_detect(&self) -> bool {
        true
    }
}

struct Fixture {
    dev: DevIndex,
    ifindex: u32,
    mock: KArc<CarrierMock>,
}

impl Fixture {
    /// Register first, attach only after it returns — the real probe order.
    fn new(mac: MacAddr) -> Option<Self> {
        let mock = KArc::try_new(CarrierMock {
            mac,
            link_up: AtomicBool::new(true),
        })
        .ok()?;
        let dyn_dev: KArc<dyn NetDevice + Send + Sync> = mock.clone();
        let handle = DEVICE_REGISTRY.register(dyn_dev)?;
        let dev = handle.index();
        let ifindex = iface::attach(dev, IfaceKind::Ethernet, mac, 1500, true, true).ok()?;
        Some(Self { dev, ifindex, mock })
    }

    /// Move the mock's link, then tell the interface layer — the order the
    /// driver's carrier poll uses.
    fn set_link(&self, up: bool) -> Option<(u32, iface::OperState, iface::OperState)> {
        self.mock.link_up.store(up, Ordering::Release);
        iface::set_carrier(self.dev, up)
    }

    fn teardown(self) {
        route::remove_device_routes(self.dev);
        drop(NEIGHBOR_CACHE.flush_device(self.dev));
        iface::detach(self.dev);
        DEVICE_REGISTRY.unregister(self.dev);
    }
}

/// Released on drop, so a failing assertion cannot leak a monitor slot.
struct Monitor {
    handle: usize,
}

impl Monitor {
    fn open(mask: u32) -> Option<Self> {
        NETMON_TABLE
            .open(TEST_OWNER, mask)
            .ok()
            .map(|handle| Self { handle })
    }

    /// Chunked through a small stack array to stay under the 2 KiB frame gate.
    fn drain_for(&self, ifindex: u32, out: &mut [NetEvent]) -> usize {
        let mut chunk = [NetEvent::default(); 8];
        let mut kept = 0usize;
        loop {
            let n = match NETMON_TABLE.drain(self.handle, &mut chunk) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            for event in &chunk[..n] {
                if event.ifindex == ifindex && kept < out.len() {
                    out[kept] = *event;
                    kept += 1;
                }
            }
        }
        kept
    }

    /// How many of `ifindex`'s records are queued, discarding them.
    fn count_for(&self, ifindex: u32) -> usize {
        let mut sink = [NetEvent::default(); 16];
        self.drain_for(ifindex, &mut sink)
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        NETMON_TABLE.close(self.handle);
    }
}

fn test_producer_carrier_loss_posts_one_iface_changed() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_IFACE) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 1])) else {
        return fail!("could not register a mock device");
    };
    // Discard the attach record; this test is about the transition.
    monitor.count_for(f.ifindex);

    let transition = f.set_link(false);
    assert_test!(
        transition.is_some(),
        "losing carrier is a transition the table records"
    );

    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 1, "exactly one record for one transition");
    assert_eq_test!(events[0].kind, NET_EV_IFACE_CHANGED, "an interface change");

    let payload = events[0].as_iface();
    assert_eq_test!(payload.oper_old, NET_OPER_UP, "it was up");
    assert_eq_test!(
        payload.oper_new,
        NET_OPER_LOWERLAYERDOWN,
        "and the lower layer is what went, not the intent"
    );
    assert_eq_test!(payload.carrier, 0, "the link is down");
    assert_eq_test!(payload.admin_up, 1, "the operator never asked for a down");
    assert_test!(
        payload.flags & IFF_UP != 0,
        "IFF_UP is intent and survives a cable"
    );
    assert_test!(
        payload.flags & IFF_RUNNING == 0,
        "IFF_RUNNING is effect and does not"
    );
    assert_eq_test!(
        payload.mtu,
        1500,
        "the record describes the whole interface"
    );

    f.teardown();
    pass!()
}

/// The driver's poll re-samples the link every 50 ms, so an unchanged link
/// must post nothing or it would fill every subscriber's ring.
fn test_producer_idempotent_carrier_posts_nothing() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_IFACE) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 2])) else {
        return fail!("could not register a mock device");
    };
    monitor.count_for(f.ifindex);

    assert_test!(
        f.set_link(false).is_some(),
        "the first loss is a transition"
    );
    assert_eq_test!(monitor.count_for(f.ifindex), 1, "and is announced");

    for _ in 0..20 {
        assert_test!(
            f.set_link(false).is_none(),
            "re-sampling an unchanged link is not a transition"
        );
    }
    assert_eq_test!(
        monitor.count_for(f.ifindex),
        0,
        "and twenty idempotent samples announce nothing"
    );

    assert_test!(f.set_link(true).is_some(), "the recovery is a transition");
    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 1, "announced exactly once");
    assert_eq_test!(
        events[0].as_iface().oper_new,
        NET_OPER_UP,
        "and reports the interface back up"
    );

    f.teardown();
    pass!()
}

fn test_producer_admin_down_posts_iface_changed() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_IFACE) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 3])) else {
        return fail!("could not register a mock device");
    };
    monitor.count_for(f.ifindex);

    if let Err(e) = iface_ctl::set_admin_up(f.ifindex, false) {
        f.teardown();
        return fail!("set_admin_up(false) failed: {:?}", e);
    }

    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 1, "one record for one administrative transition");
    assert_eq_test!(events[0].kind, NET_EV_IFACE_CHANGED, "an interface change");

    let payload = events[0].as_iface();
    assert_eq_test!(payload.oper_old, NET_OPER_UP, "it was up");
    assert_eq_test!(
        payload.oper_new,
        NET_OPER_DOWN,
        "and is administratively down"
    );
    assert_eq_test!(payload.admin_up, 0, "the intent is what moved");
    assert_test!(payload.flags & IFF_UP == 0, "so IFF_UP is gone");

    let _ = iface_ctl::set_admin_up(f.ifindex, false);
    assert_eq_test!(
        monitor.count_for(f.ifindex),
        0,
        "a repeated request announces nothing"
    );

    f.teardown();
    pass!()
}

fn test_producer_configure_posts_addr_and_routes() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_ADDR | NET_MON_ROUTE) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 4])) else {
        return fail!("could not register a mock device");
    };
    monitor.count_for(f.ifindex);

    if iface_ctl::configure_ipv4(
        f.dev,
        Ipv4Addr([10, 10, 4, 5]),
        24,
        Ipv4Addr([10, 10, 4, 1]),
        AddrOrigin::Dhcp,
    )
    .is_err()
    {
        f.teardown();
        return fail!("configure_ipv4 failed");
    }

    let mut events = [NetEvent::default(); 8];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 3, "one address and two routes");

    assert_eq_test!(events[0].kind, NET_EV_ADDR_ADDED, "the address comes first");
    let addr = events[0].as_addr();
    assert_eq_test!(addr.addr, [10, 10, 4, 5], "carrying the address");
    assert_eq_test!(addr.prefix_len, 24, "and its prefix length");
    assert_eq_test!(
        addr.origin,
        NET_ADDR_ORIGIN_DHCP,
        "and where it came from — the field a UI renders as `dhcp`"
    );
    assert_eq_test!(addr.scope, NET_ADDR_SCOPE_GLOBAL, "and its scope");

    assert_eq_test!(
        events[1].kind,
        NET_EV_ROUTE_ADDED,
        "then the connected route"
    );
    let connected = events[1].as_route();
    assert_eq_test!(connected.prefix, [10, 10, 4, 0], "for the address's prefix");
    assert_eq_test!(connected.prefix_len, 24, "at its length");
    assert_eq_test!(connected.gateway, [0, 0, 0, 0], "with no gateway");
    assert_eq_test!(
        connected.origin,
        NET_ROUTE_ORIGIN_KERNEL,
        "derived from the prefix, so the kernel is its origin"
    );

    assert_eq_test!(events[2].kind, NET_EV_ROUTE_ADDED, "then the default route");
    let default = events[2].as_route();
    assert_eq_test!(default.prefix_len, 0, "a default route");
    assert_eq_test!(default.gateway, [10, 10, 4, 1], "via the lease's router");
    assert_eq_test!(
        default.origin,
        NET_ROUTE_ORIGIN_DHCP,
        "and the lease's origin, which only the installer knows"
    );

    f.teardown();
    pass!()
}

fn test_producer_admin_down_withdraws_addr_and_routes() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 5])) else {
        return fail!("could not register a mock device");
    };
    if iface_ctl::configure_ipv4(
        f.dev,
        Ipv4Addr([10, 10, 5, 5]),
        24,
        Ipv4Addr([10, 10, 5, 1]),
        AddrOrigin::Dhcp,
    )
    .is_err()
    {
        f.teardown();
        return fail!("configure_ipv4 failed");
    }

    // Open only now, so the ring holds the teardown and nothing else.
    let Some(monitor) = Monitor::open(NET_MON_ADDR | NET_MON_ROUTE) else {
        f.teardown();
        return fail!("could not open a monitor");
    };

    if let Err(e) = iface_ctl::set_admin_up(f.ifindex, false) {
        f.teardown();
        return fail!("set_admin_up(false) failed: {:?}", e);
    }

    let mut events = [NetEvent::default(); 8];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 3, "two routes and the lease's address");
    assert_eq_test!(events[0].kind, NET_EV_ROUTE_REMOVED, "routes go first");
    assert_eq_test!(events[1].kind, NET_EV_ROUTE_REMOVED, "both of them");
    assert_eq_test!(
        events[2].kind,
        NET_EV_ADDR_REMOVED,
        "and the address they derived from goes last"
    );
    assert_eq_test!(
        events[2].as_addr().addr,
        [10, 10, 5, 5],
        "naming the address that went"
    );
    assert_eq_test!(
        events[2].as_addr().origin,
        NET_ADDR_ORIGIN_DHCP,
        "a down invalidates the lease, so the record says so"
    );

    f.teardown();
    pass!()
}

fn test_producer_attach_and_detach_are_announced() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_IFACE) else {
        return fail!("could not open a monitor");
    };

    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 6])) else {
        return fail!("could not register a mock device");
    };
    let ifindex = f.ifindex;

    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_for(ifindex, &mut events);
    assert_eq_test!(n, 1, "attaching announces the new interface");
    assert_eq_test!(events[0].kind, NET_EV_IFACE_ADDED, "as an addition");
    assert_eq_test!(
        events[0].as_iface().oper_old,
        NET_OPER_NOTPRESENT,
        "from the state of an interface that did not exist"
    );
    assert_eq_test!(events[0].as_iface().oper_new, NET_OPER_UP, "to a live one");

    f.teardown();

    let n = monitor.drain_for(ifindex, &mut events);
    assert_eq_test!(n, 1, "detaching announces the removal");
    assert_eq_test!(events[0].kind, NET_EV_IFACE_REMOVED, "as a removal");
    assert_eq_test!(
        events[0].as_iface().oper_new,
        NET_OPER_NOTPRESENT,
        "back to not existing"
    );
    pass!()
}

fn test_producer_change_wakes_a_subscribed_fd() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_DEFAULT) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 7])) else {
        return fail!("could not register a mock device");
    };
    monitor.count_for(f.ifindex);

    // Drain whatever the attach left so the fd is genuinely quiet.
    let mut sink = [NetEvent::default(); 16];
    let _ = NETMON_TABLE.drain(monitor.handle, &mut sink);
    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(monitor.handle, POLLIN),
        0,
        "a subscriber with nothing to read is not ready"
    );

    f.set_link(false);

    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(monitor.handle, POLLIN),
        POLLIN,
        "the network changing is what makes the fd readable"
    );

    let mut buf = [0u8; NET_EVENT_LEN * 4];
    let mut io = KernelIoBuf::new(&mut buf);
    let read = NETMON_FILE_OPS.read(monitor.handle, &mut io, 0, 0);
    assert_test!(
        read >= NET_EVENT_LEN as isize,
        "and the change reads out of it as whole records"
    );

    let mut record = [0u8; NET_EVENT_LEN];
    record.copy_from_slice(&buf[..NET_EVENT_LEN]);
    let event = NetEvent::from_bytes(&record);
    assert_eq_test!(event.ifindex, f.ifindex, "describing our interface");
    assert_eq_test!(event.kind, NET_EV_IFACE_CHANGED, "as a change");
    assert_eq_test!(
        event.as_iface().carrier,
        0,
        "and it is the carrier that went"
    );

    f.teardown();
    pass!()
}

/// The kernel half of what `utest_ip_e2e` checks from userland.
///
/// Downs its **own mock interface**, never `lo`: taking loopback down
/// withdraws `127.0.0.0/8` and would break every socket test after it in the
/// same boot.
fn test_producer_admin_down_makes_a_monitor_readable() -> TestResult {
    let Some(monitor) = Monitor::open(NET_MON_IFACE) else {
        return fail!("could not open a monitor");
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 10, 8])) else {
        return fail!("could not register a mock device");
    };

    let mut sink = [NetEvent::default(); 16];
    let _ = NETMON_TABLE.drain(monitor.handle, &mut sink);
    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(monitor.handle, POLLIN),
        0,
        "a drained monitor is not ready"
    );

    let before_up = iface::get(f.ifindex).map(|i| i.admin_up);
    assert_eq_test!(
        before_up,
        Some(true),
        "the interface must be up first, or the down is a no-op and correctly posts nothing"
    );

    if let Err(e) = iface_ctl::set_admin_up(f.ifindex, false) {
        f.teardown();
        return fail!("set_admin_up(false) failed: {:?}", e);
    }

    assert_eq_test!(
        NETMON_FILE_OPS.poll_events(monitor.handle, POLLIN),
        POLLIN,
        "an administrative down must make a subscribed monitor readable"
    );

    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_for(f.ifindex, &mut events);
    assert_eq_test!(n, 1, "and deliver exactly one record for that interface");
    assert_eq_test!(
        events[0].kind,
        NET_EV_IFACE_CHANGED,
        "describing the interface change"
    );
    assert_eq_test!(
        events[0].as_iface().admin_up,
        0,
        "with the intent it moved to"
    );

    f.teardown();
    pass!()
}

slopos_testing::stest!(
    name = test_producer_admin_down_posts_iface_changed,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_admin_down_withdraws_addr_and_routes,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_attach_and_detach_are_announced,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_carrier_loss_posts_one_iface_changed,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_change_wakes_a_subscribed_fd,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_configure_posts_addr_and_routes,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_idempotent_carrier_posts_nothing,
    suite = netmon_producers
);
slopos_testing::stest!(
    name = test_producer_admin_down_makes_a_monitor_readable,
    suite = netmon_producers
);
