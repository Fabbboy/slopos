//! The connectivity classifier: what the stack can currently reach.
//!
//! One value, from [`NET_CONN_NONE`] to [`NET_CONN_FULL`], distinguishing the
//! four ways the network can be down — no link, no address, no route, no
//! answer.
//!
//! Reachability past the gateway is observed rather than configured: an ICMP
//! echo *reply* from the gateway, a successful DNS resolution, a TCP connection
//! to an off-link address reaching ESTABLISHED. There is deliberately **no
//! periodic DNS probe** — unsolicited queries on a timer would widen the
//! predictable-query-ID surface. Two ICMP probes cover stale passive evidence:
//! one to the gateway after [`GATEWAY_STALE_MS`], one to an off-link resolver
//! after [`WAN_FRESH_MS`].
//!
//! [`NET_CONN_PORTAL`](slopos_abi::net::NET_CONN_PORTAL) exists in the ABI so
//! the value space matches NetworkManager's, and is never produced here —
//! deciding it needs an HTTP request. A userland daemon that wants to make that
//! call sets [`Connectivity::set_enabled`] to `false` and drives the state.
//!
//! Atomics only, so evidence can be recorded from any receive path under that
//! subsystem's own locks. Only *evaluation* posts an event, and it runs with
//! nothing held.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_abi::net::{
    NET_CONN_FULL, NET_CONN_LIMITED, NET_CONN_LOCAL, NET_CONN_NONE, NET_CONN_UNKNOWN,
    NET_EV_CONNECTIVITY, NET_IFINDEX_GLOBAL, NetEvent,
};

use crate::iface::{self, IfaceKind};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netmon::netmon_post;
use crate::route::ROUTE_TABLE;
use crate::types::{DevIndex, Ipv4Addr};

/// How long evidence that something off-link answered stays good. Two minutes:
/// long enough that an idle desktop does not flap to [`NET_CONN_LIMITED`],
/// short enough that a network that went away is reported within a few ticks.
pub const WAN_FRESH_MS: u64 = 120_000;

/// How long the gateway may stay quiet before a `Limited` state is worth one
/// probe packet.
pub const GATEWAY_STALE_MS: u64 = 30_000;

/// How often the classifier re-evaluates.
pub const TICK_MS: u64 = 5_000;

/// Identifier carried by the classifier's own echo requests. Unlikely to be
/// bound by a socket: the echo-reply path demuxes by identifier, so a collision
/// would hand our reply to somebody else's `ping`.
const PROBE_IDENT: u16 = 0xC0DE;

/// Everything the classification depends on, gathered once, so [`classify`] is
/// a pure function over five booleans that a test can enumerate exhaustively.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Evidence {
    /// A realised, non-loopback interface has carrier.
    pub any_carrier: bool,
    /// One of those interfaces also has an address.
    pub has_address: bool,
    pub has_default_route: bool,
    /// The default route's gateway is a `Reachable` neighbour, or the route is
    /// directly connected and has no gateway to resolve.
    pub gateway_reachable: bool,
    /// Something off-link has answered within [`WAN_FRESH_MS`].
    pub wan_fresh: bool,
}

/// The state `evidence` implies, as a ladder ordered most-broken first.
pub const fn classify(evidence: Evidence) -> u8 {
    if !evidence.any_carrier {
        return NET_CONN_NONE;
    }
    if !evidence.has_address {
        return NET_CONN_NONE;
    }
    if !evidence.has_default_route {
        return NET_CONN_LOCAL;
    }
    if evidence.wan_fresh {
        // Outranks the gateway rung: off-link can only have been reached
        // through the first hop, and nothing refreshes gateway evidence while
        // the state is already `Full`, so asking first would flap to `Limited`.
        return NET_CONN_FULL;
    }
    // `gateway_reachable` decides which probe is worth sending, not which state
    // to report: the ABI has one word for both.
    NET_CONN_LIMITED
}

