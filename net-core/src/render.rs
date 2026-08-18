//! The one place a `NET_*` ABI constant becomes a string.
//!
//! `ip` and the compositor's status indicator both name the same states, and
//! two spellings of them would drift. Every renderer here is total over `u8`:
//! an out-of-range value renders as `UNKNOWN`-equivalent rather than panicking,
//! because the value came across a syscall boundary from a kernel that may be
//! newer than the caller.

use core::fmt::Write;

use slopos_abi::net::{
    IFF_BROADCAST, IFF_LOOPBACK, IFF_MULTICAST, IFF_RUNNING, IFF_SLOP_CARRIER_ASSUMED,
    IFF_SLOP_DHCP, IFF_SLOP_DISABLED, IFF_SLOP_NO_CARRIER, IFF_UP, IPPROTO_ICMP,
    NET_ADDR_ORIGIN_DHCP, NET_ADDR_ORIGIN_LINKLOCAL, NET_ADDR_ORIGIN_STATIC, NET_ADDR_SCOPE_GLOBAL,
    NET_ADDR_SCOPE_HOST, NET_ADDR_SCOPE_LINK, NET_CONN_FULL, NET_CONN_LIMITED, NET_CONN_LOCAL,
    NET_CONN_NONE, NET_CONN_PORTAL, NET_DHCP_BOUND, NET_DHCP_DISABLED, NET_DHCP_INIT,
    NET_DHCP_REASON_DECLINED, NET_DHCP_REASON_NAK, NET_DHCP_REASON_NO_CARRIER, NET_DHCP_REASON_OK,
    NET_DHCP_REASON_TIMEOUT, NET_DHCP_REBINDING, NET_DHCP_RENEWING, NET_DHCP_REQUESTING,
    NET_DHCP_SELECTING, NET_IFKIND_ETHERNET, NET_IFKIND_LOOPBACK, NET_IFKIND_WIRELESS,
    NET_NEIGH_FAILED, NET_NEIGH_INCOMPLETE, NET_NEIGH_REACHABLE, NET_NEIGH_STALE, NET_OPER_DORMANT,
    NET_OPER_DOWN, NET_OPER_LOWERLAYERDOWN, NET_OPER_NOTPRESENT, NET_OPER_TESTING, NET_OPER_UP,
    NET_ROUTE_ORIGIN_DHCP, NET_ROUTE_ORIGIN_KERNEL, NET_ROUTE_ORIGIN_STATIC, NET_SOCK_CLOSE_WAIT,
    NET_SOCK_CLOSED, NET_SOCK_CLOSING, NET_SOCK_ESTABLISHED, NET_SOCK_FIN_WAIT1,
    NET_SOCK_FIN_WAIT2, NET_SOCK_LAST_ACK, NET_SOCK_LISTEN, NET_SOCK_SYN_RECV, NET_SOCK_SYN_SENT,
    NET_SOCK_TIME_WAIT, NET_SOCK_UNCONN, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM,
};

/// Operational state, spelled as `/sys/class/net/*/operstate` and `ip link`
/// spell it.
pub const fn oper_state(state: u8) -> &'static str {
    match state {
        NET_OPER_NOTPRESENT => "NOTPRESENT",
        NET_OPER_DOWN => "DOWN",
        NET_OPER_LOWERLAYERDOWN => "LOWERLAYERDOWN",
        NET_OPER_TESTING => "TESTING",
        NET_OPER_DORMANT => "DORMANT",
        NET_OPER_UP => "UP",
        _ => "UNKNOWN",
    }
}

/// Neighbour-cache state. The cache implements exactly four.
pub const fn neigh_state(state: u8) -> &'static str {
    match state {
        NET_NEIGH_INCOMPLETE => "INCOMPLETE",
        NET_NEIGH_REACHABLE => "REACHABLE",
        NET_NEIGH_STALE => "STALE",
        NET_NEIGH_FAILED => "FAILED",
        _ => "UNKNOWN",
    }
}

pub const fn dhcp_state(state: u8) -> &'static str {
    match state {
        NET_DHCP_DISABLED => "DISABLED",
        NET_DHCP_INIT => "INIT",
        NET_DHCP_SELECTING => "SELECTING",
        NET_DHCP_REQUESTING => "REQUESTING",
        NET_DHCP_BOUND => "BOUND",
        NET_DHCP_RENEWING => "RENEWING",
        NET_DHCP_REBINDING => "REBINDING",
        _ => "UNKNOWN",
    }
}

