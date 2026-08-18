//! Tests for the interface table and the state model it derives.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::iface::{
    self, AddrOrigin, AddrScope, IfName, IfaceAddr, IfaceError, IfaceKind, OperState, if_flags,
    oper_state, prefix_to_mask, realised,
};
use crate::types::{DevIndex, Ipv4Addr, MacAddr};

use slopos_abi::net::{
    IFF_BROADCAST, IFF_LOOPBACK, IFF_MULTICAST, IFF_RUNNING, IFF_SLOP_CARRIER_ASSUMED,
    IFF_SLOP_DISABLED, IFF_SLOP_NO_CARRIER, IFF_UP,
};

/// A scratch table for the tests that mutate one.
///
/// Deliberately **not** the kernel's `IFACE_TABLE`: clearing the live table
/// would delete the boot configuration out from under the running system. A
/// `static` because the ~1.2 KiB table would blow the 2 KiB stack-frame gate,
/// and its own lock class keeps the ordering here away from the real table's.
static TEST_TABLE: iface::IfaceTable = iface::IfaceTable::new(slopos_ostd::lock_class!(
    "NET_IFACES.test",
    slopos_ostd::sync::LOCK_LEVEL_REGISTRY
));

fn fresh_table() -> &'static iface::IfaceTable {
    TEST_TABLE.clear();
    &TEST_TABLE
}

fn test_ifname_validation() -> TestResult {
    assert_test!(IfName::new(b"lo").is_some(), "`lo` must be accepted");
    assert_test!(IfName::new(b"eth0").is_some(), "`eth0` must be accepted");
    assert_test!(
        IfName::new(b"veth-0_1").is_some(),
        "hyphen and underscore must be accepted"
    );

    assert_test!(IfName::new(b"").is_none(), "empty name must be rejected");
    assert_test!(
        IfName::new(b"0123456789abcdefg").is_none(),
        "a 17-byte name must be rejected"
    );
    assert_test!(
        IfName::new(b"Eth0").is_none(),
        "upper case must be rejected"
    );
    assert_test!(IfName::new(b"eth 0").is_none(), "a space must be rejected");
    assert_test!(
        IfName::new(b"eth\x000").is_none(),
        "an embedded NUL must be rejected"
    );

    let full = IfName::new(b"0123456789abcdef").expect("16 bytes is legal");
    assert_eq_test!(full.as_bytes().len(), 16, "16-byte name round-trips");
    pass!()
}

