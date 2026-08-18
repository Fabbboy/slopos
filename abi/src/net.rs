pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;

pub const SOCK_STREAM: u16 = 1;
pub const SOCK_DGRAM: u16 = 2;
pub const SOCK_RAW: u16 = 3;

pub const IPPROTO_ICMP: u16 = 1;

/// IPv4 socket address — mirrors POSIX `sockaddr_in` layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SockAddrIn {
    pub family: u16,
    /// Port in **network** byte order (big-endian).
    pub port: u16,
    /// IPv4 address in network byte order.
    pub addr: [u8; 4],
    pub _pad: [u8; 8],
}

const _: () = assert!(
    core::mem::size_of::<SockAddrIn>() == 16,
    "SockAddrIn must be exactly 16 bytes"
);

/// Initial kernel socket slab capacity, and the width of the fallback
/// wait-queue arrays. Not the maximum — the slab grows to [`MAX_SOCKET_SLOTS`].
pub const MAX_SOCKETS: usize = 64;

/// Hard maximum kernel socket slab capacity, and the width of the pinned
/// per-socket wait-queue spine, so a slab index maps to its own queue with no
/// folding. Must equal `SlabSocketTable::MAX_CAPACITY`.
pub const MAX_SOCKET_SLOTS: usize = 1024;

pub const INVALID_SOCKET_IDX: u32 = u32::MAX;

// No implicit padding in any struct below: every gap is a named `_padN`, so
// `#[derive(Default)]` writes every byte of `size_of` and the copy-out to user
// space has nothing uninitialised to disclose. A kernel-side producer must
// therefore start from `Default::default()`, never a struct literal.

/// Bytes in an interface name, including the NUL pad. Matches Linux's
/// `IFNAMSIZ` so a name that is legal there is legal here.
pub const NET_IFNAMSIZ: usize = 16;

/// Not an interface. `Iface` indices start at 1.
pub const NET_IFINDEX_NONE: u32 = 0;
/// Addresses the stack as a whole rather than one interface.
pub const NET_IFINDEX_GLOBAL: u32 = u32::MAX;

/// Upper bound on simultaneously registered interfaces, matching the device
/// registry's slot count.
pub const NET_MAX_IFACES: usize = 8;
pub const NET_MAX_ADDRS_PER_IFACE: usize = 4;
pub const NET_MAX_RESOLVERS: usize = 3;

/// Zeroed record; not a kind any interface reports.
pub const NET_IFKIND_UNSPEC: u8 = 0;
pub const NET_IFKIND_LOOPBACK: u8 = 1;
pub const NET_IFKIND_ETHERNET: u8 = 2;
/// Reserved, so a later kernel can add 802.11 without renumbering.
pub const NET_IFKIND_WIRELESS: u8 = 3;

// Values follow IANA `ifOperStatus` (RFC 2863). The three are distinct:
// `admin_up` is intent, `carrier` is the physical link, and the operational
// state is what the two combine to.

pub const NET_OPER_UNKNOWN: u8 = 0;
pub const NET_OPER_NOTPRESENT: u8 = 1;
pub const NET_OPER_DOWN: u8 = 2;
pub const NET_OPER_LOWERLAYERDOWN: u8 = 3;
pub const NET_OPER_TESTING: u8 = 4;
pub const NET_OPER_DORMANT: u8 = 5;
pub const NET_OPER_UP: u8 = 6;

// The low bits follow the Linux x86-64 `IFF_*` numeric assignments; the
// SlopOS-private bits sit high enough never to collide with an upstream one.

pub const IFF_UP: u32 = 1 << 0;
pub const IFF_BROADCAST: u32 = 1 << 1;
pub const IFF_LOOPBACK: u32 = 1 << 3;
pub const IFF_RUNNING: u32 = 1 << 6;
pub const IFF_MULTICAST: u32 = 1 << 12;

/// Admin-up, but held down by the global networking switch.
pub const IFF_SLOP_DISABLED: u32 = 1 << 24;
/// Admin-up, but the link is down.
pub const IFF_SLOP_NO_CARRIER: u32 = 1 << 25;
/// The driver cannot observe link state, so its reported `carrier` is an
/// assumption.
pub const IFF_SLOP_CARRIER_ASSUMED: u32 = 1 << 26;
/// A DHCP client is running on this interface.
pub const IFF_SLOP_DHCP: u32 = 1 << 27;

pub const NET_ADDR_ORIGIN_STATIC: u8 = 0;
pub const NET_ADDR_ORIGIN_DHCP: u8 = 1;
pub const NET_ADDR_ORIGIN_LINKLOCAL: u8 = 2;

pub const NET_ADDR_SCOPE_GLOBAL: u8 = 0;
pub const NET_ADDR_SCOPE_LINK: u8 = 1;
pub const NET_ADDR_SCOPE_HOST: u8 = 2;

/// A lifetime that never expires.
pub const NET_LFT_FOREVER: u32 = u32::MAX;

/// Derived from an address's prefix — the connected route.
pub const NET_ROUTE_ORIGIN_KERNEL: u8 = 0;
pub const NET_ROUTE_ORIGIN_STATIC: u8 = 1;
pub const NET_ROUTE_ORIGIN_DHCP: u8 = 2;

// The four states the neighbour cache implements: no DELAY/PROBE/PERMANENT,
// because the kernel cannot enter them.