/// Why the DHCP client last left a bound state.
pub const fn dhcp_reason(reason: u8) -> &'static str {
    match reason {
        NET_DHCP_REASON_OK => "ok",
        NET_DHCP_REASON_TIMEOUT => "timeout",
        NET_DHCP_REASON_NAK => "nak",
        NET_DHCP_REASON_DECLINED => "declined",
        NET_DHCP_REASON_NO_CARRIER => "no-carrier",
        _ => "unknown",
    }
}

/// How an address came to be configured. Lowercase, because `ip addr` prints
/// it as a trailing attribute rather than as a column heading.
pub const fn addr_origin(origin: u8) -> &'static str {
    match origin {
        NET_ADDR_ORIGIN_STATIC => "static",
        NET_ADDR_ORIGIN_DHCP => "dhcp",
        NET_ADDR_ORIGIN_LINKLOCAL => "link-local",
        _ => "unknown",
    }
}

/// Address scope, as the word following `scope` in `ip addr` output.
pub const fn addr_scope(scope: u8) -> &'static str {
    match scope {
        NET_ADDR_SCOPE_GLOBAL => "global",
        NET_ADDR_SCOPE_LINK => "link",
        NET_ADDR_SCOPE_HOST => "host",
        _ => "unknown",
    }
}

/// Where a route came from, as the word following `proto` in `ip route`
/// output.
pub const fn route_origin(origin: u8) -> &'static str {
    match origin {
        NET_ROUTE_ORIGIN_KERNEL => "kernel",
        NET_ROUTE_ORIGIN_STATIC => "static",
        NET_ROUTE_ORIGIN_DHCP => "dhcp",
        _ => "unknown",
    }
}

/// Interface kind, as the word following `link/` in `ip link` output.
pub const fn iface_kind(kind: u8) -> &'static str {
    match kind {
        NET_IFKIND_LOOPBACK => "loopback",
        NET_IFKIND_ETHERNET => "ether",
        NET_IFKIND_WIRELESS => "wireless",
        _ => "none",
    }
}

/// Socket state, as `ss` spells it.
///
/// `ss`'s abbreviations rather than RFC 793's full names: `ESTAB` not
/// `ESTABLISHED`, `TIME-WAIT` not `TIME_WAIT`. The column width in
/// [`crate::columns`] is sized for the longest of them.
pub const fn sock_state(state: u8) -> &'static str {
    match state {
        NET_SOCK_CLOSED => "CLOSED",
        NET_SOCK_LISTEN => "LISTEN",
        NET_SOCK_SYN_SENT => "SYN-SENT",
        NET_SOCK_SYN_RECV => "SYN-RECV",
        NET_SOCK_ESTABLISHED => "ESTAB",
        NET_SOCK_FIN_WAIT1 => "FIN-WAIT-1",
        NET_SOCK_FIN_WAIT2 => "FIN-WAIT-2",
        NET_SOCK_CLOSE_WAIT => "CLOSE-WAIT",
        NET_SOCK_CLOSING => "CLOSING",
        NET_SOCK_LAST_ACK => "LAST-ACK",
        NET_SOCK_TIME_WAIT => "TIME-WAIT",
        NET_SOCK_UNCONN => "UNCONN",
        _ => "UNKNOWN",
    }
}

/// Transport name for a `UserSockInfo`, as the `Netid` column shows it.
pub const fn sock_transport(sock_type: u8, protocol: u8) -> &'static str {
    if protocol == IPPROTO_ICMP as u8 {
        return "icmp";
    }
    match sock_type {
        t if t == SOCK_STREAM as u8 => "tcp",
        t if t == SOCK_DGRAM as u8 => "udp",
        t if t == SOCK_RAW as u8 => "raw",
        _ => "unknown",
    }
}

/// What the master networking switch being off looks like. Distinct from
/// [`NET_CONN_NONE`]'s `"Disconnected"`: nothing is wrong with the network, it
/// is simply turned off.
pub const CONNECTIVITY_DISABLED: &str = "Networking off";

