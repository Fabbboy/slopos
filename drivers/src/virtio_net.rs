use core::mem::size_of;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use slopos_ostd::dev::FromRawPtr;
use slopos_ostd::mm::frame::AnonymousMeta;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::{KBox, KVec};
use slopos_ostd::{TxReclaimToken, ZcNotifToken};

use slopos_abi::net::{
    USER_NET_MEMBER_FLAG_ARP, USER_NET_MEMBER_FLAG_IPV4, UserNetInfo, UserNetMember,
};
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
    self, PACKET_POOL, dhcp, ingress,
    napi::NapiContext,
    net_driver_service::{NetDriverServices, register_net_driver_services},
    netdev::{CsumOffload, DeviceHandle, NetDevice, NetDeviceFeatures, NetDeviceStats},
    packetbuf::PacketBuf,
    pool::PacketPool,
    socket, tcp,
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

const DHCP_REQUEST_TIMEOUT_MS: u32 = 5000;
/// Short timeout for ARP probe / scan operations (ms).  ARP replies on a
/// local LAN arrive in < 10 ms; 150 ms is generous while keeping the scan
/// responsive enough that it doesn't block the compositor for seconds.
const SCAN_RX_TIMEOUT_MS: u32 = 150;
const DEFAULT_MTU: u16 = 1500;
const PACKET_BUFFER_SIZE: usize = 2048;
const MAX_NET_MEMBERS: usize = 32;

const UDP_HEADER_LEN: usize = 8;
const ARP_HEADER_LEN: usize = 28;
const ARP_HTYPE_ETHERNET: u16 = 1;
const ARP_PTYPE_IPV4: u16 = net::EtherType::Ipv4.as_u16();
const ARP_HLEN_ETHERNET: u8 = 6;
const ARP_PLEN_IPV4: u8 = 4;
const ARP_OPER_REQUEST: u16 = 1;

const DHCP_RX_MAX_POLLS: usize = 64;
const RX_RING_SIZE: usize = 64;
const TX_RING_SIZE: usize = 64;
const NAPI_BUDGET: u32 = 64;

/// Max descriptors in one zero-copy SG TX chain: 1 header + up to 3 pinned
/// payload runs. A `<= 1472`-byte datagram spans at most 2 pages (so `<= 2`
/// coalesced runs); 4 leaves headroom. Chains needing more fall back to copy.
const MAX_TX_SG_DESCS: usize = 4;

/// `virtio_net_hdr.flags`: the driver pre-seeded a partial L4 checksum and the
/// device must complete it over `[csum_start..]` (the `NEEDS_CSUM` offload).
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

static DHCP_XID_COUNTER: AtomicU32 = AtomicU32::new(0x534c_4f50);

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

/// How a zero-copy TX chain tells the ring its pinned pages are reusable.
///
/// `Tx` is the single-shot model (UDP/ICMP, one DMA): reclaim flips the
/// generation counter. `Notif` is the refcounted model (TCP `MSG_ZEROCOPY`,
/// where the same pages may be DMA'd again on a retransmit): the driver holds
/// one reference per in-flight descriptor and releases it on reclaim, so the
/// buffer is reported reusable only once every DMA is reclaimed **and** the send
/// queue has retired the chunk on cumulative ACK.
enum TxReclaim {
    Tx(TxReclaimToken),
    Notif(ZcNotifToken),
}

/// One submitted TX chain, keyed by its head descriptor index in `tx_chains`.
/// A normal copy send is a 1-descriptor chain (`reclaim`/`keepalive` = `None`); a
/// zero-copy send is `[header] -> pinned runs` and carries the reclaim signal +
/// the page keepalive (independent owning refs on the pinned user pages). On
/// reclaim the driver signals the token, drops the keepalive, and frees every
/// descriptor slot in `descs[..desc_count]`.
struct TxChain {
    hdr_page: OwnedPageFrame,
    reclaim: Option<TxReclaim>,
    keepalive: Option<KVec<UFrame<AnonymousMeta>>>,
    descs: [u16; MAX_TX_SG_DESCS],
    desc_count: u8,
}

struct VirtioNetState {
    device: VirtioNetDevice,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
    ipv4_addr: [u8; 4],
    subnet_mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
    members: [UserNetMember; MAX_NET_MEMBERS],
    member_count: usize,
    rx_buffers: [Option<OwnedPageFrame>; RX_RING_SIZE],
    /// Per-head TX chain bookkeeping (`used.id` indexes this on reclaim).
    tx_chains: [Option<TxChain>; TX_RING_SIZE],
    /// Descriptor-slot occupancy. A chain marks every descriptor it uses busy;
    /// reclaim clears them. Run descriptors have no `tx_chains` entry, so this
    /// is the authoritative free-slot map.
    tx_busy: [bool; TX_RING_SIZE],
    tx_inflight: AtomicU32,
}

impl VirtioNetState {
    const fn new() -> Self {
        Self {
            device: VirtioNetDevice::new(),
            caps: VirtioMmioCaps::empty(),
            msix_state: None,
            ipv4_addr: [0; 4],
            subnet_mask: [0; 4],
            router: [0; 4],
            dns: [0; 4],
            members: [UserNetMember {
                ipv4: [0; 4],
                mac: [0; 6],
                flags: 0,
            }; MAX_NET_MEMBERS],
            member_count: 0,
            rx_buffers: [const { None }; RX_RING_SIZE],
            tx_chains: [const { None }; TX_RING_SIZE],
            tx_busy: [false; TX_RING_SIZE],
            tx_inflight: AtomicU32::new(0),
        }
    }
}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static VIRTIO_NET_STATE: SpinLock<VirtioNetState> =
    SpinLock::new(VirtioNetState::new(), LOCK_LEVEL_RESOURCE);
static DHCP_RX_EVENT: IrqEdgeEvent = IrqEdgeEvent::new();
/// Wake the NAPI kthread when the NIC IRQ fires. Replaces the
/// pre-refactor `NAPI_EVENT: IrqEdgeEvent` + `sleep_current_task_ms(1)`
/// polling loop with an IRQ-driven park-and-wake.
static NAPI_WAKER: slopos_net::napi_waker::NapiWaker = slopos_net::napi_waker::NapiWaker::new();
/// Wake the net-timer kthread when a sooner deadline is armed.
/// Used by code that schedules a fresh timer wheel entry it needs
/// fired before the next periodic 50 ms slice. Currently the
/// production callers do not arm this signal (the 50 ms periodic
/// cadence is enough for ARP/TCP retx latency); the signal is
/// wired up for completeness and future optimisation.
static TIMER_WAKER: slopos_net::napi_waker::NapiWaker = slopos_net::napi_waker::NapiWaker::new();
static NAPI_CONTEXT: NapiContext = NapiContext::new(NAPI_BUDGET);
static DNS_RX_EVENT: IrqEdgeEvent = IrqEdgeEvent::new();
/// Buffer for the most recent DNS response payload (UDP body only).
static DNS_RX_BUF: SpinLock<DnsRxBuf> = SpinLock::new(DnsRxBuf::new(), LOCK_LEVEL_RESOURCE);

