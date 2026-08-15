//! Network device abstraction: `NetDevice` trait, device registry, and stable device handles.
//!
//! This module establishes the boundary between network drivers (which move bytes)
//! and the protocol stack (which understands protocols).  Only [`PacketBuf`] crosses
//! this boundary.
//!
//! # Architecture
//!
//! - **[`NetDevice`] trait**: Implemented by every network driver (VirtIO-net, loopback, etc.)
//! - **[`NetDeviceRegistry`]**: `SpinLock`-protected storage, accessed only on the control plane
//! - **[`DeviceHandle`]**: Stable reference for data-plane TX/RX without the registry lock
//!
//! # Concurrency model
//!
//! The registry lock serializes registration/unregistration/enumeration.  The data
//! plane bypasses the registry entirely via [`DeviceHandle`]:
//!
//! - `tx()` acquires a per-device lock (serializes concurrent senders).
//! - `poll_rx()` requires no lock (single consumer: NAPI loop).
//!
//! All trait methods take `&self`; implementations use interior mutability
//! (e.g., `SpinLock`) for their internal state.  This allows concurrent TX and
//! RX without aliasing `&mut` references through the raw pointer in `DeviceHandle`.

use core::fmt;
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LockClassKey;

use bitflags::bitflags;
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use super::iface::IfaceKind;
use super::packetbuf::PacketBuf;
use super::pool::PacketPool;
use super::types::{DevIndex, MacAddr, NetError};

/// Hardware TX checksum-offload descriptor for a zero-copy send. The device
/// computes the one's-complement checksum of the frame from `csum_start` to the
/// end and stores it at `csum_start + csum_offset` (the virtio `NEEDS_CSUM`
/// model). Offsets are relative to the start of the L2 frame (i.e. *after* any
/// driver-private header such as the `virtio_net_hdr`). For UDP over IPv4:
/// `csum_start = 14 + 20 = 34`, `csum_offset = 6`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsumOffload {
    pub csum_start: u16,
    pub csum_offset: u16,
}

// =============================================================================
// 1C.1 — NetDevice trait
// =============================================================================

/// Abstraction for a network device (NIC, loopback, etc.).
///
/// All methods take `&self`; implementations use interior mutability (e.g.,
/// `SpinLock`) for their internal state.  This design choice avoids the need
/// for `&mut` through raw pointers in [`DeviceHandle`], eliminating a class
/// of aliasing UB.
///
/// # Concurrency
///
/// - `tx()`: May be called from multiple socket contexts concurrently.
///   The [`DeviceHandle`] serializes TX via a per-device lock.
/// - `poll_rx()`: Single consumer only (the NAPI loop).  No external lock needed.
/// - `set_up()`/`set_down()`: Control plane only, called *outside* the registry
///   lock — a driver registers itself while holding its own state lock, so the
///   registry must not call into a device while holding its.
/// - `mtu()`, `mac()`, `stats()`, `features()`: Read-only, safe from any context.
pub trait NetDevice: Send + Sync {
    /// Transmit one packet.  The packet is consumed (moved into the driver's TX ring).
    ///
    /// Returns `Err(NoBufferSpace)` if the TX ring is full.
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError>;

    /// Transmit one packet **zero-copy**: the device DMAs the payload straight
    /// from the pinned user pages (`runs` = coalesced `(paddr, len)` physical
    /// runs), prepended by the kernel-built L2/L3/L4 headers in `net_hdr`. The
    /// driver keeps `keepalive` (independent owning refs on the pinned pages)
    /// and `token` until the NIC reclaims the descriptor — then it drops the
    /// keepalive and signals the token so the ring can post `SLOPRING_CQE_F_NOTIF`.
    ///
    /// `csum` requests hardware checksum offload (UDP); `None` means the L4
    /// checksum is already complete in `net_hdr` (ICMP). The default impl
    /// rejects zero-copy (loopback and devices without DMA SG) so only the
    /// NIC driver needs to implement it.
    fn tx_zerocopy(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: TxReclaimToken,
    ) -> Result<(), NetError> {
        let _ = (net_hdr, runs, csum, keepalive, token);
        Err(NetError::OperationNotSupported)
    }

