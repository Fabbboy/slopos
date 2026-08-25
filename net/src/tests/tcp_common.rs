//! Shared helpers for the TCP test suites: reset, handshake drivers, segment
//! injection, transmit draining, matchers and mock-clock timer dispatch.

use slopos_ostd::KVec;

use crate::socket;
use crate::tcp::{
    self, Actions, ConnId, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH, TCP_FLAG_RST, TCP_FLAG_SYN,
    TcpHeader, TcpOutSegment, TcpTuple,
};

/// RFC 5737 TEST-NET-1, matching [`crate::tests::net_scope`].
///
/// The address class buys nothing on its own: DHCP installs a `0.0.0.0/0`
/// default route, so 192.0.2.2 falls through to the physical NIC exactly as
/// 10.0.0.2 did. What makes a fixture PCB unreachable is the scope's own
/// metric-0 `/24` at the blackhole sink, which wins longest-prefix over that
/// default. These constants exist so a test's 4-tuple matches the one the
/// scope routes — a test that transmits without holding a scope puts the frame
/// on the wire whichever subnet it names.
pub const LOCAL_IP: [u8; 4] = crate::tests::net_scope::TEST_LOCAL_IP;
pub const REMOTE_IP: [u8; 4] = crate::tests::net_scope::TEST_PEER_IP;
pub const REMOTE_PORT: u16 = 80;
/// Peer's Initial Send Sequence number for synthetic SYN+ACKs, so tests can
/// compute expected `rcv_nxt` values.
pub const PEER_ISS: u32 = 7000;

/// Canonical "between-tests" reset.
///
/// Socket table first: it holds TCP indices. Use it at the top of every test
/// even when only TCP primitives are exercised — socket state leaks across
/// tests through the demux tables otherwise.
pub fn reset_all() {
    socket::socket_reset_all();
    tcp::reset_all();
    #[cfg(feature = "test-hooks")]
    tcp::clock::MockClock::clear();
}

/// Outcome of a successfully driven client-side 3-way handshake.
pub struct EstablishedConn {
    pub id: ConnId,
    pub local_port: u16,
    pub our_iss: u32,
    pub peer_iss: u32,
}

/// Drive a client-side 3-way handshake against the canonical addresses and
/// [`REMOTE_PORT`], using [`PEER_ISS`] as the synthetic peer ISS.
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

    // Nagle off: sub-MSS deferral would perturb tests that do not test it.
    tcp::set_nodelay(id, true);

    EstablishedConn {
        id,
        local_port,
        our_iss,
        peer_iss: PEER_ISS,
    }
}

/// Build a minimal host-byte-order TCP header for [`tcp::input`]; window
/// scaling and urgent pointers are defaulted.
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

/// Inject a raw segment; the typed helpers below all delegate to it.
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

pub fn inject_with_options(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    options: &[u8],
    payload: &[u8],
) -> Actions {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    tcp::input(src_ip, dst_ip, &hdr, options, payload, tcp::clock::now_ms())
}

/// [`inject_with_options`] writing into a caller-provided slot: a discarded
/// ~400 B `Actions` return slot per call site inflates the caller's frame past
/// the 2 KiB stack-size gate.
///
/// `#[inline(never)]` keeps that return slot inside this helper's frame.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn inject_with_options_into(
    out: &mut Actions,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    options: &[u8],
    payload: &[u8],
) {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    *out = tcp::input(src_ip, dst_ip, &hdr, options, payload, tcp::clock::now_ms());
}

/// [`inject`] discarding the result. `#[inline(never)]` so the ~400 B `Actions`
/// slot stays in this frame: at opt-level 0 a caller with two `let _ =
/// tcp::input(..)` sites carries two of them and lands over the 2 KiB gate.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn inject_discarding(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
    now_ms: u64,
) {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    let _ = tcp::input(src_ip, dst_ip, &hdr, &[], payload, now_ms);
}

/// [`inject`] keeping only the first outgoing segment's sequence number, for
/// the same frame-size reason as [`inject_discarding`].
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn inject_for_reply_seq(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    now_ms: u64,
) -> Option<u32> {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    let actions = tcp::input(src_ip, dst_ip, &hdr, &[], &[], now_ms);
    actions.segments().next().map(|seg| seg.seq_num)
}

