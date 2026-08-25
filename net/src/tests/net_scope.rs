//! RAII fixture for kernel net tests.
//!
//! Holds four things as one unit because they are one decision: where a packet
//! a test sends goes, which wheel a `schedule` lands in, what `now_ms()`
//! returns, and whether the kernel's own net threads may deliver a frame or
//! fire a timer. Splitting them leaves a window in which a timer is scheduled
//! against one clock and fired against another, or in which a synthetic PCB
//! shares a 4-tuple with the wire.
//!
//! Two mechanisms make a fixture PCB unreachable, and neither is the address
//! class. Outbound: the fixture installs a metric-0 `/24` at the blackhole sink,
//! which wins longest-prefix over the DHCP default route — without it,
//! `192.0.2.2` falls through to the physical NIC exactly as any other address
//! would. Inbound: `ingress::quiesce_begin` gates the physical RX path and both
//! net kthreads for the scope's life. RFC 5737 TEST-NET-1 is chosen so the
//! fixture's 4-tuple is one no host network sources, which is a second line of
//! defence and not the first.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_ostd::sync::StateFlag;
use slopos_ostd::{KArc, KVec, klog_info};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::clock::{self, MockClock};
use crate::iface::{self, AddrOrigin, AddrScope, IfaceAddr, IfaceKind};
use crate::ingress;
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::{DEVICE_REGISTRY, NetDevice};
use crate::route::{self, ROUTE_TABLE, RouteEntry};
use crate::socket;
use crate::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader};
use crate::tests::env_wait::errno_i32;
use crate::timer::{self, FiredTimer, TimerKind};
use crate::types::{DevIndex, Ipv4Addr, MacAddr};

use super::blackhole::BlackholeDev;

pub const TEST_LOCAL_IP: [u8; 4] = [192, 0, 2, 1];
pub const TEST_PEER_IP: [u8; 4] = [192, 0, 2, 2];
pub const TEST_PEER_PORT: u16 = 80;

