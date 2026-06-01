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

use bitflags::bitflags;
use slopos_ostd::TxReclaimToken;
use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, KVec};

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
/// - `set_up()`/`set_down()`: Control plane only, called under the registry lock.
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
        keepalive: KVec<UFrame<AnonymousMeta>>,
        token: TxReclaimToken,
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
    /// Must be called before unregistration.  After this returns, the driver
    /// must not access any shared resources (DMA rings, interrupt vectors).
    fn set_down(&self);

    /// Maximum transmission unit (payload bytes, excluding Ethernet header).
    fn mtu(&self) -> u16;

    /// Hardware MAC address.
    fn mac(&self) -> MacAddr;

    /// Read-only snapshot of device statistics.
    fn stats(&self) -> NetDeviceStats;

    /// Capability/feature flags advertised by the driver.
    fn features(&self) -> NetDeviceFeatures;
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

/// Stable reference to a registered network device for data-plane operations.
///
/// Obtained once via [`NetDeviceRegistry::register`] and held for the device's
/// lifetime.  Bypasses the registry lock entirely:
///
/// - `tx()` acquires only the per-device TX lock (serializes concurrent senders).
/// - `poll_rx()` requires no lock (single consumer: NAPI loop).
/// - `mac()`, `mtu()`, `stats()`, `features()` are read-only and lock-free.
///
/// # Safety
///
/// The raw `dev` pointer is valid as long as the device remains registered.
/// Calling [`NetDeviceRegistry::unregister`] on a device whose handle is still
/// in use is **undefined behavior**.  The caller must ensure that NAPI polling
/// and all socket TX paths are drained before unregistration.
pub struct DeviceHandle {
    /// Stable pointer to the device, valid for its registered lifetime.
    /// The pointed-to allocation is owned by the registry's `Box<dyn NetDevice>`.
    dev: *const (dyn NetDevice + Send + Sync),
    /// Device index for identification and registry lookups.
    index: DevIndex,
    /// Per-device TX serialization.  Multiple sockets may transmit to the same
    /// device concurrently; this lock serializes their `tx()` calls without
    /// touching the global registry lock.
    tx_lock: SpinLock<()>,
}

// SAFETY: DeviceHandle is designed for cross-thread use.  The raw pointer
// targets a `dyn NetDevice + Send + Sync` allocation whose lifetime is
// managed by the registry.  TX is serialized by `tx_lock`; RX is
// single-consumer (NAPI loop).  Read-only accessors (mac, mtu, stats,
// features) are inherently safe via the `Sync` bound on `NetDevice`.