/// [`inject`] keeping only whether a reset was sent to `src_port`, for the same
/// frame-size reason as [`inject_discarding`].
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn inject_for_reset_to(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    now_ms: u64,
) -> bool {
    let hdr = make_header(src_port, dst_port, seq, ack, flags, 32768);
    let actions = tcp::input(src_ip, dst_ip, &hdr, &[], &[], now_ms);
    actions
        .segments()
        .any(|seg| seg.flags & TCP_FLAG_RST != 0 && seg.tuple.remote_port == src_port)
}

/// A window-advertising ACK from the peer, result discarded.
#[inline(never)]
pub fn inject_window_update(local_port: u16, seq: u32, ack: u32, window: u16, now_ms: u64) {
    let hdr = make_header(REMOTE_PORT, local_port, seq, ack, TCP_FLAG_ACK, window);
    let _ = tcp::input(REMOTE_IP, LOCAL_IP, &hdr, &[], &[], now_ms);
}

/// Build a 12-byte TCP Timestamp option (NOP+NOP+TSopt).
pub fn build_tsopt(tsval: u32, tsecr: u32) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[0] = 1; // NOP
    buf[1] = 1; // NOP
    buf[2] = 8; // Timestamp kind
    buf[3] = 10; // Timestamp length
    buf[4..8].copy_from_slice(&tsval.to_be_bytes());
    buf[8..12].copy_from_slice(&tsecr.to_be_bytes());
    buf
}

/// Drive a client-side 3WHS with timestamps negotiated.
pub fn establish_connection_with_ts() -> EstablishedConn {
    let (id, syn_seg) =
        tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("tcp_connect should succeed");
    let our_iss = syn_seg.seq_num;
    let local_port = syn_seg.tuple.local_port;
    assert!(syn_seg.timestamp.is_some(), "SYN should carry TSopt");
    let our_tsval = syn_seg.timestamp.unwrap().0;

    let syn_ack = TcpHeader {
        src_port: REMOTE_PORT,
        dst_port: local_port,
        seq_num: PEER_ISS,
        ack_num: our_iss.wrapping_add(1),
        data_offset: 8, // 20 + 12 options = 32 bytes → 8 words
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let tsopt = build_tsopt(1000, our_tsval);
    let _ = tcp::input(
        REMOTE_IP,
        LOCAL_IP,
        &syn_ack,
        &tsopt,
        &[],
        tcp::clock::now_ms(),
    );

    tcp::set_nodelay(id, true);

    EstablishedConn {
        id,
        local_port,
        our_iss,
        peer_iss: PEER_ISS,
    }
}

/// Poll once for an outgoing segment. Returns `None` if the connection has
/// nothing to send under the current window.
pub fn poll_once(id: ConnId) -> Option<(TcpOutSegment, KVec<u8>)> {
    let mut buf = [0u8; 1500];
    tcp::poll_transmit(id, &mut buf, tcp::clock::now_ms()).map(|(seg, len, _)| {
        let mut payload = KVec::<u8>::with_capacity(len).expect("test alloc");
        payload.extend_from_slice(&buf[..len]).expect("test alloc");
        (seg, payload)
    })
}

/// Drain `tcp_poll_transmit` until it returns `None`.
pub fn drain_transmit(id: ConnId) -> KVec<(TcpOutSegment, KVec<u8>)> {
    let mut out: KVec<(TcpOutSegment, KVec<u8>)> = KVec::new();
    while let Some(item) = poll_once(id) {
        let _ = out.push(item);
    }
    out
}

/// `with_data_state!`'s three failure arms, out of line: their format-args
/// state is charged to the caller's frame at opt-level 0.
#[cold]
#[inline(never)]
pub fn wds_listener_id() -> ! {
    panic!("with_data_state! requires a non-listener ConnId");
}

#[cold]
#[inline(never)]
pub fn wds_missing_pcb() -> ! {
    panic!("PCB should exist");
}

#[cold]
#[inline(never)]
pub fn wds_wrong_state(name: &str) -> ! {
    panic!("expected Data state, got {}", name);
}

/// Access the `DataState` of a PCB inside its per-slot lock. Panics if the PCB
/// does not exist or is not in the `Data` state.
///
/// The body executes in the caller's scope — a guard-binding block, not a
/// closure — so `return` and `?` propagate to the enclosing function.
#[macro_export]
macro_rules! with_data_state {
    ($id:expr, |$d:ident| $body:expr) => {{
        let __wds_id: crate::tcp::ConnId = $id;
        if __wds_id.is_listener() {
            crate::tests::tcp_common::wds_listener_id();
        }
        let __wds_guard = crate::tcp::table::TCP_PCB_SLOTS[__wds_id.linear_slot()].lock();
        let __wds_slot = match __wds_guard.as_ref() {
            Some(slot) => slot,
            None => crate::tests::tcp_common::wds_missing_pcb(),
        };
        match &__wds_slot.pcb.state {
            crate::tcp::PcbState::Data($d) => $body,
            other => crate::tests::tcp_common::wds_wrong_state(other.name()),
        }
    }};
}

/// Fluent matcher for [`TcpOutSegment`]: a chain of checks collects failures,
/// and `.check()` returns a `Result<(), &'static str>` suitable for the test
/// harness's `fail!` macro.
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
    failures: KVec<&'static str>,
}