const TEST_PREFIX_LEN: u8 = 24;
const TEST_DEV_MAC: MacAddr = MacAddr([0x02, 0x00, 0x5f, 0x00, 0x02, 0x01]);
const TEST_PEER_MAC: MacAddr = MacAddr([0x02, 0x00, 0x5f, 0x00, 0x02, 0x02]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeError {
    /// The fixture's route could not be installed, so its address would fall
    /// through to the live default route.
    NoRoute,
    Alloc,
    NoDeviceSlot,
    Attach,
}

static PANIC_CLEANUP_REGISTERED: StateFlag = StateFlag::new();

/// The live scope's sink, for the panic hook to retire. `NO_DEV` when no scope
/// is up. A `Drop` that never ran is the only way this is read, and only one
/// scope exists at a time, so a plain atomic is the whole synchronisation.
const NO_DEV: u32 = u32::MAX;
static LIVE_SINK: AtomicU32 = AtomicU32::new(NO_DEV);

fn ensure_panic_cleanup_registered() {
    if PANIC_CLEANUP_REGISTERED.enter() {
        slopos_ostd::panic_recovery::register_panic_cleanup(panic_reopen_dataplane);
    }
}

/// A test that panicked inside a scope never ran its `Drop`.
///
/// The three process-global switches come first because each one, left set,
/// silently disables networking for the rest of the boot, and all three are
/// atomics that take no lock a panicking context might hold. The sink is
/// retired after them, because leaving it registered is not cosmetic: the next
/// scope installs a *second* `192.0.2.0/24` at equal prefix and metric, and a
/// lookup that returns the stale device hands the SYN to whatever now occupies
/// that slot. The three calls take ordinary registry locks, which the panic
/// path has already unwound out of.
fn panic_reopen_dataplane() {
    // Before the gate reopens: a PCB the panicking test left with a latched
    // delayed ACK would otherwise survive into a live dataplane, and the
    // kthread would transmit it. Ordinary registry locks, which the panic path
    // has already unwound out of.
    socket::socket_reset_all();

    ingress::quiesce_clear();
    timer::select_test_wheel(false);
    MockClock::clear();

    let dev = LIVE_SINK.swap(NO_DEV, Ordering::AcqRel);
    if dev != NO_DEV {
        let dev = DevIndex(dev as usize);
        drop(NEIGHBOR_CACHE.flush_device(dev));
        route::remove_device_routes(dev);
        let _ = iface::detach(dev);
        DEVICE_REGISTRY.unregister(dev);
    }
}

#[must_use = "the fixture is torn down when the guard drops; bind it to a named local"]
pub struct NetTestScope {
    sink: KArc<BlackholeDev>,
    dev: DevIndex,
}

impl NetTestScope {
    pub fn enter() -> Result<Self, ScopeError> {
        Self::build(None)
    }

    /// [`enter`](Self::enter) with the mock clock pinned at `ms`, so every
    /// deadline the test creates is in mock time and an `advance` can cross it.
    pub fn enter_at_mock_ms(ms: u64) -> Result<Self, ScopeError> {
        Self::build(Some(ms))
    }

    fn build(mock_ms: Option<u64>) -> Result<Self, ScopeError> {
        ensure_panic_cleanup_registered();

        // Before the wheel swap: this cancels every live TCP and neighbour
        // token, and a token is only cancellable in the wheel that minted it.
        socket::socket_reset_all();

        ingress::quiesce_begin();

        let (sink, dev) = match arm_sink() {
            Ok(v) => v,
            Err(e) => {
                ingress::quiesce_end();
                return Err(e);
            }
        };

        // Cleared unconditionally, so the scope is a strict superset of
        // `tcp_common::reset_all()` and a caller can use it in place of one. A
        // predecessor's mock time surviving into an `enter()` scope would make
        // `check_zero_window_probe` and the timestamp option — both of which
        // read `now_ms()` directly rather than taking it as an argument — read
        // a clock nothing in this test set.
        MockClock::clear();
        if let Some(ms) = mock_ms {
            MockClock::install_at(ms);
        }

        // Anything a panicked predecessor left behind, before this scope's own
        // schedules start landing here.
        timer::TEST_TIMER_WHEEL.clear();
        timer::select_test_wheel(true);

        // After the swap, so the entry's ArpExpire lands in the test wheel and
        // the live stack's wheel gains nothing from the fixture.
        let _ = NEIGHBOR_CACHE.insert_or_update(
            dev,
            Ipv4Addr(TEST_PEER_IP),
            TEST_PEER_MAC,
            clock::now_ms(),
        );

        LIVE_SINK.store(dev.0 as u32, Ordering::Release);

        Ok(Self { sink, dev })
    }

    /// The sink the fixture's routes point at.
    pub fn dev(&self) -> DevIndex {
        self.dev
    }

    pub fn local_ip(&self) -> [u8; 4] {
        TEST_LOCAL_IP
    }

    pub fn peer_ip(&self) -> [u8; 4] {
        TEST_PEER_IP
    }

    pub fn peer_port(&self) -> u16 {
        TEST_PEER_PORT
    }

    /// Frames the fixture swallowed. A send to [`peer_ip`](Self::peer_ip)
    /// raises this and nothing else, so it is also the proof that the frame did
    /// not go out on a real device.
    pub fn tx_packets(&self) -> u64 {
        self.sink.tx_packets()
    }

    /// Fire the due timers of exactly `kind`. Entries of other kinds stay
    /// pending, so a fast-forward cannot consume — and discard — a timer the
    /// test does not own.
    pub fn dispatch_due(&self, kind: TimerKind) -> KVec<FiredTimer> {
        timer::wheel().process_due_matching(kind)
    }

    /// Complete the client 3WHS for `id` with a synthetic SYN+ACK carrying
    /// `peer_iss`, and notify the socket layer as the RX path would.
    pub fn inject_syn_ack(&self, id: tcp::ConnId, peer_iss: u32) -> Option<()> {
        let (tuple, iss) = tcp::with_pcb(id, |pcb| {
            let iss = match &pcb.state {
                tcp::PcbState::SynSent(s) => s.iss.raw(),
                tcp::PcbState::Data(d) => d.iss.raw(),
                _ => return None,
            };
            Some((pcb.tuple, iss))
        })
        .flatten()?;

        let syn_ack = TcpHeader {
            src_port: tuple.remote_port,
            dst_port: tuple.local_port,
            seq_num: peer_iss,
            ack_num: iss.wrapping_add(1),
            data_offset: 5,
            flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
            window_size: 32768,
            checksum: 0,
            urgent_ptr: 0,
        };

        let actions = tcp::input(
            tuple.remote_ip,
            tuple.local_ip,
            &syn_ack,
            &[],
            &[],
            clock::now_ms(),
        );
        socket::socket_notify_tcp_activity(&actions);
        Some(())
    }
}

impl Drop for NetTestScope {
    fn drop(&mut self) {
        // Still on the test wheel, so the tokens this cancels are the ones it
        // minted.
        socket::socket_reset_all();
        // The whole device, not just the pre-seeded peer: a send to any other
        // address in the fixture's /24 creates a pending entry holding queued
        // buffers and an ArpRetransmit token minted in the test wheel, and the
        // clear below would leave that entry naming an index in a wheel that no
        // longer holds it.
        drop(NEIGHBOR_CACHE.flush_device(self.dev));

        // Emptied before the swap back, so no token minted here can later be
        // cancelled against the live stack's wheel.
        timer::TEST_TIMER_WHEEL.clear();

        // Clock before wheel, and unconditional: deselecting first leaves a
        // window in which the live wheel is selected while `now_ms()` still
        // reads the test's fast-forwarded time, and every deadline in that
        // wheel looks due.
        MockClock::clear();
        timer::select_test_wheel(false);

        route::remove_device_routes(self.dev);
        let _ = iface::detach(self.dev);
        if !DEVICE_REGISTRY.unregister(self.dev) {
            klog_info!("net_scope: blackhole dev {} already gone", self.dev);
        }
        LIVE_SINK.store(NO_DEV, Ordering::Release);

        // Last: the live threads stay out until the tables they read are whole.
        ingress::quiesce_end();
    }
}

fn arm_sink() -> Result<(KArc<BlackholeDev>, DevIndex), ScopeError> {
    let sink = KArc::try_new(BlackholeDev::new(TEST_DEV_MAC)).map_err(|_| ScopeError::Alloc)?;
    let dyn_dev: KArc<dyn NetDevice + Send + Sync> = sink.clone();
    let handle = DEVICE_REGISTRY
        .register(dyn_dev)
        .ok_or(ScopeError::NoDeviceSlot)?;
    let dev = handle.index();

    match arm_addressing(dev) {
        Ok(()) => Ok((sink, dev)),
        Err(e) => {
            route::remove_device_routes(dev);
            let _ = iface::detach(dev);
            DEVICE_REGISTRY.unregister(dev);
            Err(e)
        }
    }
}

fn arm_addressing(dev: DevIndex) -> Result<(), ScopeError> {
    let ifindex = iface::attach(dev, IfaceKind::Ethernet, TEST_DEV_MAC, 1500, true, true)
        .map_err(|_| ScopeError::Attach)?;
    iface::add_addr(
        ifindex,
        IfaceAddr::permanent(
            Ipv4Addr(TEST_LOCAL_IP),
            TEST_PREFIX_LEN,
            AddrScope::Global,
            AddrOrigin::Static,
        ),
    )
    .map_err(|_| ScopeError::Attach)?;

    // `ROUTE_TABLE.add` rather than `route::add`: the fixture posts no netmon
    // event on the way in, and `route::remove_device_routes` on the way out
    // pairs with the `iface::detach` and `unregister` that do announce.
    // Checked: an unrouted fixture address falls through to whatever default
    // route the live stack has, which is the physical NIC — the one outcome the
    // scope exists to make impossible.
    if !ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr(TEST_LOCAL_IP).masked(TEST_PREFIX_LEN),
        prefix_len: TEST_PREFIX_LEN,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    }) {
        return Err(ScopeError::NoRoute);
    }

    Ok(())
}