    /// Like [`tx_zerocopy`](Self::tx_zerocopy) but for a send whose pinned pages
    /// may be DMA'd **more than once** before they are reusable — the TCP
    /// `MSG_ZEROCOPY` case, where a segment can be retransmitted. The driver
    /// holds the refcounted [`ZcNotifToken`] (acquiring a reference per accepted
    /// descriptor, releasing it on reclaim) instead of flipping a single-shot
    /// [`TxReclaimToken`]; the send queue retires the chunk's own reference on
    /// cumulative ACK, and the ring posts `SLOPRING_CQE_F_NOTIF` only when the
    /// count reaches zero. Default rejects (loopback / no DMA SG).
    fn tx_zerocopy_notif(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: ZcNotifToken,
    ) -> Result<(), NetError> {
        let _ = (net_hdr, runs, csum, keepalive, token);
        Err(NetError::OperationNotSupported)
    }

    /// Reclaim completed TX descriptors from the device's used ring (TX only —
    /// distinct from `poll_rx`, which is NAPI-single-consumer). Safe to call
    /// from any context; used by the SlopRing harvest to flip the zero-copy
    /// reclaim token (post `SLOPRING_CQE_F_NOTIF`) without depending on a TX
    /// completion interrupt firing while the waiter is parked. Default no-op
    /// (loopback / devices with no deferred-reclaim notion).
    fn poll_tx(&self) {}

    /// Drain up to `budget` received packets from the RX ring, allocating
    /// [`PacketBuf`] from `pool`.
    ///
    /// Returns the received packets.  An empty `Vec` means no packets are pending.
    /// Implementations should use `Vec::with_capacity(budget.min(reasonable_max))`
    /// to minimize reallocation.
    fn poll_rx(&self, budget: usize, pool: &'static PacketPool) -> KVec<PacketBuf>;

    /// Bring the link up (enable RX/TX rings, start interrupt delivery).
    fn set_up(&self);

    /// Bring the link down (drain queues, disable interrupt delivery).
    ///
    /// After this returns, `tx`/`tx_zerocopy*` must fail and `poll_rx` must
    /// yield nothing. The device enforces that under its own state lock, which
    /// is what orders a send that resolved the device just before retirement
    /// against the retirement itself — the registry has already stopped
    /// resolving this device by the time it calls here, so nothing else can.
    ///
    /// A [`DeviceHandle`] outlives unregistration and keeps addressing a live
    /// allocation, so this is also what stops a retained handle from driving a
    /// retired device.
    fn set_down(&self);

    /// Maximum transmission unit (payload bytes, excluding Ethernet header).
    fn mtu(&self) -> u16;

    /// Hardware MAC address.
    fn mac(&self) -> MacAddr;

    /// Read-only snapshot of device statistics.
    fn stats(&self) -> NetDeviceStats;

    /// Capability/feature flags advertised by the driver.
    fn features(&self) -> NetDeviceFeatures;

    /// What kind of interface this device presents. Defaults to Ethernet,
    /// which is what every real NIC is; loopback overrides it.
    fn kind(&self) -> IfaceKind {
        IfaceKind::Ethernet
    }

    /// Current link state.
    ///
    /// **Must be a lock-free read** — an atomic the driver refreshes, never a
    /// query that takes the driver's state lock. The registry calls this while
    /// enumerating and the interface layer calls it from contexts that hold
    /// their own locks; a driver that reached for a lock here would create
    /// exactly the registry-to-device edge the two-phase retirement exists to
    /// avoid.
    ///
    /// The default is `true`, paired with a `carrier_detect` of `false`: a
    /// device that cannot observe its link says so rather than claiming a
    /// state it does not know.
    fn carrier(&self) -> bool {
        true
    }

    /// Whether [`carrier`](Self::carrier) is an observation rather than an
    /// assumption. Surfaced to userland as `IFF_SLOP_CARRIER_ASSUMED` so a UI
    /// can be honest about not knowing.
    fn carrier_detect(&self) -> bool {
        false
    }
}

// =============================================================================
// 1C.2 — NetDeviceStats
// =============================================================================

/// Read-only snapshot of network device statistics.
///
/// Counters are monotonically increasing.  The driver increments
/// `rx_packets`/`tx_packets`/`rx_bytes`/`tx_bytes` on the data path;
/// the stack increments `rx_dropped` on demux failures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetDeviceStats {
    /// Total packets received successfully.
    pub rx_packets: u64,
    /// Total packets transmitted successfully.
    pub tx_packets: u64,
    /// Total bytes received (payload only, excluding driver framing).
    pub rx_bytes: u64,
    /// Total bytes transmitted (payload only).
    pub tx_bytes: u64,
    /// RX errors (CRC, length, etc.) detected by the driver.
    pub rx_errors: u64,
    /// TX errors (queue full, DMA failure, etc.) detected by the driver.
    pub tx_errors: u64,
    /// Packets dropped on RX (no buffer, demux miss, etc.).
    pub rx_dropped: u64,
    /// Packets dropped on TX (ring full after retry, etc.).
    pub tx_dropped: u64,
}