/// Every combination of `(kind, admin_up, enabled, carrier)`, asserted against
/// both derivations.
///
/// The two rows people get wrong: a realised loopback reports `Unknown` rather
/// than `Up`, matching what `ip link show lo` prints on Linux; an Ethernet
/// interface that is admin-up while networking is disabled reports `Down`
/// **without losing `IFF_UP`**, because the flag is intent.
fn test_operstate_matrix() -> TestResult {
    struct Row {
        kind: IfaceKind,
        admin_up: bool,
        enabled: bool,
        carrier: bool,
        oper: OperState,
        want_set: u32,
        want_clear: u32,
    }

    const ROWS: &[Row] = &[
        Row {
            kind: IfaceKind::Loopback,
            admin_up: true,
            enabled: true,
            carrier: true,
            oper: OperState::Unknown,
            want_set: IFF_UP | IFF_LOOPBACK | IFF_RUNNING,
            want_clear: IFF_BROADCAST | IFF_SLOP_DISABLED | IFF_SLOP_NO_CARRIER,
        },
        Row {
            kind: IfaceKind::Loopback,
            admin_up: true,
            enabled: false,
            carrier: true,
            oper: OperState::Unknown,
            want_set: IFF_UP | IFF_LOOPBACK | IFF_RUNNING,
            want_clear: IFF_SLOP_DISABLED,
        },
        Row {
            kind: IfaceKind::Loopback,
            admin_up: false,
            enabled: true,
            carrier: true,
            oper: OperState::Down,
            want_set: IFF_LOOPBACK,
            want_clear: IFF_UP | IFF_RUNNING,
        },
        Row {
            kind: IfaceKind::Loopback,
            admin_up: false,
            enabled: false,
            carrier: true,
            oper: OperState::Down,
            want_set: IFF_LOOPBACK,
            want_clear: IFF_UP | IFF_RUNNING,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: false,
            enabled: true,
            carrier: true,
            oper: OperState::Down,
            want_set: IFF_BROADCAST | IFF_MULTICAST,
            want_clear: IFF_UP | IFF_RUNNING | IFF_SLOP_DISABLED | IFF_SLOP_NO_CARRIER,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: false,
            enabled: false,
            carrier: true,
            oper: OperState::Down,
            want_set: IFF_BROADCAST | IFF_MULTICAST,
            want_clear: IFF_UP | IFF_RUNNING | IFF_SLOP_DISABLED,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: false,
            enabled: true,
            carrier: false,
            oper: OperState::Down,
            want_set: IFF_BROADCAST,
            want_clear: IFF_UP | IFF_RUNNING | IFF_SLOP_NO_CARRIER,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: false,
            enabled: false,
            carrier: false,
            oper: OperState::Down,
            want_set: IFF_BROADCAST,
            want_clear: IFF_UP | IFF_RUNNING,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: true,
            enabled: true,
            carrier: true,
            oper: OperState::Up,
            want_set: IFF_UP | IFF_RUNNING | IFF_BROADCAST | IFF_MULTICAST,
            want_clear: IFF_SLOP_DISABLED | IFF_SLOP_NO_CARRIER | IFF_LOOPBACK,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: true,
            enabled: true,
            carrier: false,
            oper: OperState::LowerLayerDown,
            want_set: IFF_UP | IFF_SLOP_NO_CARRIER,
            want_clear: IFF_RUNNING | IFF_SLOP_DISABLED,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: true,
            enabled: false,
            carrier: true,
            oper: OperState::Down,
            want_set: IFF_UP | IFF_SLOP_DISABLED,
            want_clear: IFF_RUNNING | IFF_SLOP_NO_CARRIER,
        },
        Row {
            kind: IfaceKind::Ethernet,
            admin_up: true,
            enabled: false,
            carrier: false,
            oper: OperState::Down,
            want_set: IFF_UP | IFF_SLOP_DISABLED | IFF_SLOP_NO_CARRIER,
            want_clear: IFF_RUNNING,
        },
    ];

    for (i, row) in ROWS.iter().enumerate() {
        let got = oper_state(row.kind, row.admin_up, row.enabled, row.carrier);
        if got != row.oper {
            return fail!(
                "row {}: {:?} admin={} enabled={} carrier={} -> {:?}, want {:?}",
                i,
                row.kind,
                row.admin_up,
                row.enabled,
                row.carrier,
                got,
                row.oper
            );
        }

        // carrier_detect = true, dhcp = false; both modifiers are asserted
        // separately below.
        let flags = if_flags(
            row.kind,
            row.admin_up,
            row.enabled,
            row.carrier,
            true,
            false,
        );
        if flags & row.want_set != row.want_set {
            return fail!(
                "row {}: flags {:#x} missing required bits {:#x}",
                i,
                flags,
                row.want_set & !flags
            );
        }
        if flags & row.want_clear != 0 {
            return fail!(
                "row {}: flags {:#x} has forbidden bits {:#x}",
                i,
                flags,
                flags & row.want_clear
            );
        }
    }

    assert_test!(
        realised(IfaceKind::Loopback, true, false),
        "loopback is realised even with networking disabled"
    );
    assert_test!(
        !realised(IfaceKind::Ethernet, true, false),
        "ethernet is not realised with networking disabled"
    );
    assert_test!(
        !realised(IfaceKind::Loopback, false, true),
        "admin-down loopback is not realised"
    );

    pass!()
}

/// A driver that cannot observe its link says so, rather than claiming a state
/// it does not know.
fn test_carrier_assumed_flag() -> TestResult {
    let detected = if_flags(IfaceKind::Ethernet, true, true, true, true, false);
    let assumed = if_flags(IfaceKind::Ethernet, true, true, true, false, false);
    assert_eq_test!(
        detected & IFF_SLOP_CARRIER_ASSUMED,
        0,
        "a device that detects carrier must not be flagged as assuming it"
    );
    assert_test!(
        assumed & IFF_SLOP_CARRIER_ASSUMED != 0,
        "a device that cannot detect carrier must be flagged"
    );

    let lo = if_flags(IfaceKind::Loopback, true, true, true, false, false);
    assert_eq_test!(
        lo & IFF_SLOP_CARRIER_ASSUMED,
        0,
        "loopback must never carry the carrier-assumed flag"
    );
    pass!()
}