pub const NET_NEIGH_INCOMPLETE: u8 = 0;
pub const NET_NEIGH_REACHABLE: u8 = 1;
pub const NET_NEIGH_STALE: u8 = 2;
pub const NET_NEIGH_FAILED: u8 = 3;

// The connection states RFC 793 names, plus one for a socket that has none.
// `UserSockInfo::state` holds one of these.

pub const NET_SOCK_CLOSED: u8 = 0;
pub const NET_SOCK_LISTEN: u8 = 1;
pub const NET_SOCK_SYN_SENT: u8 = 2;
pub const NET_SOCK_SYN_RECV: u8 = 3;
pub const NET_SOCK_ESTABLISHED: u8 = 4;
pub const NET_SOCK_FIN_WAIT1: u8 = 5;
pub const NET_SOCK_FIN_WAIT2: u8 = 6;
pub const NET_SOCK_CLOSE_WAIT: u8 = 7;
pub const NET_SOCK_CLOSING: u8 = 8;
pub const NET_SOCK_LAST_ACK: u8 = 9;
pub const NET_SOCK_TIME_WAIT: u8 = 10;
/// A bound or unbound datagram socket, for which "closed" would say something
/// ended when nothing did.
pub const NET_SOCK_UNCONN: u8 = 11;

// The value space matches NetworkManager's `NMConnectivityState` so a port of
// an existing indicator maps one-to-one.

pub const NET_CONN_UNKNOWN: u8 = 0;
pub const NET_CONN_NONE: u8 = 1;
/// Never produced by the kernel — detecting a captive portal needs an HTTP
/// request. Defined so a userland connectivity daemon can set it.
pub const NET_CONN_PORTAL: u8 = 2;
pub const NET_CONN_LIMITED: u8 = 3;
/// SlopOS extension: an address is configured but there is no default route,
/// so nothing off-link is reachable.
pub const NET_CONN_LOCAL: u8 = 4;
pub const NET_CONN_FULL: u8 = 5;

pub const NET_DHCP_DISABLED: u8 = 0;
pub const NET_DHCP_INIT: u8 = 1;
pub const NET_DHCP_SELECTING: u8 = 2;
pub const NET_DHCP_REQUESTING: u8 = 3;
pub const NET_DHCP_BOUND: u8 = 4;
pub const NET_DHCP_RENEWING: u8 = 5;
pub const NET_DHCP_REBINDING: u8 = 6;

/// Why the DHCP client last left a bound state.
pub const NET_DHCP_REASON_OK: u8 = 0;
pub const NET_DHCP_REASON_TIMEOUT: u8 = 1;
pub const NET_DHCP_REASON_NAK: u8 = 2;
pub const NET_DHCP_REASON_DECLINED: u8 = 3;
pub const NET_DHCP_REASON_NO_CARRIER: u8 = 4;

pub const NET_Q_IFACES: u32 = 1;
pub const NET_Q_ADDRS: u32 = 2;
pub const NET_Q_ROUTES: u32 = 3;
pub const NET_Q_NEIGH: u32 = 4;
pub const NET_Q_SOCKETS: u32 = 5;
pub const NET_Q_RESOLVER: u32 = 6;
pub const NET_Q_GLOBAL: u32 = 7;
pub const NET_Q_DHCP: u32 = 8;

/// Header written at the start of every `net_query` buffer, followed by
/// `record_count` records of `record_size` bytes each.
///
/// `record_size` is the forward-compatibility lever — there is no version field
/// and no TLV encoding, so a client that strides by the kernel's value keeps
/// reading the prefix it understands. `seq` is the global event sequence this
/// snapshot is consistent with, so a client holding a `net_monitor` fd can
/// discard events with `seq <= hdr.seq` for a gap-free handoff. Truncation
/// shows here, not in the return value: `total_count` exists, `record_count`
/// fit.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetQueryHdr {
    pub seq: u64,
    pub record_size: u32,
    pub record_count: u32,
    pub total_count: u32,
    /// Echo of the requested `what`, so a buffer is self-describing.
    pub what: u32,
}

const _: () = assert!(core::mem::size_of::<UserNetQueryHdr>() == 24);
const _: () = assert!(core::mem::align_of::<UserNetQueryHdr>() == 8);
const _: () = assert!(core::mem::offset_of!(UserNetQueryHdr, seq) == 0);
const _: () = assert!(core::mem::offset_of!(UserNetQueryHdr, record_size) == 8);
const _: () = assert!(core::mem::offset_of!(UserNetQueryHdr, record_count) == 12);
const _: () = assert!(core::mem::offset_of!(UserNetQueryHdr, total_count) == 16);
const _: () = assert!(core::mem::offset_of!(UserNetQueryHdr, what) == 20);

/// One interface, counters folded in so the common `ip -s link` case is a
/// single query.
///
/// Consumers must key on `ifindex`, never on `name`: indices are never reused,
/// but names are — a re-probed NIC becomes `eth0` again.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserIface {
    pub ifindex: u32,
    /// `IFF_*`.
    pub flags: u32,
    pub mtu: u32,
    /// `NET_IFKIND_*`.
    pub kind: u8,
    /// `NET_OPER_*`.
    pub oper_state: u8,
    pub carrier: u8,
    pub admin_up: u8,
    /// NUL-padded; not NUL-terminated when exactly `NET_IFNAMSIZ` bytes long.
    pub name: [u8; NET_IFNAMSIZ],
    pub mac: [u8; 6],
    pub _pad0: [u8; 2],
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