/// Connectivity state plus the evidence clocks behind it. Addressable rather
/// than global-only so a test can drive a scratch instance with synthetic
/// evidence.
pub struct Connectivity {
    state: AtomicU8,
    since_ms: AtomicU64,
    /// Monotonic ms at which the gateway last answered. 0 means never.
    last_gateway_ms: AtomicU64,
    /// Monotonic ms at which something off-link last answered. 0 means never.
    last_wan_ms: AtomicU64,
    /// The current default gateway, big-endian, cached so the receive path can
    /// recognise its replies with an atomic load instead of a route lookup.
    gateway_be: AtomicU32,
    /// The default route interface's network and mask, cached for the same
    /// reason: deciding a TCP peer is off-link must not take a lock on a path
    /// that already holds one.
    local_net_be: AtomicU32,
    local_mask_be: AtomicU32,
    /// Whether this classifier drives its own state. A userland daemon that
    /// takes over — the only thing that can ever report a captive portal —
    /// clears this and calls [`Connectivity::force_state`].
    enabled: AtomicBool,
}

/// The kernel's classifier.
pub static CONNECTIVITY: Connectivity = Connectivity::new();

impl Connectivity {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(NET_CONN_UNKNOWN),
            since_ms: AtomicU64::new(0),
            last_gateway_ms: AtomicU64::new(0),
            last_wan_ms: AtomicU64::new(0),
            gateway_be: AtomicU32::new(0),
            local_net_be: AtomicU32::new(0),
            local_mask_be: AtomicU32::new(0),
            enabled: AtomicBool::new(true),
        }
    }

    /// The current state. [`NET_CONN_UNKNOWN`] until the first evaluation —
    /// "nobody has looked yet" is a different answer from "nothing is
    /// reachable", and a UI that showed the second before the first boot
    /// evaluation would be reporting an outage that had not happened.
    #[inline]
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Monotonic milliseconds at which the state last changed.
    #[inline]
    pub fn since_ms(&self) -> u64 {
        self.since_ms.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Hand the classification to somebody else, or take it back.
    #[inline]
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    /// Record a state directly, returning the transition if there was one.
    ///
    /// The one path that may report a value the ladder cannot produce, which
    /// is why it is the path a userland connectivity daemon uses.
    pub fn force_state(&self, new: u8) -> Option<(u8, u8)> {
        let old = self.state.swap(new, Ordering::AcqRel);
        if old == new {
            return None;
        }
        self.since_ms
            .store(crate::clock::now_ms(), Ordering::Release);
        Some((old, new))
    }

    /// Classify `evidence` and record the result, returning the transition if
    /// the state moved. Does not announce — see
    /// [`apply_and_announce`](Self::apply_and_announce).
    pub fn apply(&self, evidence: Evidence) -> Option<(u8, u8)> {
        if !self.is_enabled() {
            return None;
        }
        self.force_state(classify(evidence))
    }

    /// [`apply`](Self::apply), announcing any transition to the monitors.
    ///
    /// The post happens after the state is committed and with nothing held —
    /// the same rule every other producer follows, and the reason this is a
    /// separate method from `apply` rather than a flag on it.
    pub fn apply_and_announce(&self, evidence: Evidence) -> Option<(u8, u8)> {
        let transition = self.apply(evidence);
        if let Some((old, new)) = transition {
            // Logged as well as posted: the netmon event reaches a subscriber,
            // and a connectivity transition is exactly the thing someone reads
            // a boot log to explain.
            slopos_ostd::klog_info!("connectivity: {} -> {}", state_name(old), state_name(new));
            announce(old, new);
        }
        transition
    }

    /// Note that the current default gateway answered.
    ///
    /// Takes the source address so the receive path does not have to know
    /// which address that is; the comparison is against the cached gateway and
    /// costs one atomic load.
    pub fn note_gateway_answer(&self, src: Ipv4Addr) {
        let gateway = self.gateway_be.load(Ordering::Acquire);
        if gateway != 0 && src.to_u32_be() == gateway {
            self.last_gateway_ms
                .store(crate::clock::now_ms(), Ordering::Release);
        }
    }

    /// Note that something off-link answered.
    pub fn note_wan_answer(&self) {
        self.last_wan_ms
            .store(crate::clock::now_ms(), Ordering::Release);
    }

    /// Note that `peer` answered, if `peer` is off-link.
    ///
    /// A connection to the local segment says nothing about the path beyond
    /// it, and counting one would report [`NET_CONN_FULL`] for a machine that
    /// can only reach its own subnet — which is the exact distinction the
    /// `Local` rung exists to make.
    pub fn note_wan_peer(&self, peer: Ipv4Addr) {
        if self.is_off_link(peer) {
            self.note_wan_answer();
        }
    }

    /// Whether `addr` is outside the default route interface's own prefix.
    ///
    /// Answered from the cached prefix rather than the interface table,
    /// because the callers are receive paths holding their own locks. Before
    /// the first evaluation the mask is zero and nothing is off-link, which is
    /// the conservative direction: evidence is dropped, never invented.
    pub fn is_off_link(&self, addr: Ipv4Addr) -> bool {
        let mask = self.local_mask_be.load(Ordering::Acquire);
        if mask == 0 {
            return false;
        }
        let net = self.local_net_be.load(Ordering::Acquire);
        (addr.to_u32_be() & mask) != net
    }

    /// Whether the gateway has answered within `window_ms`.
    fn gateway_fresh(&self, now_ms: u64, window_ms: u64) -> bool {
        let last = self.last_gateway_ms.load(Ordering::Acquire);
        last != 0 && now_ms.saturating_sub(last) <= window_ms
    }

    fn wan_fresh(&self, now_ms: u64) -> bool {
        let last = self.last_wan_ms.load(Ordering::Acquire);
        last != 0 && now_ms.saturating_sub(last) <= WAN_FRESH_MS
    }

    /// Cache what the receive paths need to recognise gateway and off-link
    /// traffic without taking a lock.
    fn cache_topology(&self, gateway: Ipv4Addr, net: Ipv4Addr, mask: u32) {
        self.gateway_be
            .store(gateway.to_u32_be(), Ordering::Release);
        self.local_net_be.store(net.to_u32_be(), Ordering::Release);
        self.local_mask_be.store(mask, Ordering::Release);
    }

    /// Test-only: reset every clock and the state, so a scratch instance
    /// starts from "never evaluated".
    #[cfg(feature = "test-hooks")]
    pub fn reset(&self) {
        self.state.store(NET_CONN_UNKNOWN, Ordering::Release);
        self.since_ms.store(0, Ordering::Release);
        self.last_gateway_ms.store(0, Ordering::Release);
        self.last_wan_ms.store(0, Ordering::Release);
        self.gateway_be.store(0, Ordering::Release);
        self.local_net_be.store(0, Ordering::Release);
        self.local_mask_be.store(0, Ordering::Release);
        self.enabled.store(true, Ordering::Release);
    }
}

