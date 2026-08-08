//! Network interfaces: the objects `ip link` and the status indicator describe.
//!
//! A [`NetDevice`](crate::netdev::NetDevice) moves bytes. An [`Iface`] is what
//! that device *is* to the rest of the system — a name, a stable index, an
//! administrative intent, a link state, and the addresses assigned to it.
//! Keeping the two apart is what lets the device registry stay a pure
//! data-plane concern while everything a person configures lives here.
//!
//! # Three states that are not one state
//!
//! `admin_up` is **intent**: somebody asked for this interface to be usable.
//! `carrier` is the **physical link**. [`OperState`] is what those combine to,
//! following IANA `ifOperStatus` (RFC 2863) so the vocabulary matches what
//! `ip link` and `/sys/class/net/*/operstate` report elsewhere. Collapsing them
//! into one boolean is the classic mistake: an unplugged cable and a
//! deliberately disabled interface are both "not working" and want completely
//! different words in a UI.
//!
//! # The master switch
//!
//! [`set_enabled`] is a **gate**, not a bulk edit. The invariant is
//!
//! ```text
//! realised(iface) == iface.admin_up && (NET_ENABLED || kind == Loopback)
//! ```
//!
//! Disabling never writes `admin_up`, so `admin_up` *is* the memory of what the
//! operator wanted and there is no remembered set to go stale. That matters for
//! the case a snapshot-and-restore design gets wrong: a device probed *while*
//! networking is disabled was in nobody's snapshot, but it still must come up
//! unrealised and be realised by the next enable.
//!
//! Loopback is exempt at the predicate rather than at each call site. Taking
//! `127.0.0.1` away would break AF_INET localhost IPC that has nothing to do
//! with networking being switched off, which is also why NetworkManager's
//! `NetworkingEnabled` leaves `lo` alone.
//!
//! # Storage
//!
//! Fixed arrays, sized by [`NET_MAX_IFACES`] and [`NET_MAX_ADDRS_PER_IFACE`].
//! Nothing here allocates, which is what makes every operation safe to perform
//! under the table's own cli-disabling lock — the allocator is where every
//! subsystem meets, and reaching it from under this lock would be a deadlock
//! edge for no benefit at a bound of eight.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use slopos_abi::net::{
    IFF_BROADCAST, IFF_LOOPBACK, IFF_MULTICAST, IFF_RUNNING, IFF_SLOP_CARRIER_ASSUMED,
    IFF_SLOP_DHCP, IFF_SLOP_DISABLED, IFF_SLOP_NO_CARRIER, IFF_UP, NET_EV_ADDR_ADDED,
    NET_EV_ADDR_REMOVED, NET_EV_IFACE_ADDED, NET_EV_IFACE_CHANGED, NET_EV_IFACE_REMOVED,
    NET_IFINDEX_NONE, NET_IFNAMSIZ, NET_MAX_ADDRS_PER_IFACE, NET_MAX_IFACES, NetEvent,
};
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use crate::netmon::netmon_post;
use crate::types::{DevIndex, Ipv4Addr, MacAddr};

// =============================================================================
// Names
// =============================================================================

/// An interface name: at most [`NET_IFNAMSIZ`] bytes, NUL-padded.
///
/// Names are **reusable** — a re-probed NIC becomes `eth0` again, which is what
/// a person expects. Indices are not. Anything that caches per-interface state
/// must therefore key on [`Iface::ifindex`], never on the name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IfName([u8; NET_IFNAMSIZ]);

impl IfName {
    /// Build a name from bytes, rejecting anything a tool could not round-trip:
    /// empty, over-long, or containing something outside `[a-z0-9_-]`.
    pub fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > NET_IFNAMSIZ {
            return None;
        }
        let mut buf = [0u8; NET_IFNAMSIZ];
        for (dst, &b) in buf.iter_mut().zip(bytes.iter()) {
            let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
            if !ok {
                return None;
            }
            *dst = b;
        }
        Some(Self(buf))
    }

    /// The name without its NUL padding.
    pub fn as_bytes(&self) -> &[u8] {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(NET_IFNAMSIZ);
        &self.0[..end]
    }

    /// The padded form, as the ABI carries it.
    #[inline]
    pub const fn raw(&self) -> [u8; NET_IFNAMSIZ] {
        self.0
    }
}

impl core::fmt::Display for IfName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for &b in self.as_bytes() {
            write!(f, "{}", b as char)?;
        }
        Ok(())
    }
}

impl core::fmt::Debug for IfName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

// =============================================================================
// Enumerations
// =============================================================================

/// What kind of thing this interface is.
///
/// Deliberately only the two kinds that exist. The ABI reserves a value for
/// 802.11 so adding it later does not renumber anything, but nothing here
/// produces it and no code should branch on a wireless case that cannot occur.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum IfaceKind {
    Loopback = slopos_abi::net::NET_IFKIND_LOOPBACK,
    Ethernet = slopos_abi::net::NET_IFKIND_ETHERNET,
}

impl IfaceKind {
    #[inline]
    pub const fn to_abi(self) -> u8 {
        self as u8
    }

    #[inline]
    const fn is_loopback(self) -> bool {
        matches!(self, IfaceKind::Loopback)
    }
}

