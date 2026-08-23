use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use slopos_ostd::dev::FromRawPtr;
use slopos_ostd::lock_class;
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::{KArc, KBox, KVec};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use slopos_net as net;
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{klog_debug, klog_info};

use crate::driver_core::bound::BoundDevice;
use crate::pci::{PciMatch, PciProbeError, ProbeOutcome};
use crate::virtio::{
    self, IrqEdgeEvent, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
    VirtioMmioCaps, VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok, setup_interrupts,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};
use slopos_net::{
    self, PACKET_POOL, ingress,
    napi::NapiContext,
    net_driver_service::{NetDriverServices, register_net_driver_services},
    netdev::{CsumOffload, DeviceHandle, NetDevice, NetDeviceFeatures, NetDeviceStats},
    packetbuf::PacketBuf,
    pool::PacketPool,
    types::{MacAddr, NetError},
};

use slopos_mm::page_alloc::OwnedPageFrame;

pub const VIRTIO_NET_DEVICE_ID_LEGACY: u16 = 0x1000;
pub const VIRTIO_NET_DEVICE_ID_MODERN: u16 = 0x1041;

const VIRTIO_NET_QUEUE_RX: u16 = 0;
const VIRTIO_NET_QUEUE_TX: u16 = 1;

const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_MTU: u64 = 1 << 3;
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;

const VIRTIO_NET_S_LINK_UP: u16 = 1;

const DEV_CFG_MAC_OFFSET: usize = 0x00;
const DEV_CFG_STATUS_OFFSET: usize = 0x06;
const DEV_CFG_MTU_OFFSET: usize = 0x0A;

const DEFAULT_MTU: u16 = 1500;
const PACKET_BUFFER_SIZE: usize = 2048;

const UDP_HEADER_LEN: usize = 8;

const RX_RING_SIZE: usize = 64;
const TX_RING_SIZE: usize = 64;
const NAPI_BUDGET: u32 = 64;

/// Max descriptors in one zero-copy SG TX chain (header + payload runs): a
/// `<= 1472`-byte datagram spans at most 2 pages, so 4 leaves headroom.
const MAX_TX_SG_DESCS: usize = 4;

/// `virtio_net_hdr.flags`: the driver pre-seeded a partial L4 checksum and the
/// device must complete it over `[csum_start..]` (the `NEEDS_CSUM` offload).
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, slopos_ostd::Pod)]
struct VirtioNetHdrV1 {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

#[repr(C)]
struct VirtioNetDevice {
    rx_queue: Virtqueue,
    tx_queue: Virtqueue,
    negotiated_features: u64,
    mac: [u8; 6],
    mtu: u16,
    ready: bool,
}

impl VirtioNetDevice {
    const fn new() -> Self {
        Self {
            rx_queue: Virtqueue::new(),
            tx_queue: Virtqueue::new(),
            negotiated_features: 0,
            mac: [0; 6],
            mtu: DEFAULT_MTU,
            ready: false,
        }
    }
}

/// How a zero-copy TX chain tells the ring its pinned pages are reusable:
/// `Tx` is single-shot (reclaim flips a generation), `Notif` is refcounted for
/// TCP `MSG_ZEROCOPY`, where a retransmit may re-DMA the same pages.
enum TxReclaim {
    Tx(TxReclaimToken),
    Notif(ZcNotifToken),
}

/// One submitted TX chain, keyed by its head descriptor index in `tx_chains`.
/// A copy send is one descriptor (`reclaim`/`keepalive` = `None`); a zero-copy
/// send is `[header] -> pinned runs`.
struct TxChain {
    hdr_page: OwnedPageFrame,
    reclaim: Option<TxReclaim>,
    keepalive: Option<KeepaliveFrames>,
    descs: [u16; MAX_TX_SG_DESCS],
    desc_count: u8,
}

struct VirtioNetState {
    device: VirtioNetDevice,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
    rx_buffers: [Option<OwnedPageFrame>; RX_RING_SIZE],
    /// Per-head TX chain bookkeeping (`used.id` indexes this on reclaim).
    tx_chains: [Option<TxChain>; TX_RING_SIZE],
    /// Descriptor-slot occupancy: run descriptors have no `tx_chains` entry, so
    /// this is the authoritative free-slot map.
    tx_busy: [bool; TX_RING_SIZE],
    tx_inflight: AtomicU32,
}

impl VirtioNetState {
    const fn new() -> Self {
        Self {
            device: VirtioNetDevice::new(),
            caps: VirtioMmioCaps::empty(),
            msix_state: None,
            rx_buffers: [const { None }; RX_RING_SIZE],
            tx_chains: [const { None }; TX_RING_SIZE],
            tx_busy: [false; TX_RING_SIZE],
            tx_inflight: AtomicU32::new(0),
        }
    }
}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static VIRTIO_NET_STATE: SpinLock<VirtioNetState> = SpinLock::new(
    VirtioNetState::new(),
    lock_class!("VIRTIO_NET_STATE", LOCK_LEVEL_RESOURCE),
);
/// Wake the NAPI kthread when the NIC IRQ fires.
static NAPI_WAKER: slopos_net::napi_waker::NapiWaker = slopos_net::napi_waker::NapiWaker::new(
    "netpoll",
    lock_class!("NETPOLL_WAKER.waiters", LOCK_LEVEL_RESOURCE),
);
/// Wake the net-timer kthread when a deadline sooner than the 50 ms periodic
/// slice is armed. No production caller arms it today.
static TIMER_WAKER: slopos_net::napi_waker::NapiWaker = slopos_net::napi_waker::NapiWaker::new(
    "net-timer",
    lock_class!("NET_TIMER_WAKER.waiters", LOCK_LEVEL_RESOURCE),
);
static NAPI_CONTEXT: NapiContext = NapiContext::new(NAPI_BUDGET);
static DNS_RX_EVENT: IrqEdgeEvent = IrqEdgeEvent::new();
/// Buffer for the most recent DNS response payload (UDP body only).
static DNS_RX_BUF: SpinLock<DnsRxBuf> = SpinLock::new(
    DnsRxBuf::new(),
    lock_class!("DNS_RX_BUF", LOCK_LEVEL_RESOURCE),
);

static DEVICE_HANDLE_PTR: AtomicPtr<DeviceHandle> = AtomicPtr::new(core::ptr::null_mut());

/// Link state as of the last carrier poll.
///
/// An atomic, not a [`VIRTIO_NET_STATE`] field: [`NetDevice::carrier`] must not
/// take a lock — callers hold their own, and a driver lock here would add the
/// registry-to-device edge two-phase retirement exists to prevent.
static LINK_UP: AtomicBool = AtomicBool::new(true);

/// Whether the device negotiated `VIRTIO_NET_F_STATUS`, i.e. whether
/// [`LINK_UP`] is an observation or an assumption.
static LINK_OBSERVABLE: AtomicBool = AtomicBool::new(false);

pub fn get_device_handle() -> Option<&'static DeviceHandle> {
    DeviceHandle::from_ptr(DEVICE_HANDLE_PTR.load(Ordering::Acquire))
}

