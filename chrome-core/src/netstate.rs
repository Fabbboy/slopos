//! The network indicator's model: what the panel knows, and the one state the
//! bar draws.
//!
//! The model is a fixed-size snapshot rather than a live view of the kernel.
//! The compositor refreshes it on a slow timer and diffs the result, so an
//! unchanged network produces no damage at all — a status indicator that
//! repaints every frame costs more than the rest of the bar put together.
//!
//! Every human-readable string comes from [`slopos_net_core::render`]. `ip`
//! and this indicator name the same states, and if each spelled them itself a
//! person reading a terminal and the bar at the same time would get two
//! answers to one question.

use slopos_abi::net::{
    NET_CONN_FULL, NET_CONN_LIMITED, NET_CONN_LOCAL, NET_CONN_NONE, NET_CONN_PORTAL,
    NET_DHCP_DISABLED, NET_IFKIND_ETHERNET, NET_IFKIND_LOOPBACK, NET_OPER_DORMANT,
    NET_OPER_LOWERLAYERDOWN,
};
use slopos_net_core::render;

/// Interfaces the model can hold. The kernel's interface table is short and
/// the panel lists all of it; overflow drops the tail rather than growing.
pub const MAX_IFACES: usize = 6;

/// Resolvers the model can hold, matching what the panel shows.
pub const MAX_DNS: usize = 3;

/// Longest interface name the model stores, matching the ABI's field width.
pub const IFNAME_LEN: usize = 16;

/// What kind of link an interface is.
///
/// There is deliberately no wireless variant: the tree has no wireless driver,
/// so a `Wireless` arm would be unreachable code claiming a capability that
/// does not exist. Appending one later is a non-breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfaceKind {
    Loopback,
    Ethernet,
}

impl IfaceKind {
    /// The `NET_IFKIND_*` value this kind serialises to, so the panel can hand
    /// it to [`slopos_net_core::render::iface_kind`] instead of spelling the
    /// word itself.
    pub const fn abi(self) -> u8 {
        match self {
            IfaceKind::Loopback => NET_IFKIND_LOOPBACK,
            IfaceKind::Ethernet => NET_IFKIND_ETHERNET,
        }
    }

    /// Read a `NET_IFKIND_*` value from the ABI.
    ///
    /// Everything that is not loopback reads as Ethernet, including a kernel's
    /// `NET_IFKIND_WIRELESS`. Lossy and deliberately so: the distinction the
    /// indicator draws is "loopback, which never speaks for the machine"
    /// against "a link that does". A kernel that grows 802.11 wants a variant
    /// here and a glyph to go with it, not a silent third meaning attached to
    /// this one.
    pub const fn from_abi(kind: u8) -> IfaceKind {
        match kind {
            NET_IFKIND_LOOPBACK => IfaceKind::Loopback,
            _ => IfaceKind::Ethernet,
        }
    }
}

/// The one state the bar's glyph shows.
///
/// Ordered by how much attention each deserves, not by any ABI value: this is
/// a presentation decision, and it is deliberately coarser than the kernel's
/// `NET_CONN_*` because a person glancing at a 14 px icon can distinguish six
/// things, not twelve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetIndicatorState {
    /// Addressed and the internet is reachable.
    Connected,
    /// Addressed, but something past the link is not reachable.
    Limited,
    /// The link is up and an address is still being negotiated.
    Configuring,
    /// The interface is administratively up but there is no link — a cable
    /// out, or a switch port down.
    NoCarrier,
    /// There is no usable interface, or the one there is has been taken down.
    Disconnected,
    /// The master networking switch is off. Not a fault.
    Disabled,
}

/// Every [`NetIndicatorState`], for exhaustive iteration. Kept beside the enum
/// so adding a variant without extending this list fails the tests that walk
/// it rather than silently skipping the new state.
pub const ALL_INDICATOR_STATES: &[NetIndicatorState] = &[
    NetIndicatorState::Connected,
    NetIndicatorState::Limited,
    NetIndicatorState::Configuring,
    NetIndicatorState::NoCarrier,
    NetIndicatorState::Disconnected,
    NetIndicatorState::Disabled,
];

