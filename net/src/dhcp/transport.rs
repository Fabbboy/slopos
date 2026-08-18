//! Binding the DHCP state machine to a real interface: transmit, arm timers,
//! install or withdraw a lease, announce the transition to the monitors.
//!
//! Runs on the network timer thread, never inline in PCI probe: probe holds
//! `VIRTIO_NET_STATE` cli- and preempt-disabled, and taking a lease there would
//! span a heap allocation and multi-second descheduling waits.
//!
//! The client lives under a leaf `SpinLock` and *no action is performed while it
//! is held*: `step` runs under the lock, the action is carried out after it
//! drops.

use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use slopos_abi::net::{
    NET_DHCP_REASON_NAK, NET_DHCP_REASON_OK, NET_DHCP_REASON_TIMEOUT, NET_EV_DHCP, NetEvent,
};

use super::client::{
    DhcpAction, DhcpBinding, DhcpClient, DhcpDest, DhcpEvent, DhcpState, UnbindReason,
};
use super::codec::{UDP_PORT_CLIENT, UDP_PORT_SERVER};
use crate::iface::{self, AddrOrigin};
use crate::netmon::netmon_post;
use crate::timer::{NET_TIMER_WHEEL, TimerKind};
use crate::types::{DevIndex, Ipv4Addr};

/// Clients the kernel runs, one per interface.
const MAX_CLIENTS: usize = slopos_abi::net::NET_MAX_IFACES;

struct Slot {
    /// `None` means the slot is free.
    dev: Option<DevIndex>,
    ifindex: u32,
    /// Bumped whenever the timers this client armed stop being meaningful; every
    /// armed timer carries its epoch and a handler drops one that mismatches.
    ///
    /// The wheel has no cancel-by-key, and checking the client's state is not
    /// enough: after a refused lease the state is legitimately `Bound` again.
    epoch: u16,
    client: DhcpClient,
}

struct ClientTable {
    slots: [Slot; MAX_CLIENTS],
}

const FREE_SLOT: Slot = Slot {
    dev: None,
    ifindex: 0,
    epoch: 0,
    client: DhcpClient::new([0; 6], 0),
};

/// Packs the interface and the epoch into the wheel's single `u32` key, so a
/// stale timer is identifiable as stale. The epoch wraps; it only has to differ
/// from the epoch in flight.
const fn timer_key(ifindex: u32, epoch: u16) -> u32 {
    ((epoch as u32) << 16) | (ifindex & 0xFFFF)
}

const fn key_ifindex(key: u32) -> u32 {
    key & 0xFFFF
}

const fn key_epoch(key: u32) -> u16 {
    (key >> 16) as u16
}

static LISTENER: ListenerInit = ListenerInit::new();

struct ListenerInit(slopos_ostd::sync::InitFlag);

impl ListenerInit {
    const fn new() -> Self {
        Self(slopos_ostd::sync::InitFlag::new())
    }
    fn init_once_then(&self, f: fn() -> bool) {
        if self.0.init_once() && !f() {
            klog_info!("dhcp: could not claim UDP port 68");
        }
    }
}

static CLIENTS: SpinLock<ClientTable> = SpinLock::new(
    ClientTable {
        slots: [const { FREE_SLOT }; MAX_CLIENTS],
    },
    lock_class!("DHCP_CLIENTS", LOCK_LEVEL_RESOURCE),
);

/// Everything the actions need, read out under the client lock so the work
/// itself happens with nothing held.
#[derive(Clone, Copy)]
struct SlotContext {
    dev: DevIndex,
    ifindex: u32,
    epoch: u16,
    addr: [u8; 4],
    state: DhcpState,
}

pub fn is_running(dev: DevIndex) -> bool {
    let table = CLIENTS.lock();
    table.slots.iter().any(|s| s.dev == Some(dev))
}

/// The client's state for `dev`, as `NET_DHCP_*`.
pub fn state_of(dev: DevIndex) -> Option<u8> {
    let table = CLIENTS.lock();
    table
        .slots
        .iter()
        .find(|s| s.dev == Some(dev))
        .map(|s| s.client.state().to_abi())
}

/// The lease seconds and server the client for `dev` last agreed, for
/// `NET_Q_DHCP`.
pub fn lease_of(dev: DevIndex) -> Option<(u32, u32, u32, [u8; 4])> {
    let table = CLIENTS.lock();
    table.slots.iter().find(|s| s.dev == Some(dev)).map(|s| {
        (
            s.client.lease_secs(),
            s.client.t1_secs(),
            s.client.t2_secs(),
            s.client.server_id(),
        )
    })
}