/// Nothing the fixture sends can reach a real device, and nothing a real device
/// receives can reach the stack while it is up.
pub fn test_net_scope_is_hermetic() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};
    use slopos_abi::syscall::ERRNO_EINPROGRESS;

    let scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    assert_eq_test!(
        ROUTE_TABLE.lookup(Ipv4Addr(TEST_PEER_IP)),
        Some((scope.dev(), Ipv4Addr(TEST_PEER_IP))),
        "the fixture's /24 wins longest-prefix over the boot default route"
    );
    assert_test!(
        ingress::dataplane_quiesced(),
        "physical-NIC ingress is gated while the scope is up"
    );

    let sock = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if sock < 0 {
        return fail!("socket_create: {}", sock);
    }
    let sock = sock as u32;
    socket::socket_set_nonblocking(sock, true);

    let rc = socket::socket_connect(sock, scope.peer_ip(), scope.peer_port());
    assert_test!(
        rc == 0 || rc == errno_i32(ERRNO_EINPROGRESS),
        "connect to the blackhole peer starts a handshake"
    );

    let Some(id) = socket::socket_lookup_tcp_idx(sock) else {
        return fail!("no PCB after connect");
    };
    let Some(tuple) = tcp::with_pcb(id, |pcb| pcb.tuple) else {
        return fail!("PCB vanished");
    };
    assert_eq_test!(
        tuple.local_ip,
        scope.local_ip(),
        "source address came from the fixture's interface"
    );
    assert_eq_test!(
        tuple.remote_ip,
        scope.peer_ip(),
        "peer is the address the fixture's /24 routes at the sink"
    );

    // Exactly one: the SYN. A second would mean the pre-seeded neighbour was
    // missed and an ARP request was built for a peer that does not exist.
    assert_eq_test!(scope.tx_packets(), 1, "the SYN went into the sink");

    pass!()
}

slopos_testing::stest!(name = test_net_scope_is_hermetic, suite = net_scope);