fn set_device_handle(handle: DeviceHandle) {
    let boxed = KBox::try_new(handle).expect("virtio_net: device handle alloc");
    let ptr = KBox::into_raw(boxed);
    DEVICE_HANDLE_PTR.store(ptr, Ordering::Release);
}

struct DnsRxBuf {
    data: [u8; 512],
    len: usize,
}

impl DnsRxBuf {
    const fn new() -> Self {
        Self {
            data: [0; 512],
            len: 0,
        }
    }
}

/// Relaxed atomics rather than fields on `VirtioNetState`: `stats()` answers a
/// query syscall that must not take the driver lock. Byte counts are payload
/// only — the virtio header is driver framing.
mod counters {
    use core::sync::atomic::{AtomicU64, Ordering};

    pub static RX_PACKETS: AtomicU64 = AtomicU64::new(0);
    pub static TX_PACKETS: AtomicU64 = AtomicU64::new(0);
    pub static RX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static TX_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static RX_ERRORS: AtomicU64 = AtomicU64::new(0);
    pub static TX_ERRORS: AtomicU64 = AtomicU64::new(0);
    pub static RX_DROPPED: AtomicU64 = AtomicU64::new(0);
    pub static TX_DROPPED: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

pub struct VirtioNetDev;

impl NetDevice for VirtioNetDev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let mut state = VIRTIO_NET_STATE.lock();
        if !state.device.ready || !link_is_up(&state) {
            // Refused before it reached the ring: a drop, not an error.
            counters::bump(&counters::TX_DROPPED, 1);
            return Err(NetError::NoBufferSpace);
        }

        let payload = pkt.payload();
        let hdr_len = size_of::<VirtioNetHdrV1>();
        if payload.len() + hdr_len > PACKET_BUFFER_SIZE {
            counters::bump(&counters::TX_ERRORS, 1);
            return Err(NetError::NoBufferSpace);
        }

        let Some(tx_page) = alloc_tx_page() else {
            counters::bump(&counters::TX_DROPPED, 1);
            return Err(NetError::NoBufferSpace);
        };

        if !tx_page.write_slice(hdr_len, payload) {
            counters::bump(&counters::TX_ERRORS, 1);
            return Err(NetError::NoBufferSpace);
        }