/// Start a client on `dev`, replacing any client already running there.
///
/// Returns `false` if the table is full or the device has no interface.
pub fn start(dev: DevIndex) -> bool {
    let Some(row) = iface::get_by_dev(dev) else {
        return false;
    };
    // Here rather than from a boot step so the listener exists before the
    // DISCOVER this call is about to send.
    LISTENER.init_once_then(init);
    // A predictable transaction id is answerable by anyone who can guess it.
    let seed = slopos_kernel_services::platform::rng_next() as u32;

    {
        let mut table = CLIENTS.lock();
        let slot = match table.slots.iter_mut().find(|s| s.dev == Some(dev)) {
            Some(existing) => existing,
            None => match table.slots.iter_mut().find(|s| s.dev.is_none()) {
                Some(free) => free,
                None => return false,
            },
        };
        slot.dev = Some(dev);
        slot.ifindex = row.ifindex;
        slot.client.reset(row.mac.0, seed);
    }

    let _ = iface::set_dhcp_managed(row.ifindex, true);
    drive(dev, DhcpEvent::Start);
    true
}

/// Stop the client on `dev`, releasing the lease first if it holds one.
pub fn stop(dev: DevIndex) {
    stop_with(dev, false)
}

/// [`stop`], optionally leaving the interface marked DHCP-managed.
///
/// The flag is the only memory that this interface gets its address from a
/// lease; clearing it would make `ip link set eth0 down; ip link set eth0 up` a
/// one-way door out of DHCP.
pub fn stop_with(dev: DevIndex, keep_managed: bool) {
    let ifindex = {
        let table = CLIENTS.lock();
        match table.slots.iter().find(|s| s.dev == Some(dev)) {
            Some(slot) => slot.ifindex,
            None => return,
        }
    };
    drive(dev, DhcpEvent::Stop);
    if !keep_managed {
        let _ = iface::set_dhcp_managed(ifindex, false);
    }

    let mut table = CLIENTS.lock();
    if let Some(slot) = table.slots.iter_mut().find(|s| s.dev == Some(dev)) {
        slot.dev = None;
    }
}

/// Step the client on `dev` and carry out what it asked for.
///
/// The single place a transmit frame is declared: at 320 bytes each, two on one
/// call path put the frame over the build's 2 KiB stack gate.
fn drive(dev: DevIndex, event: DhcpEvent<'_>) {
    let mut frame = [0u8; super::codec::DHCP_FRAME_LEN];
    let Some((action, frame_len, ctx)) = with_client(dev, event, &mut frame) else {
        return;
    };
    perform(action, &frame[..frame_len], ctx);
}

/// Feed an event to the client on `dev` and hand back what to do, with the
/// client lock already released.
fn with_client(
    dev: DevIndex,
    event: DhcpEvent<'_>,
    frame: &mut [u8; super::codec::DHCP_FRAME_LEN],
) -> Option<(DhcpAction, usize, SlotContext)> {
    let mut table = CLIENTS.lock();
    let slot = table.slots.iter_mut().find(|s| s.dev == Some(dev))?;
    let action = slot.client.step(event, crate::clock::now_ms());
    let frame_len = slot.client.frame().len();
    frame[..frame_len].copy_from_slice(slot.client.frame());
    if !matches!(action, DhcpAction::Idle | DhcpAction::Send { .. }) {
        slot.epoch = slot.epoch.wrapping_add(1);
    }
    let ctx = SlotContext {
        dev,
        ifindex: slot.ifindex,
        epoch: slot.epoch,
        addr: slot.client.address(),
        state: slot.client.state(),
    };
    Some((action, frame_len, ctx))
}

/// Same, resolved by interface index — what a timer key carries.
fn with_client_for_key(
    key: u32,
    event: DhcpEvent<'_>,
    frame: &mut [u8; super::codec::DHCP_FRAME_LEN],
) -> Option<(DhcpAction, usize, SlotContext)> {
    let ifindex = key_ifindex(key);
    let dev = {
        let table = CLIENTS.lock();
        let slot = table
            .slots
            .iter()
            .find(|s| s.dev.is_some() && (s.ifindex & 0xFFFF) == ifindex)?;
        if slot.epoch != key_epoch(key) {
            return None;
        }
        slot.dev?
    };
    with_client(dev, event, frame)
}

