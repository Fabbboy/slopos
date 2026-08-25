//! RAII net-test fixture: a metric-0 `/24` at a blackhole sink outranks the DHCP
//! default route, and `ingress::quiesce_begin` gates physical RX for its life.

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
    NoRoute,
    Alloc,
    NoDeviceSlot,
    Attach,
}

static PANIC_CLEANUP_REGISTERED: StateFlag = StateFlag::new();

/// The live scope's sink, for the panic hook to retire; `NO_DEV` when none.
const NO_DEV: u32 = u32::MAX;
static LIVE_SINK: AtomicU32 = AtomicU32::new(NO_DEV);

fn ensure_panic_cleanup_registered() {
    if PANIC_CLEANUP_REGISTERED.enter() {
        slopos_ostd::panic_recovery::register_panic_cleanup(panic_reopen_dataplane);
    }
}

/// A panicking test never ran its `Drop`. The sink must be retired too, or the
/// next scope installs a second `192.0.2.0/24` at equal prefix and metric.
fn panic_reopen_dataplane() {
    // Before the gate reopens: a latched delayed ACK must not reach a live dataplane.
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

    /// [`enter`](Self::enter) with the mock clock pinned at `ms`.
    pub fn enter_at_mock_ms(ms: u64) -> Result<Self, ScopeError> {
        Self::build(Some(ms))
    }

    fn build(mock_ms: Option<u64>) -> Result<Self, ScopeError> {
        ensure_panic_cleanup_registered();

        // Before the wheel swap: a token is only cancellable in the wheel that
        // minted it.
        socket::socket_reset_all();

        ingress::quiesce_begin();

        let (sink, dev) = match arm_sink() {
            Ok(v) => v,
            Err(e) => {
                ingress::quiesce_end();
                return Err(e);
            }
        };

        // Unconditional: a predecessor's mock time would leak into `now_ms()`
        // callers that take no clock argument.
        MockClock::clear();
        if let Some(ms) = mock_ms {
            MockClock::install_at(ms);
        }

        timer::TEST_TIMER_WHEEL.clear();
        timer::select_test_wheel(true);

        // After the swap, so the entry's ArpExpire lands in the test wheel.
        let _ = NEIGHBOR_CACHE.insert_or_update(
            dev,
            Ipv4Addr(TEST_PEER_IP),
            TEST_PEER_MAC,
            clock::now_ms(),
        );

        LIVE_SINK.store(dev.0 as u32, Ordering::Release);

        Ok(Self { sink, dev })
    }

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

    /// Frames the fixture swallowed — proof a send did not reach a real device.
    pub fn tx_packets(&self) -> u64 {
        self.sink.tx_packets()
    }

    /// Fire the due timers of exactly `kind`; other kinds stay pending.
    pub fn dispatch_due(&self, kind: TimerKind) -> KVec<FiredTimer> {
        timer::wheel().process_due_matching(kind)
    }

    /// Complete the client 3WHS for `id` with a synthetic SYN+ACK.
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
        // Still on the test wheel, so this cancels the tokens it minted.
        socket::socket_reset_all();
        // The whole device: a send anywhere in the fixture's /24 leaves a pending
        // entry naming an ArpRetransmit token the clear below would strand.
        drop(NEIGHBOR_CACHE.flush_device(self.dev));

        // Emptied before the swap back, so no token minted here outlives it.
        timer::TEST_TIMER_WHEEL.clear();

        // Clock before wheel: deselecting first leaves the live wheel selected
        // while `now_ms()` still reads fast-forwarded time.
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
    // event on the way in.
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

    // A second would mean the pre-seeded neighbour was missed and ARP ran.
    assert_eq_test!(scope.tx_packets(), 1, "the SYN went into the sink");

    pass!()
}

slopos_testing::stest!(name = test_net_scope_is_hermetic, suite = net_scope);
