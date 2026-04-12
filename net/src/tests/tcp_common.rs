//! Shared helpers for the TCP test suites.
//!
//! Before this module existed, `fn reset()` was copy-pasted into each test
//! file and `establish_connection()` had two subtly different signatures.
//! Everything new lives here; existing tests migrate via one-line imports.
//!
//! The helpers are intentionally thin so each test's intent stays obvious.
//! Matchers and macros that take on non-trivial state (like [`SegmentMatcher`])
//! carry inline documentation.

use alloc::vec::Vec;

use crate::socket;
use crate::tcp::{
    self, Actions, ConnId, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH, TCP_FLAG_RST, TCP_FLAG_SYN,
    TcpHeader, TcpOutSegment, TcpTuple,
};
#[cfg(feature = "itests")]
use crate::timer::NET_TIMER_WHEEL;

// -----------------------------------------------------------------------------
// Canonical addresses for unit tests
// -----------------------------------------------------------------------------

pub const LOCAL_IP: [u8; 4] = [10, 0, 0, 1];
pub const REMOTE_IP: [u8; 4] = [10, 0, 0, 2];
pub const REMOTE_PORT: u16 = 80;
/// Peer's Initial Send Sequence number, used in synthetic SYN+ACK and in
/// `establish_connection()` so tests can compute expected `rcv_nxt` values.
pub const PEER_ISS: u32 = 7000;

// -----------------------------------------------------------------------------
// Reset
// -----------------------------------------------------------------------------

/// Canonical "between-tests" reset.
///
/// Drops the socket table first (it holds TCP indices), then the TCP table,
/// then clears the mock clock so the next test starts at `t=0` in wall-time
/// terms.  Use this at the top of every test, even if only TCP primitives are
/// exercised — socket state can leak across tests through the demux tables
/// otherwise.
pub fn reset_all() {
    socket::socket_reset_all();
    tcp::reset_all();
    #[cfg(feature = "itests")]
    tcp::clock::MockClock::clear();
}

// -----------------------------------------------------------------------------
// 3-way handshake helpers
// -----------------------------------------------------------------------------

/// Outcome of a successfully driven client-side 3-way handshake.
///
/// Both sides' ISS are exposed so tests that need to craft synthetic segments
/// referring to either sequence space can do so without re-reading the
/// connection.
pub struct EstablishedConn {
    pub id: ConnId,
    pub local_port: u16,
    pub our_iss: u32,
    pub peer_iss: u32,
}

/// Drive a client-side 3-way handshake against the canonical addresses and
/// [`REMOTE_PORT`].  Returns the connection id and both sides' ISS.
///
/// Uses [`PEER_ISS`] for the synthetic peer ISS so tests can predict peer
/// sequence numbers deterministically.
pub fn establish_connection() -> EstablishedConn {
    let (id, syn_seg) =
        tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("tcp_connect should succeed");
    let our_iss = syn_seg.seq_num;
    let local_port = syn_seg.tuple.local_port;

    let syn_ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: local_port,
        seq_num: PEER_ISS,
        ack_num: our_iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &syn_ack, &[], &[], 0);

    EstablishedConn {
        id,
        local_port,
        our_iss,
        peer_iss: PEER_ISS,
    }
}

// -----------------------------------------------------------------------------
// Segment injection
// -----------------------------------------------------------------------------

/// Build a minimal TCP header in host-byte-order form suitable for passing to
/// [`tcp::input`].  Tests rarely need window scaling or urgent pointers,
/// so they are defaulted.
pub fn make_header(
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
) -> TcpHeader {
    TcpHeader {
        src_port,
        dst_port,
        seq_num: seq,
        ack_num: ack,
        data_offset: 5,
        flags,
        window_size: window,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// Inject a raw segment into the TCP state machine.  This is the single
/// primitive; the typed helpers below all delegate to it.
pub fn inject(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> Actions {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    tcp::input(src_ip, dst_ip, &hdr, &[], payload, tcp::clock::now_ms())
}

/// Inject a payload-carrying ACK from the peer of an established connection.
pub fn inject_data(conn: &EstablishedConn, seq: u32, ack: u32, data: &[u8]) -> Actions {
    inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        seq,
        ack,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        data,
    )
}

/// Inject a bare ACK from the peer.
pub fn inject_ack(conn: &EstablishedConn, seq: u32, ack: u32) -> Actions {
    inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        seq,
        ack,
        TCP_FLAG_ACK,
        &[],
    )
}

/// Inject a FIN from the peer.
pub fn inject_fin(conn: &EstablishedConn, seq: u32, ack: u32) -> Actions {
    inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        seq,
        ack,
        TCP_FLAG_FIN | TCP_FLAG_ACK,
        &[],
    )
}

/// Inject a RST from the peer.
pub fn inject_rst(conn: &EstablishedConn, seq: u32, ack: u32) -> Actions {
    inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        seq,
        ack,
        TCP_FLAG_RST | TCP_FLAG_ACK,
        &[],
    )
}

// -----------------------------------------------------------------------------
// Transmit draining
// -----------------------------------------------------------------------------

/// Poll once for an outgoing segment.  Returns `None` if the connection has
/// nothing to send under the current window.
pub fn poll_once(id: ConnId) -> Option<(TcpOutSegment, Vec<u8>)> {
    let mut buf = [0u8; 1500];
    tcp::poll_transmit(id, &mut buf, tcp::clock::now_ms())
        .map(|(seg, len)| (seg, buf[..len].to_vec()))
}