/// Carry out one action. **Nothing here runs under the client lock.**
fn perform(action: DhcpAction, frame: &[u8], ctx: SlotContext) {
    match action {
        DhcpAction::Idle => {}
        DhcpAction::Send { dest, retry_ms } => {
            transmit(ctx, dest, frame);
            arm_retransmit(ctx, retry_ms);
        }
        DhcpAction::Bind(binding) => {
            bind(ctx, &binding);
        }
        DhcpAction::Unbind(reason) => {
            unbind(ctx, reason);
        }
        DhcpAction::UnbindThenSend {
            reason,
            dest,
            retry_ms,
        } => {
            unbind(ctx, reason);
            transmit(ctx, dest, frame);
            arm_retransmit(ctx, retry_ms);
        }
        DhcpAction::SendThenUnbind { dest, reason } => {
            // The release must reach the wire while the address is still
            // configured: the source address identifies the client to the server.
            transmit(ctx, dest, frame);
            unbind(ctx, reason);
        }
    }
}

fn transmit(ctx: SlotContext, dest: DhcpDest, frame: &[u8]) {
    let result = match dest {
        DhcpDest::Broadcast => crate::udp::udp_broadcast_on_dev(
            ctx.dev,
            ctx.addr,
            UDP_PORT_CLIENT,
            UDP_PORT_SERVER,
            frame,
        ),
        DhcpDest::Server(server) => {
            // Falling back to broadcast when the server's MAC is unresolved
            // costs one frame; dropping the renewal because ARP aged out costs
            // the lease.
            match crate::neighbor::NEIGHBOR_CACHE.lookup(ctx.dev, Ipv4Addr(server)) {
                Some(mac) => crate::udp::udp_unicast_on_dev(
                    ctx.dev,
                    ctx.addr,
                    server,
                    UDP_PORT_CLIENT,
                    UDP_PORT_SERVER,
                    frame,
                    mac,
                ),
                None => crate::udp::udp_broadcast_on_dev(
                    ctx.dev,
                    ctx.addr,
                    UDP_PORT_CLIENT,
                    UDP_PORT_SERVER,
                    frame,
                ),
            }
        }
    };
    if let Err(err) = result {
        klog_info!("dhcp: transmit failed on dev {}: {:?}", ctx.dev, err);
    }
}

/// Install everything a lease says, in the order a consumer can follow:
/// address, then the routes derived from it, then the resolver.
fn bind(ctx: SlotContext, binding: &DhcpBinding) {
    let prefix_len = Ipv4Addr(binding.mask).to_u32_be().leading_ones() as u8;
    if let Err(err) = crate::iface_ctl::configure_ipv4(
        ctx.dev,
        Ipv4Addr(binding.addr),
        prefix_len,
        Ipv4Addr(binding.router),
        AddrOrigin::Dhcp,
    ) {
        klog_info!("dhcp: could not apply lease on dev {}: {:?}", ctx.dev, err);
        return;
    }

    if binding.dns != [0; 4] {
        crate::resolver::RESOLVER.set_from_lease(ctx.ifindex, &[Ipv4Addr(binding.dns)]);
    }

    arm_lease_timers(ctx, binding);
    post_dhcp_event(
        ctx.ifindex,
        DhcpState::Bound,
        NET_DHCP_REASON_OK,
        binding.lease_secs,
    );
    klog_info!(
        "dhcp: bound {}.{}.{}.{}/{} on iface {} for {}s",
        binding.addr[0],
        binding.addr[1],
        binding.addr[2],
        binding.addr[3],
        prefix_len,
        ctx.ifindex,
        binding.lease_secs
    );
}

/// Withdraw everything the lease installed.
fn unbind(ctx: SlotContext, reason: UnbindReason) {
    // Addresses before the routes derived from them: a connected route
    // outliving its address forwards onto a prefix the interface no longer
    // answers for.
    let _ = iface::retain_addrs(ctx.ifindex, |a| a.origin != AddrOrigin::Dhcp);
    crate::route::remove_device_routes(ctx.dev);
    crate::resolver::RESOLVER.clear_from_lease(ctx.ifindex);

    let abi_reason = match reason {
        UnbindReason::Nak => NET_DHCP_REASON_NAK,
        UnbindReason::Expired => NET_DHCP_REASON_TIMEOUT,
        UnbindReason::Stopped => NET_DHCP_REASON_OK,
    };
    post_dhcp_event(ctx.ifindex, ctx.state, abi_reason, 0);
    klog_info!("dhcp: unbound iface {} ({:?})", ctx.ifindex, reason);
}

fn post_dhcp_event(ifindex: u32, state: DhcpState, reason: u8, lease_secs: u32) {
    netmon_post(
        NET_EV_DHCP,
        ifindex,
        NetEvent::dhcp_payload(state.to_abi(), reason, lease_secs),
    );
}