        if submit_tx(&mut state, tx_page, (hdr_len + payload.len()) as u32) {
            counters::bump(&counters::TX_PACKETS, 1);
            counters::bump(&counters::TX_BYTES, payload.len() as u64);
            Ok(())
        } else {
            counters::bump(&counters::TX_DROPPED, 1);
            Err(NetError::NoBufferSpace)
        }
    }

    fn tx_zerocopy(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: TxReclaimToken,
    ) -> Result<(), NetError> {
        submit_tx_zerocopy(net_hdr, runs, csum, keepalive, TxReclaim::Tx(token))
    }

    fn tx_zerocopy_notif(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KeepaliveFrames,
        token: ZcNotifToken,
    ) -> Result<(), NetError> {
        submit_tx_zerocopy(net_hdr, runs, csum, keepalive, TxReclaim::Notif(token))
    }

    fn poll_tx(&self) {
        // The SlopRing harvest calls this so a deferred F_NOTIF progresses with
        // no TX interrupt while the waiter is parked.
        let mut state = VIRTIO_NET_STATE.lock();
        if !state.device.ready {
            return;
        }
        let _ = virtnet_clean_tx(&mut state);
    }

    fn poll_rx(&self, budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        let mut state = VIRTIO_NET_STATE.lock();
        if !state.device.ready {
            return KVec::new();
        }
        let _ = virtnet_clean_tx(&mut state);

        let mut packets = KVec::with_capacity(budget.min(64)).unwrap_or_else(|_| KVec::new());
        let mut posted = 0usize;
        let mut refill_needed = false;

        for _ in 0..budget {
            let Some(used) = state.device.rx_queue.try_pop_used() else {
                break;
            };

            let idx = (used.id as usize) % RX_RING_SIZE;
            let Some(page) = state.rx_buffers[idx].take() else {
                counters::bump(&counters::RX_ERRORS, 1);
                continue;
            };

            let hdr_len = size_of::<VirtioNetHdrV1>();
            if (used.len as usize) > hdr_len {
                let payload_len = (used.len as usize) - hdr_len;
                match page
                    .slice_at(hdr_len, payload_len)
                    .and_then(PacketBuf::from_raw_copy)
                {
                    Some(pkt) => {
                        if packets.push(pkt).is_err() {
                            counters::bump(&counters::RX_DROPPED, 1);
                        } else {
                            counters::bump(&counters::RX_PACKETS, 1);
                            counters::bump(&counters::RX_BYTES, payload_len as u64);
                        }
                    }
                    None => counters::bump(&counters::RX_DROPPED, 1),
                }
            } else {
                counters::bump(&counters::RX_ERRORS, 1);
            }

            if let Some(new_page) = OwnedPageFrame::alloc_zeroed() {
                state.device.rx_queue.write_desc(
                    idx as u16,
                    VirtqDesc {
                        addr: new_page.phys_u64(),
                        len: PACKET_BUFFER_SIZE as u32,
                        flags: VIRTQ_DESC_F_WRITE,
                        next: 0,
                    },
                );
                state.rx_buffers[idx] = Some(new_page);
                state.device.rx_queue.submit(idx as u16);
                posted += 1;
            } else {
                refill_needed = true;
            }
        }

        // A descriptor whose buffer allocation failed is unposted, not
        // retired: retry it here, or the ring drains one descriptor per
        // allocation failure and never recovers.
        if refill_needed {
            posted += virtnet_prepost_rx_buffers(&mut state);
        }

        if posted > 0 {
            queue::notify_queue(
                &state.caps.notify_cfg,
                state.caps.notify_off_multiplier,
                &state.device.rx_queue,
                VIRTIO_NET_QUEUE_RX,
            );
        }

        packets
    }

    /// Restoring `ready` is not enough: `poll_rx` consumes a descriptor's page
    /// on every receive and only `virtnet_prepost_rx_buffers` puts one back, so
    /// a ring drained while down would stay empty forever.
    fn set_up(&self) {
        let mut state = VIRTIO_NET_STATE.lock();
        state.device.ready = true;
        virtnet_refill_rx_and_notify(&mut state);
    }

    fn set_down(&self) {
        let mut state = VIRTIO_NET_STATE.lock();
        state.device.ready = false;
    }

    fn mtu(&self) -> u16 {
        VIRTIO_NET_STATE.lock().device.mtu
    }

    fn mac(&self) -> MacAddr {
        MacAddr(VIRTIO_NET_STATE.lock().device.mac)
    }

    fn stats(&self) -> NetDeviceStats {
        use core::sync::atomic::Ordering;
        let mut out = NetDeviceStats::new();
        out.rx_packets = counters::RX_PACKETS.load(Ordering::Relaxed);
        out.tx_packets = counters::TX_PACKETS.load(Ordering::Relaxed);
        out.rx_bytes = counters::RX_BYTES.load(Ordering::Relaxed);
        out.tx_bytes = counters::TX_BYTES.load(Ordering::Relaxed);
        out.rx_errors = counters::RX_ERRORS.load(Ordering::Relaxed);
        out.tx_errors = counters::TX_ERRORS.load(Ordering::Relaxed);
        out.rx_dropped = counters::RX_DROPPED.load(Ordering::Relaxed);
        out.tx_dropped = counters::TX_DROPPED.load(Ordering::Relaxed);
        out
    }

    fn features(&self) -> NetDeviceFeatures {
        let feats = VIRTIO_NET_STATE.lock().device.negotiated_features;
        let mut flags = NetDeviceFeatures::empty();
        if feats & VIRTIO_NET_F_CSUM != 0 {
            flags |= NetDeviceFeatures::CHECKSUM_TX;
        }
        if feats & VIRTIO_NET_F_GUEST_CSUM != 0 {
            flags |= NetDeviceFeatures::CHECKSUM_RX;
        }
        flags
    }

    /// Lock-free by contract — see [`LINK_UP`].
    fn carrier(&self) -> bool {
        LINK_UP.load(Ordering::Acquire)
    }

    fn carrier_detect(&self) -> bool {
        LINK_OBSERVABLE.load(Ordering::Acquire)
    }
}

/// The driver lock is released before `iface::set_carrier`: the driver lock
/// holds no out-edges into the network tables. No edge state is kept here —
/// `set_carrier` reports only real transitions.
fn poll_carrier() {
    let up = {
        let state = VIRTIO_NET_STATE.lock();
        link_status_up(&state)
    };
    LINK_UP.store(up, Ordering::Release);

    // Before registration there is no interface to carry the transition; the
    // state attach reads is this atomic, so nothing is lost.
    let Some(handle) = get_device_handle() else {
        return;
    };
    let _ = slopos_net::iface::set_carrier(handle.index(), up);
}

pub fn dns_intercept_response(payload: &[u8]) {
    let copy_len = payload.len().min(512);
    let mut dns_buf = DNS_RX_BUF.lock();
    dns_buf.data[..copy_len].copy_from_slice(&payload[..copy_len]);
    dns_buf.len = copy_len;
    drop(dns_buf);
    DNS_RX_EVENT.signal();
}

fn read_mac(caps: &VirtioMmioCaps, negotiated_features: u64) -> [u8; 6] {
    if (negotiated_features & VIRTIO_NET_F_MAC) == 0
        || !caps.has_device_cfg()
        || caps.device_cfg_len < 6
    {
        return [0; 6];
    }

    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = caps.device_cfg.read::<u8>(DEV_CFG_MAC_OFFSET + i);
    }
    mac
}

fn read_mtu(caps: &VirtioMmioCaps, negotiated_features: u64) -> u16 {
    if (negotiated_features & VIRTIO_NET_F_MTU) == 0
        || !caps.has_device_cfg()
        || caps.device_cfg_len < (DEV_CFG_MTU_OFFSET as u32 + 2)
    {
        return DEFAULT_MTU;
    }
    caps.device_cfg.read::<u16>(DEV_CFG_MTU_OFFSET)
}

/// This device's IPv4 address, read from the interface table: a driver-side
/// copy cannot learn about a renewal, a reconfiguration or a second address.
fn our_ipv4(_state: &VirtioNetState) -> [u8; 4] {
    get_device_handle()
        .and_then(|h| slopos_net::iface::our_ip(h.index()))
        .map(|ip| ip.0)
        .unwrap_or([0; 4])
}

