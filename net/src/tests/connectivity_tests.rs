//! Tests for the connectivity classifier.
//!
//! The ladder is a pure function over an explicit [`Evidence`] of five
//! booleans, so it is tested by enumerating all thirty-two inputs rather than
//! by arranging thirty-two network conditions on the machine running the test.
//! These run inside a live kernel: the classifier reads every global table and
//! the rest of the suite depends on the real NIC.

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_fs::fileio::FdTable;

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_abi::net::{
    NET_CONN_FULL, NET_CONN_LIMITED, NET_CONN_LOCAL, NET_CONN_NONE, NET_CONN_PORTAL,
    NET_CONN_UNKNOWN, NET_EV_CONNECTIVITY, NET_IFINDEX_GLOBAL, NET_MON_CONN, NetEvent,
};
use slopos_ostd::{KArc, KVec};

use crate::connectivity::{self, Connectivity, Evidence, classify};
use crate::iface::{self, IfaceKind};
use crate::ingress;
use crate::netdev::{DEVICE_REGISTRY, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::netmon::NETMON_TABLE;
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::types::{DevIndex, MacAddr, NetError};

/// The kernel's own table, rather than a synthetic pid no process could hold.
const TEST_OWNER: FdTable = FdTable::Kernel;

/// A scratch classifier. Never [`CONNECTIVITY`]: rewriting the live one would
/// make every later reader report a state nobody measured.
static SCRATCH: Connectivity = Connectivity::new();

fn fresh() -> &'static Connectivity {
    SCRATCH.reset();
    &SCRATCH
}

/// Gates physical ingress and the net timer thread, so DHCP cannot bind and ARP
/// cannot learn in the middle of an observation.
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

    /// The transition is the only discriminator a connectivity record carries;
    /// chunked through a small stack array to stay under the 2 KiB frame gate.
    fn drain_from(&self, old: u8, out: &mut [NetEvent]) -> usize {
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
                if event.kind == NET_EV_CONNECTIVITY
                    && event.as_connectivity().old == old
                    && kept < out.len()
                {
                    out[kept] = *event;
                    kept += 1;
                }
            }
        }
        kept
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        NETMON_TABLE.close(self.handle);
    }
}

/// Evidence for a fully working stack, which each test then breaks one rung of.
const WORKING: Evidence = Evidence {
    any_carrier: true,
    has_address: true,
    has_default_route: true,
    gateway_reachable: true,
    wan_fresh: true,
};

fn test_conn_ladder_rungs() -> TestResult {
    assert_eq_test!(
        classify(Evidence::default()),
        NET_CONN_NONE,
        "no carrier at all is None"
    );
    assert_eq_test!(
        classify(Evidence {
            any_carrier: true,
            ..Evidence::default()
        }),
        NET_CONN_NONE,
        "carrier without an address is still None — a cable is not connectivity"
    );
    assert_eq_test!(
        classify(Evidence {
            any_carrier: true,
            has_address: true,
            ..Evidence::default()
        }),
        NET_CONN_LOCAL,
        "an address with no default route reaches its own segment and no further"
    );
    assert_eq_test!(
        classify(Evidence {
            has_default_route: true,
            ..WORKING
        }),
        NET_CONN_FULL,
        "the working case"
    );
    // An off-link answer necessarily came through the first hop, so stale
    // gateway evidence means the ARP entry aged, not that the path broke.
    assert_eq_test!(
        classify(Evidence {
            gateway_reachable: false,
            ..WORKING
        }),
        NET_CONN_FULL,
        "stale gateway evidence does not outrank a fresh off-link answer"
    );
    assert_eq_test!(
        classify(Evidence {
            gateway_reachable: false,
            wan_fresh: false,
            ..WORKING
        }),
        NET_CONN_LIMITED,
        "a route with neither a first hop nor an off-link answer is Limited"
    );
    assert_eq_test!(
        classify(Evidence {
            wan_fresh: false,
            ..WORKING
        }),
        NET_CONN_LIMITED,
        "a reachable gateway with nothing behind it answering is Limited"
    );
    assert_eq_test!(classify(WORKING), NET_CONN_FULL, "and everything is Full");
    pass!()
}

/// All thirty-two inputs, checked against the ladder's own ordering rules
/// rather than against a second copy of the ladder.
fn test_conn_ladder_is_total_and_monotone() -> TestResult {
    for bits in 0u8..32 {
        let e = Evidence {
            any_carrier: bits & 1 != 0,
            has_address: bits & 2 != 0,
            has_default_route: bits & 4 != 0,
            gateway_reachable: bits & 8 != 0,
            wan_fresh: bits & 16 != 0,
        };
        let state = classify(e);

        assert_test!(
            matches!(
                state,
                NET_CONN_NONE | NET_CONN_LOCAL | NET_CONN_LIMITED | NET_CONN_FULL
            ),
            "every input maps to a state the ladder can produce"
        );

        if !e.any_carrier || !e.has_address {
            assert_eq_test!(state, NET_CONN_NONE, "no address means None");
        } else if !e.has_default_route {
            assert_eq_test!(state, NET_CONN_LOCAL, "no route means Local");
        } else if !e.wan_fresh {
            // The gateway rung does not appear here: it decides which probe is
            // worth sending, not which state is reported.
            assert_eq_test!(state, NET_CONN_LIMITED, "no WAN evidence means Limited");
        } else {
            assert_eq_test!(state, NET_CONN_FULL, "an off-link answer means Full");
        }

        if state == NET_CONN_FULL {
            assert_test!(
                e.any_carrier && e.has_address && e.has_default_route && e.wan_fresh,
                "Full requires a configured path and an off-link answer"
            );
        }
    }
    pass!()
}