/// What the bar says when the link is up and no address has arrived yet.
/// `slopos_net_core` has no sentence for this — it is an interface state, not
/// a connectivity verdict — so it is spelled once, here.
pub const CONFIGURING_LABEL: &str = "Getting an address";

/// What the bar says when an interface is up and its link is not.
pub const NO_CARRIER_LABEL: &str = "Cable unplugged";

/// Title of the network panel.
pub const PANEL_TITLE: &str = "Network";

/// What a panel row says for an interface with no address.
pub const NO_ADDRESS_LABEL: &str = "no address";

/// Every literal the network panel can draw that is not already a renderer's
/// output. Kept beside the strings themselves so the coverage guard below
/// walks all of them — a label added to the panel and not to this list is a
/// label nothing checks the font can draw.
pub const PANEL_LABELS: &[&str] = &[
    PANEL_TITLE,
    NO_ADDRESS_LABEL,
    GATEWAY_LABEL,
    IFACE_SEPARATOR,
    IFACE_OFF_LABEL,
    IFACE_NO_CARRIER_LABEL,
    TRUNCATION,
];

/// The panel's label for the default gateway.
pub const GATEWAY_LABEL: &str = "Gateway";

/// What separates an interface's kind from its status on the detail line.
///
/// U+00B7 MIDDLE DOT, which the atlas covers (Latin-1) and which is the
/// conventional inline-metadata separator, leaving the hyphen to mean only
/// range or minus.
pub const IFACE_SEPARATOR: &str = " \u{B7} ";

/// What a panel row says for an administratively down interface.
pub const IFACE_OFF_LABEL: &str = "off";

/// What a panel row says for an interface whose link is down.
pub const IFACE_NO_CARRIER_LABEL: &str = "unplugged";

/// What a truncated string ends with. Two periods, not an ellipsis: the atlas
/// has no U+2026.
pub const TRUNCATION: &str = "..";

/// What a panel row is currently doing, as one ordered decision.
///
/// The dot's colour and the detail line's status word are two renderings of
/// this one value, so they cannot disagree. `carrier` on its own is not that
/// value: a `LOWERLAYERDOWN` interface still reports one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfaceState {
    /// Administratively down: someone turned it off.
    Off,
    /// Up, but the link is not.
    NoCarrier,
    /// Link is up and nothing has given it an address.
    NoAddress,
    /// Carrying traffic.
    Up,
}

/// Classify one row. The order is the order the causes mask each other in:
/// an interface someone switched off has no carrier worth reporting, and an
/// interface with no carrier cannot be blamed for having no address.
pub const fn iface_state(row: &IfaceRow) -> IfaceState {
    if !row.admin_up {
        IfaceState::Off
    } else if !row.has_carrier() {
        IfaceState::NoCarrier
    } else if !row.has_addr() {
        IfaceState::NoAddress
    } else {
        IfaceState::Up
    }
}

/// One interface, as the panel lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfaceRow {
    pub name: [u8; IFNAME_LEN],
    pub name_len: u8,
    pub kind: IfaceKind,
    pub admin_up: bool,
    pub carrier: bool,
    /// `NET_OPER_*`.
    pub oper: u8,
    pub ipv4: [u8; 4],
    pub prefix_len: u8,
    /// `NET_DHCP_*`.
    pub dhcp: u8,
}

impl IfaceRow {
    /// A row with no name and nothing up — the array filler, never shown.
    pub const EMPTY: IfaceRow = IfaceRow {
        name: [0; IFNAME_LEN],
        name_len: 0,
        kind: IfaceKind::Ethernet,
        admin_up: false,
        carrier: false,
        oper: 0,
        ipv4: [0; 4],
        prefix_len: 0,
        dhcp: NET_DHCP_DISABLED,
    };

    /// A named row of `kind`, with everything else at its down/unset value.
    /// Names longer than [`IFNAME_LEN`] are truncated rather than rejected;
    /// the name is a label, and a caller has no useful recovery.
    pub fn named(name: &[u8], kind: IfaceKind) -> IfaceRow {
        let len = name.len().min(IFNAME_LEN);
        let mut row = IfaceRow {
            kind,
            ..IfaceRow::EMPTY
        };
        row.name[..len].copy_from_slice(&name[..len]);
        row.name_len = len as u8;
        row
    }