const _: () = assert!(core::mem::size_of::<UserIface>() == 104);
const _: () = assert!(core::mem::align_of::<UserIface>() == 8);
const _: () = assert!(core::mem::offset_of!(UserIface, ifindex) == 0);
const _: () = assert!(core::mem::offset_of!(UserIface, flags) == 4);
const _: () = assert!(core::mem::offset_of!(UserIface, mtu) == 8);
const _: () = assert!(core::mem::offset_of!(UserIface, kind) == 12);
const _: () = assert!(core::mem::offset_of!(UserIface, oper_state) == 13);
const _: () = assert!(core::mem::offset_of!(UserIface, carrier) == 14);
const _: () = assert!(core::mem::offset_of!(UserIface, admin_up) == 15);
const _: () = assert!(core::mem::offset_of!(UserIface, name) == 16);
const _: () = assert!(core::mem::offset_of!(UserIface, mac) == 32);
const _: () = assert!(core::mem::offset_of!(UserIface, rx_packets) == 40);
const _: () = assert!(core::mem::offset_of!(UserIface, tx_dropped) == 96);

/// One address assigned to one interface.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserAddr {
    pub ifindex: u32,
    pub addr: [u8; 4],
    pub prefix_len: u8,
    /// `AF_INET`, narrowed to a byte.
    pub family: u8,
    /// `NET_ADDR_SCOPE_*`.
    pub scope: u8,
    /// `NET_ADDR_ORIGIN_*`.
    pub origin: u8,
    pub flags: u32,
    /// Seconds remaining, or [`NET_LFT_FOREVER`].
    pub valid_lft_s: u32,
    pub pref_lft_s: u32,
}

const _: () = assert!(core::mem::size_of::<UserAddr>() == 24);
const _: () = assert!(core::mem::align_of::<UserAddr>() == 4);
const _: () = assert!(core::mem::offset_of!(UserAddr, ifindex) == 0);
const _: () = assert!(core::mem::offset_of!(UserAddr, addr) == 4);
const _: () = assert!(core::mem::offset_of!(UserAddr, prefix_len) == 8);
const _: () = assert!(core::mem::offset_of!(UserAddr, flags) == 12);
const _: () = assert!(core::mem::offset_of!(UserAddr, valid_lft_s) == 16);
const _: () = assert!(core::mem::offset_of!(UserAddr, pref_lft_s) == 20);

/// One routing-table entry.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserRoute {
    pub prefix: [u8; 4],
    /// `0.0.0.0` means directly connected.
    pub gateway: [u8; 4],
    pub prefix_len: u8,
    /// `NET_ROUTE_ORIGIN_*`.
    pub origin: u8,
    pub _pad0: [u8; 2],
    pub ifindex: u32,
    pub metric: u32,
    pub flags: u32,
}

const _: () = assert!(core::mem::size_of::<UserRoute>() == 24);
const _: () = assert!(core::mem::align_of::<UserRoute>() == 4);
const _: () = assert!(core::mem::offset_of!(UserRoute, prefix) == 0);
const _: () = assert!(core::mem::offset_of!(UserRoute, gateway) == 4);
const _: () = assert!(core::mem::offset_of!(UserRoute, prefix_len) == 8);
const _: () = assert!(core::mem::offset_of!(UserRoute, origin) == 9);
const _: () = assert!(core::mem::offset_of!(UserRoute, ifindex) == 12);
const _: () = assert!(core::mem::offset_of!(UserRoute, metric) == 16);
const _: () = assert!(core::mem::offset_of!(UserRoute, flags) == 20);

/// One neighbour-cache entry.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNeigh {
    pub ifindex: u32,
    pub addr: [u8; 4],
    /// All zero while the entry is `INCOMPLETE`.
    pub mac: [u8; 6],
    /// `NET_NEIGH_*`.
    pub state: u8,
    pub _pad0: u8,
    pub confirmed_ms_ago: u32,
    pub queued_pkts: u32,
}

const _: () = assert!(core::mem::size_of::<UserNeigh>() == 24);
const _: () = assert!(core::mem::align_of::<UserNeigh>() == 4);
const _: () = assert!(core::mem::offset_of!(UserNeigh, ifindex) == 0);
const _: () = assert!(core::mem::offset_of!(UserNeigh, addr) == 4);
const _: () = assert!(core::mem::offset_of!(UserNeigh, mac) == 8);
const _: () = assert!(core::mem::offset_of!(UserNeigh, state) == 14);
const _: () = assert!(core::mem::offset_of!(UserNeigh, confirmed_ms_ago) == 16);
const _: () = assert!(core::mem::offset_of!(UserNeigh, queued_pkts) == 20);

/// One socket, as `ss` renders it. Ports are **host** byte order, unlike
/// [`SockAddrIn`], because every consumer formats them for a human rather than
/// putting them on a wire.
///
/// Every caller sees every row, matching `/proc/net/tcp`'s mode 0444; what is
/// restricted is socket→process attribution. `owner_pid` holds the pid `getpid`
/// returns in the owning task for rows the caller's address space owns, and
/// every row to a caller holding `NET_ADMIN`; otherwise [`INVALID_PROCESS_ID`].
/// That sentinel means either "no process owns this" or "not disclosed to you",
/// deliberately indistinguishable, so a renderer must print nothing for it
/// rather than guess.
///
/// [`INVALID_PROCESS_ID`]: crate::task::INVALID_PROCESS_ID
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserSockInfo {
    pub local_addr: [u8; 4],
    pub remote_addr: [u8; 4],
    pub local_port: u16,
    pub remote_port: u16,
    pub family: u8,
    pub sock_type: u8,
    pub protocol: u8,
    pub state: u8,
    pub owner_pid: u32,
    pub rx_queue: u32,
    pub tx_queue: u32,
    pub sock_idx: u32,
}