/// The kernel must never claim a captive portal: it has no HTTP client. The
/// value exists so the space matches NetworkManager's and userland can set it.
fn test_conn_kernel_never_reports_portal() -> TestResult {
    for bits in 0u8..32 {
        let e = Evidence {
            any_carrier: bits & 1 != 0,
            has_address: bits & 2 != 0,
            has_default_route: bits & 4 != 0,
            gateway_reachable: bits & 8 != 0,
            wan_fresh: bits & 16 != 0,
        };
        assert_test!(
            classify(e) != NET_CONN_PORTAL,
            "no input to the ladder yields PORTAL"
        );
    }

    let c = fresh();
    for bits in 0u8..32 {
        let e = Evidence {
            any_carrier: bits & 1 != 0,
            has_address: bits & 2 != 0,
            has_default_route: bits & 4 != 0,
            gateway_reachable: bits & 8 != 0,
            wan_fresh: bits & 16 != 0,
        };
        c.apply(e);
        assert_test!(c.state() != NET_CONN_PORTAL, "and no state ever becomes it");
    }

    c.set_enabled(false);
    assert_test!(
        c.force_state(NET_CONN_PORTAL).is_some(),
        "a userland classifier can still set it"
    );
    assert_eq_test!(c.state(), NET_CONN_PORTAL, "and it sticks");
    pass!()
}

/// A classifier starts at `Unknown`: "nobody has looked yet" is not the same
/// answer as "nothing is reachable".
fn test_conn_starts_unknown_and_transitions_once() -> TestResult {
    let c = fresh();
    assert_eq_test!(c.state(), NET_CONN_UNKNOWN, "nothing evaluated yet");
    assert_eq_test!(c.since_ms(), 0, "and no transition has been timed");

    let first = c.apply(WORKING);
    assert_eq_test!(
        first,
        Some((NET_CONN_UNKNOWN, NET_CONN_FULL)),
        "the first evaluation is a transition from Unknown"
    );
    assert_eq_test!(c.state(), NET_CONN_FULL, "and is recorded");

    assert_eq_test!(
        c.apply(WORKING),
        None,
        "re-evaluating to the same state is not a transition"
    );

    let demote = c.apply(Evidence {
        wan_fresh: false,
        ..WORKING
    });
    assert_eq_test!(
        demote,
        Some((NET_CONN_FULL, NET_CONN_LIMITED)),
        "stale WAN evidence demotes Full to Limited"
    );
    pass!()
}

/// A userland daemon can take the classification over; while it holds it, the
/// kernel's own evaluation stops writing the state.
fn test_conn_disabled_classifier_stops_evaluating() -> TestResult {
    let c = fresh();
    c.apply(WORKING);
    assert_eq_test!(c.state(), NET_CONN_FULL, "kernel classified it");

    c.set_enabled(false);
    assert_eq_test!(
        c.apply(Evidence::default()),
        None,
        "a disabled classifier does not act on evidence"
    );
    assert_eq_test!(c.state(), NET_CONN_FULL, "and leaves the state alone");

    c.set_enabled(true);
    assert_eq_test!(
        c.apply(Evidence::default()),
        Some((NET_CONN_FULL, NET_CONN_NONE)),
        "taking it back resumes classification"
    );
    pass!()
}

/// Off-link is decided against the cached prefix, and before the first
/// evaluation nothing is off-link — evidence is dropped rather than invented.
fn test_conn_off_link_needs_a_cached_prefix() -> TestResult {
    let c = fresh();
    assert_test!(
        !c.is_off_link(crate::types::Ipv4Addr([8, 8, 8, 8])),
        "with no prefix cached, nothing is off-link"
    );

    c.note_wan_peer(crate::types::Ipv4Addr([8, 8, 8, 8]));
    assert_eq_test!(
        c.apply(Evidence {
            wan_fresh: false,
            ..WORKING
        }),
        Some((NET_CONN_UNKNOWN, NET_CONN_LIMITED)),
        "so an off-link peer noted before the first evaluation is not evidence"
    );
    pass!()
}