/// IANA `ifOperStatus` (RFC 2863).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OperState {
    Unknown = slopos_abi::net::NET_OPER_UNKNOWN,
    NotPresent = slopos_abi::net::NET_OPER_NOTPRESENT,
    Down = slopos_abi::net::NET_OPER_DOWN,
    LowerLayerDown = slopos_abi::net::NET_OPER_LOWERLAYERDOWN,
    Dormant = slopos_abi::net::NET_OPER_DORMANT,
    Up = slopos_abi::net::NET_OPER_UP,
}

impl OperState {
    #[inline]
    pub const fn to_abi(self) -> u8 {
        self as u8
    }
}

/// Where an address came from, which decides whether it survives an
/// administrative down: a lease does not, a static assignment does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum AddrOrigin {
    Static = slopos_abi::net::NET_ADDR_ORIGIN_STATIC,
    Dhcp = slopos_abi::net::NET_ADDR_ORIGIN_DHCP,
    LinkLocal = slopos_abi::net::NET_ADDR_ORIGIN_LINKLOCAL,
}

/// How far an address is meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum AddrScope {
    Global = slopos_abi::net::NET_ADDR_SCOPE_GLOBAL,
    Link = slopos_abi::net::NET_ADDR_SCOPE_LINK,
    Host = slopos_abi::net::NET_ADDR_SCOPE_HOST,
}

// =============================================================================
// Pure derivations
// =============================================================================
//
// No locks, no I/O, no table access. That is deliberate: these three functions
// are the entirety of the interface state model, so keeping them pure makes the
// whole model a unit test rather than something only observable by booting.

/// Whether the interface's administrative intent is actually in effect.
///
/// See the module docs for why loopback ignores the master switch.
#[inline]
pub const fn realised(kind: IfaceKind, admin_up: bool, enabled: bool) -> bool {
    admin_up && (enabled || kind.is_loopback())
}

/// The RFC 2863 operational state implied by intent, the master switch and the
/// link.
///
/// A realised loopback reports `Unknown`, not `Up` — it has no lower layer
/// whose state could be reported, and `Unknown` is what Linux reports for `lo`
/// and therefore what `ip link show lo` prints everywhere else.
#[inline]
pub const fn oper_state(
    kind: IfaceKind,
    admin_up: bool,
    enabled: bool,
    carrier: bool,
) -> OperState {
    if !realised(kind, admin_up, enabled) {
        return OperState::Down;
    }
    if kind.is_loopback() {
        return OperState::Unknown;
    }
    if !carrier {
        return OperState::LowerLayerDown;
    }
    OperState::Up
}

/// The `IFF_*` word a tool renders as `<BROADCAST,MULTICAST,UP>`.
///
/// The SlopOS-private bits exist so a UI can say *why* an interface that was
/// asked to be up is not: held down by the master switch, no carrier, or a
/// driver that cannot observe carrier at all and is therefore guessing.
#[inline]
pub const fn if_flags(
    kind: IfaceKind,
    admin_up: bool,
    enabled: bool,
    carrier: bool,
    carrier_detect: bool,
    dhcp: bool,
) -> u32 {
    let mut flags = 0u32;
    if admin_up {
        flags |= IFF_UP;
    }
    if kind.is_loopback() {
        flags |= IFF_LOOPBACK;
    } else {
        flags |= IFF_BROADCAST | IFF_MULTICAST;
    }

    // RUNNING tracks the operational state, not the intent: it is set exactly
    // when traffic could actually flow.
    match oper_state(kind, admin_up, enabled, carrier) {
        OperState::Up | OperState::Unknown => flags |= IFF_RUNNING,
        _ => {}
    }

    if !kind.is_loopback() {
        if admin_up && !enabled {
            flags |= IFF_SLOP_DISABLED;
        }
        if admin_up && !carrier {
            flags |= IFF_SLOP_NO_CARRIER;
        }
        if !carrier_detect {
            flags |= IFF_SLOP_CARRIER_ASSUMED;
        }
    }
    if dhcp {
        flags |= IFF_SLOP_DHCP;
    }
    flags
}

// =============================================================================
// Addresses
// =============================================================================

/// One address assigned to an interface.
#[derive(Clone, Copy)]
pub struct IfaceAddr {
    pub addr: Ipv4Addr,
    pub prefix_len: u8,
    pub scope: AddrScope,
    pub origin: AddrOrigin,
    /// Monotonic milliseconds at which the address stops being valid, or
    /// `u64::MAX` for an address that never expires.
    pub valid_until_ms: u64,
    /// Monotonic milliseconds at which the address stops being preferred.
    pub pref_until_ms: u64,
}

impl IfaceAddr {
    /// An address with no expiry — what a static assignment and loopback get.
    pub const fn permanent(
        addr: Ipv4Addr,
        prefix_len: u8,
        scope: AddrScope,
        origin: AddrOrigin,
    ) -> Self {
        Self {
            addr,
            prefix_len,
            scope,
            origin,
            valid_until_ms: u64::MAX,
            pref_until_ms: u64::MAX,
        }
    }