const _: () = assert!(core::mem::size_of::<UserSockInfo>() == 32);
const _: () = assert!(core::mem::align_of::<UserSockInfo>() == 4);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, local_addr) == 0);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, remote_addr) == 4);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, local_port) == 8);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, remote_port) == 10);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, family) == 12);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, owner_pid) == 16);
const _: () = assert!(core::mem::offset_of!(UserSockInfo, sock_idx) == 28);

/// Resolver configuration. One record; `NET_Q_RESOLVER` never returns more.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserResolver {
    pub servers: [[u8; 4]; NET_MAX_RESOLVERS],
    pub n_servers: u8,
    /// `NET_RESOLVER_SRC_*`.
    pub source: u8,
    pub _pad0: [u8; 2],
    /// The interface a DHCP-learned configuration came from, or
    /// [`NET_IFINDEX_NONE`] for a static override.
    pub source_ifindex: u32,
    pub timeout_ms: u32,
    pub attempts: u32,
}

/// Set explicitly, and outranks anything DHCP learns.
pub const NET_RESOLVER_SRC_STATIC: u8 = 0;
pub const NET_RESOLVER_SRC_DHCP: u8 = 1;

const _: () = assert!(core::mem::size_of::<UserResolver>() == 28);
const _: () = assert!(core::mem::align_of::<UserResolver>() == 4);
const _: () = assert!(core::mem::offset_of!(UserResolver, servers) == 0);
const _: () = assert!(core::mem::offset_of!(UserResolver, n_servers) == 12);
const _: () = assert!(core::mem::offset_of!(UserResolver, source) == 13);
const _: () = assert!(core::mem::offset_of!(UserResolver, source_ifindex) == 16);
const _: () = assert!(core::mem::offset_of!(UserResolver, timeout_ms) == 20);
const _: () = assert!(core::mem::offset_of!(UserResolver, attempts) == 24);

/// Whole-stack state. One record.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserNetGlobal {
    /// Global event sequence at the moment of the snapshot.
    pub seq: u64,
    /// The master networking switch.
    pub enabled: u8,
    /// `NET_CONN_*`.
    pub connectivity: u8,
    pub n_ifaces: u8,
    /// Interfaces whose operational state is `NET_OPER_UP`.
    pub n_ifaces_running: u8,
    pub n_routes: u16,
    pub n_neigh: u16,
    /// The interface carrying the default route, or [`NET_IFINDEX_NONE`].
    pub default_ifindex: u32,
    pub default_gateway: [u8; 4],
    /// Monotonic milliseconds at which `connectivity` last changed.
    pub conn_since_ms: u64,
}

const _: () = assert!(core::mem::size_of::<UserNetGlobal>() == 32);
const _: () = assert!(core::mem::align_of::<UserNetGlobal>() == 8);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, seq) == 0);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, enabled) == 8);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, connectivity) == 9);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, n_ifaces) == 10);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, n_ifaces_running) == 11);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, n_routes) == 12);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, n_neigh) == 14);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, default_ifindex) == 16);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, default_gateway) == 20);
const _: () = assert!(core::mem::offset_of!(UserNetGlobal, conn_since_ms) == 24);

/// DHCP client state for one interface.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserDhcpStatus {
    pub ifindex: u32,
    pub server_id: [u8; 4],
    /// `NET_DHCP_*`.
    pub state: u8,
    /// `NET_DHCP_REASON_*`.
    pub last_reason: u8,
    pub retries: u8,
    pub _pad0: u8,
    pub lease_remaining_s: u32,
    pub t1_remaining_s: u32,
    pub t2_remaining_s: u32,
}

const _: () = assert!(core::mem::size_of::<UserDhcpStatus>() == 24);
const _: () = assert!(core::mem::align_of::<UserDhcpStatus>() == 4);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, ifindex) == 0);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, server_id) == 4);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, state) == 8);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, last_reason) == 9);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, retries) == 10);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, lease_remaining_s) == 12);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, t1_remaining_s) == 16);
const _: () = assert!(core::mem::offset_of!(UserDhcpStatus, t2_remaining_s) == 20);

pub const NET_EV_IFACE_ADDED: u16 = 1;
pub const NET_EV_IFACE_REMOVED: u16 = 2;
pub const NET_EV_IFACE_CHANGED: u16 = 3;
pub const NET_EV_ADDR_ADDED: u16 = 4;
pub const NET_EV_ADDR_REMOVED: u16 = 5;
pub const NET_EV_ROUTE_ADDED: u16 = 6;
pub const NET_EV_ROUTE_REMOVED: u16 = 7;
pub const NET_EV_RESOLVER: u16 = 8;
pub const NET_EV_CONNECTIVITY: u16 = 9;
pub const NET_EV_DHCP: u16 = 10;
pub const NET_EV_GLOBAL_ENABLE: u16 = 11;
/// Records were dropped. Delivered regardless of the subscription mask, once
/// per overflow episode, ordered *before* the records that followed the drop.
pub const NET_EV_OVERFLOW: u16 = 12;
pub const NET_EV_NEIGH_CHANGED: u16 = 13;