impl DeviceHandle {
    /// Transmit a packet through this device.
    ///
    /// Acquires the per-device TX lock (**not** the registry lock).  Multiple
    /// callers (socket TX paths) are serialized by this lock.
    pub fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let _guard = self.tx_lock.lock();
        // Caller invariant on `self.dev`: published by the registry,
        // valid for the device's registered lifetime; the trait
        // method takes `&self`, so no mutable aliasing.
        let dev = slopos_ostd::dev::borrow_dyn(self.dev);
        dev.tx(pkt)
    }

    /// Poll for received packets.
    ///
    /// **Must be called from the NAPI loop only** (single consumer).
    /// Does not acquire any lock — the NAPI loop is the sole consumer of the
    /// RX ring for a given device.
    pub fn poll_rx(&self, budget: usize, pool: &'static PacketPool) -> KVec<PacketBuf> {
        // Same lifetime/aliasing argument as `tx`; this is the single
        // RX consumer.
        let dev = slopos_ostd::dev::borrow_dyn(self.dev);
        dev.poll_rx(budget, pool)
    }

    /// Device index.
    #[inline]
    pub fn index(&self) -> DevIndex {
        self.index
    }

    /// Read the device's MAC address (lock-free).
    pub fn mac(&self) -> MacAddr {
        // Pointer valid for device lifetime; mac() is read-only.
        slopos_ostd::dev::borrow_dyn(self.dev).mac()
    }

    /// Read the device's MTU (lock-free).
    pub fn mtu(&self) -> u16 {
        // Pointer valid for device lifetime; mtu() is read-only.
        slopos_ostd::dev::borrow_dyn(self.dev).mtu()
    }

    /// Read a snapshot of device statistics (lock-free).
    pub fn stats(&self) -> NetDeviceStats {
        // Pointer valid for device lifetime; stats() is read-only.
        slopos_ostd::dev::borrow_dyn(self.dev).stats()
    }

    /// Read device feature flags (lock-free).
    pub fn features(&self) -> NetDeviceFeatures {
        // Pointer valid for device lifetime; features() is read-only.
        slopos_ostd::dev::borrow_dyn(self.dev).features()
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
/// - The `Box<dyn NetDevice>` heap allocation is stable: moving the `Box`
///   (pointer-sized value) does not move the pointee.
/// - [`DeviceHandle`] raw pointers remain valid until [`unregister`](Self::unregister)
///   drops the corresponding `Box`.
/// - The caller must drain all data-plane activity (NAPI, socket TX) before
///   calling `unregister`.
pub struct NetDeviceRegistry {
    pub(crate) inner: SpinLock<RegistryInner>,
}

/// Inner state behind the registry's `SpinLock`.
pub(crate) struct RegistryInner {
    /// Device slots.  `None` = empty slot.
    slots: [Option<KBox<dyn NetDevice + Send + Sync>>; MAX_DEVICES],
    /// Number of occupied slots.
    count: usize,
}

// SAFETY: All access is serialized through the `SpinLock`.

/// The global network device registry.
///
/// Drivers call [`register`](NetDeviceRegistry::register) during probe to add
/// themselves, and receive a [`DeviceHandle`] for data-plane operations.
pub static DEVICE_REGISTRY: NetDeviceRegistry = NetDeviceRegistry::new();

impl NetDeviceRegistry {
    /// Create an empty registry.
    ///
    /// No heap allocation occurs until the first [`register`](Self::register) call.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(
                RegistryInner {
                    slots: [const { None }; MAX_DEVICES],
                    count: 0,
                },
                LOCK_LEVEL_REGISTRY,
            ),
        }
    }

    /// Register a network device and obtain a stable [`DeviceHandle`].
    ///
    /// Assigns the next available [`DevIndex`] and returns a handle that
    /// bypasses the registry lock for data-plane operations.
    ///
    /// Returns `None` if all `MAX_DEVICES` slots are occupied.
    pub fn register(&self, dev: KBox<dyn NetDevice + Send + Sync>) -> Option<DeviceHandle> {
        let mut inner = self.inner.lock();
        for (i, slot) in inner.slots.iter_mut().enumerate() {
            if slot.is_none() {
                // Extract the raw pointer BEFORE moving the Box into the slot.
                // The Box heap allocation is stable — moving the Box (a pointer-
                // sized value) does not move the pointee.  The raw pointer
                // captures both the data address and the vtable.
                let dev_ptr: *const (dyn NetDevice + Send + Sync) = &*dev;
                *slot = Some(dev);
                inner.count += 1;
                return Some(DeviceHandle {
                    dev: dev_ptr,
                    index: DevIndex(i),
                    tx_lock: SpinLock::new((), LOCK_LEVEL_RESOURCE),
                });
            }
        }
        None
    }

    /// Unregister a network device.
    ///
    /// Calls [`set_down()`](NetDevice::set_down) on the device, then frees the slot.
    /// **The caller must ensure** that no [`DeviceHandle`] for this device is
    /// still in use — any outstanding raw pointers become dangling after this call.
    ///
    /// Returns `true` if a device was found and removed, `false` if the slot
    /// was already empty.
    pub fn unregister(&self, index: DevIndex) -> bool {
        let mut inner = self.inner.lock();
        let idx = index.0;
        if idx >= MAX_DEVICES {
            return false;
        }
        if let Some(dev) = inner.slots[idx].take() {
            dev.set_down();
            inner.count -= 1;
            // `dev` (Box) is dropped here, freeing the heap allocation.
            // Any DeviceHandle raw pointers are now dangling — the caller
            // must have drained all data-plane activity before this call.
            true
        } else {
            false
        }
    }

    /// Enumerate all registered devices.
    ///
    /// Returns a list of `(DevIndex, MacAddr, is_up)` tuples.  Currently
    /// `is_up` is always `true` for registered devices (link-state tracking
    /// is deferred).
    pub fn enumerate(&self) -> KVec<(DevIndex, MacAddr, bool)> {
        let inner = self.inner.lock();
        let mut result = KVec::new();
        for (i, slot) in inner.slots.iter().enumerate() {
            if let Some(dev) = slot {
                let _ = result.push((DevIndex(i), dev.mac(), true));
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
    /// Takes the registry lock briefly.  The device's `tx()` method uses
    /// `&self` with interior mutability, so concurrent TX calls are safe
    /// (serialized by the device's own internal lock).
    ///
    /// For hot-path TX where a [`DeviceHandle`] is already available,
    /// prefer [`DeviceHandle::tx`] which bypasses the registry lock.
    pub fn tx_by_index(&self, index: DevIndex, pkt: PacketBuf) -> Result<(), NetError> {
        let inner = self.inner.lock();
        match inner.slots.get(index.0) {
            Some(Some(dev)) => dev.tx(pkt),
            _ => Err(NetError::NetworkUnreachable),
        }
    }

    /// Zero-copy transmit through a device identified by index (see
    /// [`NetDevice::tx_zerocopy`]). Mirrors [`tx_by_index`](Self::tx_by_index)'s
    /// registry-lock shape so the SlopRing `OP_SEND_ZC` path keeps the exact
    /// lock ordering of the copy path (ring → registry → device state) — no new
    /// cross-lock edge.
    pub fn tx_zerocopy_by_index(
        &self,
        index: DevIndex,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KVec<UFrame<AnonymousMeta>>,
        token: TxReclaimToken,
    ) -> Result<(), NetError> {
        let inner = self.inner.lock();
        match inner.slots.get(index.0) {
            Some(Some(dev)) => dev.tx_zerocopy(net_hdr, runs, csum, keepalive, token),
            _ => Err(NetError::NetworkUnreachable),
        }
    }

    /// Reclaim completed TX descriptors on every registered device (see
    /// [`NetDevice::poll_tx`]). The SlopRing harvest calls this when it has
    /// in-flight zero-copy sends so the deferred `SLOPRING_CQE_F_NOTIF` makes
    /// progress without relying on a TX-completion interrupt — the waiter drives
    /// its own reclaim (caller-as-waiter). Takes only the registry lock; each
    /// device's `poll_tx` takes its own state lock.
    pub fn poll_tx_all(&self) {
        let inner = self.inner.lock();
        for slot in inner.slots.iter() {
            if let Some(dev) = slot {
                dev.poll_tx();
            }
        }
    }

    /// Read the MAC address of a device by index.
    ///
    /// Returns `None` if the device is not registered.
    pub fn mac_by_index(&self, index: DevIndex) -> Option<MacAddr> {
        let inner = self.inner.lock();
        inner.slots.get(index.0)?.as_ref().map(|dev| dev.mac())
    }

    /// Read the feature flags of a device by index.
    ///
    /// Returns `None` if the device is not registered.
    pub fn features_by_index(&self, index: DevIndex) -> Option<NetDeviceFeatures> {
        let inner = self.inner.lock();
        inner.slots.get(index.0)?.as_ref().map(|dev| dev.features())
    }

    /// Poll RX packets from a device by index.
    ///
    /// Takes the registry lock briefly.  Returns an empty Vec if the device
    /// is not registered.
    pub fn poll_rx_by_index(
        &self,
        index: DevIndex,
        budget: usize,
        pool: &'static super::pool::PacketPool,
    ) -> KVec<PacketBuf> {
        let inner = self.inner.lock();
        match inner.slots.get(index.0) {
            Some(Some(dev)) => dev.poll_rx(budget, pool),
            _ => KVec::new(),
        }
    }
}