fn test_prefix_to_mask() -> TestResult {
    assert_eq_test!(prefix_to_mask(0), 0, "/0 is 0.0.0.0");
    assert_eq_test!(prefix_to_mask(8), 0xFF00_0000, "/8 is 255.0.0.0");
    assert_eq_test!(prefix_to_mask(24), 0xFFFF_FF00, "/24 is 255.255.255.0");
    assert_eq_test!(prefix_to_mask(32), u32::MAX, "/32 is all ones");
    // A shift of 32 on a u32 panics in debug Rust, so the implementation must
    // special-case it rather than rely on wrapping.
    assert_eq_test!(prefix_to_mask(33), u32::MAX, "over-long prefix saturates");

    for len in 0..=32u8 {
        let mask = prefix_to_mask(len);
        assert_eq_test!(
            mask.leading_ones() as u8,
            len,
            "mask for a prefix must have exactly that many leading ones"
        );
        assert_eq_test!(
            mask.count_ones() as u8,
            len,
            "mask for a prefix must have no ones after the run"
        );
    }
    pass!()
}

fn test_addr_derived_fields() -> TestResult {
    let a = IfaceAddr::permanent(
        Ipv4Addr([10, 0, 0, 50]),
        24,
        AddrScope::Global,
        AddrOrigin::Dhcp,
    );
    assert_eq_test!(a.netmask().0, [255, 255, 255, 0], "netmask from /24");
    assert_eq_test!(a.network().0, [10, 0, 0, 0], "network from /24");
    assert_eq_test!(a.broadcast().0, [10, 0, 0, 255], "broadcast from /24");
    assert_test!(a.is_local(Ipv4Addr([10, 0, 0, 1])), "same subnet is local");
    assert_test!(
        !a.is_local(Ipv4Addr([10, 0, 1, 1])),
        "a different /24 is not local"
    );

    let lo = IfaceAddr::permanent(Ipv4Addr::LOCALHOST, 8, AddrScope::Host, AddrOrigin::Static);
    assert_eq_test!(lo.netmask().0, [255, 0, 0, 0], "netmask from /8");
    assert_test!(
        lo.is_local(Ipv4Addr([127, 255, 255, 254])),
        "the whole 127/8 is local to loopback"
    );

    let host = IfaceAddr::permanent(
        Ipv4Addr([192, 168, 1, 7]),
        32,
        AddrScope::Global,
        AddrOrigin::Static,
    );
    assert_eq_test!(
        host.broadcast().0,
        [192, 168, 1, 7],
        "a /32's broadcast is itself"
    );
    pass!()
}

/// Interface indices are never reused, even when a device index and a name
/// are: a monitor consumer that missed a removal must not apply a later event
/// to a different interface wearing the same slot.
fn test_ifindex_is_monotonic_while_names_are_reused() -> TestResult {
    let t = fresh_table();

    let first = match t.attach(
        DevIndex(3),
        IfaceKind::Ethernet,
        MacAddr([2, 0, 0, 0, 0, 1]),
        1500,
        true,
        true,
    ) {
        Ok(idx) => idx,
        Err(e) => return fail!("attach failed: {:?}", e),
    };
    let name_first = t.get(first).expect("attached").name;

    assert_test!(t.detach(DevIndex(3)).is_some(), "detach must find the row");

    let second = match t.attach(
        DevIndex(3),
        IfaceKind::Ethernet,
        MacAddr([2, 0, 0, 0, 0, 2]),
        1500,
        true,
        true,
    ) {
        Ok(idx) => idx,
        Err(e) => return fail!("re-attach failed: {:?}", e),
    };
    let name_second = t.get(second).expect("attached").name;

    assert_test!(
        second > first,
        "a re-attached device must get a higher ifindex, got {} then {}",
        first,
        second
    );
    assert_test!(
        name_first == name_second,
        "the name is expected to be reused; that is why consumers key on ifindex"
    );

    pass!()
}