pub const NET_MON_IFACE: u32 = 1 << 0;
pub const NET_MON_ADDR: u32 = 1 << 1;
pub const NET_MON_ROUTE: u32 = 1 << 2;
pub const NET_MON_RESOLV: u32 = 1 << 3;
pub const NET_MON_CONN: u32 = 1 << 4;
pub const NET_MON_DHCP: u32 = 1 << 5;
pub const NET_MON_GLOBAL: u32 = 1 << 6;
/// Neighbour churn. Off by default: ARP is the stack's only high-rate source,
/// and subscribing keeps a bounded ring in permanent overflow, masking the
/// events a subscriber actually opened the fd for.
pub const NET_MON_NEIGH: u32 = 1 << 7;

/// Everything except [`NET_MON_NEIGH`].
pub const NET_MON_DEFAULT: u32 = NET_MON_IFACE
    | NET_MON_ADDR
    | NET_MON_ROUTE
    | NET_MON_RESOLV
    | NET_MON_CONN
    | NET_MON_DHCP
    | NET_MON_GLOBAL;

/// One record read from a `net_monitor` fd.
///
/// The payload is a fixed 16 bytes rather than a per-kind union, so adding an
/// event kind never changes the record size: `read()` stays a pure stride and
/// an older client skips a kind it does not know without losing framing.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NetEvent {
    /// Global, monotonic, and shared by every subscriber, which is what makes
    /// the snapshot-then-drain handoff in [`UserNetQueryHdr`] work.
    pub seq: u64,
    /// `NET_EV_*`.
    pub kind: u16,
    pub flags: u16,
    /// [`NET_IFINDEX_GLOBAL`] for events that name no interface.
    pub ifindex: u32,
    pub payload: [u8; 16],
}

const _: () = assert!(core::mem::size_of::<NetEvent>() == 32);
const _: () = assert!(core::mem::align_of::<NetEvent>() == 8);
const _: () = assert!(core::mem::offset_of!(NetEvent, seq) == 0);
const _: () = assert!(core::mem::offset_of!(NetEvent, kind) == 8);
const _: () = assert!(core::mem::offset_of!(NetEvent, flags) == 10);
const _: () = assert!(core::mem::offset_of!(NetEvent, ifindex) == 12);
const _: () = assert!(core::mem::offset_of!(NetEvent, payload) == 16);

/// Serialised size of one [`NetEvent`]; a `read` shorter than this is `EINVAL`.
pub const NET_EVENT_LEN: usize = 32;

/// Decoded [`NET_EV_IFACE_ADDED`] / [`NET_EV_IFACE_CHANGED`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventIface {
    pub oper_old: u8,
    pub oper_new: u8,
    pub carrier: u8,
    pub admin_up: u8,
    pub flags: u32,
    pub mtu: u32,
}

/// Decoded [`NET_EV_ADDR_ADDED`] / [`NET_EV_ADDR_REMOVED`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventAddr {
    pub addr: [u8; 4],
    pub prefix_len: u8,
    pub origin: u8,
    pub scope: u8,
}

/// Decoded [`NET_EV_ROUTE_ADDED`] / [`NET_EV_ROUTE_REMOVED`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventRoute {
    pub prefix: [u8; 4],
    pub gateway: [u8; 4],
    pub prefix_len: u8,
    pub origin: u8,
    pub metric: u32,
}

/// Decoded [`NET_EV_DHCP`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventDhcp {
    pub state: u8,
    pub reason: u8,
    pub lease_remaining_s: u32,
}

/// Decoded [`NET_EV_CONNECTIVITY`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventConnectivity {
    pub old: u8,
    pub new: u8,
}

/// Decoded [`NET_EV_RESOLVER`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventResolver {
    pub primary: [u8; 4],
    pub n_servers: u8,
    pub source: u8,
}

/// Decoded [`NET_EV_NEIGH_CHANGED`] payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NetEventNeigh {
    pub addr: [u8; 4],
    pub state: u8,
}

#[inline]
const fn p_u32(p: &[u8; 16], at: usize) -> u32 {
    u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]])
}

#[inline]
const fn p_ipv4(p: &[u8; 16], at: usize) -> [u8; 4] {
    [p[at], p[at + 1], p[at + 2], p[at + 3]]
}

impl NetEvent {
    /// The single construction point, so a producer cannot disagree with the
    /// accessors below about payload layout.
    #[inline]
    pub const fn new(seq: u64, kind: u16, ifindex: u32, payload: [u8; 16]) -> Self {
        Self {
            seq,
            kind,
            flags: 0,
            ifindex,
            payload,
        }
    }

    /// The record's wire bytes, for a `read` that copies out.
    pub fn to_bytes(&self) -> [u8; NET_EVENT_LEN] {
        let mut out = [0u8; NET_EVENT_LEN];
        out[0..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..10].copy_from_slice(&self.kind.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..16].copy_from_slice(&self.ifindex.to_le_bytes());
        out[16..32].copy_from_slice(&self.payload);
        out
    }