impl NetDeviceStats {
    /// Create a zeroed stats snapshot.
    pub const fn new() -> Self {
        Self {
            rx_packets: 0,
            tx_packets: 0,
            rx_bytes: 0,
            tx_bytes: 0,
            rx_errors: 0,
            tx_errors: 0,
            rx_dropped: 0,
            tx_dropped: 0,
        }
    }

    /// Total packets (rx + tx).
    #[inline]
    pub const fn total_packets(&self) -> u64 {
        self.rx_packets + self.tx_packets
    }

    /// Total bytes (rx + tx).
    #[inline]
    pub const fn total_bytes(&self) -> u64 {
        self.rx_bytes + self.tx_bytes
    }

    /// Total errors (rx + tx).
    #[inline]
    pub const fn total_errors(&self) -> u64 {
        self.rx_errors + self.tx_errors
    }

    /// Total dropped (rx + tx).
    #[inline]
    pub const fn total_dropped(&self) -> u64 {
        self.rx_dropped + self.tx_dropped
    }
}

impl fmt::Display for NetDeviceStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rx: {} pkts/{} bytes, tx: {} pkts/{} bytes, err: {}/{}, drop: {}/{}",
            self.rx_packets,
            self.rx_bytes,
            self.tx_packets,
            self.tx_bytes,
            self.rx_errors,
            self.tx_errors,
            self.rx_dropped,
            self.tx_dropped
        )
    }
}

// =============================================================================
// 1C.3 — NetDeviceFeatures
// =============================================================================

bitflags! {
    /// Capability flags advertised by a network device.
    ///
    /// Drivers set these based on hardware capabilities during initialization.
    /// The stack queries them to decide whether to offload work (e.g., skip
    /// software checksum if `CHECKSUM_TX` is set).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct NetDeviceFeatures: u32 {
        /// Driver can compute TX checksums in hardware/firmware.
        const CHECKSUM_TX = 1 << 0;
        /// Driver has verified RX checksums; stack can skip verification.
        const CHECKSUM_RX = 1 << 1;
        /// TCP segmentation offload (reserved — not implemented).
        const TSO         = 1 << 2;
        /// Driver strips/inserts VLAN tags (reserved — not implemented).
        const VLAN_TAG    = 1 << 3;
    }
}

impl Default for NetDeviceFeatures {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Display for NetDeviceFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "(none)");
        }
        let mut first = true;
        for (name, _) in self.iter_names() {
            if !first {
                write!(f, " | ")?;
            }
            write!(f, "{name}")?;
            first = false;
        }
        Ok(())
    }
}

// =============================================================================
// 1C.4 — DeviceHandle
// =============================================================================

/// Owning reference to a registered network device for data-plane operations.
///
/// Obtained once via [`NetDeviceRegistry::register`] and held for as long as
/// the holder needs the device.  Bypasses the registry lock entirely:
///
/// - `tx()` acquires only the per-device TX lock (serializes concurrent senders).
/// - `poll_rx()` requires no lock (single consumer: NAPI loop).
/// - `mac()`, `mtu()`, `stats()`, `features()` are read-only and lock-free.
///
/// The handle holds its own `KArc`, so [`NetDeviceRegistry::unregister`]
/// drops only the registry's reference: a device stays alive for as long as
/// any handle to it does, and the data plane cannot be left addressing freed
/// memory by an unregistration it did not observe.
pub struct DeviceHandle {
    /// The device itself. Shared ownership with the registry slot.
    dev: KArc<dyn NetDevice + Send + Sync>,
    /// Device index for identification and registry lookups.
    index: DevIndex,
    /// Per-device TX serialization.  Multiple sockets may transmit to the same
    /// device concurrently; this lock serializes their `tx()` calls without
    /// touching the global registry lock.
    tx_lock: SpinLock<()>,
}