/// The link state the device reports, independent of whether the driver is in
/// service: kept apart from [`link_is_up`] so an administrative down does not
/// look like an unplugged cable. Without `VIRTIO_NET_F_STATUS` this reports up.
fn link_status_up(state: &VirtioNetState) -> bool {
    if (state.device.negotiated_features & VIRTIO_NET_F_STATUS) == 0
        || !state.caps.has_device_cfg()
        || state.caps.device_cfg_len < (DEV_CFG_STATUS_OFFSET as u32 + 2)
    {
        return true;
    }

    (state.caps.device_cfg.read::<u16>(DEV_CFG_STATUS_OFFSET) & VIRTIO_NET_S_LINK_UP) != 0
}

fn link_is_up(state: &VirtioNetState) -> bool {
    state.device.ready && link_status_up(state)
}

/// Shared body of the zero-copy TX submit, parameterised by the reclaim signal.
/// The `Notif` signal takes a reference at commit — paired with the `release`
/// `virtnet_clean_tx` does on reclaim — so an in-flight DMA always holds the
/// pinned pages reusable.
fn submit_tx_zerocopy(
    net_hdr: &[u8],
    runs: &[(u64, u32)],
    csum: Option<CsumOffload>,
    keepalive: KeepaliveFrames,
    reclaim: TxReclaim,
) -> Result<(), NetError> {
    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || !link_is_up(&state) {
        return Err(NetError::NoBufferSpace);
    }
    let vhdr_len = size_of::<VirtioNetHdrV1>();
    // `InvalidArgument` is a permanent reject (the net leaf falls back to
    // copy); a full ring returns `NoBufferSpace` so the ring defers and retries.
    if net_hdr.len() + vhdr_len > PACKET_BUFFER_SIZE {
        return Err(NetError::InvalidArgument);
    }
    if runs.is_empty() || runs.len() + 1 > MAX_TX_SG_DESCS {
        return Err(NetError::InvalidArgument);
    }

    let Some(hdr_page) = alloc_tx_page() else {
        return Err(NetError::NoBufferSpace);
    };
    let mut vhdr = VirtioNetHdrV1::default();
    if let Some(c) = csum {
        vhdr.flags = VIRTIO_NET_HDR_F_NEEDS_CSUM;
        vhdr.csum_start = c.csum_start;
        vhdr.csum_offset = c.csum_offset;
    }
    if !hdr_page.write_at::<VirtioNetHdrV1>(0, &vhdr) || !hdr_page.write_slice(vhdr_len, net_hdr) {
        return Err(NetError::NoBufferSpace);
    }

    let _ = virtnet_clean_tx(&mut state);
    let n = runs.len() + 1;
    let mut slots = [0u16; MAX_TX_SG_DESCS];
    if !alloc_tx_slots(&state, n, &mut slots) {
        return Err(NetError::NoBufferSpace);
    }
    let hdr_pa = hdr_page.phys_u64();
    let hdr_total = (vhdr_len + net_hdr.len()) as u32;
    let Some(chain) = build_tx_chain(&slots[..n], hdr_pa, hdr_total, runs) else {
        return Err(NetError::NoBufferSpace);
    };
    for &(slot, ref desc) in chain.iter() {
        state.device.tx_queue.write_desc(slot, *desc);
    }
    let head = slots[0];
    for &s in slots.iter().take(n) {
        state.tx_busy[s as usize] = true;
    }
    let mut descs = [0u16; MAX_TX_SG_DESCS];
    descs[..n].copy_from_slice(&slots[..n]);
    if let TxReclaim::Notif(token) = &reclaim {
        token.acquire();
    }
    state.tx_chains[head as usize] = Some(TxChain {
        hdr_page,
        reclaim: Some(reclaim),
        keepalive: Some(keepalive),
        descs,
        desc_count: n as u8,
    });
    state.tx_inflight.fetch_add(1, Ordering::Relaxed);

    state.device.tx_queue.submit(head);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.tx_queue,
        VIRTIO_NET_QUEUE_TX,
    );
    counters::bump(&counters::TX_PACKETS, 1);
    counters::bump(
        &counters::TX_BYTES,
        runs.iter().map(|(_, len)| *len as u64).sum::<u64>(),
    );
    Ok(())
}

fn virtnet_clean_tx(state: &mut VirtioNetState) -> usize {
    let mut cleaned = 0usize;
    while let Some(used) = state.device.tx_queue.try_pop_used() {
        let head = (used.id as usize) % TX_RING_SIZE;
        if let Some(chain) = state.tx_chains[head].take() {
            let TxChain {
                hdr_page,
                reclaim,
                keepalive,
                descs,
                desc_count,
            } = chain;
            // Signal the ring before releasing the keepalive refs, so it can
            // post SLOPRING_CQE_F_NOTIF while the pages are still pinned.
            match &reclaim {
                Some(TxReclaim::Tx(token)) => token.signal_reclaimed(),
                Some(TxReclaim::Notif(token)) => token.release(),
                None => {}
            }
            for &d in descs.iter().take(desc_count as usize) {
                state.tx_busy[(d as usize) % TX_RING_SIZE] = false;
            }
            drop(hdr_page);
            drop(keepalive);
        } else {
            // Used entry with no recorded chain: clear the head bit so the slot
            // is not leaked.
            state.tx_busy[head] = false;
        }
        state.tx_inflight.fetch_sub(1, Ordering::Relaxed);
        cleaned += 1;
    }
    cleaned
}

/// Find `n` free TX descriptor slots, returning them in `out[..n]`. `false` if
/// fewer than `n` are free or `n` exceeds [`MAX_TX_SG_DESCS`].
fn alloc_tx_slots(state: &VirtioNetState, n: usize, out: &mut [u16; MAX_TX_SG_DESCS]) -> bool {
    if n == 0 || n > MAX_TX_SG_DESCS {
        return false;
    }
    let mut found = 0usize;
    for idx in 0..TX_RING_SIZE {
        if !state.tx_busy[idx] {
            out[found] = idx as u16;
            found += 1;
            if found == n {
                return true;
            }
        }
    }
    false
}