    /// The interface name's bytes.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(IFNAME_LEN)]
    }

    /// The interface name, or `""` if it is not UTF-8. Interface names are
    /// bytes in the ABI and stay bytes until something has to draw one.
    pub fn name_str(&self) -> &str {
        core::str::from_utf8(self.name_bytes()).unwrap_or("")
    }

    /// Whether this interface has an IPv4 address it could send from.
    #[inline]
    pub const fn has_addr(&self) -> bool {
        let a = self.ipv4;
        a[0] != 0 || a[1] != 0 || a[2] != 0 || a[3] != 0
    }

    /// Whether the link is physically usable — a carrier, and an operational
    /// state that does not say the layer below is down.
    #[inline]
    pub const fn has_carrier(&self) -> bool {
        self.carrier && self.oper != NET_OPER_LOWERLAYERDOWN
    }
}

/// Everything the network panel shows, and everything the bar's indicator is
/// derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPanelModel {
    /// The master networking switch. Off outranks every other signal.
    pub enabled: bool,
    /// `NET_CONN_*`, the stack's own verdict on what is reachable.
    pub connectivity: u8,
    pub ifaces: [IfaceRow; MAX_IFACES],
    pub iface_count: usize,
    pub gateway: [u8; 4],
    pub dns: [[u8; 4]; MAX_DNS],
    pub dns_count: usize,
}

impl NetPanelModel {
    /// The model before the first refresh: networking on, nothing known yet.
    /// [`indicator_state_for`] reads this as `Disconnected` — no interface —
    /// which is the honest answer until the first query returns.
    pub const EMPTY: NetPanelModel = NetPanelModel {
        enabled: true,
        connectivity: NET_CONN_NONE,
        ifaces: [IfaceRow::EMPTY; MAX_IFACES],
        iface_count: 0,
        gateway: [0; 4],
        dns: [[0; 4]; MAX_DNS],
        dns_count: 0,
    };

    /// The interfaces actually populated.
    pub fn ifaces(&self) -> &[IfaceRow] {
        &self.ifaces[..self.iface_count.min(MAX_IFACES)]
    }

    /// Append `row`, or drop it if the model is full.
    pub fn push_iface(&mut self, row: IfaceRow) {
        if self.iface_count < MAX_IFACES {
            self.ifaces[self.iface_count] = row;
            self.iface_count += 1;
        }
    }

    /// Append a resolver, or drop it if the model is full.
    pub fn push_dns(&mut self, addr: [u8; 4]) {
        if self.dns_count < MAX_DNS {
            self.dns[self.dns_count] = addr;
            self.dns_count += 1;
        }
    }

    /// The interface the indicator speaks for: the first non-loopback row.
    ///
    /// Loopback is never it. A machine with only `lo` is not connected to
    /// anything, and reporting `127.0.0.1` as an address would put a
    /// connected glyph in the bar of a machine with no network at all.
    pub fn primary(&self) -> Option<&IfaceRow> {
        self.ifaces().iter().find(|r| r.kind != IfaceKind::Loopback)
    }

    /// The interfaces a person should be shown, which excludes loopback.
    ///
    /// `lo` is not a network anyone connects to or disconnects from. It also
    /// drags `UNKNOWN` into the panel: loopback's operational state is
    /// genuinely unknowable per RFC 2863, which is the right thing for
    /// `ip link` to print and jargon in a status menu.
    ///
    /// Kept separate from [`ifaces`](Self::ifaces) rather than filtered at the
    /// source, because the indicator's own logic still has to see loopback to
    /// know that a machine holding only `lo` is not connected.
    pub fn listed_ifaces(&self) -> impl Iterator<Item = &IfaceRow> {
        self.ifaces()
            .iter()
            .filter(|r| r.kind != IfaceKind::Loopback)
    }