/// Connectivity as a sentence for a person, not a constant name.
///
/// Every string here is ASCII: the console font covers no dash above U+007F,
/// so `"Connected - no internet"` uses a hyphen and not an em dash. See
/// [`is_renderable`].
pub const fn connectivity(state: u8) -> &'static str {
    match state {
        NET_CONN_NONE => "Disconnected",
        NET_CONN_PORTAL => "Sign-in required",
        NET_CONN_LIMITED => "Connected - no internet",
        NET_CONN_LOCAL => "No internet connection",
        NET_CONN_FULL => "Connected",
        // The stack has not finished deciding, which is worth showing rather
        // than hiding.
        _ => "Checking...",
    }
}

/// [`connectivity`], with the master switch folded in — the switch outranks
/// whatever connectivity value was last computed.
pub const fn connectivity_label(enabled: bool, state: u8) -> &'static str {
    if enabled {
        connectivity(state)
    } else {
        CONNECTIVITY_DISABLED
    }
}

/// The `IFF_*` bits and their names, in the order [`write_if_flags`] emits them.
pub const IF_FLAG_NAMES: [(u32, &str); 9] = [
    (IFF_SLOP_NO_CARRIER, "NO-CARRIER"),
    (IFF_LOOPBACK, "LOOPBACK"),
    (IFF_BROADCAST, "BROADCAST"),
    (IFF_MULTICAST, "MULTICAST"),
    (IFF_UP, "UP"),
    (IFF_RUNNING, "RUNNING"),
    (IFF_SLOP_DISABLED, "DISABLED"),
    (IFF_SLOP_CARRIER_ASSUMED, "CARRIER-ASSUMED"),
    (IFF_SLOP_DHCP, "DHCP"),
];

/// Writes the `<NO-CARRIER,BROADCAST,MULTICAST,UP>` bracketed flag list.
///
/// `NO-CARRIER` sorts first, as iproute2 puts it, because it is the one flag
/// that explains why the rest of the line looks wrong; burying it between
/// `MULTICAST` and `UP` is how a down cable gets missed. An interface with no
/// flags set renders `<>` rather than nothing, so the field is always present
/// and a column never collapses.
pub fn write_if_flags<W: Write + ?Sized>(out: &mut W, flags: u32) -> core::fmt::Result {
    out.write_char('<')?;
    let mut first = true;
    for (bit, name) in IF_FLAG_NAMES {
        if flags & bit == 0 {
            continue;
        }
        if !first {
            out.write_char(',')?;
        }
        out.write_str(name)?;
        first = false;
    }
    out.write_char('>')
}

/// Whether the console font has a glyph for `cp`.
///
/// Mirrors the coverage in `font/src/lib.rs`: ASCII `0x20..=0x7E`, the Latin-1
/// supplement `0xA0..=0xFF`, and exactly `€ ˚ ˇ`. A codepoint outside those
/// draws as the replacement glyph, so a string containing one is a rendering
/// bug wherever it is produced. Encoded here rather than taken as a dependency
/// on `slopos-font`, which is a kernel-side crate this one has no other reason
/// to link.
pub const fn is_renderable(cp: u32) -> bool {
    matches!(cp, 0x20..=0x7E | 0xA0..=0xFF | 0x20AC | 0x02DA | 0x02C7)
}

#[cfg(test)]
mod tests {
    use slopos_abi::net::{NET_CONN_UNKNOWN, NET_IFKIND_UNSPEC, NET_OPER_UNKNOWN};

    use super::*;
    use crate::columns::TestBuf;

    fn flags_string(flags: u32) -> TestBuf {
        let mut buf = TestBuf::new();
        write_if_flags(&mut buf, flags).unwrap();
        buf
    }

    #[test]
    fn oper_state_names() {
        assert_eq!(oper_state(NET_OPER_UNKNOWN), "UNKNOWN");
        assert_eq!(oper_state(NET_OPER_NOTPRESENT), "NOTPRESENT");
        assert_eq!(oper_state(NET_OPER_DOWN), "DOWN");
        assert_eq!(oper_state(NET_OPER_LOWERLAYERDOWN), "LOWERLAYERDOWN");
        assert_eq!(oper_state(NET_OPER_TESTING), "TESTING");
        assert_eq!(oper_state(NET_OPER_DORMANT), "DORMANT");
        assert_eq!(oper_state(NET_OPER_UP), "UP");
        assert_eq!(oper_state(200), "UNKNOWN");
    }