/// Build the scatter-gather TX chain for a zero-copy send: header descriptor
/// then one descriptor per coalesced pinned-payload run, linked via
/// `VIRTQ_DESC_F_NEXT`. `slots` must be `1 + runs.len()`; the head is `slots[0]`.
fn build_tx_chain(
    slots: &[u16],
    hdr_pa: u64,
    hdr_len: u32,
    runs: &[(u64, u32)],
) -> Option<KVec<(u16, VirtqDesc)>> {
    if runs.is_empty() || slots.len() != runs.len() + 1 {
        return None;
    }
    let mut out = KVec::with_capacity(slots.len()).ok()?;
    out.push((
        slots[0],
        VirtqDesc {
            addr: hdr_pa,
            len: hdr_len,
            flags: VIRTQ_DESC_F_NEXT,
            next: slots[1],
        },
    ))
    .ok()?;
    for (i, &(pa, len)) in runs.iter().enumerate() {
        let is_last = i + 1 == runs.len();
        out.push((
            slots[i + 1],
            VirtqDesc {
                addr: pa,
                len,
                flags: if is_last { 0 } else { VIRTQ_DESC_F_NEXT },
                next: if is_last { 0 } else { slots[i + 2] },
            },
        ))
        .ok()?;
    }
    Some(out)
}

/// Test-only view into [`build_tx_chain`], each descriptor flattened to
/// `(slot, addr, len, flags, next)`.
#[cfg(feature = "test-hooks")]
pub fn build_tx_chain_for_test(
    slots: &[u16],
    hdr_pa: u64,
    hdr_len: u32,
    runs: &[(u64, u32)],
) -> Option<KVec<(u16, u64, u32, u16, u16)>> {
    let chain = build_tx_chain(slots, hdr_pa, hdr_len, runs)?;
    let mut out = KVec::with_capacity(chain.len()).ok()?;
    for &(slot, ref d) in chain.iter() {
        out.push((slot, d.addr, d.len, d.flags, d.next)).ok()?;
    }
    Some(out)
}

fn submit_tx(state: &mut VirtioNetState, page: OwnedPageFrame, total_len: u32) -> bool {
    let _ = virtnet_clean_tx(state);

    let mut slots = [0u16; MAX_TX_SG_DESCS];
    if !alloc_tx_slots(state, 1, &mut slots) {
        return false;
    }
    let head = slots[0];

    state.device.tx_queue.write_desc(
        head,
        VirtqDesc {
            addr: page.phys_u64(),
            len: total_len,
            flags: 0,
            next: 0,
        },
    );
    state.tx_busy[head as usize] = true;
    let mut descs = [0u16; MAX_TX_SG_DESCS];
    descs[0] = head;
    state.tx_chains[head as usize] = Some(TxChain {
        hdr_page: page,
        reclaim: None,
        keepalive: None,
        descs,
        desc_count: 1,
    });
    state.tx_inflight.fetch_add(1, Ordering::Relaxed);

    state.device.tx_queue.submit(head);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.tx_queue,
        VIRTIO_NET_QUEUE_TX,
    );
    true
}

fn alloc_tx_page() -> Option<OwnedPageFrame> {
    let page = OwnedPageFrame::alloc_zeroed()?;
    page.write_at::<VirtioNetHdrV1>(0, &VirtioNetHdrV1::default());
    Some(page)
}