impl<'a> SegmentMatcher<'a> {
    pub fn new(seg: &'a TcpOutSegment) -> Self {
        Self {
            seg,
            failures: KVec::new(),
        }
    }

    pub fn seq(mut self, s: u32) -> Self {
        if self.seg.seq_num != s {
            let _ = self.failures.push("seq_num mismatch");
        }
        self
    }

    pub fn ack(mut self, a: u32) -> Self {
        if self.seg.ack_num != a {
            let _ = self.failures.push("ack_num mismatch");
        }
        self
    }

    pub fn flags_eq(mut self, f: u8) -> Self {
        if self.seg.flags != f {
            let _ = self.failures.push("flags mismatch");
        }
        self
    }

    pub fn has_flag(mut self, f: u8) -> Self {
        if (self.seg.flags & f) == 0 {
            let _ = self.failures.push("required flag missing");
        }
        self
    }

    pub fn no_flag(mut self, f: u8) -> Self {
        if (self.seg.flags & f) != 0 {
            let _ = self.failures.push("forbidden flag set");
        }
        self
    }

    pub fn window_gt(mut self, w: u16) -> Self {
        if self.seg.window_size <= w {
            let _ = self.failures.push("window_size not greater than bound");
        }
        self
    }

    pub fn mss(mut self, mss: u16) -> Self {
        if self.seg.mss != Some(mss) {
            let _ = self.failures.push("mss mismatch");
        }
        self
    }

    pub fn tuple(mut self, t: TcpTuple) -> Self {
        if self.seg.tuple != t {
            let _ = self.failures.push("tuple mismatch");
        }
        self
    }

    /// Consume the matcher and return `Ok` or the first collected failure.
    pub fn check(self) -> Result<(), &'static str> {
        match self.failures.first() {
            Some(&msg) => Err(msg),
            None => Ok(()),
        }
    }
}

/// Advance the mock clock by `ms` milliseconds then dispatch any expired
/// timers. Returns the number of timers dispatched.
#[cfg(feature = "test-hooks")]
pub fn tick_ms(ms: u64) -> usize {
    tcp::clock::MockClock::advance(ms);
    dispatch_fired_timers()
}

/// Drive the net timer wheel once and dispatch every expired TCP timer through
/// its real callback. Mirrors the production dispatcher in `net/src/timer.rs`.
///
/// `timer::wheel()`, not `NET_TIMER_WHEEL`: under a scope a `schedule` lands in
/// the test wheel, and draining the live one would fire nothing the test armed
/// while leaving what it armed for the live thread to fire instead.
#[cfg(feature = "test-hooks")]
pub fn dispatch_fired_timers() -> usize {
    use crate::timer::TimerKind;

    let fired = crate::timer::wheel().process_due();
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