impl Default for Connectivity {
    fn default() -> Self {
        Self::new()
    }
}

/// Announce a transition to the monitors.
///
/// Separate from the state change so it is impossible to post while holding
/// something: every caller commits first and announces after.
fn announce(old: u8, new: u8) {
    netmon_post(
        NET_EV_CONNECTIVITY,
        NET_IFINDEX_GLOBAL,
        NetEvent::connectivity_payload(old, new),
    );
}

// =============================================================================
// Gathering
// =============================================================================

/// Read the stack's current shape into an [`Evidence`], refreshing the cached
/// gateway and local prefix as a side effect.
///
/// Each table is read in its own critical section and released before the
/// next: the interface table, then the route table, then the neighbour cache.
/// That is the same one-lock-at-a-time rule [`crate::iface_ctl`] documents, and
/// it is why this cannot run from inside any of them.
pub fn gather_evidence() -> Evidence {
    let enabled = iface::is_enabled();

    let mut any_carrier = false;
    let mut has_address = false;
    iface::for_each(|i| {
        // Loopback is exempt: `127.0.0.1` is always up and says nothing about
        // whether this machine is on a network.
        if matches!(i.kind, IfaceKind::Loopback) || !i.is_realised(enabled) || !i.carrier {
            return;
        }
        any_carrier = true;
        if !i.addrs().is_empty() {
            has_address = true;
        }
    });

    let routes = ROUTE_TABLE.all_routes();
    let default = routes
        .iter()
        .filter(|r| r.prefix_len == 0)
        .min_by_key(|r| r.metric)
        .copied();
    drop(routes);

    let Some(route) = default else {
        CONNECTIVITY.cache_topology(Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED, 0);
        return Evidence {
            any_carrier,
            has_address,
            has_default_route: false,
            gateway_reachable: false,
            wan_fresh: false,
        };
    };

    // Cache the prefix of the interface the default route leaves by; that is
    // the one whose segment counts as "local" for off-link decisions.
    let (net, mask) = iface::get_by_dev(route.dev)
        .and_then(|i| i.primary_addr())
        .map(|a| (a.network(), crate::iface::prefix_to_mask(a.prefix_len)))
        .unwrap_or((Ipv4Addr::UNSPECIFIED, 0));
    CONNECTIVITY.cache_topology(route.gateway, net, mask);

    // A directly connected default route has no first hop to resolve, so
    // there is nothing that could fail to answer.
    let now_ms = crate::clock::now_ms();
    let gateway_reachable = if route.gateway.is_unspecified() {
        true
    } else {
        // Either the neighbour is confirmed, or this classifier heard from the
        // gateway recently. The second half is not redundant: a `Reachable`
        // entry ages to `Stale` after `REACHABLE_TIME_MS` — every 30 seconds on
        // a perfectly healthy link — so reading only the cache would flap the
        // indicator through "no internet" on that cycle. Neighbour aging is a
        // cache-management policy; whether the first hop answers is what this
        // asks, and `last_gateway_ms` is the record of that.
        NEIGHBOR_CACHE.is_reachable(route.dev, route.gateway)
            || CONNECTIVITY.gateway_fresh(now_ms, GATEWAY_STALE_MS)
    };

    Evidence {
        any_carrier,
        has_address,
        has_default_route: true,
        gateway_reachable,
        wan_fresh: CONNECTIVITY.wan_fresh(now_ms),
    }
}

