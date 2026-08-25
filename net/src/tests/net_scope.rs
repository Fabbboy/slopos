//! RAII fixture for kernel net tests.
//!
//! Holds four things as one unit because they are one decision: where a packet
//! a test sends goes, which wheel a `schedule` lands in, what `now_ms()`
//! returns, and whether the kernel's own net threads may run. Splitting them
//! leaves a window in which a timer is scheduled against one clock and fired
//! against another, or in which a synthetic PCB shares a 4-tuple with the wire.
//!
//! The destination is RFC 5737 TEST-NET-1, which no host network may source, so
//! the fixture's 4-tuple is unreachable from the wire even with the ingress gate
//! open.

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
    Alloc,
    NoDeviceSlot,
    Attach,
}

static PANIC_CLEANUP_REGISTERED: StateFlag = StateFlag::new();

fn ensure_panic_cleanup_registered() {
    if PANIC_CLEANUP_REGISTERED.enter() {
        slopos_ostd::panic_recovery::register_panic_cleanup(panic_reopen_dataplane);
    }
}

/// A test that panicked inside a scope never ran its `Drop`. Only the three
/// process-global switches are reset here: each one, left set, silently
/// disables networking for the rest of the boot, and all three are atomics, so
/// this takes no lock a panicking context might already hold.
fn panic_reopen_dataplane() {
    ingress::quiesce_clear();
    timer::select_test_wheel(false);
    MockClock::clear();
}

#[must_use = "the fixture is torn down when the guard drops; bind it to a named local"]
pub struct NetTestScope {
    sink: KArc<BlackholeDev>,
    dev: DevIndex,
    mock_clock: bool,
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

        Ok(Self {
            sink,
            dev,
            mock_clock: mock_ms.is_some(),
        })
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
        drop(NEIGHBOR_CACHE.remove(self.dev, Ipv4Addr(TEST_PEER_IP)));

        // Emptied before the swap back, so no token minted here can later be
        // cancelled against the live stack's wheel.
        timer::TEST_TIMER_WHEEL.clear();
        timer::select_test_wheel(false);

        if self.mock_clock {
            MockClock::clear();
        }

        route::remove_device_routes(self.dev);
        iface::detach(self.dev);
        if !DEVICE_REGISTRY.unregister(self.dev) {
            klog_info!("net_scope: blackhole dev {} already gone", self.dev);
        }

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
            iface::detach(dev);
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
    ROUTE_TABLE.add(RouteEntry {
        prefix: Ipv4Addr(TEST_LOCAL_IP).masked(TEST_PREFIX_LEN),
        prefix_len: TEST_PREFIX_LEN,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    });

    Ok(())
}

fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
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
        "peer is TEST-NET-1, which no host network sources"
    );

    // Exactly one: the SYN. A second would mean the pre-seeded neighbour was
    // missed and an ARP request was built for a peer that does not exist.
    assert_eq_test!(scope.tx_packets(), 1, "the SYN went into the sink");

    pass!()
}

slopos_testing::stest!(name = test_net_scope_is_hermetic, suite = net_scope);