/// Drain `tcp_poll_transmit` until it returns `None`.  Used by data-transfer
/// tests that want to observe exactly what bytes were serialized.
pub fn drain_transmit(id: ConnId) -> Vec<(TcpOutSegment, Vec<u8>)> {
    let mut out = Vec::new();
    while let Some(item) = poll_once(id) {
        out.push(item);
    }
    out
}

// -----------------------------------------------------------------------------
// State-access macros
// -----------------------------------------------------------------------------

/// Access the `DataState` of a PCB inside a closure under the `PCB_TABLE`
/// lock.  Panics if the PCB doesn't exist or isn't in the `Data` state.
///
/// Usage: `with_data_state!(conn.id, |d| assert_eq!(d.snd_nxt.raw(), ...));`
#[macro_export]
macro_rules! with_data_state {
    ($id:expr, |$d:ident| $body:expr) => {{
        let table = crate::tcp::table::PCB_TABLE.lock();
        let pcb = table.get($id).expect("PCB should exist");
        match &pcb.state {
            crate::tcp::PcbState::Data($d) => $body,
            other => panic!("expected Data state, got {}", other.name()),
        }
    }};
}

// -----------------------------------------------------------------------------
// Segment matcher
// -----------------------------------------------------------------------------

/// Fluent matcher for [`TcpOutSegment`].
///
/// Replaces hand-written `assert_eq_test!(seg.field, expected, "label")` lines
/// with a builder-style chain that collects failures and reports them all at
/// once.  Call `.check()` to retrieve a `Result<(), &'static str>` suitable
/// for the test harness's `fail!` macro.
///
/// Example:
/// ```ignore
/// SegmentMatcher::new(&seg)
///     .has_flag(TCP_FLAG_SYN)
///     .no_flag(TCP_FLAG_FIN)
///     .window_gt(0)
///     .check()
///     .map_err(|e| fail!("{}", e))?;
/// ```
#[must_use]
pub struct SegmentMatcher<'a> {
    seg: &'a TcpOutSegment,
    failures: Vec<&'static str>,
}

impl<'a> SegmentMatcher<'a> {
    pub fn new(seg: &'a TcpOutSegment) -> Self {
        Self {
            seg,
            failures: Vec::new(),
        }
    }

    pub fn seq(mut self, s: u32) -> Self {
        if self.seg.seq_num != s {
            self.failures.push("seq_num mismatch");
        }
        self
    }

    pub fn ack(mut self, a: u32) -> Self {
        if self.seg.ack_num != a {
            self.failures.push("ack_num mismatch");
        }
        self
    }

    pub fn flags_eq(mut self, f: u8) -> Self {
        if self.seg.flags != f {
            self.failures.push("flags mismatch");
        }
        self
    }

    pub fn has_flag(mut self, f: u8) -> Self {
        if (self.seg.flags & f) == 0 {
            self.failures.push("required flag missing");
        }
        self
    }

    pub fn no_flag(mut self, f: u8) -> Self {
        if (self.seg.flags & f) != 0 {
            self.failures.push("forbidden flag set");
        }
        self
    }

    pub fn window_gt(mut self, w: u16) -> Self {
        if self.seg.window_size <= w {
            self.failures.push("window_size not greater than bound");
        }
        self
    }

    pub fn mss(mut self, mss: u16) -> Self {
        if self.seg.mss != mss {
            self.failures.push("mss mismatch");
        }
        self
    }

    pub fn tuple(mut self, t: TcpTuple) -> Self {
        if self.seg.tuple != t {
            self.failures.push("tuple mismatch");
        }
        self
    }

    /// Consume the matcher and return Ok or the first failure.  Tests that
    /// need to surface the failing label can inspect the collected list
    /// themselves before calling `.check()`.
    pub fn check(self) -> Result<(), &'static str> {
        match self.failures.first() {
            Some(&msg) => Err(msg),
            None => Ok(()),
        }
    }
}

// -----------------------------------------------------------------------------
// Timer wheel driving (used by mock-clock tests in P2.3 onwards)
// -----------------------------------------------------------------------------

/// Advance the mock clock by `ms` milliseconds then dispatch any expired
/// timers.  Returns the number of timers dispatched.
#[cfg(feature = "itests")]
pub fn tick_ms(ms: u64) -> usize {
    tcp::clock::MockClock::advance(ms);
    dispatch_fired_timers()
}

/// Drive the net timer wheel once and dispatch every expired TCP timer
/// through its real callback.  Returns the number of timers processed.
///
/// Mirrors the production dispatcher in `net/src/timer.rs` so that tests can
/// exercise retransmit, keepalive, and TIME_WAIT paths deterministically
/// without waiting for the scheduler's real tick.
#[cfg(feature = "itests")]
pub fn dispatch_fired_timers() -> usize {
    use crate::timer::TimerKind;

    let fired = NET_TIMER_WHEEL.tick();
    let mut count = 0usize;
    for timer in fired {
        match timer.kind {
            TimerKind::TcpRetransmit => {
                let _ = tcp::on_retransmit(timer.key);
            }
            TimerKind::TcpKeepalive => {
                let _ = tcp::on_keepalive(timer.key);
            }
            TimerKind::TcpTimeWait => {
                tcp::on_time_wait_expire(timer.key);
            }
            _ => {}
        }
        count += 1;
    }
    count
}