    pub fn from_bytes(buf: &[u8; NET_EVENT_LEN]) -> Self {
        let mut payload = [0u8; 16];
        payload.copy_from_slice(&buf[16..32]);
        Self {
            seq: u64::from_le_bytes([
                buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
            ]),
            kind: u16::from_le_bytes([buf[8], buf[9]]),
            flags: u16::from_le_bytes([buf[10], buf[11]]),
            ifindex: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            payload,
        }
    }

    pub const fn iface_payload(
        oper_old: u8,
        oper_new: u8,
        carrier: u8,
        admin_up: u8,
        flags: u32,
        mtu: u32,
    ) -> [u8; 16] {
        let f = flags.to_le_bytes();
        let m = mtu.to_le_bytes();
        [
            oper_old, oper_new, carrier, admin_up, f[0], f[1], f[2], f[3], m[0], m[1], m[2], m[3],
            0, 0, 0, 0,
        ]
    }

    pub const fn as_iface(&self) -> NetEventIface {
        NetEventIface {
            oper_old: self.payload[0],
            oper_new: self.payload[1],
            carrier: self.payload[2],
            admin_up: self.payload[3],
            flags: p_u32(&self.payload, 4),
            mtu: p_u32(&self.payload, 8),
        }
    }

    pub const fn addr_payload(addr: [u8; 4], prefix_len: u8, origin: u8, scope: u8) -> [u8; 16] {
        [
            addr[0], addr[1], addr[2], addr[3], prefix_len, origin, scope, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ]
    }

    pub const fn as_addr(&self) -> NetEventAddr {
        NetEventAddr {
            addr: p_ipv4(&self.payload, 0),
            prefix_len: self.payload[4],
            origin: self.payload[5],
            scope: self.payload[6],
        }
    }

    pub const fn route_payload(
        prefix: [u8; 4],
        gateway: [u8; 4],
        prefix_len: u8,
        origin: u8,
        metric: u32,
    ) -> [u8; 16] {
        let m = metric.to_le_bytes();
        [
            prefix[0], prefix[1], prefix[2], prefix[3], gateway[0], gateway[1], gateway[2],
            gateway[3], prefix_len, origin, 0, 0, m[0], m[1], m[2], m[3],
        ]
    }

    pub const fn as_route(&self) -> NetEventRoute {
        NetEventRoute {
            prefix: p_ipv4(&self.payload, 0),
            gateway: p_ipv4(&self.payload, 4),
            prefix_len: self.payload[8],
            origin: self.payload[9],
            metric: p_u32(&self.payload, 12),
        }
    }

    pub const fn dhcp_payload(state: u8, reason: u8, lease_remaining_s: u32) -> [u8; 16] {
        let l = lease_remaining_s.to_le_bytes();
        [
            state, reason, 0, 0, l[0], l[1], l[2], l[3], 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    pub const fn as_dhcp(&self) -> NetEventDhcp {
        NetEventDhcp {
            state: self.payload[0],
            reason: self.payload[1],
            lease_remaining_s: p_u32(&self.payload, 4),
        }
    }

    pub const fn connectivity_payload(old: u8, new: u8) -> [u8; 16] {
        [old, new, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    pub const fn as_connectivity(&self) -> NetEventConnectivity {
        NetEventConnectivity {
            old: self.payload[0],
            new: self.payload[1],
        }
    }

    pub const fn resolver_payload(primary: [u8; 4], n_servers: u8, source: u8) -> [u8; 16] {
        [
            primary[0], primary[1], primary[2], primary[3], n_servers, source, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]
    }

    pub const fn as_resolver(&self) -> NetEventResolver {
        NetEventResolver {
            primary: p_ipv4(&self.payload, 0),
            n_servers: self.payload[4],
            source: self.payload[5],
        }
    }

    pub const fn neigh_payload(addr: [u8; 4], state: u8) -> [u8; 16] {
        [
            addr[0], addr[1], addr[2], addr[3], state, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    pub const fn as_neigh(&self) -> NetEventNeigh {
        NetEventNeigh {
            addr: p_ipv4(&self.payload, 0),
            state: self.payload[4],
        }
    }

    /// Payload for [`NET_EV_OVERFLOW`] / [`NET_EV_GLOBAL_ENABLE`], both of
    /// which carry a single scalar.
    pub const fn u32_payload(v: u32) -> [u8; 16] {
        let b = v.to_le_bytes();
        [b[0], b[1], b[2], b[3], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    pub const fn as_u32(&self) -> u32 {
        p_u32(&self.payload, 0)
    }
}

// Each mutating syscall takes exactly one of these, checked against
// `size_of::<T>()` at the boundary — hence three narrow syscalls rather than
// one `net_config(op, ptr, len)` whose operand type depends on an op code.

/// `net_iface_ctl` operations. All of their operands fit in the two scalar
/// arguments, so this family touches no user memory at all.
pub const NET_IFOP_SET_ADMIN_UP: u32 = 1;
pub const NET_IFOP_SET_MTU: u32 = 2;
pub const NET_IFOP_FLUSH_NEIGH: u32 = 3;
/// `arg` is the neighbour's IPv4 address as a big-endian `u32`.
pub const NET_IFOP_DEL_NEIGH: u32 = 4;
pub const NET_IFOP_DHCP_START: u32 = 5;
pub const NET_IFOP_DHCP_STOP: u32 = 6;
pub const NET_IFOP_DHCP_RENEW: u32 = 7;
pub const NET_IFOP_DHCP_RELEASE: u32 = 8;
pub const NET_IFOP_FLUSH_ADDRS: u32 = 9;
/// Master networking switch. Requires `ifindex == NET_IFINDEX_GLOBAL`.
pub const NET_IFOP_SET_ENABLED: u32 = 100;
/// Force a connectivity re-evaluation. Requires `ifindex == NET_IFINDEX_GLOBAL`.
pub const NET_IFOP_CONN_RECHECK: u32 = 101;

pub const NET_ADDROP_ADD: u32 = 1;
pub const NET_ADDROP_DEL: u32 = 2;

pub const NET_ROUTEOP_ADD: u32 = 1;
pub const NET_ROUTEOP_DEL: u32 = 2;

/// Operand of `net_addr_ctl`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserAddrReq {
    pub ifindex: u32,
    pub addr: [u8; 4],
    pub prefix_len: u8,
    pub family: u8,
    pub scope: u8,
    pub _pad0: u8,
    pub flags: u32,
}

const _: () = assert!(core::mem::size_of::<UserAddrReq>() == 16);
const _: () = assert!(core::mem::align_of::<UserAddrReq>() == 4);
const _: () = assert!(core::mem::offset_of!(UserAddrReq, ifindex) == 0);
const _: () = assert!(core::mem::offset_of!(UserAddrReq, addr) == 4);
const _: () = assert!(core::mem::offset_of!(UserAddrReq, prefix_len) == 8);
const _: () = assert!(core::mem::offset_of!(UserAddrReq, flags) == 12);

/// Operand of `net_route_ctl`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserRouteReq {
    pub prefix: [u8; 4],
    /// `0.0.0.0` requests a directly connected route.
    pub gateway: [u8; 4],
    pub prefix_len: u8,
    pub _pad0: [u8; 3],
    pub ifindex: u32,
    pub metric: u32,
    pub flags: u32,
}

const _: () = assert!(core::mem::size_of::<UserRouteReq>() == 24);
const _: () = assert!(core::mem::align_of::<UserRouteReq>() == 4);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, prefix) == 0);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, gateway) == 4);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, prefix_len) == 8);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, ifindex) == 12);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, metric) == 16);
const _: () = assert!(core::mem::offset_of!(UserRouteReq, flags) == 20);