static DEVICE_HANDLE_PTR: AtomicPtr<DeviceHandle> = AtomicPtr::new(core::ptr::null_mut());

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

pub struct VirtioNetDev;

impl NetDevice for VirtioNetDev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let mut state = VIRTIO_NET_STATE.lock();
        if !state.device.ready || !link_is_up(&state) {
            return Err(NetError::NoBufferSpace);
        }

        let payload = pkt.payload();
        let hdr_len = size_of::<VirtioNetHdrV1>();
        if payload.len() + hdr_len > PACKET_BUFFER_SIZE {
            return Err(NetError::NoBufferSpace);
        }

        let Some(tx_page) = alloc_tx_page() else {
            return Err(NetError::NoBufferSpace);
        };

        if !tx_page.write_slice(hdr_len, payload) {
            return Err(NetError::NoBufferSpace);
        }

        if submit_tx(&mut state, tx_page, (hdr_len + payload.len()) as u32) {
            Ok(())
        } else {
            Err(NetError::NoBufferSpace)
        }
    }

    fn tx_zerocopy(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KVec<UFrame<AnonymousMeta>>,
        token: TxReclaimToken,
    ) -> Result<(), NetError> {
        submit_tx_zerocopy(net_hdr, runs, csum, keepalive, TxReclaim::Tx(token))
    }

    fn tx_zerocopy_notif(
        &self,
        net_hdr: &[u8],
        runs: &[(u64, u32)],
        csum: Option<CsumOffload>,
        keepalive: KVec<UFrame<AnonymousMeta>>,
        token: ZcNotifToken,
    ) -> Result<(), NetError> {
        submit_tx_zerocopy(net_hdr, runs, csum, keepalive, TxReclaim::Notif(token))
    }

    fn poll_tx(&self) {
        // TX-only reclaim (drains the TX used ring, signals any zero-copy
        // tokens). Serialized with `poll_rx`/`submit_tx` by the state lock; the
        // SlopRing harvest calls this so a deferred F_NOTIF progresses even with
        // no TX interrupt while the waiter is parked.
        let mut state = VIRTIO_NET_STATE.lock();
        let _ = virtnet_clean_tx(&mut state);
    }

    fn poll_rx(&self, budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        let mut state = VIRTIO_NET_STATE.lock();
        let _ = virtnet_clean_tx(&mut state);

        let mut packets = KVec::with_capacity(budget.min(64)).unwrap_or_else(|_| KVec::new());
        let mut posted = 0usize;

        for _ in 0..budget {
            let Some(used) = state.device.rx_queue.try_pop_used() else {
                break;
            };

            let idx = (used.id as usize) % RX_RING_SIZE;
            let Some(page) = state.rx_buffers[idx].take() else {
                continue;
            };

            let hdr_len = size_of::<VirtioNetHdrV1>();
            if (used.len as usize) > hdr_len {
                let payload_len = (used.len as usize) - hdr_len;
                if let Some(frame) = page.slice_at(hdr_len, payload_len) {
                    if let Some(pkt) = PacketBuf::from_raw_copy(frame) {
                        let _ = packets.push(pkt);
                    }
                }
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
            }
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

    fn set_up(&self) {}

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
        NetDeviceStats::new()
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
}

pub fn dns_intercept_response(payload: &[u8]) {
    let copy_len = payload.len().min(512);
    let mut dns_buf = DNS_RX_BUF.lock();
    dns_buf.data[..copy_len].copy_from_slice(&payload[..copy_len]);
    dns_buf.len = copy_len;
    drop(dns_buf);
    DNS_RX_EVENT.signal();
}

pub fn sniff_packet_for_members(frame: &[u8]) {
    let mut state = VIRTIO_NET_STATE.lock();
    sniff_frame_for_members(&mut state, frame);
}

// =============================================================================
// Device configuration helpers
// =============================================================================

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

fn link_is_up(state: &VirtioNetState) -> bool {
    if !state.device.ready {
        return false;
    }

    if (state.device.negotiated_features & VIRTIO_NET_F_STATUS) == 0
        || !state.caps.has_device_cfg()
        || state.caps.device_cfg_len < (DEV_CFG_STATUS_OFFSET as u32 + 2)
    {
        return true;
    }

    (state.caps.device_cfg.read::<u16>(DEV_CFG_STATUS_OFFSET) & VIRTIO_NET_S_LINK_UP) != 0
}

// =============================================================================
// Network member tracking
// =============================================================================

fn add_or_update_member(state: &mut VirtioNetState, mac: [u8; 6], ipv4: [u8; 4], flag: u16) {
    if mac == [0; 6] {
        return;
    }

    for entry in &mut state.members[..state.member_count] {
        if entry.mac == mac || (ipv4 != [0; 4] && entry.ipv4 == ipv4) {
            entry.mac = mac;
            if ipv4 != [0; 4] {
                entry.ipv4 = ipv4;
            }
            entry.flags |= flag;
            return;
        }
    }

    if state.member_count < state.members.len() {
        state.members[state.member_count] = UserNetMember {
            ipv4,
            mac,
            flags: flag,
        };
        state.member_count += 1;
    }
}

/// Inspect an incoming Ethernet frame and record any new MAC/IP associations.
fn sniff_frame_for_members(state: &mut VirtioNetState, frame: &[u8]) {
    if frame.len() < net::ETH_HEADER_LEN {
        return;
    }

    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    if ethertype == net::EtherType::Arp.as_u16() {
        if frame.len() < net::ETH_HEADER_LEN + ARP_HEADER_LEN {
            return;
        }
        let arp = &frame[net::ETH_HEADER_LEN..net::ETH_HEADER_LEN + ARP_HEADER_LEN];
        let htype = u16::from_be_bytes([arp[0], arp[1]]);
        let ptype = u16::from_be_bytes([arp[2], arp[3]]);
        if htype != ARP_HTYPE_ETHERNET
            || ptype != ARP_PTYPE_IPV4
            || arp[4] != ARP_HLEN_ETHERNET
            || arp[5] != ARP_PLEN_IPV4
        {
            return;
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().unwrap();
        let sender_ip: [u8; 4] = arp[14..18].try_into().unwrap();
        add_or_update_member(state, sender_mac, sender_ip, USER_NET_MEMBER_FLAG_ARP);
        return;
    }

    if ethertype == net::EtherType::Ipv4.as_u16() {
        if frame.len() < net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN {
            return;
        }
        let src_ip: [u8; 4] = frame[26..30].try_into().unwrap();
        add_or_update_member(state, src_mac, src_ip, USER_NET_MEMBER_FLAG_IPV4);
    }
}

// =============================================================================
// Virtqueue I/O helpers
// =============================================================================

/// Shared body of the zero-copy TX submit, parameterised by the reclaim signal.
/// Builds the header DMA page (with optional csum offload), allocates an SG
/// descriptor chain (`[header] -> pinned runs`), records the chain + keepalive +
/// reclaim signal, and kicks the queue. For the refcounted `Notif` signal a
/// reference is taken at commit — paired with the `release` `virtnet_clean_tx`
/// does on reclaim — so an in-flight DMA always holds the pages reusable.
fn submit_tx_zerocopy(
    net_hdr: &[u8],
    runs: &[(u64, u32)],
    csum: Option<CsumOffload>,
    keepalive: KVec<UFrame<AnonymousMeta>>,
    reclaim: TxReclaim,
) -> Result<(), NetError> {
    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || !link_is_up(&state) {
        return Err(NetError::NoBufferSpace);
    }
    let vhdr_len = size_of::<VirtioNetHdrV1>();
    // `InvalidArgument` (vs `NoBufferSpace`) means "permanent reject" — the
    // net leaf maps it to fall-back-to-copy, not retry. A full ring below
    // returns `NoBufferSpace` so the ring defers + re-attempts.
    if net_hdr.len() + vhdr_len > PACKET_BUFFER_SIZE {
        return Err(NetError::InvalidArgument);
    }
    if runs.is_empty() || runs.len() + 1 > MAX_TX_SG_DESCS {
        return Err(NetError::InvalidArgument);
    }

    // Header DMA page: virtio_net_hdr (with optional csum offload) at 0,
    // then the kernel-built L2/L3/L4 headers; the payload stays in the
    // pinned pages the SG runs point at.
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
    // Commit point: for the refcounted TCP token, this in-flight DMA now holds a
    // reference on the pinned pages (balanced by `release` on reclaim below).
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
    Ok(())
}

fn virtnet_clean_tx(state: &mut VirtioNetState) -> usize {
    let mut cleaned = 0usize;
    while let Some(used) = state.device.tx_queue.try_pop_used() {
        // `used.id` is the chain's head descriptor index.
        let head = (used.id as usize) % TX_RING_SIZE;
        if let Some(chain) = state.tx_chains[head].take() {
            let TxChain {
                hdr_page,
                reclaim,
                keepalive,
                descs,
                desc_count,
            } = chain;
            // The NIC is done with the pinned pages: signal the ring (so it can
            // post SLOPRING_CQE_F_NOTIF) before releasing the keepalive refs.
            // UDP/ICMP flip a single-shot generation; TCP releases one of the
            // refcounted token's references (the buffer is reusable only once the
            // count — chunk + all in-flight DMAs — reaches zero).
            match &reclaim {
                Some(TxReclaim::Tx(token)) => token.signal_reclaimed(),
                Some(TxReclaim::Notif(token)) => token.release(),
                None => {}
            }
            for &d in descs.iter().take(desc_count as usize) {
                state.tx_busy[(d as usize) % TX_RING_SIZE] = false;
            }
            drop(hdr_page); // free the header DMA page
            drop(keepalive); // release the independent pinned-page refs (NIC done)
        } else {
            // Defensive: a used entry with no recorded chain — clear the head
            // bit so the slot isn't leaked.
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

/// Build a scatter-gather TX descriptor chain for a zero-copy send: a header
/// descriptor (`hdr_pa`/`hdr_len`, device-readable) followed by one descriptor
/// per coalesced pinned-payload run, linked via `VIRTQ_DESC_F_NEXT`. So the NIC
/// DMAs the payload straight from the pinned user pages — no kernel copy.
///
/// `slots` supplies the descriptor-table indices to occupy; its length must be
/// `1 + runs.len()` (head + one per run). Returns `(slot, desc)` pairs the
/// caller writes via `write_desc`; the head is `slots[0]`. Pure (no device
/// state), so it is unit-testable without a NIC (SLOPRING § 13).
fn build_tx_chain(
    slots: &[u16],
    hdr_pa: u64,
    hdr_len: u32,
    runs: &[(u64, u32)],
) -> Option<KVec<(u16, VirtqDesc)>> {
    // Need exactly one slot for the header plus one per payload run, and at
    // least one payload run (an empty datagram uses the inline copy path).
    if runs.is_empty() || slots.len() != runs.len() + 1 {
        return None;
    }
    let mut out = KVec::with_capacity(slots.len()).ok()?;
    // Header descriptor → first payload run.
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
    // One descriptor per coalesced pinned run; the last terminates the chain.
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

/// Test-only view into [`build_tx_chain`] for the SG-chain stest (no NIC): runs
/// the pure builder and flattens each descriptor to
/// `(slot, addr, len, flags, next)` so the harness can assert the link
/// structure without touching `VirtqDesc` internals.
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

/// Allocate a page and write the virtio-net header at the start.
/// Returns `(page, buffer_start)` where `buffer_start` points just past the header.
fn alloc_tx_page() -> Option<OwnedPageFrame> {
    let page = OwnedPageFrame::alloc_zeroed()?;
    page.write_at::<VirtioNetHdrV1>(0, &VirtioNetHdrV1::default());
    Some(page)
}

// =============================================================================
// ARP
// =============================================================================

fn transmit_arp_request(state: &mut VirtioNetState, target_ip: [u8; 4]) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() {
        return false;
    }

    let Some(mut tx_page) = alloc_tx_page() else {
        return false;
    };

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let frame_len = net::ETH_HEADER_LEN + ARP_HEADER_LEN;
    let total_len = hdr_len + frame_len;

    if total_len > PACKET_BUFFER_SIZE {
        return false;
    }

    {
        let Some(frame) = tx_page.slice_at_mut(hdr_len, frame_len) else {
            return false;
        };

        // Ethernet header
        frame[0..net::ETH_ADDR_LEN].copy_from_slice(&net::MacAddr::BROADCAST.0);
        frame[net::ETH_ADDR_LEN..net::ETH_ADDR_LEN * 2].copy_from_slice(&state.device.mac);
        frame[net::ETH_ADDR_LEN * 2..net::ETH_HEADER_LEN]
            .copy_from_slice(&net::EtherType::Arp.to_be_bytes());

        // ARP payload
        let a = net::ETH_HEADER_LEN;
        frame[a..a + 2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
        frame[a + 2..a + 4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
        frame[a + 4] = ARP_HLEN_ETHERNET;
        frame[a + 5] = ARP_PLEN_IPV4;
        frame[a + 6..a + 8].copy_from_slice(&ARP_OPER_REQUEST.to_be_bytes());
        frame[a + 8..a + 14].copy_from_slice(&state.device.mac);
        frame[a + 14..a + 18].copy_from_slice(&state.ipv4_addr);
        frame[a + 18..a + 24].copy_from_slice(&[0; net::ETH_ADDR_LEN]);
        frame[a + 24..a + 28].copy_from_slice(&target_ip);
    }

    submit_tx(state, tx_page, total_len as u32)
}

// =============================================================================
// Receive path
// =============================================================================

fn virtnet_prepost_rx_buffers(state: &mut VirtioNetState) {
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

    if posted > 0 {
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.device.rx_queue,
            VIRTIO_NET_QUEUE_RX,
        );
    }
}

fn parse_udp_header(payload: &[u8]) -> Option<(u16, u16, &[u8])> {
    if payload.len() < UDP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    let udp_len = u16::from_be_bytes([payload[4], payload[5]]) as usize;

    if udp_len < UDP_HEADER_LEN || udp_len > payload.len() {
        return None;
    }

    Some((src_port, dst_port, &payload[UDP_HEADER_LEN..udp_len]))
}

fn dispatch_rx_frame(state: &mut VirtioNetState, frame: &[u8]) {
    sniff_frame_for_members(state, frame);
    if frame.len() < net::ETH_HEADER_LEN {
        return;
    }

    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != net::EtherType::Ipv4.as_u16() {
        return;
    }
    if frame.len() < net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN {
        return;
    }

    let ip_off = net::ETH_HEADER_LEN;
    let ihl = ((frame[ip_off] & 0x0f) as usize) * 4;
    if ihl < net::IPV4_HEADER_LEN || frame.len() < ip_off + ihl {
        return;
    }

    let proto = frame[ip_off + 9];
    let src_ip: [u8; 4] = frame[ip_off + 12..ip_off + 16].try_into().unwrap_or([0; 4]);
    let dst_ip: [u8; 4] = frame[ip_off + 16..ip_off + 20].try_into().unwrap_or([0; 4]);
    let ip_payload = &frame[ip_off + ihl..];

    match proto {
        p if p == net::IpProtocol::Tcp.as_u8() => {
            let Some(hdr) = tcp::parse_header(ip_payload) else {
                return;
            };
            let hdr_len = hdr.header_len();
            if hdr_len < tcp::TCP_HEADER_LEN || ip_payload.len() < hdr_len {
                return;
            }
            let options = &ip_payload[tcp::TCP_HEADER_LEN..hdr_len];
            let payload = &ip_payload[hdr_len..];
            let now_ms = slopos_kernel_services::clock::uptime_ms();
            let actions = tcp::input(src_ip, dst_ip, &hdr, options, payload, now_ms);
            for seg in actions.segments() {
                let _ = socket::socket_send_tcp_segment(seg, &[]);
            }
            socket::socket_notify_tcp_activity(&actions);
        }
        p if p == net::IpProtocol::Udp.as_u8() => {
            if let Some((src_port, dst_port, udp_payload)) = parse_udp_header(ip_payload) {
                // Intercept DNS responses (src port 53) for the in-kernel resolver
                if src_port == net::dns::DNS_PORT {
                    let copy_len = udp_payload.len().min(512);
                    let mut dns_buf = DNS_RX_BUF.lock();
                    dns_buf.data[..copy_len].copy_from_slice(&udp_payload[..copy_len]);
                    dns_buf.len = copy_len;
                    drop(dns_buf);
                    DNS_RX_EVENT.signal();
                }
                // Always deliver to socket table too (userland might have a UDP
                // socket bound to port 53 for its own purposes).
                socket::socket_deliver_udp_from_dispatch(
                    src_ip,
                    dst_ip,
                    src_port,
                    dst_port,
                    udp_payload,
                );
            }
        }
        p if p == net::IpProtocol::Icmp.as_u8() => {
            let _ = (src_ip, dst_ip, ip_payload);
        }
        _ => {}
    }
}

fn virtnet_poll(state: &mut VirtioNetState, budget: u32) -> usize {
    let mut processed = 0usize;
    let mut posted = 0usize;
    let _ = virtnet_clean_tx(state);

    while (processed as u32) < budget {
        let Some(used) = state.device.rx_queue.try_pop_used() else {
            break;
        };

        let idx = (used.id as usize) % RX_RING_SIZE;
        let Some(page) = state.rx_buffers[idx].take() else {
            continue;
        };

        let hdr_len = size_of::<VirtioNetHdrV1>();
        if (used.len as usize) > hdr_len {
            let payload_len = (used.len as usize) - hdr_len;
            if let Some(frame) = page.slice_at(hdr_len, payload_len) {
                dispatch_rx_frame(state, frame);
            }
        }

        processed += 1;

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
        }
    }

    if posted > 0 {
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.device.rx_queue,
            VIRTIO_NET_QUEUE_RX,
        );
    }

    processed
}

fn poll_one_rx_frame(state: &mut VirtioNetState, out_payload: Option<&mut [u8]>) -> Option<usize> {
    poll_one_rx_frame_timeout(state, out_payload, DHCP_REQUEST_TIMEOUT_MS)
}

fn poll_one_rx_frame_timeout(
    state: &mut VirtioNetState,
    out_payload: Option<&mut [u8]>,
    timeout_ms: u32,
) -> Option<usize> {
    let rx_page = OwnedPageFrame::alloc_zeroed()?;
    let rx_phys = rx_page.phys_u64();

    state.device.rx_queue.write_desc(
        0,
        VirtqDesc {
            addr: rx_phys,
            len: PACKET_BUFFER_SIZE as u32,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0,
        },
    );

    state.device.rx_queue.submit(0);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.rx_queue,
        VIRTIO_NET_QUEUE_RX,
    );

    if !DHCP_RX_EVENT.wait_timeout_ms(timeout_ms) {
        // Intentional leak: device may still be DMA-ing.
        let _ = rx_page.into_phys();
        return None;
    }
    let used = state.device.rx_queue.try_pop_used()?;

    let hdr_len = size_of::<VirtioNetHdrV1>();
    if (used.len as usize) <= hdr_len {
        return Some(0);
    }

    let payload_len = (used.len as usize) - hdr_len;
    let frame = rx_page.slice_at(hdr_len, payload_len)?;
    sniff_frame_for_members(state, frame);

    if let Some(dst) = out_payload {
        let copy_len = payload_len.min(dst.len());
        dst[..copy_len].copy_from_slice(&frame[..copy_len]);
        Some(copy_len)
    } else {
        Some(payload_len)
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

// =============================================================================
// DHCP client
// =============================================================================

fn transmit_dhcp_packet(state: &mut VirtioNetState, payload: &[u8]) -> bool {
    transmit_udp_packet_locked(
        state,
        [0; 4],
        net::Ipv4Addr::BROADCAST.0,
        dhcp::UDP_PORT_CLIENT,
        dhcp::UDP_PORT_SERVER,
        payload,
    )
}

fn parse_dhcp_reply(frame: &[u8], xid: u32, expected_type: u8) -> Option<dhcp::DhcpOffer> {
    let min_len =
        net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN;
    if frame.len() < min_len {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != net::EtherType::Ipv4.as_u16() {
        return None;
    }

    let ip_off = net::ETH_HEADER_LEN;
    let ihl = ((frame[ip_off] & 0x0f) as usize) * 4;
    if ihl < net::IPV4_HEADER_LEN
        || frame.len() < ip_off + ihl + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN
    {
        return None;
    }
    if frame[ip_off + 9] != net::IpProtocol::Udp.as_u8() {
        return None;
    }

    let udp_off = ip_off + ihl;
    let src_port = u16::from_be_bytes([frame[udp_off], frame[udp_off + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_off + 2], frame[udp_off + 3]]);
    if src_port != dhcp::UDP_PORT_SERVER || dst_port != dhcp::UDP_PORT_CLIENT {
        return None;
    }

    let udp_len = u16::from_be_bytes([frame[udp_off + 4], frame[udp_off + 5]]) as usize;
    if udp_len < UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN || frame.len() < udp_off + udp_len {
        return None;
    }

    let payload = &frame[udp_off + UDP_HEADER_LEN..udp_off + udp_len];
    dhcp::parse_bootp_reply(payload, xid, expected_type)
}

fn wait_for_dhcp_reply(
    state: &mut VirtioNetState,
    xid: u32,
    expected_type: u8,
) -> Option<dhcp::DhcpOffer> {
    // Heap-allocate the receive buffer. An inline
    // `[u8; PACKET_BUFFER_SIZE]` would add ~2 KiB to this function's
    // stack frame, over the stack-safety gate.
    let mut frame = slopos_ostd::KVec::<u8>::zeroed(PACKET_BUFFER_SIZE).ok()?;
    for _ in 0..DHCP_RX_MAX_POLLS {
        let len = poll_one_rx_frame(state, Some(frame.as_mut()))?;
        if len > 0
            && let Some(reply) = parse_dhcp_reply(&frame[..len], xid, expected_type)
        {
            return Some(reply);
        }
    }
    None
}

/// Use the preferred value unless it's zeroed, in which case fall back.
fn or_fallback(preferred: [u8; 4], fallback: [u8; 4]) -> [u8; 4] {
    if preferred != [0; 4] {
        preferred
    } else {
        fallback
    }
}

fn dhcp_acquire_lease(state: &mut VirtioNetState) -> Option<dhcp::DhcpLease> {
    let xid = DHCP_XID_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let mut packet = [0u8; 320];

    let discover_len = dhcp::build_discover(state.device.mac, xid, &mut packet);
    if !transmit_dhcp_packet(state, &packet[..discover_len]) {
        return None;
    }

    let offer = wait_for_dhcp_reply(state, xid, dhcp::MSG_OFFER)?;
    let request_len = dhcp::build_request(state.device.mac, xid, offer, &mut packet);
    if !transmit_dhcp_packet(state, &packet[..request_len]) {
        return None;
    }

    let ack = wait_for_dhcp_reply(state, xid, dhcp::MSG_ACK)?;

    let lease = dhcp::DhcpLease {
        ipv4: ack.yiaddr,
        subnet_mask: or_fallback(ack.subnet_mask, offer.subnet_mask),
        router: or_fallback(ack.router, offer.router),
        dns: or_fallback(ack.dns, offer.dns),
    };
    if lease.is_valid() { Some(lease) } else { None }
}

// =============================================================================
// PCI probe
// =============================================================================

/// Drain one NAPI burst: poll the NIC RX ring up to `NAPI_BUDGET`,
/// run each packet through ingress, then drain the loopback queue.
///
/// Returns the number of packets the NIC produced this call so the
/// kthread can re-arm the waker when budget was exhausted (more work
/// likely pending). Re-entrancy is structurally impossible — a single
/// `NapiWaker` -> single-kthread shape means only `napi_thread_entry`
/// ever calls this from outside a `#[cfg(test)]` site.
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
        sniff_packet_for_members(pkt.payload());
        ingress::net_rx(handle, pkt);
    }
    NAPI_CONTEXT.add_processed(processed);

    // Loopback packets are queued internally by `LoopbackDev::tx` and
    // need to be drained back through the ingress pipeline.
    poll_loopback();

    processed
}

/// Poll the loopback device and feed packets through ingress.
///
/// Called from the NAPI loop and idle wakeup.  The loopback device (DevIndex 0)
/// stores TX'd packets internally; this function drains them back through
/// `net_rx()` so they appear as received local traffic.
fn poll_loopback() {
    use slopos_net::netdev::DEVICE_REGISTRY;
    use slopos_net::types::DevIndex;

    // The loopback device is at DevIndex(0).  Use the registry to poll it.
    let lo_packets = DEVICE_REGISTRY.poll_rx_by_index(DevIndex(0), 32, &PACKET_POOL);

    for pkt in lo_packets {
        // Loopback packets bypass MAC filtering — they go straight
        // to IPv4/ARP dispatch.  We call ipv4::handle_rx directly.
        let checksum_rx = true; // Loopback doesn't need checksum verification.
        let data = pkt.payload();
        if data.len() >= slopos_net::ETH_HEADER_LEN {
            let ethertype_raw = u16::from_be_bytes([data[12], data[13]]);
            let mut pkt = pkt;
            // Set layer offsets.
            pkt.set_l2(pkt.head());
            pkt.set_l3(pkt.head() + slopos_net::ETH_HEADER_LEN as u16);
            // Pull Ethernet header.
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

/// Force a NAPI poll cycle from a non-IRQ context.
///
/// Production callers were retired in Phase 2: the IRQ-driven netpoll
/// kthread is the sole RX path. This entry remains for test fixtures
/// (`net/src/tests/tcp_live_tests.rs`, ICMP/NAPI scheduling tests) and
/// for the host wrapper that needs deterministic synchronous drain.
/// Wakes the kthread so it sees one more burst when scheduled and
/// runs a direct burst here for the caller that cannot wait.
pub fn virtnet_force_napi_poll() {
    NAPI_WAKER.arm_and_wake();
    let _ = run_napi_burst();
    slopos_net::socket::socket_process_timers();
}

/// Wake-only counterpart to [`virtnet_force_napi_poll`]. Registered
/// with `slopos_net::napi::register_wake_napi` so loopback tx (and
/// any future intra-kernel producer) can wake the netpoll kthread
/// without re-entering the synchronous poll loop. Required because
/// the loopback `tx` runs under `LoopbackDev::inner` lock — a
/// synchronous `virtnet_napi_poll_loop` here would re-enter
/// `VIRTIO_NET_STATE` and risk lock recursion in tightly-coupled
/// configurations.
pub fn virtnet_wake_napi() {
    NAPI_WAKER.arm_and_wake();
}

/// Long-lived netpoll worker (threaded NAPI).
///
/// Spawned once per virtio-net probe via [`slopos_ostd::spawn_kernel_io!`]
/// at [`TaskPriority::KernelIo`] (strictly above any user task). The
/// kthread parks on [`NAPI_WAKER`]; the per-queue IRQ handler
/// (`virtio_net_irq_handler`) calls `arm_and_wake` to wake the
/// kthread on each completion.
///
/// Post-burst recheck (Phase 2): after each burst returns we peek the
/// virtio used-ring index without taking the state lock. If the IRQ
/// arrived between the last `try_pop_used` and `wait` re-park, the
/// peek catches it and we re-arm the waker so the next `wait` returns
/// immediately. Mirrors Linux NAPI's `napi_complete_done` pending
/// recheck and closes the lost-wakeup window structurally.
///
/// Budget exhaustion: when `processed >= NAPI_BUDGET` more work is
/// likely pending; we re-arm and `yield_with_deadline(Immediate)` so
/// any equal-or-higher-priority task gets a chance to run before the
/// next burst.
fn napi_thread_entry(token: slopos_ostd::sync::kernel_io_task::KernelIoToken<'static>) {
    use slopos_ostd::sync::kernel_io_task::{Deadline, yield_with_deadline};
    loop {
        NAPI_WAKER.wait();
        let processed = run_napi_burst();
        slopos_net::socket::socket_process_timers();

        // Post-burst recheck: catch packets the IRQ committed between
        // the last `poll_rx` drain and now. `has_pending_rx` reads
        // the used-ring atomically with no driver-state lock.
        if has_pending_rx() {
            NAPI_WAKER.rearm();
        }

        if processed >= NAPI_CONTEXT.budget() {
            NAPI_WAKER.rearm();
            yield_with_deadline(&token, Deadline::Immediate);
        }
    }
}

/// Lock-free pending-RX peek. Reads the virtio used-ring `idx` and
/// compares against the driver-cached `last_used_idx` — the same
/// comparison `try_pop_used` performs, but without acquiring the
/// `VIRTIO_NET_STATE` spinlock. Safe to call concurrently with the
/// kthread because virtio used.idx is producer-monotonic (modulo
/// 16-bit wrap) and the only way `last_used_idx` advances is via the
/// kthread itself.
fn has_pending_rx() -> bool {
    // We take the lock briefly here because `Virtqueue::has_pending`
    // requires `&Virtqueue` and `state.device.rx_queue` is behind
    // `VIRTIO_NET_STATE`. A future refactor can expose the queue's
    // used-ring base independently so this becomes a pure atomic
    // read — for now the lock is held for ~5 ns over a single
    // `read_volatile`.
    let state = VIRTIO_NET_STATE.lock();
    state.device.rx_queue.has_pending()
}

/// Net-timer kthread (Phase-1).
///
/// Separated from `napi_thread_entry` so the RX hot path is not
/// charged for `net_timer_process` cost. Sleeps `NET_TIMER_PERIOD_MS`
/// (50 ms) between ticks; can be woken sooner by code that arms a
/// soon-firing wheel entry via `TIMER_WAKER.arm_and_wake()`.
///
/// The kthread runs at [`TaskPriority::KernelIo`] (strictly above
/// user tasks) so ARP aging, TCP retransmit, IP-reassembly expire,
/// and delayed-ACK fire on time even when user-space is busy.
fn net_timer_thread_entry(token: slopos_ostd::sync::kernel_io_task::KernelIoToken<'static>) {
    use slopos_ostd::sync::kernel_io_task::{Deadline, yield_with_deadline};
    const NET_TIMER_PERIOD_MS: u32 = 50;
    loop {
        // Wait either for the period to expire or for an explicit
        // wake from `TIMER_WAKER.arm_and_wake()`. Either way, run
        // one round of timer processing.
        let _woken_early = TIMER_WAKER.wait_timeout_ms(NET_TIMER_PERIOD_MS);
        slopos_net::timer::net_timer_process();
        slopos_net::socket::socket_process_timers();
        // Yield with deadline so any equal-priority task gets a
        // chance to run between ticks. The next iteration's
        // `wait_timeout_ms` parks again.
        yield_with_deadline(&token, Deadline::Immediate);
    }
}

/// Per-queue interrupt handler for virtio-net.
///
/// `queue_idx` is 0 for RX, 1 for TX (matching the queue setup order
/// in `virtio_net_probe`). On either queue the handler wakes the
/// netpoll kthread via the IRQ-safe [`NapiWaker::arm_and_wake`]; on
/// queue 0 it additionally pulses the DHCP edge so the boot-time
/// DHCP loop unblocks. Hard-IRQ path is intentionally tiny — no
/// scheduler interaction, no protocol work — to keep IRQ-disabled
/// time minimal.
fn virtio_net_irq_handler(queue_idx: u8) {
    match queue_idx {
        0 => {
            DHCP_RX_EVENT.signal();
            NAPI_WAKER.arm_and_wake();
        }
        1 => {
            NAPI_WAKER.arm_and_wake();
        }
        _ => {}
    }
}

/// Single 12-arg `klog_info!` pulled out so its `format_args!` scratch
/// stays local.
#[inline(never)]
fn virtio_net_log_dhcp_lease(lease: &dhcp::DhcpLease) {
    klog_info!(
        "virtio-net: DHCP lease ip={}.{}.{}.{} gw={}.{}.{}.{} dns={}.{}.{}.{}",
        lease.ipv4[0],
        lease.ipv4[1],
        lease.ipv4[2],
        lease.ipv4[3],
        lease.router[0],
        lease.router[1],
        lease.router[2],
        lease.router[3],
        lease.dns[0],
        lease.dns[1],
        lease.dns[2],
        lease.dns[3]
    );
}

/// DHCP acquire + lease-field propagation + prepost-RX + PACKET_POOL init
/// + netstack configuration. Extracted from `virtio_net_probe` so its
/// locked-state block scratch doesn't inflate the probe's frame.
/// Returns `false` on `KBox::try_new(VirtioNetDev)` allocation failure.
#[inline(never)]
fn virtio_net_acquire_dhcp_and_register(state: &mut VirtioNetState) -> bool {
    let dhcp_lease = dhcp_acquire_lease(state);
    if let Some(ref lease) = dhcp_lease {
        state.ipv4_addr = lease.ipv4;
        state.subnet_mask = lease.subnet_mask;
        state.router = lease.router;
        state.dns = lease.dns;
        virtio_net_log_dhcp_lease(lease);
    } else {
        klog_info!("virtio-net: DHCP lease unavailable");
    }

    virtnet_prepost_rx_buffers(state);

    PACKET_POOL.init();

    use slopos_net::netdev::DEVICE_REGISTRY;
    let dev: KBox<dyn slopos_net::netdev::NetDevice + Send + Sync> =
        match KBox::try_new(VirtioNetDev) {
            Ok(d) => d,
            Err(_) => {
                klog_info!("virtio-net: alloc failed");
                return false;
            }
        };
    if let Some(handle) = DEVICE_REGISTRY.register(dev) {
        let actual_idx = handle.index();
        klog_info!(
            "virtio-net: registered as dev {} in device registry",
            actual_idx
        );

        if let Some(ref lease) = dhcp_lease {
            use slopos_net::netstack::NET_STACK;
            use slopos_net::types::Ipv4Addr;
            NET_STACK.configure(
                actual_idx,
                Ipv4Addr::from_bytes(lease.ipv4),
                Ipv4Addr::from_bytes(lease.subnet_mask),
                Ipv4Addr::from_bytes(lease.router),
                [Ipv4Addr::from_bytes(lease.dns), Ipv4Addr::UNSPECIFIED],
            );
        }

        set_device_handle(handle);
    } else {
        klog_info!("virtio-net: failed to register in device registry");
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

    // --- MSI-X / MSI interrupt setup ---
    // Request 2 vectors: one for RX (queue 0), one for TX (queue 1).
    // setup_interrupts allocates the IDT vectors via OSTD's IrqAllocator,
    // registers per-queue closures that call virtio_net_irq_handler, and
    // programs the device's MSI-X/MSI capability.
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
        // Queues are set up in place inside the heap-resident state so the
        // ~200-byte `Virtqueue`s never land on this probe's stack frame
        // (2 KiB frame gate). Both must be enabled before DRIVER_OK
        // (VirtIO spec §3.1.1).
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
        state.ipv4_addr = [0; 4];
        state.subnet_mask = [0; 4];
        state.router = [0; 4];
        state.dns = [0; 4];

        if !virtio_net_acquire_dhcp_and_register(&mut state) {
            return Err(PciProbeError::OutOfMemory);
        }
    }

    // Sync-kick: user-task syscall paths call `napi::kick` to drain
    // the RX ring inline on the caller's CPU, ensuring the wake
    // observes the most recent committed used-ring state.
    slopos_net::napi::register_kick(virtnet_force_napi_poll);
    // Async-wake: non-IRQ producers (loopback tx) signal the netpoll
    // kthread without re-entering the synchronous poll machinery.
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
        scan_members: virtio_net_scan_members,
        is_ready: virtio_net_is_ready,
        get_info: virtio_net_get_info,
    };
    register_net_driver_services(&NET_DRIVER_SVC);

    // Phase-1 threaded NAPI: the netpoll kthread runs at
    // TaskPriority::KernelIo (strictly above any user task) and
    // parks indefinitely on NAPI_WAKER. The IRQ handler
    // (`virtio_net_irq_handler`) calls `NAPI_WAKER.arm_and_wake()`
    // to wake the kthread on each NIC RX/TX completion. The
    // `spawn_kernel_io!` macro emits a hidden trampoline that
    // constructs a `KernelIoToken` and hands it to the entry —
    // every yield in the kthread must name a `Deadline` so the
    // pre-refactor "sleep_current_task_ms(1) in a tight loop"
    // starvation pattern is structurally unreachable.
    if let Err(err) = slopos_ostd::spawn_kernel_io!("netpoll", napi_thread_entry) {
        klog_info!(
            "virtio-net: failed to spawn netpoll kernel thread ({:?})",
            err
        );
        DEVICE_CLAIMED.reset();
        return Err(PciProbeError::OutOfMemory);
    }
    // Phase-1.7 net-timer split: timer-wheel processing runs in a
    // dedicated `KernelIo` kthread so the RX hot path is not charged
    // for `net_timer_process` cost and timer-driven work (ARP aging,
    // TCP retransmit, delayed-ACK, IP-reassembly expire) fires on its
    // own cadence regardless of NIC activity.
    if let Err(err) = slopos_ostd::spawn_kernel_io!("net-timer", net_timer_thread_entry) {
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
// =============================================================================
// Driver registration & public API
// =============================================================================

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

pub fn virtio_net_scan_members(out: &mut [UserNetMember], active_probe: bool) -> usize {
    if out.is_empty() {
        return 0;
    }

    // Snapshot config, register self, and gather probe targets under a
    // SHORT lock. The `VIRTIO_NET_STATE` SpinLock disables preemption while
    // held, so it MUST NOT be held across the probe sleeps below: a task
    // that deschedules with a preempt-disabling lock held strands the lock
    // on the blocked task and unbalances the per-CPU preempt count. The
    // lock is therefore re-acquired in short windows around each sleep.
    let mut targets = [[0u8; 4]; 3];
    let mut target_count = 0usize;
    {
        let mut state = VIRTIO_NET_STATE.lock();
        if !state.device.ready || !link_is_up(&state) {
            return 0;
        }

        let self_mac = state.device.mac;
        let self_ipv4 = state.ipv4_addr;
        if self_ipv4 != [0; 4] {
            add_or_update_member(&mut state, self_mac, self_ipv4, USER_NET_MEMBER_FLAG_IPV4);
        }

        if active_probe {
            if state.router != [0; 4] {
                targets[target_count] = state.router;
                target_count += 1;
            }

            // Probe .2 and .3 in the local subnet as simple neighbor discovery
            for last_octet in [2u8, 3] {
                if state.ipv4_addr != [0; 4] && target_count < targets.len() {
                    let mut t = state.ipv4_addr;
                    t[3] = last_octet;
                    targets[target_count] = t;
                    target_count += 1;
                }
            }
        }
    }

    if active_probe {
        for target in &targets[..target_count] {
            // Transmit the ARP under the lock, then release it before sleeping.
            {
                let mut state = VIRTIO_NET_STATE.lock();
                let _ = transmit_arp_request(&mut state, *target);
            }
            // The NAPI kthread (TaskPriority::KernelIo) wakes on each RX IRQ
            // via NAPI_WAKER and drains the ring inline. We wait for it to
            // make progress with NO lock held so this task can deschedule
            // safely — a short sleep is enough on the local segment.
            slopos_kernel_services::driver_runtime::sleep_current_task_ms(SCAN_RX_TIMEOUT_MS);
            {
                let mut state = VIRTIO_NET_STATE.lock();
                let _ = virtnet_poll(&mut state, NAPI_BUDGET);
            }
        }

        // Drain any remaining rx frames from the above probes (no sleep here).
        let mut state = VIRTIO_NET_STATE.lock();
        for _ in 0..8 {
            if virtnet_poll(&mut state, NAPI_BUDGET) == 0 {
                break;
            }
        }
    }

    let state = VIRTIO_NET_STATE.lock();
    let copy_count = out.len().min(state.member_count);
    out[..copy_count].copy_from_slice(&state.members[..copy_count]);
    copy_count
}

pub fn virtio_net_queue_sizes() -> Option<(u16, u16)> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some((state.device.rx_queue.size, state.device.tx_queue.size))
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
    if !state.device.ready || state.ipv4_addr == [0; 4] {
        return None;
    }
    Some(state.ipv4_addr)
}

pub fn virtio_net_get_info(out: &mut UserNetInfo) {
    let state = VIRTIO_NET_STATE.lock();
    out.nic_ready = u8::from(state.device.ready);
    out.link_up = u8::from(state.device.ready && link_is_up(&state));
    out.mac = state.device.mac;
    out.mtu = state.device.mtu;

    // prefer NetStack as the source of truth for IP config.
    if let Some(iface) = slopos_net::netstack::NET_STACK.first_iface() {
        out.ipv4 = iface.ipv4_addr.0;
        out.subnet_mask = iface.netmask.0;
        out.gateway = iface.gateway.0;
        out.dns = iface.dns[0].0;
    } else {
        // Legacy fallback — still read from VirtioNetState.
        out.ipv4 = state.ipv4_addr;
        out.subnet_mask = state.subnet_mask;
        out.gateway = state.router;
        out.dns = state.dns;
    }
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

// =============================================================================
// DNS resolver accessors
// =============================================================================

/// Return the DHCP-provided DNS server address, or `None` if not configured.
pub fn virtio_net_dns() -> Option<[u8; 4]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || state.dns == [0; 4] {
        return None;
    }
    Some(state.dns)
}

/// Clear any stale DNS response buffer.
pub fn dns_rx_clear() {
    DNS_RX_EVENT.try_consume();
    let mut buf = DNS_RX_BUF.lock();
    buf.len = 0;
}

/// Wait for a DNS response with timeout. Returns `true` if signaled.
///
/// The IRQ handler signals `NAPI_EVENT` (not `DNS_RX_EVENT`), so we must
/// poll NAPI inline after each wakeup to process RX frames; NAPI's
/// `dispatch_rx_frame` intercepts DNS replies and signals `DNS_RX_EVENT`.
pub fn dns_rx_wait(timeout_ms: u32) -> bool {
    let start = slopos_kernel_services::clock::uptime_ms();
    loop {
        // Already arrived?
        if DNS_RX_EVENT.try_consume() {
            return true;
        }
        let elapsed = slopos_kernel_services::clock::uptime_ms() - start;
        if elapsed >= timeout_ms as u64 {
            return false;
        }
        let remaining = (timeout_ms as u64 - elapsed) as u32;
        // The NAPI kthread (TaskPriority::KernelIo) processes
        // incoming frames inline on each RX IRQ via `NAPI_WAKER`.
        // DNS replies route through `dispatch_rx_frame` →
        // `dns_intercept_response`, which sets `DNS_RX_EVENT`.
        // Sleep for a bounded slice, then re-check.
        slopos_kernel_services::driver_runtime::sleep_current_task_ms(remaining.min(20));
    }
}

/// Read the most recent DNS response into the provided buffer.
/// Returns the number of bytes copied.
pub fn dns_rx_read(out: &mut [u8]) -> usize {
    let buf = DNS_RX_BUF.lock();
    let copy_len = buf.len.min(out.len());
    out[..copy_len].copy_from_slice(&buf.data[..copy_len]);
    copy_len
}

// =============================================================================
// Test-only accessors
// =============================================================================

/// Return a snapshot of the MSI-X state for the claimed VirtIO-net device.
///
/// Only available in test builds (`test-hooks` feature).  Returns `None` if the
/// device was not probed or MSI-X was not configured (i.e. MSI fallback).
#[cfg(feature = "test-hooks")]
pub fn virtio_net_msix_state() -> Option<VirtioMsixState> {
    VIRTIO_NET_STATE.lock().msix_state.clone()
}