    /// The netmask implied by the prefix length.
    #[inline]
    pub const fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from_u32_be(prefix_to_mask(self.prefix_len))
    }

    /// The subnet's broadcast address.
    #[inline]
    pub const fn broadcast(&self) -> Ipv4Addr {
        let mask = prefix_to_mask(self.prefix_len);
        Ipv4Addr::from_u32_be(self.addr.to_u32_be() | !mask)
    }

    /// The network prefix this address sits in.
    #[inline]
    pub const fn network(&self) -> Ipv4Addr {
        let mask = prefix_to_mask(self.prefix_len);
        Ipv4Addr::from_u32_be(self.addr.to_u32_be() & mask)
    }

    /// Whether `ip` is on this address's directly connected subnet.
    #[inline]
    pub const fn is_local(&self, ip: Ipv4Addr) -> bool {
        let mask = prefix_to_mask(self.prefix_len);
        (ip.to_u32_be() & mask) == (self.addr.to_u32_be() & mask)
    }
}

/// Prefix length to a big-endian netmask. `/0` is `0.0.0.0`, `/32` is all ones.
#[inline]
pub const fn prefix_to_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else if prefix_len >= 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix_len)
    }
}

// =============================================================================
// The interface itself
// =============================================================================

/// One network interface.
#[derive(Clone, Copy)]
pub struct Iface {
    /// Monotonic and **never reused**. Device-registry slots recycle, so a
    /// monitor consumer that missed a removal would otherwise apply a later
    /// event to a different interface wearing the same index.
    pub ifindex: u32,
    /// The device this interface fronts.
    pub dev: DevIndex,
    pub name: IfName,
    pub kind: IfaceKind,
    pub mac: MacAddr,
    pub mtu: u16,
    /// Administrative intent. Survives the master switch and carrier loss.
    pub admin_up: bool,
    /// Cached link state, refreshed from `NetDevice::carrier()`.
    pub carrier: bool,
    /// Whether `carrier` is an observation or an assumption.
    pub carrier_detect: bool,
    /// Set while an administrative transition is in flight, so a concurrent
    /// caller is refused rather than interleaved with a half-applied change.
    /// Mirrors `DeviceSlot::retiring` in the device registry.
    pub admin_busy: bool,
    /// A DHCP client is running on this interface.
    pub dhcp_managed: bool,
    addrs: [IfaceAddr; NET_MAX_ADDRS_PER_IFACE],
    n_addrs: u8,
}

impl Iface {
    /// The addresses assigned to this interface.
    #[inline]
    pub fn addrs(&self) -> &[IfaceAddr] {
        &self.addrs[..self.n_addrs as usize]
    }

    /// This interface's operational state under the current master switch.
    #[inline]
    pub fn oper_state(&self, enabled: bool) -> OperState {
        oper_state(self.kind, self.admin_up, enabled, self.carrier)
    }

    /// This interface's `IFF_*` word under the current master switch.
    #[inline]
    pub fn flags(&self, enabled: bool) -> u32 {
        if_flags(
            self.kind,
            self.admin_up,
            enabled,
            self.carrier,
            self.carrier_detect,
            self.dhcp_managed,
        )
    }

    /// Whether this interface's administrative intent is in effect.
    #[inline]
    pub fn is_realised(&self, enabled: bool) -> bool {
        realised(self.kind, self.admin_up, enabled)
    }

    /// The first address assigned, which is what source selection and "our IP"
    /// queries mean.
    #[inline]
    pub fn primary_addr(&self) -> Option<IfaceAddr> {
        self.addrs().first().copied()
    }
}

// =============================================================================
// The table
// =============================================================================

/// A set of interfaces.
///
/// Addressable rather than global, following [`RouteTable`](crate::route::RouteTable)
/// and [`NeighborCache`](crate::neighbor::NeighborCache): a test builds a
/// scratch table and asserts against it, instead of reaching for the live one
/// and destroying the boot configuration that every other test reads.
pub struct IfaceTable {
    inner: SpinLock<IfaceTableInner>,
    /// The master networking switch, per table so a scratch instance can be
    /// toggled without touching the running system.
    enabled: AtomicBool,
}

struct IfaceTableInner {
    slots: [Option<Iface>; NET_MAX_IFACES],
}

/// Source of interface indices, shared by every table.
///
/// Global on purpose: an index must be unique for the lifetime of the kernel,
/// and a per-table counter would let a scratch table mint an index the live
/// table is already using.
static NEXT_IFINDEX: AtomicU32 = AtomicU32::new(1);

/// The kernel's interface table.
pub static IFACE_TABLE: IfaceTable =
    IfaceTable::new(lock_class!("NET_IFACES", LOCK_LEVEL_REGISTRY));

/// Why an interface operation could not be performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IfaceError {
    /// No interface with that index.
    NoSuchIface,
    /// The interface table is full.
    NoSpace,
    /// This interface already holds [`NET_MAX_ADDRS_PER_IFACE`] addresses.
    TooManyAddrs,
    /// An administrative transition is already in flight.
    Busy,
    /// The address, prefix length or name was not representable.
    Invalid,
    /// The address is not assigned to this interface.
    NotFound,
}