    #[test]
    fn neigh_state_names() {
        assert_eq!(neigh_state(NET_NEIGH_INCOMPLETE), "INCOMPLETE");
        assert_eq!(neigh_state(NET_NEIGH_REACHABLE), "REACHABLE");
        assert_eq!(neigh_state(NET_NEIGH_STALE), "STALE");
        assert_eq!(neigh_state(NET_NEIGH_FAILED), "FAILED");
        assert_eq!(neigh_state(99), "UNKNOWN");
    }

    #[test]
    fn dhcp_names() {
        assert_eq!(dhcp_state(NET_DHCP_DISABLED), "DISABLED");
        assert_eq!(dhcp_state(NET_DHCP_BOUND), "BOUND");
        assert_eq!(dhcp_state(NET_DHCP_REBINDING), "REBINDING");
        assert_eq!(dhcp_state(200), "UNKNOWN");
        assert_eq!(dhcp_reason(NET_DHCP_REASON_OK), "ok");
        assert_eq!(dhcp_reason(NET_DHCP_REASON_NO_CARRIER), "no-carrier");
        assert_eq!(dhcp_reason(200), "unknown");
    }

    #[test]
    fn address_and_route_attribute_names() {
        assert_eq!(addr_origin(NET_ADDR_ORIGIN_STATIC), "static");
        assert_eq!(addr_origin(NET_ADDR_ORIGIN_DHCP), "dhcp");
        assert_eq!(addr_origin(NET_ADDR_ORIGIN_LINKLOCAL), "link-local");
        assert_eq!(addr_scope(NET_ADDR_SCOPE_GLOBAL), "global");
        assert_eq!(addr_scope(NET_ADDR_SCOPE_LINK), "link");
        assert_eq!(addr_scope(NET_ADDR_SCOPE_HOST), "host");
        assert_eq!(route_origin(NET_ROUTE_ORIGIN_KERNEL), "kernel");
        assert_eq!(route_origin(NET_ROUTE_ORIGIN_STATIC), "static");
        assert_eq!(route_origin(NET_ROUTE_ORIGIN_DHCP), "dhcp");
    }

    #[test]
    fn iface_kind_names() {
        assert_eq!(iface_kind(NET_IFKIND_UNSPEC), "none");
        assert_eq!(iface_kind(NET_IFKIND_LOOPBACK), "loopback");
        assert_eq!(iface_kind(NET_IFKIND_ETHERNET), "ether");
        assert_eq!(iface_kind(NET_IFKIND_WIRELESS), "wireless");
        assert_eq!(iface_kind(77), "none");
    }

    /// `ss`'s spellings, not RFC 793's. A person reading this beside `ss`
    /// output from another system should recognise every word.
    #[test]
    fn socket_state_names_match_ss() {
        assert_eq!(sock_state(NET_SOCK_LISTEN), "LISTEN");
        assert_eq!(sock_state(NET_SOCK_ESTABLISHED), "ESTAB");
        assert_eq!(sock_state(NET_SOCK_TIME_WAIT), "TIME-WAIT");
        assert_eq!(sock_state(NET_SOCK_UNCONN), "UNCONN");
        assert_eq!(sock_state(NET_SOCK_CLOSED), "CLOSED");
        assert_eq!(sock_state(200), "UNKNOWN");
    }

    /// Every state name has to fit the column it is printed in, or the row
    /// after it shifts.
    #[test]
    fn every_socket_state_fits_its_column() {
        for value in 0u8..=255 {
            let name = sock_state(value);
            assert!(
                name.len() <= crate::columns::BRIEF_STATE,
                "{name:?} is wider than the state column"
            );
        }
    }

    #[test]
    fn socket_transport_names() {
        assert_eq!(sock_transport(SOCK_STREAM as u8, 0), "tcp");
        assert_eq!(sock_transport(SOCK_DGRAM as u8, 0), "udp");
        assert_eq!(sock_transport(SOCK_RAW as u8, 0), "raw");
        // Protocol outranks type: an ICMP datagram socket is not "udp".
        assert_eq!(sock_transport(SOCK_DGRAM as u8, IPPROTO_ICMP as u8), "icmp");
        assert_eq!(sock_transport(99, 0), "unknown");
    }

    #[test]
    fn connectivity_sentences() {
        assert_eq!(connectivity(NET_CONN_FULL), "Connected");
        assert_eq!(connectivity(NET_CONN_LIMITED), "Connected - no internet");
        assert_eq!(connectivity(NET_CONN_LOCAL), "No internet connection");
        assert_eq!(connectivity(NET_CONN_NONE), "Disconnected");
        assert_eq!(connectivity(NET_CONN_UNKNOWN), "Checking...");
        assert_eq!(connectivity(250), "Checking...");
    }