    /// How many rows [`listed_ifaces`](Self::listed_ifaces) will yield.
    pub fn listed_count(&self) -> usize {
        self.listed_ifaces().count()
    }
}

impl Default for NetPanelModel {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Reduce the model to the one state the bar draws.
///
/// The order of the tests is the order of the questions a person asks: is it
/// switched on, is there an interface, is it up, is there a link, does it have
/// an address, and only then — can it reach anything. Each answer outranks
/// every later one, so an unplugged cable never shows as "no internet".
pub fn indicator_state_for(model: &NetPanelModel) -> NetIndicatorState {
    if !model.enabled {
        return NetIndicatorState::Disabled;
    }
    let Some(iface) = model.primary() else {
        return NetIndicatorState::Disconnected;
    };
    if !iface.admin_up {
        return NetIndicatorState::Disconnected;
    }
    if !iface.has_carrier() {
        return NetIndicatorState::NoCarrier;
    }
    // Dormant is a link that exists but is not passing traffic yet.
    if iface.oper == NET_OPER_DORMANT {
        return NetIndicatorState::Configuring;
    }
    if !iface.has_addr() {
        return NetIndicatorState::Configuring;
    }
    match model.connectivity {
        NET_CONN_FULL => NetIndicatorState::Connected,
        NET_CONN_LIMITED | NET_CONN_PORTAL | NET_CONN_LOCAL => NetIndicatorState::Limited,
        NET_CONN_NONE => NetIndicatorState::Disconnected,
        // NET_CONN_UNKNOWN and anything a newer kernel adds: the stack has not
        // finished deciding, which is a state worth showing rather than hiding.
        _ => NetIndicatorState::Configuring,
    }
}

/// The one-line sentence that goes with a state.
///
/// A function of the state alone, not of the model, so the sentence and the
/// glyph can never contradict each other. Four of the six delegate to
/// [`slopos_net_core::render`]; the two that do not are interface states the
/// connectivity vocabulary has no word for.
pub const fn indicator_label(state: NetIndicatorState) -> &'static str {
    match state {
        NetIndicatorState::Connected => render::connectivity(NET_CONN_FULL),
        NetIndicatorState::Limited => render::connectivity(NET_CONN_LIMITED),
        NetIndicatorState::Configuring => CONFIGURING_LABEL,
        NetIndicatorState::NoCarrier => NO_CARRIER_LABEL,
        NetIndicatorState::Disconnected => render::connectivity(NET_CONN_NONE),
        NetIndicatorState::Disabled => render::CONNECTIVITY_DISABLED,
    }
}

#[cfg(test)]
mod tests {
    use slopos_abi::net::{NET_CONN_UNKNOWN, NET_OPER_DOWN, NET_OPER_UP};

    use super::*;

    fn eth(admin_up: bool, carrier: bool, addr: [u8; 4]) -> IfaceRow {
        let mut row = IfaceRow::named(b"eth0", IfaceKind::Ethernet);
        row.admin_up = admin_up;
        row.carrier = carrier;
        row.oper = if carrier { NET_OPER_UP } else { NET_OPER_DOWN };
        row.ipv4 = addr;
        row.prefix_len = 24;
        row
    }

    fn loopback() -> IfaceRow {
        let mut row = IfaceRow::named(b"lo", IfaceKind::Loopback);
        row.admin_up = true;
        row.carrier = true;
        row.oper = NET_OPER_UP;
        row.ipv4 = [127, 0, 0, 1];
        row.prefix_len = 8;
        row
    }

    fn model(enabled: bool, connectivity: u8, rows: &[IfaceRow]) -> NetPanelModel {
        let mut m = NetPanelModel {
            enabled,
            connectivity,
            ..NetPanelModel::EMPTY
        };
        for &row in rows {
            m.push_iface(row);
        }
        m
    }

    #[test]
    fn the_master_switch_outranks_everything() {
        let up = eth(true, true, [10, 0, 2, 15]);
        assert_eq!(
            indicator_state_for(&model(false, NET_CONN_FULL, &[loopback(), up])),
            NetIndicatorState::Disabled
        );
        assert_eq!(
            indicator_state_for(&model(true, NET_CONN_FULL, &[loopback(), up])),
            NetIndicatorState::Connected
        );
    }