/// `lo` for loopback, `ethN` for everything else, with the lowest free suffix
/// rather than a running count.
fn test_names_follow_kind() -> TestResult {
    let t = fresh_table();

    let lo = t
        .attach(
            DevIndex(0),
            IfaceKind::Loopback,
            MacAddr::ZERO,
            65535,
            true,
            true,
        )
        .expect("loopback attach");
    let a = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("eth attach");
    let b = t
        .attach(
            DevIndex(2),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 2]),
            1500,
            true,
            true,
        )
        .expect("eth attach");

    assert_test!(
        t.get(lo).expect("lo").name.as_bytes() == b"lo",
        "loopback must be named lo"
    );
    assert_test!(
        t.get(a).expect("eth0").name.as_bytes() == b"eth0",
        "first ethernet must be eth0"
    );
    assert_test!(
        t.get(b).expect("eth1").name.as_bytes() == b"eth1",
        "second ethernet must be eth1"
    );

    t.detach(DevIndex(1));
    let c = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 3]),
            1500,
            true,
            true,
        )
        .expect("eth re-attach");
    assert_test!(
        t.get(c).expect("eth0").name.as_bytes() == b"eth0",
        "a freed name must be reusable"
    );

    assert_test!(
        t.get_by_name(b"eth1").is_some(),
        "lookup by name must find eth1"
    );
    assert_test!(
        t.get_by_name(b"eth9").is_none(),
        "lookup by name must miss an absent interface"
    );

    pass!()
}

/// Hitting the bound must not disturb the addresses already there.
fn test_addr_list_is_bounded() -> TestResult {
    let t = fresh_table();
    let idx = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");

    for i in 0..slopos_abi::net::NET_MAX_ADDRS_PER_IFACE {
        let a = IfaceAddr::permanent(
            Ipv4Addr([10, 0, 0, 10 + i as u8]),
            24,
            AddrScope::Global,
            AddrOrigin::Static,
        );
        if let Err(e) = t.add_addr(idx, a) {
            return fail!("address {} should have fit: {:?}", i, e);
        }
    }

    let overflow = IfaceAddr::permanent(
        Ipv4Addr([10, 0, 0, 99]),
        24,
        AddrScope::Global,
        AddrOrigin::Static,
    );
    assert_eq_test!(
        t.add_addr(idx, overflow),
        Err(IfaceError::TooManyAddrs),
        "one address past the bound must be refused"
    );
    assert_eq_test!(
        t.get(idx).expect("iface").addrs().len(),
        slopos_abi::net::NET_MAX_ADDRS_PER_IFACE,
        "the refusal must not have disturbed the existing addresses"
    );

    let replace = IfaceAddr::permanent(
        Ipv4Addr([10, 0, 0, 10]),
        24,
        AddrScope::Global,
        AddrOrigin::Dhcp,
    );
    assert_test!(
        t.add_addr(idx, replace).is_ok(),
        "re-adding an existing address must replace it"
    );
    let ifc = t.get(idx).expect("iface");
    assert_eq_test!(
        ifc.addrs().len(),
        slopos_abi::net::NET_MAX_ADDRS_PER_IFACE,
        "replacing must not grow the list"
    );
    assert_test!(
        ifc.addrs()[0].origin == AddrOrigin::Dhcp,
        "replacing must overwrite the origin"
    );

    pass!()
}

/// An administrative down drops the lease but keeps the operator's own
/// configuration.
fn test_retain_addrs_keeps_static_drops_dhcp() -> TestResult {
    let t = fresh_table();
    let idx = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");

    t.add_addr(
        idx,
        IfaceAddr::permanent(
            Ipv4Addr([10, 0, 0, 50]),
            24,
            AddrScope::Global,
            AddrOrigin::Dhcp,
        ),
    )
    .expect("dhcp addr");
    t.add_addr(
        idx,
        IfaceAddr::permanent(
            Ipv4Addr([192, 168, 9, 9]),
            24,
            AddrScope::Global,
            AddrOrigin::Static,
        ),
    )
    .expect("static addr");

    let dropped = t
        .retain_addrs(idx, |a| a.origin != AddrOrigin::Dhcp)
        .expect("retain");
    assert_eq_test!(dropped, 1, "exactly the DHCP address must be dropped");

    let ifc = t.get(idx).expect("iface");
    assert_eq_test!(ifc.addrs().len(), 1, "the static address must survive");
    assert_eq_test!(
        ifc.addrs()[0].addr.0,
        [192, 168, 9, 9],
        "and it must be the right one"
    );

    pass!()
}