/// Returns the number of descriptors newly posted. The caller notifies: a
/// refill folded into a poll shares that poll's single notification.
fn virtnet_prepost_rx_buffers(state: &mut VirtioNetState) -> usize {
    let mut posted = 0usize;
    let queue_size = (state.device.rx_queue.size as usize).min(RX_RING_SIZE);
    for idx in 0..queue_size {
        if state.rx_buffers[idx].is_some() {
            continue;
        }
        let Some(page) = OwnedPageFrame::alloc_zeroed() else {
            continue;
        };
        state.device.rx_queue.write_desc(
            idx as u16,
            VirtqDesc {
                addr: page.phys_u64(),
                len: PACKET_BUFFER_SIZE as u32,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        state.rx_buffers[idx] = Some(page);
        state.device.rx_queue.submit(idx as u16);
        posted += 1;
    }

    posted
}

fn virtnet_refill_rx_and_notify(state: &mut VirtioNetState) {
    if virtnet_prepost_rx_buffers(state) > 0 {
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.device.rx_queue,
            VIRTIO_NET_QUEUE_RX,
        );
    }
}

fn transmit_udp_packet_locked(
    state: &mut VirtioNetState,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() || !link_is_up(state) {
        return false;
    }

    let Some(mut tx_page) = alloc_tx_page() else {
        return false;
    };

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let frame_len = net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
    let total_len = hdr_len + frame_len;
    if total_len > PACKET_BUFFER_SIZE || payload.len() > u16::MAX as usize - UDP_HEADER_LEN {
        return false;
    }

    {
        let Some(frame) = tx_page.slice_at_mut(hdr_len, frame_len) else {
            return false;
        };

        frame[0..net::ETH_ADDR_LEN].copy_from_slice(&net::MacAddr::BROADCAST.0);
        frame[net::ETH_ADDR_LEN..net::ETH_ADDR_LEN * 2].copy_from_slice(&state.device.mac);
        frame[net::ETH_ADDR_LEN * 2..net::ETH_HEADER_LEN]
            .copy_from_slice(&net::EtherType::Ipv4.to_be_bytes());

        let ip = net::ETH_HEADER_LEN;
        let ip_total = net::IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
        frame[ip] = 0x45;
        frame[ip + 1] = 0;
        frame[ip + 2..ip + 4].copy_from_slice(&(ip_total as u16).to_be_bytes());
        frame[ip + 4..ip + 6].copy_from_slice(&0u16.to_be_bytes());
        frame[ip + 6..ip + 8].copy_from_slice(&0u16.to_be_bytes());
        frame[ip + 8] = 64;
        frame[ip + 9] = net::IpProtocol::Udp.as_u8();
        frame[ip + 10..ip + 12].copy_from_slice(&0u16.to_be_bytes());
        frame[ip + 12..ip + 16].copy_from_slice(&src_ip);
        frame[ip + 16..ip + 20].copy_from_slice(&dst_ip);
        let ip_csum = net::checksum::internet_checksum(&frame[ip..ip + net::IPV4_HEADER_LEN]);
        frame[ip + 10..ip + 12].copy_from_slice(&ip_csum.to_be_bytes());

        let udp = ip + net::IPV4_HEADER_LEN;
        let udp_total = UDP_HEADER_LEN + payload.len();
        frame[udp..udp + 2].copy_from_slice(&src_port.to_be_bytes());
        frame[udp + 2..udp + 4].copy_from_slice(&dst_port.to_be_bytes());
        frame[udp + 4..udp + 6].copy_from_slice(&(udp_total as u16).to_be_bytes());
        frame[udp + 6..udp + 8].copy_from_slice(&0u16.to_be_bytes());
        frame[udp + UDP_HEADER_LEN..udp + UDP_HEADER_LEN + payload.len()].copy_from_slice(payload);

        let udp_csum = net::checksum::udp_checksum(src_ip, dst_ip, src_port, dst_port, payload);
        frame[udp + 6..udp + 8].copy_from_slice(&udp_csum.to_be_bytes());
    }

    submit_tx(state, tx_page, total_len as u32)
}

pub fn transmit_udp_packet(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    let mut state = VIRTIO_NET_STATE.lock();
    transmit_udp_packet_locked(&mut state, src_ip, dst_ip, src_port, dst_port, payload)
}

/// Drain one NAPI burst. Returns the NIC packet count so the caller can re-arm
/// the waker when budget was exhausted.
fn run_napi_burst() -> u32 {
    let Some(handle) = get_device_handle() else {
        return 0;
    };

    {
        let state = VIRTIO_NET_STATE.lock();
        if !state.device.ready || !link_is_up(&state) {
            return 0;
        }
    }

    let packets = handle.poll_rx(NAPI_CONTEXT.budget() as usize, &PACKET_POOL);
    let processed = packets.len() as u32;
    for pkt in packets {
        ingress::net_rx(handle, pkt);
    }
    NAPI_CONTEXT.add_processed(processed);

    poll_loopback();

    processed
}

/// Drain the loopback device (DevIndex 0), which stores TX'd packets
/// internally, back through ingress so they appear as received local traffic.
fn poll_loopback() {
    use slopos_net::netdev::DEVICE_REGISTRY;
    use slopos_net::types::DevIndex;

    let lo_packets = DEVICE_REGISTRY.poll_rx_by_index(DevIndex(0), 32, &PACKET_POOL);

    for pkt in lo_packets {
        // Loopback bypasses MAC filtering.
        let checksum_rx = true; // Loopback doesn't need checksum verification.
        let data = pkt.payload();
        if data.len() >= slopos_net::ETH_HEADER_LEN {
            let ethertype_raw = u16::from_be_bytes([data[12], data[13]]);
            let mut pkt = pkt;
            pkt.set_l2(pkt.head());
            pkt.set_l3(pkt.head() + slopos_net::ETH_HEADER_LEN as u16);
            if pkt.pull_header(slopos_net::ETH_HEADER_LEN).is_ok() {
                match slopos_net::EtherType::from_u16(ethertype_raw) {
                    Some(slopos_net::EtherType::Ipv4) => {
                        slopos_net::ipv4::handle_rx(DevIndex(0), pkt, checksum_rx);
                    }
                    _ => {
                        // Loopback only handles IPv4 for now.
                    }
                }
            }
        }
    }
}

/// Force a synchronous NAPI poll cycle from a non-IRQ context, for test
/// fixtures and the host wrapper; production RX runs on the netpoll kthread.
pub fn virtnet_force_napi_poll() {
    NAPI_WAKER.arm_and_wake();
    let _ = run_napi_burst();
    slopos_net::socket::socket_process_timers();
}

/// Wake-only counterpart to [`virtnet_force_napi_poll`]: loopback tx calls it
/// under the `LoopbackDev::inner` lock, where a synchronous poll would re-enter
/// `VIRTIO_NET_STATE`.
pub fn virtnet_wake_napi() {
    NAPI_WAKER.arm_and_wake();
}

/// Long-lived netpoll worker (threaded NAPI), parked on [`NAPI_WAKER`] which the
/// per-queue IRQ handler arms. After each burst it peeks the used ring and
/// re-arms if the IRQ landed inside the drain-to-park window (lost wakeup).
fn napi_thread_entry(token: slopos_ostd::sync::kernel_io_task::KernelIoToken<'static>) {
    use slopos_ostd::sync::kernel_io_task::{KthreadWait, yield_now};
    loop {
        let waited = NAPI_WAKER.wait(&token);
        if waited == KthreadWait::Stop {
            // Packets the IRQ already committed are in the used ring and
            // nothing else will collect them.
            let _ = run_napi_burst();
            break;
        }
        let processed = run_napi_burst();
        slopos_net::socket::socket_process_timers();

        if has_pending_rx() {
            NAPI_WAKER.rearm();
        }

        if processed >= NAPI_CONTEXT.budget() {
            NAPI_WAKER.rearm();
            yield_now(&token);
        }
    }
    NAPI_WAKER.stop().note_exited();
}

/// Does the used ring hold an entry the kthread has not popped? Compares used
/// `idx` against the driver-cached `last_used_idx`, which only the kthread
/// advances.
fn has_pending_rx() -> bool {
    // TODO(tech-debt): takes `VIRTIO_NET_STATE` for a single volatile read
    // because `has_pending` needs `&Virtqueue` — expose the used-ring base so
    // this becomes a pure atomic load.
    let state = VIRTIO_NET_STATE.lock();
    state.device.rx_queue.has_pending()
}

/// Net-timer kthread, separated from `napi_thread_entry` so the RX hot path is
/// not charged for `net_timer_process`. Runs at [`TaskPriority::KernelIo`] so
/// ARP aging, TCP retransmit and delayed-ACK fire on time under user load.
fn net_timer_thread_entry(token: slopos_ostd::sync::kernel_io_task::KernelIoToken<'static>) {
    use slopos_ostd::sync::kernel_io_task::{KthreadWait, yield_now};
    const NET_TIMER_PERIOD_MS: u32 = 50;
    loop {
        if TIMER_WAKER.wait_timeout_ms(&token, NET_TIMER_PERIOD_MS) == KthreadWait::Stop {
            break;
        }
        slopos_net::timer::net_timer_process();
        slopos_net::socket::socket_process_timers();
        // Polled here rather than from the config-change interrupt: reading the
        // status register needs the driver lock and acting on a transition
        // needs four more plus an allocation, none of which a hard IRQ may do.
        poll_carrier();
        yield_now(&token);
    }
    TIMER_WAKER.stop().note_exited();
}

/// Per-queue interrupt handler: `queue_idx` 0 is RX, 1 is TX. Deliberately tiny
/// — wake the netpoll kthread and do no protocol or scheduler work in hard IRQ.
fn virtio_net_irq_handler(queue_idx: u8) {
    match queue_idx {
        0 => {
            NAPI_WAKER.arm_and_wake();
        }
        1 => {
            NAPI_WAKER.arm_and_wake();
        }
        _ => {}
    }
}

/// Runs with `VIRTIO_NET_STATE` held: nothing here may block, allocate, take
/// another subsystem's lock or deschedule — that belongs in
/// [`virtio_net_publish_device`].
#[inline(never)]
fn virtio_net_register_device(state: &mut VirtioNetState) -> bool {
    virtnet_refill_rx_and_notify(state);

    // Seed before the device is visible: `carrier()` is answerable the moment
    // `register` returns, and the registry enumerates without asking us first.
    LINK_OBSERVABLE.store(
        (state.device.negotiated_features & VIRTIO_NET_F_STATUS) != 0,
        Ordering::Release,
    );
    LINK_UP.store(link_status_up(state), Ordering::Release);
    true
}

/// **Runs with `VIRTIO_NET_STATE` released.** Every step either allocates,
/// takes another subsystem's lock, or re-enters this driver's own `tx()` —
/// none permissible under a lock that disables interrupts and preemption.
///
/// Returns `false` only on allocation failure.
fn virtio_net_publish_device(mac: [u8; 6], mtu: u16) -> bool {
    use slopos_net::netdev::DEVICE_REGISTRY;

    PACKET_POOL.init();

    let dev: KArc<dyn slopos_net::netdev::NetDevice + Send + Sync> =
        match KArc::try_new(VirtioNetDev) {
            Ok(d) => d,
            Err(_) => {
                klog_info!("virtio-net: alloc failed");
                return false;
            }
        };
    let Some(handle) = DEVICE_REGISTRY.register(dev) else {
        klog_info!("virtio-net: failed to register in device registry");
        return true;
    };

    let actual_idx = handle.index();
    klog_info!(
        "virtio-net: registered as dev {} in device registry",
        actual_idx
    );

    match slopos_net::iface::attach(
        actual_idx,
        slopos_net::iface::IfaceKind::Ethernet,
        slopos_net::types::MacAddr(mac),
        mtu,
        LINK_UP.load(Ordering::Acquire),
        LINK_OBSERVABLE.load(Ordering::Acquire),
    ) {
        Ok(ifindex) => klog_info!("virtio-net: attached interface {}", ifindex),
        Err(err) => klog_info!("virtio-net: failed to attach interface: {:?}", err),
    }

    set_device_handle(handle);

    // Only queues a DISCOVER and arms a timer, so probe returns whether or not
    // a server answers, and a late one is retried.
    if !slopos_net::dhcp::start(actual_idx) {
        klog_info!("virtio-net: could not start the DHCP client");
    }
    true
}

fn virtio_net_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    if !DEVICE_CLAIMED.claim() {
        klog_debug!("virtio-net: already own a NIC; declining additional device");
        return Ok(ProbeOutcome::Declined);
    }

    let info = *bound.info();
    klog_info!(
        "virtio-net: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(&info);

    let caps = parse_capabilities(&info);
    klog_debug!(
        "virtio-net: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-net: missing common cfg");
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::Unsupported);
    }

    if !caps.has_notify_cfg() {
        klog_info!("virtio-net: missing notify cfg");
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::Unsupported);
    }

    let required_features = virtio::VIRTIO_F_VERSION_1;
    let optional_features = VIRTIO_NET_F_CSUM
        | VIRTIO_NET_F_GUEST_CSUM
        | VIRTIO_NET_F_MAC
        | VIRTIO_NET_F_STATUS
        | VIRTIO_NET_F_MTU;
    let feat_result = negotiate_features(&caps, required_features, optional_features);
    if !feat_result.success {
        klog_info!("virtio-net: features negotiation failed");
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::DeviceFault);
    }

    // Two vectors: one for RX (queue 0), one for TX (queue 1).
    let (irq_mode, msix_state) = setup_interrupts(bound, &caps, 2, virtio_net_irq_handler)
        .unwrap_or_else(|msg| {
            panic!(
                "virtio-net: {}:{}.{} {}",
                info.bus, info.device, info.function, msg
            )
        });
    let rx_msix_entry = msix_state.as_ref().map_or(VIRTIO_MSI_NO_VECTOR, |s| {
        s.queue_msix_entry(VIRTIO_NET_QUEUE_RX)
    });
    let tx_msix_entry = msix_state.as_ref().map_or(VIRTIO_MSI_NO_VECTOR, |s| {
        s.queue_msix_entry(VIRTIO_NET_QUEUE_TX)
    });

    let negotiated_features = feat_result.driver_features;
    let mac = read_mac(&caps, negotiated_features);
    let mtu = read_mtu(&caps, negotiated_features);

    {
        // Set up in place inside the heap-resident state so the ~200-byte
        // `Virtqueue`s never land on this probe's stack frame (2 KiB gate).
        // Both queues must be enabled before DRIVER_OK (VirtIO spec §3.1.1).
        let mut state = VIRTIO_NET_STATE.lock();
        if !queue::setup_queue_into(
            &caps.common_cfg,
            VIRTIO_NET_QUEUE_RX,
            DEFAULT_QUEUE_SIZE,
            rx_msix_entry,
            &mut state.device.rx_queue,
        ) {
            klog_info!("virtio-net: rx queue setup failed");
            DEVICE_CLAIMED.reset();
            return Err(PciProbeError::OutOfMemory);
        }

        if !queue::setup_queue_into(
            &caps.common_cfg,
            VIRTIO_NET_QUEUE_TX,
            DEFAULT_QUEUE_SIZE,
            tx_msix_entry,
            &mut state.device.tx_queue,
        ) {
            klog_info!("virtio-net: tx queue setup failed");
            DEVICE_CLAIMED.reset();
            return Err(PciProbeError::OutOfMemory);
        }

        set_driver_ok(&caps);

        state.device.negotiated_features = negotiated_features;
        state.device.mac = mac;
        state.device.mtu = mtu;
        state.device.ready = true;
        state.caps = caps;
        state.msix_state = msix_state;

        if !virtio_net_register_device(&mut state) {
            return Err(PciProbeError::OutOfMemory);
        }
    }

    if !virtio_net_publish_device(mac, mtu) {
        return Err(PciProbeError::OutOfMemory);
    }

    slopos_net::napi::register_wake_napi(virtnet_wake_napi);
    static NET_DRIVER_SVC: NetDriverServices = NetDriverServices {
        virtio_net_ipv4_addr,
        virtio_net_dns,
        dns_rx_clear,
        transmit_udp_packet,
        dns_rx_wait,
        dns_rx_read,
        virtio_net_mac,
        get_device_handle,
        dns_intercept_response,
        virtio_net_is_ready,
        virtio_net_transmit,
        virtnet_force_napi_poll,
    };
    register_net_driver_services(&NET_DRIVER_SVC);

    if let Err(err) = slopos_ostd::spawn_kernel_io!(NAPI_WAKER.stop(), napi_thread_entry) {
        klog_info!(
            "virtio-net: failed to spawn netpoll kernel thread ({:?})",
            err
        );
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::OutOfMemory);
    }
    if let Err(err) = slopos_ostd::spawn_kernel_io!(TIMER_WAKER.stop(), net_timer_thread_entry) {
        klog_info!(
            "virtio-net: failed to spawn net-timer kernel thread ({:?})",
            err
        );
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::OutOfMemory);
    }

    klog_info!(
        "virtio-net: ready mtu={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} irq {:?}",
        mtu,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        irq_mode,
    );

    Ok(ProbeOutcome::Bound)
}