impl DeviceHandle {
    /// Transmit a packet through this device.
    ///
    /// Acquires the per-device TX lock (**not** the registry lock).  Multiple
    /// callers (socket TX paths) are serialized by this lock.
    pub fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let _guard = self.tx_lock.lock();
        self.dev.tx(pkt)
    }

    /// Poll for received packets.
    ///
    /// **Must be called from the NAPI loop only** (single consumer).
    /// Does not acquire any lock — the NAPI loop is the sole consumer of the
    /// RX ring for a given device.
    pub fn poll_rx(&self, budget: usize, pool: &'static PacketPool) -> KVec<PacketBuf> {
        self.dev.poll_rx(budget, pool)
    }

    /// Device index.
    #[inline]
    pub fn index(&self) -> DevIndex {
        self.index
    }

    /// Read the device's MAC address (lock-free).
    pub fn mac(&self) -> MacAddr {
        self.dev.mac()
    }

    /// Read the device's MTU (lock-free).
    pub fn mtu(&self) -> u16 {
        self.dev.mtu()
    }

    /// Read a snapshot of device statistics (lock-free).
    pub fn stats(&self) -> NetDeviceStats {
        self.dev.stats()
    }

    /// Read device feature flags (lock-free).
    pub fn features(&self) -> NetDeviceFeatures {
        self.dev.features()
    }
}

impl fmt::Debug for DeviceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceHandle({})", self.index)
    }
}

// =============================================================================
// 1C.4 — NetDeviceRegistry
// =============================================================================

/// Maximum number of simultaneously registered network devices.
const MAX_DEVICES: usize = 8;

/// Control-plane storage for registered network devices.
///
/// The registry lock is taken **only** for registration, unregistration, and
/// enumeration — never on the data path.  Data-plane access goes through
/// [`DeviceHandle`], which stores a stable raw pointer to the device's heap
/// allocation.
///
/// # Invariants
///
/// - Each registered device occupies exactly one slot in the fixed-size array.
/// - The device outlives every [`DeviceHandle`] to it: registry and handles
///   share ownership through `KArc`, so `unregister` frees the device only
///   once the last handle is gone.
/// - A retiring slot stays occupied until `set_down` returns, so its index
///   cannot be reissued to a different device while the old one is still
///   shutting down. The registry stops resolving it the moment retirement
///   begins, so no new call reaches a device that is going away.
pub struct NetDeviceRegistry {
    pub(crate) inner: SpinLock<RegistryInner>,
}

/// One registry slot.
pub(crate) struct DeviceSlot {
    dev: KArc<dyn NetDevice + Send + Sync>,
    /// Set for the window between `unregister` deciding to remove this device
    /// and `set_down` returning. The slot is neither resolvable nor free.
    retiring: bool,
}

/// Inner state behind the registry's `SpinLock`.
pub(crate) struct RegistryInner {
    /// Device slots.  `None` = empty slot.
    slots: [Option<DeviceSlot>; MAX_DEVICES],
    /// Number of occupied slots.
    count: usize,
}

// SAFETY: All access is serialized through the `SpinLock`.

/// The global network device registry.
///
/// Drivers call [`register`](NetDeviceRegistry::register) during probe to add
/// themselves, and receive a [`DeviceHandle`] for data-plane operations.
pub static DEVICE_REGISTRY: NetDeviceRegistry =
    NetDeviceRegistry::new(lock_class!("DEVICE_REGISTRY", LOCK_LEVEL_REGISTRY));