/// The administrative guard refuses a second entrant rather than letting two
/// half-applied transitions interleave.
fn test_admin_guard_is_exclusive() -> TestResult {
    let t = fresh_table();
    let idx = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");

    assert_test!(
        t.try_begin_admin(idx).is_ok(),
        "the first claim must succeed"
    );
    assert_eq_test!(
        t.try_begin_admin(idx),
        Err(IfaceError::Busy),
        "a concurrent claim must be refused"
    );
    t.end_admin(idx);
    assert_test!(
        t.try_begin_admin(idx).is_ok(),
        "the guard must be reclaimable after release"
    );
    t.end_admin(idx);

    assert_eq_test!(
        t.try_begin_admin(9999),
        Err(IfaceError::NoSuchIface),
        "an unknown interface must report NoSuchIface, not Busy"
    );

    pass!()
}

/// An unplugged cable is not a request to disable the interface.
fn test_carrier_loss_keeps_admin_intent() -> TestResult {
    let t = fresh_table();
    let idx = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");

    let change = t.set_carrier(DevIndex(1), false);
    let Some((changed_idx, before, after)) = change else {
        return fail!("losing carrier must report a state change");
    };
    assert_eq_test!(changed_idx, idx, "the change must name the right interface");
    assert_eq_test!(before, OperState::Up, "it was up before");
    assert_eq_test!(
        after,
        OperState::LowerLayerDown,
        "no carrier is LOWERLAYERDOWN, not DOWN"
    );

    let ifc = t.get(idx).expect("iface");
    assert_test!(
        ifc.admin_up,
        "administrative intent must survive carrier loss"
    );

    assert_test!(
        t.set_carrier(DevIndex(1), false).is_none(),
        "an unchanged carrier must not report a transition"
    );

    pass!()
}

/// Loopback is registered first and would otherwise answer every source
/// selection with 127.0.0.1.
fn test_first_ipv4_skips_loopback() -> TestResult {
    let t = fresh_table();

    let lo = t
        .attach(
            DevIndex(0),
            IfaceKind::Loopback,
            MacAddr::ZERO,
            65535,
            true,
            true,
        )
        .expect("lo attach");
    t.add_addr(
        lo,
        IfaceAddr::permanent(Ipv4Addr::LOCALHOST, 8, AddrScope::Host, AddrOrigin::Static),
    )
    .expect("lo addr");

    assert_eq_test!(
        t.first_ipv4().map(|ip| ip.0),
        Some([127, 0, 0, 1]),
        "loopback alone is better than no address"
    );

    let eth = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("eth attach");
    t.add_addr(
        eth,
        IfaceAddr::permanent(
            Ipv4Addr([10, 0, 0, 50]),
            24,
            AddrScope::Global,
            AddrOrigin::Dhcp,
        ),
    )
    .expect("eth addr");

    assert_eq_test!(
        t.first_ipv4().map(|ip| ip.0),
        Some([10, 0, 0, 50]),
        "a real interface must outrank loopback"
    );
    assert_test!(
        t.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "our own address must be recognised"
    );
    assert_test!(
        t.is_our_addr(Ipv4Addr::LOCALHOST),
        "loopback's address must be recognised"
    );
    assert_test!(
        !t.is_our_addr(Ipv4Addr([10, 0, 0, 1])),
        "the gateway is not ours"
    );

    pass!()
}

/// An unrealised interface's addresses stop counting as ours, which is what
/// makes RX acceptance follow administrative state for free.
fn test_unrealised_addrs_are_not_ours() -> TestResult {
    let t = fresh_table();
    let eth = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");
    t.add_addr(
        eth,
        IfaceAddr::permanent(
            Ipv4Addr([10, 0, 0, 50]),
            24,
            AddrScope::Global,
            AddrOrigin::Static,
        ),
    )
    .expect("addr");

    assert_test!(
        t.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "an up interface's address is ours"
    );

    t.set_admin_intent(eth, false).expect("set intent");
    assert_test!(
        !t.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "an admin-down interface's address is not ours"
    );
    assert_test!(
        t.get(eth).expect("iface").addrs().len() == 1,
        "the address itself is retained; only its realisation changed"
    );

    t.set_admin_intent(eth, true).expect("set intent");
    t.set_enabled_flag(false);
    assert_test!(
        !t.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "networking disabled must unrealise the address"
    );
    assert_test!(
        t.get(eth).expect("iface").admin_up,
        "disabling must not have edited administrative intent"
    );
    t.set_enabled_flag(true);
    assert_test!(
        t.is_our_addr(Ipv4Addr([10, 0, 0, 50])),
        "re-enabling must restore realisation with no other action"
    );

    pass!()
}