impl IfaceTable {
    /// An empty table. The class comes from the caller so a scratch table built
    /// by a test is a different lockdep class from the live one — they are
    /// genuinely different locks.
    pub const fn new(class: &'static slopos_ostd::sync::lock_tracking::LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                IfaceTableInner {
                    slots: [const { None }; NET_MAX_IFACES],
                },
                class,
            ),
            enabled: AtomicBool::new(true),
        }
    }

    /// Whether networking is enabled for this table.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Move the master switch, reporting whether it changed.
    ///
    /// This only moves the gate. Realising or unrealising the interfaces it now
    /// covers is the caller's job, because that has to happen with no interface
    /// lock held — see [`crate::iface_ctl`].
    #[inline]
    pub fn set_enabled_flag(&self, on: bool) -> bool {
        self.enabled.swap(on, Ordering::AcqRel) != on
    }

    /// Register an interface for a device that has just been added to the
    /// device registry.
    ///
    /// Called **after** `NetDeviceRegistry::register` returns, never from
    /// inside it. Keeping the two sequential is what stops the device registry
    /// and this table from ever appearing in each other's lock-order edges: an
    /// administrative down needs this table and then the registry, so an edge
    /// the other way would close a cycle.
    ///
    /// A device attached while networking is disabled comes up with `admin_up`
    /// set — matching "a probed NIC is immediately usable" — but unrealised.
    /// The next enable realises it. That is the case a remembered-set design
    /// gets wrong, because the device was in nobody's snapshot.
    pub fn attach(
        &self,
        dev: DevIndex,
        kind: IfaceKind,
        mac: MacAddr,
        mtu: u16,
        carrier: bool,
        carrier_detect: bool,
    ) -> Result<u32, IfaceError> {
        let mut table = self.inner.lock();

        let name = allocate_name(&table, kind).ok_or(IfaceError::NoSpace)?;
        let slot = table
            .slots
            .iter_mut()
            .find(|s| s.is_none())
            .ok_or(IfaceError::NoSpace)?;

        let ifindex = NEXT_IFINDEX.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Iface {
            ifindex,
            dev,
            name,
            kind,
            mac,
            mtu,
            admin_up: true,
            carrier,
            carrier_detect,
            admin_busy: false,
            dhcp_managed: false,
            addrs: [const {
                IfaceAddr::permanent(
                    Ipv4Addr::UNSPECIFIED,
                    0,
                    AddrScope::Global,
                    AddrOrigin::Static,
                )
            }; NET_MAX_ADDRS_PER_IFACE],
            n_addrs: 0,
        });
        Ok(ifindex)
    }

    /// Drop an interface's row. The device is retired separately, by the
    /// registry.
    pub fn detach(&self, dev: DevIndex) -> Option<Iface> {
        let mut table = self.inner.lock();
        let slot = table
            .slots
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|i| i.dev == dev))?;
        slot.take()
    }

    /// A copy of one interface, by index.
    pub fn get(&self, ifindex: u32) -> Option<Iface> {
        let table = self.inner.lock();
        table
            .slots
            .iter()
            .flatten()
            .find(|i| i.ifindex == ifindex)
            .copied()
    }

    /// A copy of one interface, by the device it fronts.
    pub fn get_by_dev(&self, dev: DevIndex) -> Option<Iface> {
        let table = self.inner.lock();
        table.slots.iter().flatten().find(|i| i.dev == dev).copied()
    }

    /// A copy of one interface, by name.
    pub fn get_by_name(&self, name: &[u8]) -> Option<Iface> {
        let want = IfName::new(name)?;
        let table = self.inner.lock();
        table
            .slots
            .iter()
            .flatten()
            .find(|i| i.name == want)
            .copied()
    }

    /// Copy every interface into `out`, returning `(written, total)`.
    ///
    /// Both numbers matter: a caller with a short buffer needs to know it was
    /// truncated, which it cannot infer from `written` alone.
    pub fn snapshot(&self, out: &mut [Iface]) -> (usize, usize) {
        let table = self.inner.lock();
        let mut total = 0usize;
        let mut written = 0usize;
        for iface in table.slots.iter().flatten() {
            total += 1;
            if written < out.len() {
                out[written] = *iface;
                written += 1;
            }
        }
        (written, total)
    }

    /// Visit every interface under the table lock.
    ///
    /// `f` must not allocate or take another network lock. The intended use is
    /// pushing into a `KVec` that the caller reserved *before* calling, so the
    /// fill itself cannot reach the allocator.
    pub fn for_each(&self, mut f: impl FnMut(&Iface)) {
        let table = self.inner.lock();
        for iface in table.slots.iter().flatten() {
            f(iface);
        }
    }

    /// Number of registered interfaces.
    pub fn count(&self) -> usize {
        self.inner.lock().slots.iter().flatten().count()
    }

    /// Apply `f` to the interface with `ifindex`, under the table lock.
    ///
    /// `f` must not allocate, block, or take another network lock — the whole
    /// point of the fixed-array storage is that nothing here needs to.
    fn with_mut<T>(&self, ifindex: u32, f: impl FnOnce(&mut Iface) -> T) -> Result<T, IfaceError> {
        let mut table = self.inner.lock();
        let iface = table
            .slots
            .iter_mut()
            .flatten()
            .find(|i| i.ifindex == ifindex)
            .ok_or(IfaceError::NoSuchIface)?;
        Ok(f(iface))
    }

    /// Record a carrier transition, returning `(ifindex, before, after)` or
    /// `None` when nothing changed.
    pub fn set_carrier(&self, dev: DevIndex, up: bool) -> Option<(u32, OperState, OperState)> {
        let enabled = self.is_enabled();
        let mut table = self.inner.lock();
        let iface = table.slots.iter_mut().flatten().find(|i| i.dev == dev)?;
        if iface.carrier == up {
            return None;
        }
        let before = iface.oper_state(enabled);
        iface.carrier = up;
        iface.carrier_detect = true;
        let after = iface.oper_state(enabled);
        Some((iface.ifindex, before, after))
    }

    /// Set administrative intent without realising it, returning the
    /// operational state before and after.
    ///
    /// Realisation — calling into the device, withdrawing routes, flushing
    /// neighbours — happens in [`crate::iface_ctl`] with no lock held.
    /// Splitting it this way is what keeps this module free of out-edges.
    pub fn set_admin_intent(
        &self,
        ifindex: u32,
        up: bool,
    ) -> Result<(OperState, OperState), IfaceError> {
        let enabled = self.is_enabled();
        self.with_mut(ifindex, |iface| {
            let before = iface.oper_state(enabled);
            iface.admin_up = up;
            (before, iface.oper_state(enabled))
        })
    }

    /// Claim the administrative-transition guard, or report [`IfaceError::Busy`].
    pub fn try_begin_admin(&self, ifindex: u32) -> Result<(), IfaceError> {
        self.with_mut(ifindex, |iface| {
            if iface.admin_busy {
                Err(IfaceError::Busy)
            } else {
                iface.admin_busy = true;
                Ok(())
            }
        })?
    }

    /// Release the administrative-transition guard.
    pub fn end_admin(&self, ifindex: u32) {
        let _ = self.with_mut(ifindex, |iface| iface.admin_busy = false);
    }

    /// Mark whether a DHCP client is running on this interface.
    pub fn set_dhcp_managed(&self, ifindex: u32, managed: bool) -> Result<(), IfaceError> {
        self.with_mut(ifindex, |iface| iface.dhcp_managed = managed)
    }

    /// Set the interface MTU.
    pub fn set_mtu(&self, ifindex: u32, mtu: u16) -> Result<(), IfaceError> {
        // Below the IPv4 minimum reassembly buffer there is nothing the stack
        // could legally send.
        if mtu < 68 {
            return Err(IfaceError::Invalid);
        }
        self.with_mut(ifindex, |iface| iface.mtu = mtu)
    }

    /// Assign an address, replacing any existing entry for the same
    /// address/prefix pair.
    pub fn add_addr(&self, ifindex: u32, new: IfaceAddr) -> Result<(), IfaceError> {
        if new.prefix_len > 32 || new.addr.is_unspecified() {
            return Err(IfaceError::Invalid);
        }
        self.with_mut(ifindex, |iface| {
            let n = iface.n_addrs as usize;
            if let Some(existing) = iface.addrs[..n]
                .iter_mut()
                .find(|a| a.addr == new.addr && a.prefix_len == new.prefix_len)
            {
                *existing = new;
                return Ok(());
            }
            if n >= NET_MAX_ADDRS_PER_IFACE {
                return Err(IfaceError::TooManyAddrs);
            }
            iface.addrs[n] = new;
            iface.n_addrs = (n + 1) as u8;
            Ok(())
        })?
    }

    /// Remove one address.
    pub fn del_addr(&self, ifindex: u32, addr: Ipv4Addr, prefix_len: u8) -> Result<(), IfaceError> {
        self.with_mut(ifindex, |iface| {
            let n = iface.n_addrs as usize;
            let pos = iface.addrs[..n]
                .iter()
                .position(|a| a.addr == addr && a.prefix_len == prefix_len)
                .ok_or(IfaceError::NotFound)?;
            for i in pos..n - 1 {
                iface.addrs[i] = iface.addrs[i + 1];
            }
            iface.n_addrs = (n - 1) as u8;
            Ok(())
        })?
    }

    /// Keep only the addresses `pred` accepts, returning how many went.
    ///
    /// This is how an administrative down drops a lease while keeping a static
    /// assignment: the operator's configuration is not the lease's to discard.
    pub fn retain_addrs(
        &self,
        ifindex: u32,
        mut pred: impl FnMut(&IfaceAddr) -> bool,
    ) -> Result<usize, IfaceError> {
        self.with_mut(ifindex, |iface| {
            let n = iface.n_addrs as usize;
            let mut kept = 0usize;
            for i in 0..n {
                let a = iface.addrs[i];
                if pred(&a) {
                    iface.addrs[kept] = a;
                    kept += 1;
                }
            }
            iface.n_addrs = kept as u8;
            n - kept
        })
    }

    /// Copy the addresses of one interface (or of every interface when
    /// `ifindex` is [`NET_IFINDEX_NONE`]), returning `(written, total)`.
    pub fn snapshot_addrs(&self, ifindex: u32, out: &mut [(u32, IfaceAddr)]) -> (usize, usize) {
        let table = self.inner.lock();
        let mut total = 0usize;
        let mut written = 0usize;
        for iface in table.slots.iter().flatten() {
            if ifindex != NET_IFINDEX_NONE && iface.ifindex != ifindex {
                continue;
            }
            for addr in iface.addrs() {
                total += 1;
                if written < out.len() {
                    out[written] = (iface.ifindex, *addr);
                    written += 1;
                }
            }
        }
        (written, total)
    }

    /// The primary address of `dev`, if it has one and is realised.
    pub fn our_ip(&self, dev: DevIndex) -> Option<Ipv4Addr> {
        let enabled = self.is_enabled();
        let table = self.inner.lock();
        let iface = table.slots.iter().flatten().find(|i| i.dev == dev)?;
        if !iface.is_realised(enabled) {
            return None;
        }
        iface.primary_addr().map(|a| a.addr)
    }

    /// Whether `ip` is an address of a realised interface — the RX path's "is
    /// this packet for us" test.
    pub fn is_our_addr(&self, ip: Ipv4Addr) -> bool {
        let enabled = self.is_enabled();
        let table = self.inner.lock();
        table
            .slots
            .iter()
            .flatten()
            .filter(|i| i.is_realised(enabled))
            .any(|i| i.addrs().iter().any(|a| a.addr == ip))
    }

    /// The first address of any realised non-loopback interface.
    ///
    /// Loopback is skipped because it registers first and would otherwise
    /// answer every "what is our address" question with `127.0.0.1`.
    pub fn first_ipv4(&self) -> Option<Ipv4Addr> {
        let enabled = self.is_enabled();
        let table = self.inner.lock();
        table
            .slots
            .iter()
            .flatten()
            .filter(|i| i.is_realised(enabled) && !i.kind.is_loopback())
            .find_map(|i| i.primary_addr().map(|a| a.addr))
            .or_else(|| {
                table
                    .slots
                    .iter()
                    .flatten()
                    .filter(|i| i.is_realised(enabled))
                    .find_map(|i| i.primary_addr().map(|a| a.addr))
            })
    }

    /// Empty the table and re-enable networking.
    ///
    /// Test-only. Production has no reason to empty this table, and doing so
    /// mid-boot would strand every route pointing at a device that no longer
    /// has a name.
    #[cfg(feature = "test-hooks")]
    pub fn clear(&self) {
        let mut table = self.inner.lock();
        for slot in table.slots.iter_mut() {
            *slot = None;
        }
        drop(table);
        self.enabled.store(true, Ordering::Release);
    }

    /// The interface a directly connected `ip` belongs to, if any.
    pub fn iface_for_local(&self, ip: Ipv4Addr) -> Option<Iface> {
        let enabled = self.is_enabled();
        let table = self.inner.lock();
        table
            .slots
            .iter()
            .flatten()
            .find(|i| i.is_realised(enabled) && i.addrs().iter().any(|a| a.is_local(ip)))
            .copied()
    }
}