crate::pci_driver! {
    pub static VIRTIO_NET_DRIVER = {
        name: "virtio-net",
        match_table: &[
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_NET_DEVICE_ID_LEGACY,
            },
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_NET_DEVICE_ID_MODERN,
            },
        ],
        probe: virtio_net_probe,
    };
}

pub fn virtio_net_is_ready() -> bool {
    VIRTIO_NET_STATE.lock().device.ready
}

pub fn virtio_net_link_up() -> bool {
    let state = VIRTIO_NET_STATE.lock();
    link_is_up(&state)
}

pub fn virtio_net_mac() -> Option<[u8; 6]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some(state.device.mac)
}

pub fn virtio_net_mtu() -> Option<u16> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some(state.device.mtu)
}

pub fn virtio_net_ipv4_addr() -> Option<[u8; 4]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    let addr = our_ipv4(&state);
    if addr == [0; 4] { None } else { Some(addr) }
}

pub fn virtio_net_transmit(packet: &[u8]) -> bool {
    if packet.is_empty() {
        return true;
    }

    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || !link_is_up(&state) {
        return false;
    }

    let hdr_len = size_of::<VirtioNetHdrV1>();
    if packet.len() + hdr_len > PACKET_BUFFER_SIZE {
        return false;
    }

    let Some(tx_page) = alloc_tx_page() else {
        return false;
    };

    if !tx_page.write_slice(hdr_len, packet) {
        return false;
    }

    submit_tx(&mut state, tx_page, (hdr_len + packet.len()) as u32)
}

/// Return the DHCP-provided DNS server address, or `None` if not configured.
pub fn virtio_net_dns() -> Option<[u8; 4]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    slopos_net::resolver::primary().map(|ip| ip.0)
}

pub fn dns_rx_clear() {
    DNS_RX_EVENT.try_consume();
    let mut buf = DNS_RX_BUF.lock();
    buf.len = 0;
}

/// The edge is latched by `dns_intercept_response` on the netpoll kthread's own
/// drain, so this needs no drain of its own.
pub fn dns_rx_wait(timeout_ms: u32) -> bool {
    DNS_RX_EVENT.wait_timeout_ms(timeout_ms)
}

pub fn dns_rx_read(out: &mut [u8]) -> usize {
    let buf = DNS_RX_BUF.lock();
    let copy_len = buf.len.min(out.len());
    out[..copy_len].copy_from_slice(&buf.data[..copy_len]);
    copy_len
}

/// Snapshot of the MSI-X state, or `None` if the device was not probed or fell
/// back to MSI.
#[cfg(feature = "test-hooks")]
pub fn virtio_net_msix_state() -> Option<VirtioMsixState> {
    VIRTIO_NET_STATE.lock().msix_state.clone()
}