/// A device attached while networking is disabled comes up with intent set but
/// unrealised, and the next enable realises it — the case a
/// remember-which-were-up snapshot design gets wrong.
fn test_attach_while_disabled_is_unrealised() -> TestResult {
    let t = fresh_table();
    t.set_enabled_flag(false);

    let eth = t
        .attach(
            DevIndex(1),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, 1]),
            1500,
            true,
            true,
        )
        .expect("attach");
    let ifc = t.get(eth).expect("iface");

    assert_test!(
        ifc.admin_up,
        "a freshly probed NIC carries administrative intent"
    );
    assert_test!(
        !ifc.is_realised(false),
        "but it is not realised while networking is disabled"
    );
    assert_eq_test!(
        ifc.oper_state(false),
        OperState::Down,
        "and it reports DOWN"
    );
    assert_test!(
        ifc.flags(false) & IFF_SLOP_DISABLED != 0,
        "the reason must be legible: held down by the master switch"
    );

    t.set_enabled_flag(true);
    let ifc = t.get(eth).expect("iface");
    assert_test!(
        ifc.is_realised(true),
        "enabling must realise an interface that was never in a snapshot"
    );
    assert_eq_test!(ifc.oper_state(true), OperState::Up, "and it comes up");

    pass!()
}

/// `snapshot` reports both what it wrote and the true total, so a caller with
/// a short buffer can tell it was truncated.
fn test_snapshot_reports_truncation() -> TestResult {
    let t = fresh_table();
    for i in 0..3u8 {
        t.attach(
            DevIndex(i as usize),
            IfaceKind::Ethernet,
            MacAddr([2, 0, 0, 0, 0, i]),
            1500,
            true,
            true,
        )
        .expect("attach");
    }

    let mut none: [iface::Iface; 0] = [];
    let (written, total) = t.snapshot(&mut none);
    assert_eq_test!(written, 0, "a zero-length buffer writes nothing");
    assert_eq_test!(
        total,
        3,
        "but still reports the true count — the sizing query"
    );

    let mut one = [t.get_by_dev(DevIndex(0)).expect("seed")];
    let (written, total) = t.snapshot(&mut one);
    assert_eq_test!(written, 1, "a short buffer writes what fits");
    assert_eq_test!(total, 3, "and reports the total, not what it wrote");

    assert_eq_test!(t.count(), 3, "count agrees with the snapshot total");

    pass!()
}

slopos_testing::stest!(name = test_ifname_validation, suite = iface);
slopos_testing::stest!(name = test_operstate_matrix, suite = iface);
slopos_testing::stest!(name = test_carrier_assumed_flag, suite = iface);
slopos_testing::stest!(name = test_prefix_to_mask, suite = iface);
slopos_testing::stest!(name = test_addr_derived_fields, suite = iface);
slopos_testing::stest!(
    name = test_ifindex_is_monotonic_while_names_are_reused,
    suite = iface
);
slopos_testing::stest!(name = test_names_follow_kind, suite = iface);
slopos_testing::stest!(name = test_addr_list_is_bounded, suite = iface);
slopos_testing::stest!(
    name = test_retain_addrs_keeps_static_drops_dhcp,
    suite = iface
);
slopos_testing::stest!(name = test_admin_guard_is_exclusive, suite = iface);
slopos_testing::stest!(name = test_carrier_loss_keeps_admin_intent, suite = iface);
slopos_testing::stest!(name = test_first_ipv4_skips_loopback, suite = iface);
slopos_testing::stest!(name = test_unrealised_addrs_are_not_ours, suite = iface);
slopos_testing::stest!(
    name = test_attach_while_disabled_is_unrealised,
    suite = iface
);
slopos_testing::stest!(name = test_snapshot_reports_truncation, suite = iface);