    #[test]
    fn connectivity_label_folds_the_master_switch() {
        assert_eq!(connectivity_label(true, NET_CONN_FULL), "Connected");
        assert_eq!(connectivity_label(false, NET_CONN_FULL), "Networking off");
        assert_eq!(connectivity_label(false, NET_CONN_NONE), "Networking off");
        assert_eq!(CONNECTIVITY_DISABLED, "Networking off");
    }

    #[test]
    fn if_flags_no_carrier_sorts_first() {
        assert_eq!(
            flags_string(IFF_SLOP_NO_CARRIER | IFF_BROADCAST | IFF_MULTICAST | IFF_UP).as_str(),
            "<NO-CARRIER,BROADCAST,MULTICAST,UP>"
        );
    }

    #[test]
    fn if_flags_common_shapes() {
        assert_eq!(
            flags_string(IFF_LOOPBACK | IFF_UP | IFF_RUNNING).as_str(),
            "<LOOPBACK,UP,RUNNING>"
        );
        assert_eq!(
            flags_string(IFF_BROADCAST | IFF_MULTICAST).as_str(),
            "<BROADCAST,MULTICAST>"
        );
        assert_eq!(flags_string(0).as_str(), "<>");
    }

    #[test]
    fn if_flags_include_the_slopos_bits() {
        assert_eq!(
            flags_string(IFF_UP | IFF_SLOP_DISABLED | IFF_SLOP_DHCP).as_str(),
            "<UP,DISABLED,DHCP>"
        );
        assert_eq!(
            flags_string(IFF_SLOP_CARRIER_ASSUMED).as_str(),
            "<CARRIER-ASSUMED>"
        );
    }

    #[test]
    fn if_flags_ignore_unassigned_bits() {
        assert_eq!(flags_string(1 << 20).as_str(), "<>");
        assert_eq!(flags_string(IFF_UP | (1 << 20)).as_str(), "<UP>");
    }

    /// Every string this crate can hand to a renderer must be drawable by the
    /// console font. An em dash in a status line is a bug that only shows up
    /// on screen, so it is caught here instead.
    #[test]
    fn every_produced_string_is_renderable() {
        let mut checked = 0usize;
        let mut check = |s: &str| {
            for c in s.chars() {
                assert!(
                    is_renderable(c as u32),
                    "{s:?} contains U+{:04X}, which the console font cannot draw",
                    c as u32
                );
            }
            checked += 1;
        };

        for value in 0u8..=255 {
            check(oper_state(value));
            check(neigh_state(value));
            check(dhcp_state(value));
            check(dhcp_reason(value));
            check(addr_origin(value));
            check(addr_scope(value));
            check(route_origin(value));
            check(iface_kind(value));
            check(connectivity(value));
            check(sock_state(value));
            check(sock_transport(value, 0));
            check(sock_transport(0, value));
            check(connectivity_label(true, value));
            check(connectivity_label(false, value));
        }
        for (_, name) in IF_FLAG_NAMES {
            check(name);
        }
        check(CONNECTIVITY_DISABLED);
        for keyword in crate::ip_plan::ALL_GRAMMAR_WORDS {
            check(keyword);
        }
        assert!(checked > 2000);
    }

    /// The glyph range this crate encodes is the range `font/src/lib.rs`
    /// covers, including the exclusions that matter for human-facing strings.
    #[test]
    fn renderable_range_matches_the_font() {
        assert!(is_renderable(0x20));
        assert!(is_renderable(0x7E));
        assert!(!is_renderable(0x1F));
        assert!(!is_renderable(0x7F));
        assert!(!is_renderable(0x9F));
        assert!(is_renderable(0xA0));
        assert!(is_renderable(0xB7)); // middot
        assert!(is_renderable(0xFF));
        assert!(is_renderable(0x20AC)); // euro
        assert!(is_renderable(0x02DA)); // ring above
        assert!(is_renderable(0x02C7)); // caron
        assert!(!is_renderable(0x2013)); // en dash
        assert!(!is_renderable(0x2014)); // em dash
        assert!(!is_renderable(0x2026)); // horizontal ellipsis
    }
}