// =============================================================================
// Kernel-classifier shorthands
// =============================================================================

/// The system's connectivity state.
#[inline]
pub fn state() -> u8 {
    CONNECTIVITY.state()
}

/// Monotonic milliseconds at which the system state last changed.
#[inline]
pub fn since_ms() -> u64 {
    CONNECTIVITY.since_ms()
}

/// Re-evaluate now and announce any transition.
///
/// Called from the classifier's timer, from the control plane after a topology
/// change, and from `NET_IFOP_CONN_RECHECK`. Never call it holding a network
/// lock: it reads all three tables and posts.
pub fn recheck() {
    CONNECTIVITY.apply_and_announce(gather_evidence());
}

/// Record that the default gateway answered an echo.
#[inline]
pub fn note_gateway_answer(src: Ipv4Addr) {
    CONNECTIVITY.note_gateway_answer(src);
}

/// Record a successful name resolution as evidence the path works.
#[inline]
pub fn note_dns_success() {
    CONNECTIVITY.note_wan_answer();
}

/// Record a TCP connection to `peer` reaching ESTABLISHED.
#[inline]
pub fn note_tcp_established(peer: Ipv4Addr) {
    CONNECTIVITY.note_wan_peer(peer);
}

/// Record that `peer` answered. Ignored unless `peer` is off-link.
#[inline]
pub fn note_wan_peer(peer: Ipv4Addr) {
    CONNECTIVITY.note_wan_peer(peer);
}

// =============================================================================
// The timer
// =============================================================================

use slopos_ostd::sync::InitFlag;

static TIMER_ARMED: InitFlag = InitFlag::new();