/// Operand of `net_resolver_set`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserResolverReq {
    pub servers: [[u8; 4]; NET_MAX_RESOLVERS],
    pub n_servers: u8,
    /// `NET_RESOLVER_SRC_*`. A caller setting `STATIC` pins the configuration
    /// against later DHCP leases.
    pub source: u8,
    pub _pad0: [u8; 2],
    pub timeout_ms: u32,
    pub attempts: u32,
}

const _: () = assert!(core::mem::size_of::<UserResolverReq>() == 24);
const _: () = assert!(core::mem::align_of::<UserResolverReq>() == 4);
const _: () = assert!(core::mem::offset_of!(UserResolverReq, servers) == 0);
const _: () = assert!(core::mem::offset_of!(UserResolverReq, n_servers) == 12);
const _: () = assert!(core::mem::offset_of!(UserResolverReq, source) == 13);
const _: () = assert!(core::mem::offset_of!(UserResolverReq, timeout_ms) == 16);
const _: () = assert!(core::mem::offset_of!(UserResolverReq, attempts) == 20);

// The per-field `offset_of!` assertions above pin where every field starts;
// these pin that each struct ends where its last field ends. Together: no hole
// anywhere in the layout.

const _: () = assert!(
    core::mem::size_of::<UserNetQueryHdr>()
        == core::mem::offset_of!(UserNetQueryHdr, what) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserIface>()
        == core::mem::offset_of!(UserIface, tx_dropped) + core::mem::size_of::<u64>()
);
const _: () = assert!(
    core::mem::size_of::<UserAddr>()
        == core::mem::offset_of!(UserAddr, pref_lft_s) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserRoute>()
        == core::mem::offset_of!(UserRoute, flags) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserNeigh>()
        == core::mem::offset_of!(UserNeigh, queued_pkts) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserSockInfo>()
        == core::mem::offset_of!(UserSockInfo, sock_idx) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserResolver>()
        == core::mem::offset_of!(UserResolver, attempts) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserNetGlobal>()
        == core::mem::offset_of!(UserNetGlobal, conn_since_ms) + core::mem::size_of::<u64>()
);
const _: () = assert!(
    core::mem::size_of::<UserDhcpStatus>()
        == core::mem::offset_of!(UserDhcpStatus, t2_remaining_s) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<NetEvent>()
        == core::mem::offset_of!(NetEvent, payload) + core::mem::size_of::<[u8; 16]>()
);
const _: () = assert!(
    core::mem::size_of::<UserAddrReq>()
        == core::mem::offset_of!(UserAddrReq, flags) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserRouteReq>()
        == core::mem::offset_of!(UserRouteReq, flags) + core::mem::size_of::<u32>()
);
const _: () = assert!(
    core::mem::size_of::<UserResolverReq>()
        == core::mem::offset_of!(UserResolverReq, attempts) + core::mem::size_of::<u32>()
);

#[cfg(test)]
mod tests {
    use super::*;