    /// A machine with only `lo` has no network. Loopback must never be the
    /// interface the indicator speaks for.
    #[test]
    fn loopback_alone_is_disconnected() {
        assert_eq!(
            indicator_state_for(&model(true, NET_CONN_FULL, &[loopback()])),
            NetIndicatorState::Disconnected
        );
        assert_eq!(
            indicator_state_for(&model(true, NET_CONN_FULL, &[])),
            NetIndicatorState::Disconnected
        );
        assert_eq!(model(true, NET_CONN_FULL, &[loopback()]).primary(), None);
    }

    #[test]
    fn interface_state_outranks_connectivity() {
        // Every connectivity value the kernel can report, against an interface
        // whose own state already answers the question.
        for connectivity in 0u8..=255 {
            assert_eq!(
                indicator_state_for(&model(
                    true,
                    connectivity,
                    &[eth(false, true, [10, 0, 2, 15])]
                )),
                NetIndicatorState::Disconnected,
                "admin down, connectivity={connectivity}"
            );
            assert_eq!(
                indicator_state_for(&model(
                    true,
                    connectivity,
                    &[eth(true, false, [10, 0, 2, 15])]
                )),
                NetIndicatorState::NoCarrier,
                "no carrier, connectivity={connectivity}"
            );
            assert_eq!(
                indicator_state_for(&model(true, connectivity, &[eth(true, true, [0, 0, 0, 0])])),
                NetIndicatorState::Configuring,
                "no address, connectivity={connectivity}"
            );
        }
    }

    #[test]
    fn a_lower_layer_down_link_has_no_carrier() {
        let mut row = eth(true, true, [10, 0, 2, 15]);
        row.oper = NET_OPER_LOWERLAYERDOWN;
        assert_eq!(
            indicator_state_for(&model(true, NET_CONN_FULL, &[row])),
            NetIndicatorState::NoCarrier
        );
    }

    #[test]
    fn a_dormant_link_is_still_configuring() {
        let mut row = eth(true, true, [10, 0, 2, 15]);
        row.oper = NET_OPER_DORMANT;
        assert_eq!(
            indicator_state_for(&model(true, NET_CONN_FULL, &[row])),
            NetIndicatorState::Configuring
        );
    }

    #[test]
    fn connectivity_decides_once_the_interface_is_ready() {
        let ready = eth(true, true, [10, 0, 2, 15]);
        let cases = [
            (NET_CONN_FULL, NetIndicatorState::Connected),
            (NET_CONN_LIMITED, NetIndicatorState::Limited),
            (NET_CONN_PORTAL, NetIndicatorState::Limited),
            (NET_CONN_LOCAL, NetIndicatorState::Limited),
            (NET_CONN_NONE, NetIndicatorState::Disconnected),
            (NET_CONN_UNKNOWN, NetIndicatorState::Configuring),
            (200, NetIndicatorState::Configuring),
        ];
        for (connectivity, expected) in cases {
            assert_eq!(
                indicator_state_for(&model(true, connectivity, &[loopback(), ready])),
                expected,
                "connectivity={connectivity}"
            );
        }
    }