/// Arm the periodic evaluation once, the first time the network timer runs.
///
/// Self-arming rather than a boot step because the classifier has no
/// initialisation of its own to order against — it needs the timer wheel and
/// the thread that drains it, and the first tick of that thread is exactly
/// when both exist.
pub fn ensure_armed() {
    if TIMER_ARMED.init_once() {
        arm();
    }
}

fn arm() {
    crate::timer::NET_TIMER_WHEEL.schedule(TICK_MS, crate::timer::TimerKind::ConnProbe, 0);
}

/// One classifier tick: re-arm, re-evaluate, and probe if that is the only way
/// to make progress.
///
/// Runs from the network timer thread with nothing held, which is what makes
/// both the evaluation (three table reads and a post) and the probe (a packet
/// allocation and a send) legal here.
pub fn on_timer() {
    arm();

    let evidence = gather_evidence();
    CONNECTIVITY.apply_and_announce(evidence);

    // Active probes, in the order the ladder doubts things. Both are ICMP and
    // both are conditional on fresh passive evidence being absent, so a working
    // machine sends nothing at all.
    if classify(evidence) != NET_CONN_LIMITED {
        return;
    }
    let now = crate::clock::now_ms();

    // Rung one: the first hop. Nothing beyond it is worth asking about until
    // this answers.
    let gateway = CONNECTIVITY.gateway_be.load(Ordering::Acquire);
    if gateway != 0 && !CONNECTIVITY.gateway_fresh(now, GATEWAY_STALE_MS) {
        probe_gateway(Ipv4Addr::from_u32_be(gateway));
        return;
    }

    // Rung two: the path beyond it. Without this the classifier could only
    // learn about the internet from traffic somebody else generated, so a
    // freshly booted machine would report "no internet" until its user happened
    // to run something — the absence of evidence as evidence of absence.
    //
    // ICMP to an address we already hold, not a DNS query: a kernel timer
    // emitting unsolicited queries would widen the open predictable-query-ID
    // surface, and this needs no name resolved. Off-link only, because a
    // resolver on the local segment proves nothing about the path past the
    // gateway.
    if !CONNECTIVITY.wan_fresh(now) {
        probe_wan();
    }
}

/// Send one ICMP echo to the configured resolver, if it is off-link.
fn probe_wan() {
    let Some(target) = crate::resolver::primary() else {
        return;
    };
    if !CONNECTIVITY.is_off_link(target) {
        return;
    }
    let sequence = (crate::clock::now_ms() / TICK_MS) as u16;
    let _ = crate::icmp::send_echo_request(target.0, PROBE_IDENT, sequence, &[]);
}

/// Send one ICMP echo request to `gateway`.
fn probe_gateway(gateway: Ipv4Addr) {
    // The sequence is the low bits of the clock rather than a counter: it is
    // only ever used to tell one probe's reply from the next, and a counter
    // would be state to keep for no gain.
    let sequence = (crate::clock::now_ms() / TICK_MS) as u16;
    let _ = crate::icmp::send_echo_request(gateway.0, PROBE_IDENT, sequence, &[]);
}

/// The default gateway this classifier has cached — diagnostic use.
#[inline]
pub fn cached_gateway() -> Option<Ipv4Addr> {
    let raw = CONNECTIVITY.gateway_be.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        Some(Ipv4Addr::from_u32_be(raw))
    }
}

/// The device the default route leaves by, if there is one — diagnostic use.
pub fn default_device() -> Option<DevIndex> {
    let routes = ROUTE_TABLE.all_routes();
    routes
        .iter()
        .filter(|r| r.prefix_len == 0)
        .min_by_key(|r| r.metric)
        .map(|r| r.dev)
}

/// A human-readable name for a `NET_CONN_*` value.
pub const fn state_name(state: u8) -> &'static str {
    match state {
        NET_CONN_NONE => "none",
        NET_CONN_LOCAL => "local",
        NET_CONN_LIMITED => "limited",
        NET_CONN_FULL => "full",
        slopos_abi::net::NET_CONN_PORTAL => "portal",
        _ => "unknown",
    }
}