/// Pick the next free name for `kind`: `lo` for loopback, `ethN` for the rest.
///
/// Names are reused deliberately, so this looks for the lowest unused suffix
/// rather than counting attachments.
fn allocate_name(table: &IfaceTableInner, kind: IfaceKind) -> Option<IfName> {
    if kind.is_loopback() {
        return IfName::new(b"lo");
    }
    let mut buf = [0u8; NET_IFNAMSIZ];
    for n in 0..NET_MAX_IFACES {
        buf[0] = b'e';
        buf[1] = b't';
        buf[2] = b'h';
        buf[3] = b'0' + (n as u8);
        let candidate = IfName::new(&buf[..4])?;
        if !table
            .slots
            .iter()
            .flatten()
            .any(|iface| iface.name == candidate)
        {
            return Some(candidate);
        }
    }
    None
}

// =============================================================================
// Kernel-table shorthands
// =============================================================================
//
// The protocol path asks "what is our address" far more often than it asks
// "which table", so these delegate to [`IFACE_TABLE`] and keep those call sites
// readable.
//
// They are also where a change to the kernel table becomes a monitor event.
// That placement is deliberate on two counts. The [`IfaceTable`] method still
// holds its lock while it computes what to return, and posting from inside it
// would give the table an out-edge into the event bus — the one thing
// [`crate::netmon`] is shaped to avoid. And a scratch table built by a test is
// not the system's state; only the kernel's table has any business narrating
// itself to a subscriber.