    /// The whole cross product the indicator is a function of. Total, so a
    /// future reordering of the checks changes a recorded answer rather than
    /// slipping through.
    #[test]
    fn the_state_is_total_over_the_inputs() {
        for enabled in [false, true] {
            for admin_up in [false, true] {
                for carrier in [false, true] {
                    for has_addr in [false, true] {
                        for connectivity in [
                            NET_CONN_UNKNOWN,
                            NET_CONN_NONE,
                            NET_CONN_PORTAL,
                            NET_CONN_LIMITED,
                            NET_CONN_LOCAL,
                            NET_CONN_FULL,
                        ] {
                            let addr = if has_addr { [10, 0, 2, 15] } else { [0; 4] };
                            let m = model(
                                enabled,
                                connectivity,
                                &[loopback(), eth(admin_up, carrier, addr)],
                            );
                            let expected = if !enabled {
                                NetIndicatorState::Disabled
                            } else if !admin_up {
                                NetIndicatorState::Disconnected
                            } else if !carrier {
                                NetIndicatorState::NoCarrier
                            } else if !has_addr {
                                NetIndicatorState::Configuring
                            } else {
                                match connectivity {
                                    NET_CONN_FULL => NetIndicatorState::Connected,
                                    NET_CONN_LIMITED | NET_CONN_PORTAL | NET_CONN_LOCAL => {
                                        NetIndicatorState::Limited
                                    }
                                    NET_CONN_NONE => NetIndicatorState::Disconnected,
                                    _ => NetIndicatorState::Configuring,
                                }
                            };
                            assert_eq!(
                                indicator_state_for(&m),
                                expected,
                                "enabled={enabled} admin_up={admin_up} carrier={carrier} \
                                 has_addr={has_addr} connectivity={connectivity}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn iface_names_truncate_rather_than_overflow() {
        let row = IfaceRow::named(b"a-very-long-interface-name", IfaceKind::Ethernet);
        assert_eq!(row.name_bytes().len(), IFNAME_LEN);
        assert_eq!(row.name_str(), "a-very-long-inte");

        let short = IfaceRow::named(b"eth0", IfaceKind::Ethernet);
        assert_eq!(short.name_str(), "eth0");
        assert_eq!(IfaceRow::EMPTY.name_str(), "");
    }

    #[test]
    fn the_model_drops_overflow_rather_than_growing() {
        let mut m = NetPanelModel::EMPTY;
        for _ in 0..MAX_IFACES + 4 {
            m.push_iface(eth(true, true, [10, 0, 2, 15]));
        }
        for _ in 0..MAX_DNS + 4 {
            m.push_dns([1, 1, 1, 1]);
        }
        assert_eq!(m.iface_count, MAX_IFACES);
        assert_eq!(m.ifaces().len(), MAX_IFACES);
        assert_eq!(m.dns_count, MAX_DNS);
    }

    #[test]
    fn kinds_serialise_to_the_abi() {
        assert_eq!(render::iface_kind(IfaceKind::Loopback.abi()), "loopback");
        assert_eq!(render::iface_kind(IfaceKind::Ethernet.abi()), "ether");
    }

    #[test]
    fn every_state_has_a_distinct_sentence() {
        let mut seen: [&str; 6] = [""; 6];
        for (i, &state) in ALL_INDICATOR_STATES.iter().enumerate() {
            let label = indicator_label(state);
            assert!(!label.is_empty(), "{state:?} has no label");
            assert!(
                !seen[..i].contains(&label),
                "{state:?} reuses the sentence {label:?}"
            );
            seen[i] = label;
        }
        // The four states connectivity has a word for take that word rather
        // than a second spelling of it.
        assert_eq!(
            indicator_label(NetIndicatorState::Connected),
            render::connectivity(NET_CONN_FULL)
        );
        assert_eq!(
            indicator_label(NetIndicatorState::Disabled),
            render::CONNECTIVITY_DISABLED
        );
    }

    /// Every string this crate can put on screen must be drawable by the
    /// console font — ASCII, Latin-1, and exactly `€ ˚ ˇ`. An em dash is a
    /// rendering bug that only shows up on a framebuffer, so it is caught
    /// here instead.
    #[test]
    fn every_produced_string_is_renderable() {
        let mut checked = 0usize;
        let mut check = |s: &str| {
            for c in s.chars() {
                assert!(
                    render::is_renderable(c as u32),
                    "{s:?} contains U+{:04X}, which the console font cannot draw",
                    c as u32
                );
            }
            checked += 1;
        };

        for &state in ALL_INDICATOR_STATES {
            check(indicator_label(state));
        }
        check(CONFIGURING_LABEL);
        check(NO_CARRIER_LABEL);
        for label in PANEL_LABELS {
            check(label);
        }
        for kind in [IfaceKind::Loopback, IfaceKind::Ethernet] {
            check(render::iface_kind(kind.abi()));
        }
        assert_eq!(checked, ALL_INDICATOR_STATES.len() + 4 + PANEL_LABELS.len());
    }
}
