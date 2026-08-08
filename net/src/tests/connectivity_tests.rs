//! Tests for the connectivity classifier.
//!
//! # Why almost all of this is a pure function
//!
//! The ladder is the whole state machine, and it depends on five booleans. So
//! it is written as a pure function over an explicit [`Evidence`] and tested by
//! enumerating all thirty-two inputs, rather than by arranging thirty-two
//! network conditions on the machine the test is running on.
//!
//! That is not only convenience. These run inside a live kernel whose real NIC
//! the rest of the suite depends on: the hazard `iface_ctl_tests` documents —
//! toggling global network state breaks twenty-six unrelated socket and NAPI
//! tests — applies here with more force, because the classifier reads *every*
//! global table. The few tests below that need a classifier instance drive a
//! scratch [`Connectivity`], never [`CONNECTIVITY`], and the one that needs an
//! interface registers its own mock device and removes it again.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_abi::net::{
    NET_CONN_FULL, NET_CONN_LIMITED, NET_CONN_LOCAL, NET_CONN_NONE, NET_CONN_PORTAL,
    NET_CONN_UNKNOWN, NET_EV_CONNECTIVITY, NET_IFINDEX_GLOBAL, NET_MON_CONN, NetEvent,
};
use slopos_ostd::{KArc, KVec};

use crate::connectivity::{self, Connectivity, Evidence, classify};
use crate::iface::{self, IfaceKind};
use crate::netdev::{DEVICE_REGISTRY, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::netmon::NETMON_TABLE;
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::types::{DevIndex, MacAddr, NetError};

const TEST_PID: u32 = 0xC0FF_EE01;

/// A scratch classifier. Never [`CONNECTIVITY`]: that one describes the machine
/// this test is running on, and rewriting it would make every later reader —
/// including `net_query` — report a state nobody measured.
static SCRATCH: Connectivity = Connectivity::new();

fn fresh() -> &'static Connectivity {
    SCRATCH.reset();
    &SCRATCH
}

/// Evidence for a fully working stack, which each test then breaks one rung of.
const WORKING: Evidence = Evidence {
    any_carrier: true,
    has_address: true,
    has_default_route: true,
    gateway_reachable: true,
    wan_fresh: true,
};

// =============================================================================
// The ladder
// =============================================================================

/// Every rung, stated as the condition that produces it.
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
    // An unreachable first hop does not demote a path something off-link has
    // just answered over: the answer necessarily came through that hop, so
    // stale gateway evidence says the cache aged, not that the path broke.
    // Read the other way, a working machine flaps to Limited every time its
    // ARP entry ages out.
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
///
/// Enumerating beats sampling here because the function is total over five
/// booleans: an exhaustive check is the same length as a careful selection and
/// cannot miss the combination nobody thought of.
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

        // The kernel's ladder produces exactly four values.
        assert_test!(
            matches!(
                state,
                NET_CONN_NONE | NET_CONN_LOCAL | NET_CONN_LIMITED | NET_CONN_FULL
            ),
            "every input maps to a state the ladder can produce"
        );

        // Each rung's precondition, restated as an implication.
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

        // Full is reachable only with every rung satisfied — the property a
        // status indicator's green dot depends on.
        if state == NET_CONN_FULL {
            assert_test!(
                e.any_carrier && e.has_address && e.has_default_route && e.wan_fresh,
                "Full requires a configured path and an off-link answer"
            );
        }
    }
    pass!()
}

/// The kernel must never claim a captive portal. It has no HTTP client and will
/// not grow one to light an icon; the value exists so the space matches
/// NetworkManager's and so a userland daemon can set it.
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

    // Nor can any sequence of evidence drive an instance into it.
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

    // It is reachable only through the door left open for userland.
    c.set_enabled(false);
    assert_test!(
        c.force_state(NET_CONN_PORTAL).is_some(),
        "a userland classifier can still set it"
    );
    assert_eq_test!(c.state(), NET_CONN_PORTAL, "and it sticks");
    pass!()
}

// =============================================================================
// State, transitions, and the takeover switch
// =============================================================================

/// A classifier starts at `Unknown` — "nobody has looked yet" is not the same
/// answer as "nothing is reachable", and a UI shown the second before the first
/// evaluation would report an outage that never happened.
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

/// A transition announces itself to the monitors exactly once, with the states
/// either side in the payload.
fn test_conn_transition_posts_one_event() -> TestResult {
    let handle = match NETMON_TABLE.open(TEST_PID, NET_MON_CONN) {
        Ok(h) => h,
        Err(_) => return fail!("the kernel registry must have a free slot"),
    };
    let c = fresh();

    // Drain anything the live system posted between the open and here.
    let mut sink = [NetEvent::default(); 8];
    let _ = NETMON_TABLE.drain(handle, &mut sink);

    c.apply_and_announce(WORKING);
    c.apply_and_announce(WORKING); // not a transition, so not an event

    let mut events = [NetEvent::default(); 8];
    let n = NETMON_TABLE.drain(handle, &mut events).unwrap_or(0);
    NETMON_TABLE.close(handle);

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

// =============================================================================
// Gathering from the live tables
// =============================================================================

/// A device with no carrier is not counted, which is the bottom rung read off
/// the real interface table rather than a synthetic input.
///
/// This is the only test here that touches a global table, and it touches it
/// the way `iface_ctl_tests` does: its own mock, registered and removed.
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

    // `gather_evidence` reads the real tables, so it also sees the live NIC.
    // What is asserted is the part this test owns: a dark interface adds
    // nothing. Asserting the whole verdict would be asserting on whatever the
    // machine's network happens to be doing.
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
/// It lives with the classifier's tests because it is the classifier's main
/// consumer, and because what it proves is specific: `run_net` walks four
/// tables and formats every row, so "it is registered" (which the kconsole
/// suite's own uniqueness and well-formedness tests already cover) says
/// nothing about whether the body faults, deadlocks on a second network lock,
/// or blows the stack budget. Driving it through `request` + `drain` runs it
/// on the real dispatch path with the real data.
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

    // Reaching here at all is the assertion: the body walked the interface
    // table, the route table, the neighbour cache and the monitor registry,
    // taking each in its own critical section. A second network lock taken
    // under the first would have tripped the validator, and a fault would not
    // have returned.
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