/// Describe an interface for a `NET_EV_IFACE_*` record.
///
/// `oper_old`/`oper_new` are the transition; every other field is read from the
/// row as it now stands, so a consumer can render the interface from the event
/// alone without a follow-up query.
fn iface_event_payload(iface: &Iface, oper_old: OperState, oper_new: OperState) -> [u8; 16] {
    let enabled = IFACE_TABLE.is_enabled();
    NetEvent::iface_payload(
        oper_old.to_abi(),
        oper_new.to_abi(),
        iface.carrier as u8,
        iface.admin_up as u8,
        iface.flags(enabled),
        iface.mtu as u32,
    )
}

fn post_iface_event(kind: u16, iface: &Iface, oper_old: OperState, oper_new: OperState) {
    netmon_post(
        kind,
        iface.ifindex,
        iface_event_payload(iface, oper_old, oper_new),
    );
}

/// Announce that an interface changed. Public to the crate because the control
/// plane posts the administrative transition itself — that event means "the
/// whole sequence finished", which only [`crate::iface_ctl`] knows.
pub(crate) fn post_iface_changed(iface: &Iface, oper_old: OperState, oper_new: OperState) {
    post_iface_event(NET_EV_IFACE_CHANGED, iface, oper_old, oper_new);
}

/// Announce that an interface's flag word moved without an operational
/// transition — an MTU or DHCP-management change, which a renderer still shows.
fn post_iface_attr_changed(ifindex: u32) {
    if let Some(iface) = IFACE_TABLE.get(ifindex) {
        let oper = iface.oper_state(IFACE_TABLE.is_enabled());
        post_iface_event(NET_EV_IFACE_CHANGED, &iface, oper, oper);
    }
}