/// The live classifier posts to the same registry; a record leaving `Unknown`
/// can only be the scratch one's, since the ladder never returns there.
fn test_conn_transition_posts_one_event() -> TestResult {
    connectivity::recheck();
    assert_test!(
        connectivity::state() != NET_CONN_UNKNOWN,
        "the live classifier has evaluated, so the filter below discriminates"
    );

    let Some(monitor) = Monitor::open(NET_MON_CONN) else {
        return fail!("the kernel registry must have a free slot");
    };
    let c = fresh();

    c.apply_and_announce(WORKING);
    c.apply_and_announce(WORKING); // not a transition, so not an event

    let mut events = [NetEvent::default(); 4];
    let n = monitor.drain_from(NET_CONN_UNKNOWN, &mut events);

    assert_eq_test!(n, 1, "one transition, one record");
    assert_eq_test!(
        events[0].kind,
        NET_EV_CONNECTIVITY,
        "and it is a connectivity record"
    );
    assert_eq_test!(
        events[0].ifindex,
        NET_IFINDEX_GLOBAL,
        "connectivity addresses the stack, not an interface"
    );
    let payload = events[0].as_connectivity();
    assert_eq_test!(payload.old, NET_CONN_UNKNOWN, "from Unknown");
    assert_eq_test!(payload.new, NET_CONN_FULL, "to Full");
    pass!()
}

fn test_conn_gather_ignores_a_carrierless_device() -> TestResult {
    struct DarkMock {
        mac: MacAddr,
        link: AtomicBool,
    }
    impl NetDevice for DarkMock {
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
            self.link.load(Ordering::Acquire)
        }
        fn carrier_detect(&self) -> bool {
            true
        }
    }

    // Held across both gathers, or a DHCP bind between them reads as the mock's.
    let _quiesced = Quiesced::enter();

    let mac = MacAddr([2, 0, 0, 0, 11, 1]);
    let Ok(mock) = KArc::try_new(DarkMock {
        mac,
        link: AtomicBool::new(false),
    }) else {
        return fail!("alloc");
    };
    let dyn_dev: KArc<dyn NetDevice + Send + Sync> = mock.clone();
    let Some(handle) = DEVICE_REGISTRY.register(dyn_dev) else {
        return fail!("could not register a mock device");
    };
    let dev: DevIndex = handle.index();
    // Attached with carrier down: the interface exists but the link does not.
    let Ok(ifindex) = iface::attach(dev, IfaceKind::Ethernet, mac, 1500, false, true) else {
        DEVICE_REGISTRY.unregister(dev);
        return fail!("attach");
    };

    let Some(row) = iface::get(ifindex) else {
        iface::detach(dev);
        DEVICE_REGISTRY.unregister(dev);
        return fail!("interface vanished");
    };
    assert_test!(!row.carrier, "the mock is attached without carrier");
    assert_test!(
        row.addrs().is_empty(),
        "and contributes no address to the gather"
    );

    // `gather_evidence` reads the real tables, so it also sees the live NIC:
    // only the delta a dark interface makes can be asserted on.
    let before = connectivity::gather_evidence();
    iface::detach(dev);
    DEVICE_REGISTRY.unregister(dev);
    let after = connectivity::gather_evidence();

    assert_eq_test!(
        before.any_carrier,
        after.any_carrier,
        "a carrierless interface changes no rung, present or absent"
    );
    assert_eq_test!(
        before.has_address,
        after.has_address,
        "and contributes no address"
    );
    pass!()
}

/// The `n` console command runs to completion against the live tables.
///
/// Driven through `request` + `drain` so it runs on the real dispatch path:
/// registration alone says nothing about whether the body faults, deadlocks on
/// a second network lock, or blows the stack budget.
fn test_net_kconsole_command_runs() -> TestResult {
    let commands = slopos_ostd::kconsole::commands();
    let Some(entry) = commands.iter().find(|c| c.key == b'n') else {
        return fail!("the net command is not in the kconsole registry");
    };
    assert_eq_test!(entry.name, "net", "and it is ours");

    // Informational, so the default policy mask runs it.
    slopos_ostd::kconsole::request(b'n');
    let did_work = slopos_ostd::kconsole::drain();
    assert_test!(
        did_work,
        "a queued informational command must be drained and run"
    );

    // Reaching here is the assertion: a second network lock taken under the
    // first would have tripped the validator, and a fault would not have
    // returned.
    pass!()
}

slopos_testing::stest!(
    name = test_conn_disabled_classifier_stops_evaluating,
    suite = connectivity
);
slopos_testing::stest!(
    name = test_conn_gather_ignores_a_carrierless_device,
    suite = connectivity
);
slopos_testing::stest!(
    name = test_conn_kernel_never_reports_portal,
    suite = connectivity
);
slopos_testing::stest!(
    name = test_conn_ladder_is_total_and_monotone,
    suite = connectivity
);
slopos_testing::stest!(name = test_conn_ladder_rungs, suite = connectivity);
slopos_testing::stest!(
    name = test_conn_off_link_needs_a_cached_prefix,
    suite = connectivity
);
slopos_testing::stest!(
    name = test_conn_starts_unknown_and_transitions_once,
    suite = connectivity
);
slopos_testing::stest!(
    name = test_conn_transition_posts_one_event,
    suite = connectivity
);
slopos_testing::stest!(name = test_net_kconsole_command_runs, suite = connectivity);