fn arm_retransmit(ctx: SlotContext, retry_ms: u32) {
    NET_TIMER_WHEEL.schedule(
        u64::from(retry_ms),
        TimerKind::DhcpRetransmit,
        timer_key(ctx.ifindex, ctx.epoch),
    );
}

fn arm_lease_timers(ctx: SlotContext, binding: &DhcpBinding) {
    let ifindex = timer_key(ctx.ifindex, ctx.epoch);
    // A server that omitted option 51 is asking for a lease that never renews
    // and never expires.
    if binding.lease_secs == 0 {
        return;
    }
    NET_TIMER_WHEEL.schedule(
        u64::from(binding.t1_secs) * 1000,
        TimerKind::DhcpT1,
        ifindex,
    );
    NET_TIMER_WHEEL.schedule(
        u64::from(binding.t2_secs) * 1000,
        TimerKind::DhcpT2,
        ifindex,
    );
    NET_TIMER_WHEEL.schedule(
        u64::from(binding.lease_secs) * 1000,
        TimerKind::DhcpExpire,
        ifindex,
    );
}

/// A datagram arrived on port 68.
///
/// Registered as a kernel port listener, so it runs on the receive path with the
/// packet in hand.
pub fn on_udp_receive(_src_ip: [u8; 4], _src_port: u16, payload: &[u8]) {
    // Only the client can check the transaction id inside the payload, so offer
    // it to each running client and let the one whose transaction it is act.
    let devs = running_devices();
    let mut frame = [0u8; super::codec::DHCP_FRAME_LEN];
    for dev in devs.iter().flatten() {
        let Some((action, frame_len, ctx)) =
            with_client(*dev, DhcpEvent::Reply(payload), &mut frame)
        else {
            continue;
        };
        if matches!(action, DhcpAction::Idle) {
            continue;
        }
        perform(action, &frame[..frame_len], ctx);
        return;
    }
}

/// A snapshot of which devices have clients, so the receive path does not hold
/// the client lock while it works through them.
fn running_devices() -> [Option<DevIndex>; MAX_CLIENTS] {
    let table = CLIENTS.lock();
    let mut out = [None; MAX_CLIENTS];
    for (slot, out_slot) in table.slots.iter().zip(out.iter_mut()) {
        *out_slot = slot.dev;
    }
    out
}

pub fn on_retransmit_timer(key: u32) {
    dispatch_timer(key, DhcpEvent::Retransmit);
}

pub fn on_t1_timer(key: u32) {
    dispatch_timer(key, DhcpEvent::T1);
}

pub fn on_t2_timer(key: u32) {
    dispatch_timer(key, DhcpEvent::T2);
}

pub fn on_expire_timer(key: u32) {
    dispatch_timer(key, DhcpEvent::Expire);
}

fn dispatch_timer(key: u32, event: DhcpEvent<'_>) {
    let mut frame = [0u8; super::codec::DHCP_FRAME_LEN];
    let Some((action, frame_len, ctx)) = with_client_for_key(key, event, &mut frame) else {
        return;
    };
    perform(action, &frame[..frame_len], ctx);
}

/// Renew now, on request. Drives the same T1 transition the timer would, so
/// there is no second renewal path to keep in step.
pub fn renew_now(dev: DevIndex) {
    drive(dev, DhcpEvent::T1);
}

/// The link on `dev` changed. Carrier loss keeps the address and stops the
/// timers; carrier return confirms the address the client still holds.
pub fn on_carrier(dev: DevIndex, up: bool) {
    let event = if up {
        DhcpEvent::CarrierUp
    } else {
        DhcpEvent::CarrierDown
    };
    drive(dev, event);
}

/// The transaction id the client on `dev` currently has in flight. Test-only:
/// the client matches replies itself, and a second matcher would disagree.
#[cfg(feature = "test-hooks")]
pub fn xid_of(dev: DevIndex) -> Option<u32> {
    let table = CLIENTS.lock();
    table
        .slots
        .iter()
        .find(|s| s.dev == Some(dev))
        .map(|s| s.client.xid())
}

/// The timer key the client's current lease armed its expiry with. Test-only:
/// lets a test hold a key across a lease change and prove the epoch refuses it.
#[cfg(feature = "test-hooks")]
pub fn expire_key(dev: DevIndex) -> Option<u32> {
    let table = CLIENTS.lock();
    table
        .slots
        .iter()
        .find(|s| s.dev == Some(dev))
        .map(|s| timer_key(s.ifindex, s.epoch))
}

/// Register the port-68 listener. Called once, from network initialisation.
pub fn init() -> bool {
    crate::udp::register_port_listener(UDP_PORT_CLIENT, on_udp_receive)
}