fn post_addr_event(kind: u16, ifindex: u32, addr: &IfaceAddr) {
    netmon_post(
        kind,
        ifindex,
        NetEvent::addr_payload(
            addr.addr.0,
            addr.prefix_len,
            addr.origin as u8,
            addr.scope as u8,
        ),
    );
}

/// Whether networking is enabled system-wide.
#[inline]
pub fn is_enabled() -> bool {
    IFACE_TABLE.is_enabled()
}

/// Move the system master switch, reporting whether it changed.
#[inline]
pub fn set_enabled_flag(on: bool) -> bool {
    IFACE_TABLE.set_enabled_flag(on)
}

/// Register an interface in the kernel table. See [`IfaceTable::attach`].
///
/// The `NET_EV_IFACE_ADDED` record reports a transition *from*
/// [`OperState::NotPresent`], which is the RFC 2863 state of an interface that
/// did not exist — so a consumer folding operational transitions needs no
/// special case for the first event about an interface.
pub fn attach(
    dev: DevIndex,
    kind: IfaceKind,
    mac: MacAddr,
    mtu: u16,
    carrier: bool,
    carrier_detect: bool,
) -> Result<u32, IfaceError> {
    let ifindex = IFACE_TABLE.attach(dev, kind, mac, mtu, carrier, carrier_detect)?;
    if let Some(iface) = IFACE_TABLE.get(ifindex) {
        let oper = iface.oper_state(IFACE_TABLE.is_enabled());
        post_iface_event(NET_EV_IFACE_ADDED, &iface, OperState::NotPresent, oper);
    }
    Ok(ifindex)
}

/// Remove an interface from the kernel table.
///
/// The removal record describes the row as it last stood, because after this
/// returns there is nothing left to describe it with.
pub fn detach(dev: DevIndex) -> Option<Iface> {
    let iface = IFACE_TABLE.detach(dev)?;
    let oper = iface.oper_state(IFACE_TABLE.is_enabled());
    post_iface_event(NET_EV_IFACE_REMOVED, &iface, oper, OperState::NotPresent);
    Some(iface)
}

/// One kernel interface, by index.
#[inline]
pub fn get(ifindex: u32) -> Option<Iface> {
    IFACE_TABLE.get(ifindex)
}

/// One kernel interface, by device.
#[inline]
pub fn get_by_dev(dev: DevIndex) -> Option<Iface> {
    IFACE_TABLE.get_by_dev(dev)
}

/// One kernel interface, by name.
#[inline]
pub fn get_by_name(name: &[u8]) -> Option<Iface> {
    IFACE_TABLE.get_by_name(name)
}

/// Snapshot the kernel interface table.
#[inline]
pub fn snapshot(out: &mut [Iface]) -> (usize, usize) {
    IFACE_TABLE.snapshot(out)
}

/// Visit every kernel interface under the table lock.
#[inline]
pub fn for_each(f: impl FnMut(&Iface)) {
    IFACE_TABLE.for_each(f)
}

/// Snapshot the kernel table's addresses.
#[inline]
pub fn snapshot_addrs(ifindex: u32, out: &mut [(u32, IfaceAddr)]) -> (usize, usize) {
    IFACE_TABLE.snapshot_addrs(ifindex, out)
}

/// Number of kernel interfaces.
#[inline]
pub fn count() -> usize {
    IFACE_TABLE.count()
}

/// Assign an address on the kernel table.
pub fn add_addr(ifindex: u32, new: IfaceAddr) -> Result<(), IfaceError> {
    IFACE_TABLE.add_addr(ifindex, new)?;
    post_addr_event(NET_EV_ADDR_ADDED, ifindex, &new);
    Ok(())
}