impl NetDeviceRegistry {
    /// Create an empty registry.
    ///
    /// No heap allocation occurs until the first [`register`](Self::register) call.
    /// The class comes from the caller so a scratch registry built by a
    /// test is a different lockdep class from the global one — the two are
    /// genuinely different locks, and a test that deliberately inverts its
    /// own order must not teach that order about the production registry.
    pub const fn new(class: &'static LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                RegistryInner {
                    slots: [const { None }; MAX_DEVICES],
                    count: 0,
                },
                class,
            ),
        }
    }

    /// Register a network device and obtain a stable [`DeviceHandle`].
    ///
    /// Assigns the next available [`DevIndex`] and returns a handle that
    /// bypasses the registry lock for data-plane operations.
    ///
    /// Returns `None` if all `MAX_DEVICES` slots are occupied.
    pub fn register(&self, dev: KArc<dyn NetDevice + Send + Sync>) -> Option<DeviceHandle> {
        let mut inner = self.inner.lock();
        for (i, slot) in inner.slots.iter_mut().enumerate() {
            // A retiring slot is still `Some`, so it cannot be selected here
            // while its previous device is shutting down.
            if slot.is_none() {
                let handle = DeviceHandle {
                    dev: KArc::clone(&dev),
                    index: DevIndex(i),
                    tx_lock: SpinLock::new(
                        (),
                        lock_class!("DeviceHandle.tx_lock", LOCK_LEVEL_RESOURCE),
                    ),
                };
                *slot = Some(DeviceSlot {
                    dev,
                    retiring: false,
                });
                inner.count += 1;
                return Some(handle);
            }
        }
        None
    }

    /// Unregister a network device.
    ///
    /// Retirement runs in two phases so the index is never reissued while the
    /// old device is still shutting down: the slot is marked retiring under
    /// the lock, [`set_down()`](NetDevice::set_down) runs outside it, and only
    /// then is the slot freed. Calling out with the registry lock held would
    /// close a registry/device-state cycle, because a driver registers itself
    /// while holding the same state lock its methods take.
    ///
    /// Outstanding [`DeviceHandle`]s keep the device alive; they observe a
    /// downed device rather than freed memory.
    ///
    /// Returns `true` if a device was found and retired, `false` if the slot
    /// was already empty or another caller is already retiring it.
    pub fn unregister(&self, index: DevIndex) -> bool {
        let idx = index.0;
        if idx >= MAX_DEVICES {
            return false;
        }
        let dev = {
            let mut inner = self.inner.lock();
            let Some(slot) = inner.slots[idx].as_mut() else {
                return false;
            };
            if slot.retiring {
                return false;
            }
            slot.retiring = true;
            let dev = KArc::clone(&slot.dev);
            inner.count -= 1;
            dev
        };

        dev.set_down();

        // Freeing the slot last is what makes the index safe to reissue: no
        // resolve has handed this device out since the retiring mark, and it
        // is now down.
        self.inner.lock().slots[idx] = None;
        true
    }

    /// Clone the device at `index` out of the registry and release the lock.
    ///
    /// Every call *into* a device must go through this, for the lock-order
    /// reason [`Self::unregister`] gives. A retiring slot resolves to `None`,
    /// so a device that is going away receives no new work.
    ///
    /// Public because the interface control plane calls `set_up`/`set_down`
    /// during an administrative transition, and must do so with the registry
    /// lock released.
    pub fn device_at(&self, index: DevIndex) -> Option<KArc<dyn NetDevice + Send + Sync>> {
        let inner = self.inner.lock();
        let slot = inner.slots.get(index.0)?.as_ref()?;
        (!slot.retiring).then(|| KArc::clone(&slot.dev))
    }

    /// Snapshot every resolvable device, releasing the lock before the caller
    /// touches any of them. Same rationale as [`Self::device_at`].
    ///
    /// Fills a caller-provided array rather than allocating: this runs on the
    /// TX-completion polling path, where an allocation failure would silently
    /// drop a device and stall its reclaim with nothing to report.
    fn snapshot_devices(&self, out: &mut [Option<KArc<dyn NetDevice + Send + Sync>>; MAX_DEVICES]) {
        let inner = self.inner.lock();
        for (dst, slot) in out.iter_mut().zip(inner.slots.iter()) {
            *dst = slot
                .as_ref()
                .filter(|s| !s.retiring)
                .map(|s| KArc::clone(&s.dev));
        }
    }

    /// Enumerate all registered devices.
    ///
    /// Returns a list of `(DevIndex, MacAddr, carrier)` tuples. `carrier` is
    /// the device's real link state, read after `snapshot_devices` has released
    /// the registry lock — which is safe precisely because
    /// [`NetDevice::carrier`] is required to be a lock-free read.
    pub fn enumerate(&self) -> KVec<(DevIndex, MacAddr, bool)> {
        let mut devices = [const { None }; MAX_DEVICES];
        self.snapshot_devices(&mut devices);
        // Reserved up front so no device is lost to a mid-loop allocation
        // failure; a failed reserve yields an empty list, which is the
        // existing total-failure answer.
        let Ok(mut result) = KVec::with_capacity(MAX_DEVICES) else {
            return KVec::new();
        };
        for (i, dev) in devices.iter().enumerate() {
            if let Some(dev) = dev {
                let _ = result.push((DevIndex(i), dev.mac(), dev.carrier()));
            }
        }
        result
    }

    /// Number of currently registered devices.
    #[inline]
    pub fn device_count(&self) -> usize {
        self.inner.lock().count
    }

    /// Transmit a packet through a device identified by index.
    ///
    /// Resolves the device under the registry lock and releases it before
    /// transmitting, so no registry/device-state edge is created. The
    /// device's `tx()` takes `&self` with interior mutability, so concurrent
    /// TX calls are serialised by the device's own lock.
    ///
    /// For hot-path TX where a [`DeviceHandle`] is already available,
    /// prefer [`DeviceHandle::tx`] which bypasses the registry lock.
    pub fn tx_by_index(&self, index: DevIndex, pkt: PacketBuf) -> Result<(), NetError> {
        match self.device_at(index) {
            Some(dev) => dev.tx(pkt),
            None => Err(NetError::NetworkUnreachable),
        }
    }

    /// Zero-copy transmit through a device identified by index (see
    /// [`NetDevice::tx_zerocopy`]). Mirrors [`tx_by_index`](Self::tx_by_index)'s
    /// resolve-then-release shape, so the SlopRing `OP_SEND_ZC` path keeps
    /// the exact lock ordering of the copy path and adds no cross-lock edge.
    pub fn tx_zerocopy_by_index(
        &self,
        index: DevIndex,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: TxReclaimToken,
    ) -> Result<(), NetError> {
        match self.device_at(index) {
            Some(dev) => dev.tx_zerocopy(net_hdr, runs, csum, keepalive, token),
            None => Err(NetError::NetworkUnreachable),
        }
    }

    /// Refcount-token zero-copy transmit by index (see
    /// [`NetDevice::tx_zerocopy_notif`]) — the TCP `MSG_ZEROCOPY` retransmit-safe
    /// path. Same registry-lock shape as [`tx_zerocopy_by_index`](Self::tx_zerocopy_by_index).
    pub fn tx_zerocopy_notif_by_index(
        &self,
        index: DevIndex,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: ZcNotifToken,
    ) -> Result<(), NetError> {
        match self.device_at(index) {
            Some(dev) => dev.tx_zerocopy_notif(net_hdr, runs, csum, keepalive, token),
            None => Err(NetError::NetworkUnreachable),
        }
    }

    /// Reclaim completed TX descriptors on every registered device (see
    /// [`NetDevice::poll_tx`]). The SlopRing harvest calls this when it has
    /// in-flight zero-copy sends so the deferred `SLOPRING_CQE_F_NOTIF` makes
    /// progress without relying on a TX-completion interrupt — the waiter drives
    /// its own reclaim (caller-as-waiter). The registry lock is released before
    /// any device's `poll_tx` runs.
    pub fn poll_tx_all(&self) {
        let mut devices = [const { None }; MAX_DEVICES];
        self.snapshot_devices(&mut devices);
        for dev in devices.iter().flatten() {
            dev.poll_tx();
        }
    }

    /// Read the MAC address of a device by index.
    ///
    /// Returns `None` if the device is not registered.
    pub fn mac_by_index(&self, index: DevIndex) -> Option<MacAddr> {
        Some(self.device_at(index)?.mac())
    }

    /// Read the feature flags of a device by index.
    ///
    /// Returns `None` if the device is not registered.
    pub fn features_by_index(&self, index: DevIndex) -> Option<NetDeviceFeatures> {
        Some(self.device_at(index)?.features())
    }

    /// Read a device's counters by index.
    ///
    /// `device_at` resolves and releases the registry lock before the driver is
    /// touched, so this never calls into a device while holding it.
    pub fn stats_by_index(&self, index: DevIndex) -> Option<NetDeviceStats> {
        Some(self.device_at(index)?.stats())
    }

    /// Read a device's link state by index. Lock-free on the driver side, by
    /// the [`NetDevice::carrier`] contract.
    pub fn carrier_by_index(&self, index: DevIndex) -> Option<bool> {
        Some(self.device_at(index)?.carrier())
    }

    /// Poll RX packets from a device by index.
    ///
    /// Resolves under the registry lock and releases it before polling.
    /// Returns an empty Vec if the device is not registered.
    pub fn poll_rx_by_index(
        &self,
        index: DevIndex,
        budget: usize,
        pool: &'static super::pool::PacketPool,
    ) -> KVec<PacketBuf> {
        match self.device_at(index) {
            Some(dev) => dev.poll_rx(budget, pool),
            None => KVec::new(),
        }
    }
}