    // Encoder/accessor drift is a silent mis-render, not a compile error, so
    // these pin every pair.
    #[test]
    fn iface_payload_round_trips() {
        let p = NetEvent::iface_payload(
            NET_OPER_LOWERLAYERDOWN,
            NET_OPER_UP,
            1,
            1,
            IFF_UP | IFF_RUNNING | IFF_SLOP_DHCP,
            1500,
        );
        let ev = NetEvent::new(7, NET_EV_IFACE_CHANGED, 2, p);
        assert_eq!(
            ev.as_iface(),
            NetEventIface {
                oper_old: NET_OPER_LOWERLAYERDOWN,
                oper_new: NET_OPER_UP,
                carrier: 1,
                admin_up: 1,
                flags: IFF_UP | IFF_RUNNING | IFF_SLOP_DHCP,
                mtu: 1500,
            }
        );
    }

    #[test]
    fn addr_payload_round_trips() {
        let p = NetEvent::addr_payload(
            [10, 0, 2, 15],
            24,
            NET_ADDR_ORIGIN_DHCP,
            NET_ADDR_SCOPE_GLOBAL,
        );
        let ev = NetEvent::new(1, NET_EV_ADDR_ADDED, 2, p);
        assert_eq!(
            ev.as_addr(),
            NetEventAddr {
                addr: [10, 0, 2, 15],
                prefix_len: 24,
                origin: NET_ADDR_ORIGIN_DHCP,
                scope: NET_ADDR_SCOPE_GLOBAL,
            }
        );
    }

    #[test]
    fn route_payload_round_trips() {
        let p = NetEvent::route_payload([0, 0, 0, 0], [10, 0, 2, 2], 0, NET_ROUTE_ORIGIN_DHCP, 100);
        let ev = NetEvent::new(1, NET_EV_ROUTE_ADDED, 2, p);
        assert_eq!(
            ev.as_route(),
            NetEventRoute {
                prefix: [0, 0, 0, 0],
                gateway: [10, 0, 2, 2],
                prefix_len: 0,
                origin: NET_ROUTE_ORIGIN_DHCP,
                metric: 100,
            }
        );
    }

    #[test]
    fn dhcp_payload_round_trips() {
        let p = NetEvent::dhcp_payload(NET_DHCP_BOUND, NET_DHCP_REASON_OK, 86_400);
        let ev = NetEvent::new(1, NET_EV_DHCP, 2, p);
        assert_eq!(
            ev.as_dhcp(),
            NetEventDhcp {
                state: NET_DHCP_BOUND,
                reason: NET_DHCP_REASON_OK,
                lease_remaining_s: 86_400,
            }
        );
    }

    #[test]
    fn connectivity_resolver_neigh_payloads_round_trip() {
        let ev = NetEvent::new(
            1,
            NET_EV_CONNECTIVITY,
            NET_IFINDEX_GLOBAL,
            NetEvent::connectivity_payload(NET_CONN_LIMITED, NET_CONN_FULL),
        );
        assert_eq!(
            ev.as_connectivity(),
            NetEventConnectivity {
                old: NET_CONN_LIMITED,
                new: NET_CONN_FULL,
            }
        );

        let ev = NetEvent::new(
            2,
            NET_EV_RESOLVER,
            NET_IFINDEX_GLOBAL,
            NetEvent::resolver_payload([10, 0, 2, 3], 1, NET_RESOLVER_SRC_DHCP),
        );
        assert_eq!(
            ev.as_resolver(),
            NetEventResolver {
                primary: [10, 0, 2, 3],
                n_servers: 1,
                source: NET_RESOLVER_SRC_DHCP,
            }
        );

        let ev = NetEvent::new(
            3,
            NET_EV_NEIGH_CHANGED,
            2,
            NetEvent::neigh_payload([10, 0, 2, 2], NET_NEIGH_REACHABLE),
        );
        assert_eq!(
            ev.as_neigh(),
            NetEventNeigh {
                addr: [10, 0, 2, 2],
                state: NET_NEIGH_REACHABLE,
            }
        );
    }

    #[test]
    fn u32_payload_carries_overflow_count() {
        let ev = NetEvent::new(
            9,
            NET_EV_OVERFLOW,
            NET_IFINDEX_GLOBAL,
            NetEvent::u32_payload(4_000_000_000),
        );
        assert_eq!(ev.as_u32(), 4_000_000_000);
    }

    #[test]
    fn net_event_wire_round_trips() {
        let ev = NetEvent::new(
            0xDEAD_BEEF_1234_5678,
            NET_EV_IFACE_CHANGED,
            42,
            NetEvent::iface_payload(NET_OPER_DOWN, NET_OPER_UP, 1, 1, IFF_UP, 9000),
        );
        let bytes = ev.to_bytes();
        let back = NetEvent::from_bytes(&bytes);
        assert_eq!(back.seq, ev.seq);
        assert_eq!(back.kind, ev.kind);
        assert_eq!(back.flags, ev.flags);
        assert_eq!(back.ifindex, ev.ifindex);
        assert_eq!(back.payload, ev.payload);
        assert_eq!(bytes.len(), NET_EVENT_LEN);
    }

    #[test]
    fn default_monitor_mask_excludes_neigh() {
        assert_eq!(NET_MON_DEFAULT & NET_MON_NEIGH, 0);
        assert_ne!(NET_MON_DEFAULT & NET_MON_IFACE, 0);
        assert_ne!(NET_MON_DEFAULT & NET_MON_DHCP, 0);
    }
}