/// Remove an address from the kernel table.
pub fn del_addr(ifindex: u32, addr: Ipv4Addr, prefix_len: u8) -> Result<(), IfaceError> {
    // Read the row before the removal: origin and scope live only there, and a
    // record that omitted them would make a consumer re-query for state that no
    // longer exists.
    let doomed = IFACE_TABLE.get(ifindex).and_then(|iface| {
        iface
            .addrs()
            .iter()
            .find(|a| a.addr == addr && a.prefix_len == prefix_len)
            .copied()
    });
    IFACE_TABLE.del_addr(ifindex, addr, prefix_len)?;
    if let Some(gone) = doomed {
        post_addr_event(NET_EV_ADDR_REMOVED, ifindex, &gone);
    }
    Ok(())
}

/// Keep only the kernel-table addresses `pred` accepts.
///
/// What went is established by comparing the row before and after rather than
/// by instrumenting `pred`: the predicate belongs to the caller, and the table
/// is the only authority on what it actually kept.
pub fn retain_addrs(
    ifindex: u32,
    pred: impl FnMut(&IfaceAddr) -> bool,
) -> Result<usize, IfaceError> {
    let before = IFACE_TABLE.get(ifindex);
    let dropped = IFACE_TABLE.retain_addrs(ifindex, pred)?;
    if dropped > 0 {
        if let Some(before) = before {
            let after = IFACE_TABLE.get(ifindex);
            for addr in before.addrs() {
                let kept = after.as_ref().is_some_and(|iface| {
                    iface
                        .addrs()
                        .iter()
                        .any(|a| a.addr == addr.addr && a.prefix_len == addr.prefix_len)
                });
                if !kept {
                    post_addr_event(NET_EV_ADDR_REMOVED, ifindex, addr);
                }
            }
        }
    }
    Ok(dropped)
}

/// Record a carrier transition on the kernel table.
///
/// [`IfaceTable::set_carrier`] reports only a real transition, so the edge
/// detection a poller would otherwise have to keep lives here: calling this
/// every tick with an unchanged link posts nothing.
pub fn set_carrier(dev: DevIndex, up: bool) -> Option<(u32, OperState, OperState)> {
    let (ifindex, before, after) = IFACE_TABLE.set_carrier(dev, up)?;
    if let Some(iface) = IFACE_TABLE.get(ifindex) {
        post_iface_changed(&iface, before, after);
    }
    // A lease outlives a cable, but the client still has to know: it stops its
    // timers while the link is down and confirms the address it holds when the
    // link returns, rather than letting a renewal time out into a teardown.
    crate::dhcp::on_carrier(dev, up);
    Some((ifindex, before, after))
}

/// Set administrative intent on the kernel table.
#[inline]
pub fn set_admin_intent(ifindex: u32, up: bool) -> Result<(OperState, OperState), IfaceError> {
    IFACE_TABLE.set_admin_intent(ifindex, up)
}

/// Claim the kernel table's administrative guard.
#[inline]
pub fn try_begin_admin(ifindex: u32) -> Result<(), IfaceError> {
    IFACE_TABLE.try_begin_admin(ifindex)
}

/// Release the kernel table's administrative guard.
#[inline]
pub fn end_admin(ifindex: u32) {
    IFACE_TABLE.end_admin(ifindex)
}

/// Mark DHCP management on the kernel table.
///
/// Reported because it moves `IFF_SLOP_DHCP`, which is how a UI tells an
/// address a client is maintaining from one somebody typed in.
pub fn set_dhcp_managed(ifindex: u32, managed: bool) -> Result<(), IfaceError> {
    IFACE_TABLE.set_dhcp_managed(ifindex, managed)?;
    post_iface_attr_changed(ifindex);
    Ok(())
}

/// Set an MTU on the kernel table.
pub fn set_mtu(ifindex: u32, mtu: u16) -> Result<(), IfaceError> {
    IFACE_TABLE.set_mtu(ifindex, mtu)?;
    post_iface_attr_changed(ifindex);
    Ok(())
}

/// The primary address of a kernel device.
#[inline]
pub fn our_ip(dev: DevIndex) -> Option<Ipv4Addr> {
    IFACE_TABLE.our_ip(dev)
}

/// Whether `ip` belongs to a realised kernel interface.
#[inline]
pub fn is_our_addr(ip: Ipv4Addr) -> bool {
    IFACE_TABLE.is_our_addr(ip)
}

/// The first address of a realised, non-loopback kernel interface.
#[inline]
pub fn first_ipv4() -> Option<Ipv4Addr> {
    IFACE_TABLE.first_ipv4()
}

/// The kernel interface a directly connected `ip` belongs to.
#[inline]
pub fn iface_for_local(ip: Ipv4Addr) -> Option<Iface> {
    IFACE_TABLE.iface_for_local(ip)
}

/// Pick the source address for a packet destined to `dst`.
///
/// Route first, per RFC 1122 §3.3.4.2: the address to use is the one on the
/// interface the route chose. The fallback is [`first_ipv4`], which skips
/// loopback — loopback registers before the NIC, so taking simply the first
/// address we hold would put `127.0.0.1` in outbound SYNs.
pub fn source_ip_for(dst: Ipv4Addr) -> Option<Ipv4Addr> {
    if let Some((dev, _next_hop)) = crate::route::ROUTE_TABLE.lookup(dst)
        && let Some(ip) = our_ip(dev)
        && !ip.is_unspecified()
    {
        return Some(ip);
    }
    first_ipv4()
}
