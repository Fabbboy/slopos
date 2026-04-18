//! TCP (Transmission Control Protocol) implementation — RFC 793 + RFC 7413.
//!
//! Provides TCP header parsing/construction, one's-complement checksum with
//! IPv4 pseudo-header, a full TCP state machine, connection table, three-way
//! handshake (active and passive open), and connection teardown.
//!
//! This module is purely protocol logic — it does **not** drive the NIC
//! directly.  Higher layers wire it into the VirtIO net driver
//! for actual packet I/O.

pub mod actions;
pub mod buffer;
pub mod challenge_ack;
pub mod checksum;
pub mod clock;
pub mod cong;
pub mod header;
pub mod isn;
pub mod listener;
pub mod pcb;
pub mod reasm;
pub mod retx;
pub mod rtt;
pub mod segment;
pub mod seq;
pub mod table;
pub mod tuple;

pub use actions::{Actions, MAX_SEGMENTS, MAX_TIMER_OPS, SocketNotify, TimerOp};
pub use tuple::{TcpError, TcpTuple};

pub use buffer::{
    DELAYED_ACK_MS, DELAYED_ACK_SEGMENTS, TCP_BUFFER_SIZE, TcpBuffer, TcpBufferPair, TcpRecvState,
    TcpSendState, ZWP_INTERVAL_MS,
};
pub use checksum::{tcp_checksum, verify_checksum};
pub use header::{
    DEFAULT_MSS, DEFAULT_WINDOW_SIZE, ParsedTcpOptions, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH,
    TCP_FLAG_RST, TCP_FLAG_SYN, TCP_FLAG_URG, TCP_HEADER_LEN, TCP_HEADER_MAX_LEN, TCP_OPT_END,
    TCP_OPT_MSS, TCP_OPT_MSS_LEN, TCP_OPT_NOP, TCP_OPT_WINDOW_SCALE, TCP_OPT_WINDOW_SCALE_LEN,
    TcpHeader, build_header, our_window_scale, parse_header, parse_tcp_options, write_header,
    write_mss_option, write_window_scale_option,
};
pub use pcb::data::{ClosePhase, DataState};
pub use pcb::{ObservedSocketState, PcbState, TcpState};
pub use pcb::{Pcb, SocketId};
pub use reasm::Assembler;
pub use segment::{TcpOutSegment, write_tcp_segment};
pub use seq::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};
pub use table::ConnId;

use self::cong::CongestionControl;
use self::segment::SegmentBuilder;
use self::table::{TCP_LISTENERS, TCP_SHARDS, TcpShard};
use crate::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};

use slopos_utils::klog_debug;

// =============================================================================
// Non-header constants
// =============================================================================

/// Initial retransmission timeout in milliseconds (RFC 6298 recommends 1s).
pub const INITIAL_RTO_MS: u32 = 1000;

/// Maximum retransmission timeout in milliseconds.
pub const MAX_RTO_MS: u32 = 60_000;

/// TIME_WAIT duration in milliseconds (2 × MSL, MSL = 30s).
pub const TIME_WAIT_MS: u64 = 60_000;

/// Maximum retransmission attempts before giving up.
pub const MAX_RETRANSMITS: u8 = 8;

/// FIN_WAIT_2 timeout in milliseconds (60 s, matches Linux `tcp_fin_timeout`).
pub const FIN_WAIT2_TIMEOUT_MS: u64 = 60_000;

const TICKS_PER_SEC: u64 = 100;
const TCP_KEEPALIVE_IDLE_TICKS: u64 = 7_200 * TICKS_PER_SEC;
const TCP_KEEPALIVE_INTERVAL_TICKS: u64 = 75 * TICKS_PER_SEC;
const TCP_KEEPALIVE_PROBES_MAX: u8 = 9;

// Re-export ISN generator (used internally).
pub(crate) use isn::generate_isn;

// =============================================================================
// Input entry point
// =============================================================================

/// Process an incoming TCP segment.
///
/// Looks up the connection in shards, falls back to listeners, dispatches
/// to the matching `Pcb::on_segment`, applies timer ops, installs child
/// PCBs from LISTEN accepts, and returns `Actions` for the caller to
/// drain (segments to send, socket-layer wake-ups, etc.).
pub fn input(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> Actions {
    let incoming_tuple = TcpTuple {
        local_ip: dst_ip,
        local_port: hdr.dst_port,
        remote_ip: src_ip,
        remote_port: hdr.src_port,
    };

    // Phase 1: try exact match in the hash-indexed shard.
    let shard_idx = table::tcp_hash(&incoming_tuple);
    {
        let mut shard = TCP_SHARDS[shard_idx].lock();
        if let Some(slot) = shard.find_exact(&incoming_tuple) {
            let id = ConnId::new_shard(shard_idx, slot);
            return input_process_established(&mut shard, id, slot, hdr, options, payload, now_ms);
        }
    } // drop shard lock

    // Phase 2: try listener wildcard match.
    let (actions, parent_sock) =
        match input_try_listener(&incoming_tuple, hdr, options, payload, now_ms) {
            ListenerOutcome::Matched(a, s) => (a, s),
            ListenerOutcome::NoMatchSendRst => {
                return input_no_match_rst(hdr, dst_ip, src_ip);
            }
            ListenerOutcome::NoMatchDrop => return Actions::new(),
        };

    // Install child PCB from LISTEN accept.
    if actions.accepted.is_some() {
        install_accepted_child(&incoming_tuple, &actions, hdr, parent_sock);
    }

    actions
}

/// What `input_try_listener` resolved to.
enum ListenerOutcome {
    Matched(Actions, Option<pcb::SocketId>),
    NoMatchDrop,
    NoMatchSendRst,
}

/// Listener-lock phase. Returns matched `Actions` + parent socket id, or a
/// no-match outcome. `#[inline(never)]` so the ~400 B `Actions` return
/// value stays in this helper's frame.
#[inline(never)]
fn input_try_listener(
    incoming_tuple: &TcpTuple,
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> ListenerOutcome {
    let mut listeners = TCP_LISTENERS.lock();
    if let Some(slot) = listeners.find_by_port(incoming_tuple.local_ip, incoming_tuple.local_port) {
        let id = ConnId::new_listener(slot);
        let pcb = listeners.get_mut(slot).expect("find returned a live slot");
        let mut actions = pcb.on_segment(None, hdr, options, payload, now_ms);
        actions.conn_id = Some(id);
        let parent_sock = pcb.socket_id;
        ListenerOutcome::Matched(actions, parent_sock)
    } else if hdr.is_rst() {
        ListenerOutcome::NoMatchDrop
    } else {
        ListenerOutcome::NoMatchSendRst
    }
}

/// Build a single-RST `Actions` for the no-matching-connection path.
/// Separate function so the 400 B `Actions` return slot isn't merged
/// into `tcp::input`'s frame.
#[inline(never)]
fn input_no_match_rst(hdr: &TcpHeader, dst_ip: [u8; 4], src_ip: [u8; 4]) -> Actions {
    let mut actions = Actions::new();
    actions.push_segment(SegmentBuilder::rst_for(hdr, dst_ip, src_ip));
    actions
}

/// Install a child PCB accepted by the listener phase. `SynRecvState` is
/// ~80 B; isolating this path keeps `tcp::input`'s frame small.
#[inline(never)]
fn install_accepted_child(
    incoming_tuple: &TcpTuple,
    actions: &Actions,
    hdr: &TcpHeader,
    parent_sock: Option<pcb::SocketId>,
) {
    let accepted = match &actions.accepted {
        Some(a) => a,
        None => return,
    };
    let child_iss = SeqNum::new(accepted.iss);
    let child_irs = SeqNum::new(accepted.irs);
    let mut child_state = pcb::SynRecvState::new(child_iss, child_irs);
    child_state.peer_mss = accepted.peer_mss;
    child_state.sack_permitted = accepted.sack_permitted;
    child_state.snd_wnd = hdr.window_size as u32;
    if let Some(tsval) = accepted.peer_tsval {
        child_state.ts_enabled = true;
        child_state.peer_tsval = tsval;
    }

    let _ = table::install_established(*incoming_tuple, PcbState::SynRecv(child_state), |child| {
        child.socket_id = parent_sock;
    });
}

/// Process a segment for an established/transient connection already found
/// in a shard.  Called with the shard lock held.
///
/// `#[inline(never)]` so the ~400 B `Actions` return value stays in this
/// function's frame rather than doubling `tcp::input`'s frame via inlining.
#[inline(never)]
fn input_process_established(
    shard: &mut TcpShard,
    id: ConnId,
    slot: usize,
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> Actions {
    let (pcb, bufs) = shard
        .get_pcb_and_opt_bufs(slot)
        .expect("find returned a live slot");
    let mut actions = pcb.on_segment(bufs, hdr, options, payload, now_ms);
    actions.conn_id = Some(id);

    // Apply timer operations.  State handlers emit `key: 0` as a
    // sentinel — we substitute the real ConnId.
    for i in 0..actions.timer_ops_len as usize {
        if let Some(ref op) = actions.timer_ops[i] {
            match *op {
                TimerOp::Schedule {
                    kind,
                    key: _,
                    delay_ticks,
                } => {
                    let token = NET_TIMER_WHEEL.schedule(delay_ticks, kind, id.0);
                    // Store the token back into the PCB's state-specific
                    // timer slot so future transitions can cancel it.
                    if let Some(pcb) = shard.get_mut(slot) {
                        match kind {
                            TimerKind::TcpRetransmit => {
                                set_retransmit_token(pcb, Some(token));
                            }
                            TimerKind::TcpTimeWait => {
                                if let PcbState::TimeWait(tw) = &mut pcb.state {
                                    tw.expire_token = Some(token);
                                }
                            }
                            TimerKind::TcpKeepalive => {
                                if let PcbState::Data(d) = &mut pcb.state {
                                    d.keepalive_token = Some(token);
                                }
                            }
                            TimerKind::TcpFinWait2 => {
                                if let PcbState::Data(d) = &mut pcb.state {
                                    d.fin_wait2_token = Some(token);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                TimerOp::Cancel { token } => {
                    NET_TIMER_WHEEL.cancel(token);
                }
            }
        }
    }

    // Allocate buffer for newly-established connections (SynRecv/SynSent→Data).
    if actions.notify.contains(SocketNotify::NEW_ESTABLISHED) {
        shard
            .alloc_buffer_for(slot)
            .expect("tcp: kernel OOM allocating connection buffer");
    }

    // Free buffer on transition to TimeWait (Data→TimeWait).
    if let Some(pcb) = shard.get(slot) {
        if matches!(pcb.state, PcbState::TimeWait(_)) && shard.has_buffer(slot) {
            shard.free_buffer_for(slot);
        }
    }

    // Schedule keepalive timer for newly-established connections.
    if actions.notify.contains(SocketNotify::NEW_ESTABLISHED) {
        if let Some(pcb) = shard.get_mut(slot) {
            let keepalive_enabled = pcb
                .socket_id
                .map(|sid| crate::socket::socket_keepalive_enabled_by_index(sid.0 as usize))
                .unwrap_or(false);
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some(delay) = d.schedule_initial_keepalive(keepalive_enabled) {
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.0);
                    d.keepalive_token = Some(token);
                }
            }
        }
    }

    // Reset keepalive timer on data activity.
    if !actions.release
        && actions
            .notify
            .intersects(SocketNotify::RECV_WAKE | SocketNotify::SEND_WAKE)
    {
        if let Some(pcb) = shard.get_mut(slot) {
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some((old_token, delay)) = d.reset_keepalive_on_activity() {
                    NET_TIMER_WHEEL.cancel(old_token);
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.0);
                    d.keepalive_token = Some(token);
                }
            }
        }
    }

    if actions.release {
        shard.release(slot);
    }

    actions
}

/// Helper: set the retransmit_token on whichever state variant owns it.
fn set_retransmit_token(pcb: &mut Pcb, token: Option<TimerToken>) {
    match &mut pcb.state {
        PcbState::SynSent(s) => s.retransmit_token = token,
        PcbState::SynRecv(s) => s.retransmit_token = token,
        PcbState::Data(d) => d.retransmit_token = token,
        _ => {}
    }
}

// =============================================================================
// Lifecycle API
// =============================================================================

/// Open an active connection (client: SYN → SYN_SENT).
///
/// Returns `(ConnId, outgoing_SYN_segment)`.
pub fn connect(
    local_ip: [u8; 4],
    remote_ip: [u8; 4],
    remote_port: u16,
) -> Result<(ConnId, TcpOutSegment), TcpError> {
    let local_port = table::alloc_ephemeral_port().ok_or(TcpError::AddrInUse)?;
    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip,
        remote_port,
    };
    let iss = generate_isn(&tuple);

    let wscale = our_window_scale();
    let mut syn_sent = pcb::SynSentState::new(SeqNum::new(iss));
    syn_sent.our_wscale = wscale;

    let id = table::install_established(tuple, PcbState::SynSent(syn_sent), |_| {})?;

    klog_debug!(
        "tcp: CONNECT {}:{} -> {}:{} ISS={} id={}",
        local_ip[0],
        local_ip[1],
        local_port,
        remote_port,
        iss,
        id.0
    );

    let seg =
        SegmentBuilder::active_syn(tuple, iss, wscale).with_timestamp(clock::now_ms() as u32, 0);
    Ok((id, seg))
}

/// Open a passive connection (server: → LISTEN).
pub fn listen(local_ip: [u8; 4], local_port: u16) -> Result<ConnId, TcpError> {
    if table::port_in_use(local_ip, local_port) {
        return Err(TcpError::AddrInUse);
    }

    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    let id = table::install_listener(tuple, PcbState::Listen(pcb::ListenState::new()), |_| {})?;

    klog_debug!("tcp: LISTEN on port {} id={}", local_port, id.0);
    Ok(id)
}

/// Close a connection (initiate graceful teardown).
///
/// Returns the outgoing FIN segment if one should be sent.
pub fn close(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let mut listeners = TCP_LISTENERS.lock();
        let pcb = listeners.get(id.slot()).ok_or(TcpError::NotFound)?;
        let name = pcb.state.name();
        listeners.release(id.slot());
        klog_debug!("tcp: CLOSE id={} from {} — released", id.0, name);
        return Ok(None);
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();

    // Check existence + handle states that don't need mutable PCB access.
    let pcb = shard.get(slot).ok_or(TcpError::NotFound)?;
    if matches!(pcb.state, PcbState::Listen(_) | PcbState::SynSent(_)) {
        let name = pcb.state.name();
        shard.release(slot);
        klog_debug!("tcp: CLOSE id={} from {} — released", id.0, name);
        return Ok(None);
    }
    if matches!(pcb.state, PcbState::TimeWait(_)) {
        klog_debug!("tcp: CLOSE id={} TIME_WAIT — no-op", id.0);
        return Ok(None);
    }

    // SynRecv → Data(FinWait1): allocate buffer before transition.
    let is_syn_recv = matches!(shard.get(slot).unwrap().state, PcbState::SynRecv(_));
    if is_syn_recv {
        shard
            .alloc_buffer_for(slot)
            .expect("tcp: kernel OOM allocating connection buffer on close");
    }

    let pcb = shard.get_mut(slot).unwrap();
    match &mut pcb.state {
        PcbState::SynRecv(_) => close_syn_recv_transition(pcb, id),
        PcbState::Data(d) => {
            let tuple = pcb.tuple;
            match d.close_phase {
                ClosePhase::Established => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::FinWait1;
                    cancel_keepalive(d);
                    let mut seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                    seg.timestamp = d.ts_option(clock::now_ms());
                    pcb.assert_invariants();
                    klog_debug!(
                        "tcp: CLOSE id={} ESTABLISHED -> FIN_WAIT_1, FIN seq={}",
                        id.0,
                        seq
                    );
                    Ok(Some(seg))
                }
                ClosePhase::CloseWait => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::LastAck;
                    cancel_keepalive(d);
                    let mut seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                    seg.timestamp = d.ts_option(clock::now_ms());
                    pcb.assert_invariants();
                    klog_debug!(
                        "tcp: CLOSE id={} CLOSE_WAIT -> LAST_ACK, FIN seq={}",
                        id.0,
                        seq
                    );
                    Ok(Some(seg))
                }
                _ => {
                    klog_debug!(
                        "tcp: CLOSE id={} already closing ({:?})",
                        id.0,
                        d.close_phase
                    );
                    Ok(None)
                }
            }
        }
        _ => Err(TcpError::InvalidState),
    }
}

/// `SynRecv → Data(FinWait1)` transition for `tcp::close`. Extracted so
/// the `KBox::try_init(DataState::init_from_syn_recv)` closure frame
/// doesn't inflate `tcp::close`'s stack frame via inlining — the
/// stack-safety gate otherwise rejects `close`.
#[inline(never)]
fn close_syn_recv_transition(
    pcb: &mut pcb::Pcb,
    id: ConnId,
) -> Result<Option<TcpOutSegment>, TcpError> {
    let s = match &pcb.state {
        PcbState::SynRecv(s) => s,
        _ => unreachable!("close_syn_recv_transition called on non-SynRecv pcb"),
    };
    let tuple = pcb.tuple;
    let seq = s.snd_nxt.raw();
    let ack = s.rcv_nxt.raw();
    let window = s.rcv_wnd;
    let now_ms = clock::now_ms();
    let ts_enabled = s.ts_enabled;
    // Heap-direct: build the new DataState in place inside a fresh
    // KBox, then patch close_phase / snd_nxt through DerefMut.
    let mut ds = slopos_alloc::KBox::try_init(DataState::init_from_syn_recv(s))?;
    ds.close_phase = ClosePhase::FinWait1;
    ds.snd_nxt = ds.snd_nxt.wrapping_add(1);
    let ts = if ts_enabled {
        Some((now_ms as u32, ds.ts_recent))
    } else {
        None
    };
    pcb.state = PcbState::Data(ds);
    pcb.assert_invariants();
    let mut seg = SegmentBuilder::fin_ack(tuple, seq, ack, window);
    seg.timestamp = ts;
    klog_debug!("tcp: CLOSE id={} SYN_RECV -> FIN_WAIT_1", id.0);
    Ok(Some(seg))
}

/// Abort a connection (send RST, release immediately).
pub fn abort(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let mut listeners = TCP_LISTENERS.lock();
        let pcb = listeners.get(id.slot()).ok_or(TcpError::NotFound)?;
        klog_debug!("tcp: ABORT id={} from {}", id.0, pcb.state.name());
        listeners.release(id.slot());
        return Ok(None);
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();
    let pcb = shard.get(slot).ok_or(TcpError::NotFound)?;

    let seg = match &pcb.state {
        PcbState::Listen(_) => None,
        PcbState::SynSent(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
        PcbState::SynRecv(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
        PcbState::Data(d) => Some(SegmentBuilder::bare_rst(pcb.tuple, d.snd_nxt.raw())),
        PcbState::TimeWait(tw) => Some(SegmentBuilder::bare_rst(pcb.tuple, tw.last_snd_nxt.raw())),
    };

    klog_debug!("tcp: ABORT id={} from {}", id.0, pcb.state.name());
    shard.release(slot);
    Ok(seg)
}

/// Shutdown the write half of a connection (send FIN without releasing).
pub fn shutdown_write(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();

    // SynRecv → Data(FinWait1): allocate buffer before transition.
    let is_syn_recv = matches!(
        shard.get(slot).ok_or(TcpError::NotFound)?.state,
        PcbState::SynRecv(_)
    );
    if is_syn_recv {
        shard
            .alloc_buffer_for(slot)
            .expect("tcp: kernel OOM allocating connection buffer on shutdown_write");
    }

    let pcb = shard.get_mut(slot).ok_or(TcpError::NotFound)?;
    match &mut pcb.state {
        PcbState::Data(d) => {
            let tuple = pcb.tuple;
            match d.close_phase {
                ClosePhase::Established => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::FinWait1;
                    cancel_keepalive(d);
                    let mut seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                    seg.timestamp = d.ts_option(clock::now_ms());
                    pcb.assert_invariants();
                    klog_debug!("tcp: SHUTDOWN_WR id={} ESTABLISHED -> FIN_WAIT_1", id.0);
                    Ok(Some(seg))
                }
                ClosePhase::CloseWait => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::LastAck;
                    cancel_keepalive(d);
                    let mut seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                    seg.timestamp = d.ts_option(clock::now_ms());
                    pcb.assert_invariants();
                    klog_debug!("tcp: SHUTDOWN_WR id={} CLOSE_WAIT -> LAST_ACK", id.0);
                    Ok(Some(seg))
                }
                _ => {
                    klog_debug!(
                        "tcp: SHUTDOWN_WR id={} already closing ({:?})",
                        id.0,
                        d.close_phase
                    );
                    Ok(None)
                }
            }
        }
        PcbState::SynRecv(_) => shutdown_write_syn_recv_transition(pcb, id),
        _ => Err(TcpError::InvalidState),
    }
}

/// `SynRecv → Data(FinWait1)` transition for `tcp::shutdown_write`.
/// Identical shape to `close_syn_recv_transition` but with
/// `shutdown_write` logging semantics. `#[inline(never)]` for the same
/// stack-frame reason.
#[inline(never)]
fn shutdown_write_syn_recv_transition(
    pcb: &mut pcb::Pcb,
    id: ConnId,
) -> Result<Option<TcpOutSegment>, TcpError> {
    let s = match &pcb.state {
        PcbState::SynRecv(s) => s,
        _ => unreachable!(),
    };
    let tuple = pcb.tuple;
    let seq = s.snd_nxt.raw();
    let ack = s.rcv_nxt.raw();
    let window = s.rcv_wnd;
    let now_ms = clock::now_ms();
    let ts_enabled = s.ts_enabled;
    let mut ds = slopos_alloc::KBox::try_init(DataState::init_from_syn_recv(s))?;
    ds.close_phase = ClosePhase::FinWait1;
    ds.snd_nxt = ds.snd_nxt.wrapping_add(1);
    let ts = if ts_enabled {
        Some((now_ms as u32, ds.ts_recent))
    } else {
        None
    };
    pcb.state = PcbState::Data(ds);
    pcb.assert_invariants();
    let mut seg = SegmentBuilder::fin_ack(tuple, seq, ack, window);
    seg.timestamp = ts;
    klog_debug!("tcp: SHUTDOWN_WR id={} SYN_RECV -> FIN_WAIT_1", id.0);
    Ok(Some(seg))
}

/// Discard all data in the receive buffer (for SHUT_RD).
pub fn recv_discard(id: ConnId) {
    if id.is_listener() {
        return;
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();
    if shard.get(slot).is_some() {
        if let Some(bufs) = shard.bufs_mut(slot) {
            bufs.recv.clear();
        }
        klog_debug!("tcp: RECV_DISCARD id={} — recv buffer cleared", id.0);
    }
}

/// Cancel keepalive timer on a DataState.
fn cancel_keepalive(d: &mut DataState) {
    if let Some(token) = d.keepalive_token.take() {
        NET_TIMER_WHEEL.cancel(token);
    }
}

// =============================================================================
// Query helpers
// =============================================================================

/// Get the RFC 793 state name for a connection.
pub fn get_state(id: ConnId) -> Option<TcpState> {
    table::with_pcb(id, |pcb| pcb.state.tcp_state())
}

/// Get the number of active connections.
pub fn active_count() -> usize {
    table::active_count()
}

/// Find a connection by tuple.
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    table::find(tuple)
}

/// Set or clear the socket back-pointer on a connection.
pub fn set_socket_idx(id: ConnId, socket_id: Option<SocketId>) {
    table::with_pcb_mut(id, |pcb| {
        pcb.socket_id = socket_id;
    });
}

/// Check whether the peer has closed their write half (sent FIN).
pub fn is_peer_closed(id: ConnId) -> bool {
    table::with_pcb(id, |pcb| match &pcb.state {
        PcbState::Data(d) => d.peer_closed,
        PcbState::TimeWait(_) => true,
        _ => false,
    })
    .unwrap_or(false)
}

/// Check whether the connection was reset.
pub fn is_reset(id: ConnId) -> bool {
    table::with_pcb(id, |pcb| match &pcb.state {
        PcbState::Data(d) => d.reset_received,
        _ => false,
    })
    .unwrap_or(false)
}

/// Available send buffer space for a connection.
pub fn send_buffer_space(id: ConnId) -> usize {
    if id.is_listener() {
        return 0;
    }
    TCP_SHARDS[id.shard()]
        .lock()
        .bufs(id.slot())
        .map(|b| b.send.free_space())
        .unwrap_or(0)
}

/// Bytes available to read from a connection's receive buffer.
pub fn recv_available(id: ConnId) -> usize {
    if id.is_listener() {
        return 0;
    }
    TCP_SHARDS[id.shard()]
        .lock()
        .bufs(id.slot())
        .map(|b| b.recv.available())
        .unwrap_or(0)
}

/// Whether a connection has data pending transmission.
pub fn has_pending_data(id: ConnId) -> bool {
    if id.is_listener() {
        return false;
    }
    TCP_SHARDS[id.shard()]
        .lock()
        .bufs(id.slot())
        .map(|b| b.send.unsent_len() > 0)
        .unwrap_or(false)
}

/// Closure-based read access to a PCB.
pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    table::with_pcb(id, f)
}

/// Closure-based mutable access to a PCB.
pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    table::with_pcb_mut(id, f)
}

// =============================================================================
// Socket options
// =============================================================================

/// Set the effective send buffer capacity (SO_SNDBUF).
/// Values above TCP_BUFFER_SIZE are silently capped.
pub fn set_sndbuf(id: ConnId, bytes: usize) {
    if id.is_listener() {
        return;
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    let capped = core::cmp::min(bytes, buffer::TCP_BUFFER_SIZE);
    if let Some(bufs) = shard.bufs_mut(id.slot()) {
        bufs.send.effective_capacity = capped;
    }
}

/// Set the effective receive buffer capacity (SO_RCVBUF).
/// Values above TCP_BUFFER_SIZE are silently capped.
pub fn set_rcvbuf(id: ConnId, bytes: usize) {
    if id.is_listener() {
        return;
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    let capped = core::cmp::min(bytes, buffer::TCP_BUFFER_SIZE);
    if let Some(bufs) = shard.bufs_mut(id.slot()) {
        bufs.recv.effective_capacity = capped;
    }
}

/// Set or clear TCP_NODELAY (disables/enables Nagle algorithm).
pub fn set_nodelay(id: ConnId, nodelay: bool) {
    table::with_pcb_mut(id, |pcb| {
        if let PcbState::Data(d) = &mut pcb.state {
            d.nagle_enabled = !nodelay;
        }
    });
}

// =============================================================================
// Data path
// =============================================================================

/// Write data into a connection's send buffer.
pub fn send(id: ConnId, data: &[u8]) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    // Check existence separately from buffer presence so a valid PCB in
    // a non-Data state returns InvalidState (not NotFound, which would
    // mislead callers about the connection's existence).  Only Data
    // states have a buffer allocated (lazy buffer lifecycle, Phase 6a).
    let (pcb, bufs) = shard
        .get_pcb_and_opt_bufs(id.slot())
        .ok_or(TcpError::NotFound)?;
    match &pcb.state {
        PcbState::Data(d)
            if matches!(
                d.close_phase,
                ClosePhase::Established | ClosePhase::CloseWait
            ) => {}
        _ => return Err(TcpError::InvalidState),
    }
    // State check succeeded → Data state → buffer is always allocated.
    let bufs = bufs.expect("Data state must have a buffer");
    Ok(bufs.send.enqueue(data))
}

/// Read data from a connection's receive buffer.
pub fn recv(id: ConnId, out: &mut [u8]) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    let (pcb, bufs) = shard
        .get_pcb_and_opt_bufs(id.slot())
        .ok_or(TcpError::NotFound)?;

    // Only Data-phase connections have a receive buffer to read from.
    // Any other state (SynSent, SynRecv, Listen, TimeWait) is invalid
    // for recv().
    let Some(bufs) = bufs else {
        return Err(TcpError::InvalidState);
    };

    let read = bufs.recv.dequeue(out);
    if read == 0 && bufs.recv.available() == 0 {
        if let PcbState::Data(d) = &pcb.state {
            if d.reset_received {
                return Err(TcpError::ConnectionReset);
            }
        }
    }

    // Update rcv_wnd from the live receive buffer.
    let recv_window = bufs.recv.window();
    if let PcbState::Data(d) = &mut pcb.state {
        d.rcv_wnd = recv_window;
    }
    Ok(read)
}

/// Generate the next outgoing data segment for a connection.
pub fn poll_transmit(
    id: ConnId,
    payload_buf: &mut [u8],
    now_ms: u64,
) -> Option<(TcpOutSegment, usize)> {
    if id.is_listener() {
        return None;
    }
    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();
    let (pcb, bufs) = shard.get_with_bufs(slot)?;

    let PcbState::Data(d) = &mut pcb.state else {
        return None;
    };
    // Project once through the KBox `DerefMut` so the borrow checker
    // can split sub-field borrows below (KBox lacks Box's compiler-
    // magic projection rules).
    let d: &mut DataState = &mut **d;
    if !matches!(
        d.close_phase,
        ClosePhase::Established | ClosePhase::CloseWait | ClosePhase::FinWait1
    ) {
        return None;
    }

    let tuple = pcb.tuple;
    let rto_ms = d.rtt.rto_ms() as u64;
    let peer_mss = d.peer_mss as usize;
    let snd_wnd = d.snd_wnd as usize;

    // Window calculation uses pipe (RFC 6675) — bytes believed to be
    // in the network — instead of raw inflight.
    let pipe = d.sendmap.pipe() as usize;
    let wnd_avail = snd_wnd.saturating_sub(pipe);
    let cwnd_avail = (d.cc.cwnd() as usize).saturating_sub(pipe);
    let effective_wnd = core::cmp::min(wnd_avail, cwnd_avail);

    // Priority 1: selective retransmit of Lost entries.
    if let Some(lost) = d.sendmap.next_lost() {
        let len = lost.len as usize;
        if len <= effective_wnd && len <= payload_buf.len() {
            let offset = d.snd_una.distance_to(lost.seq) as usize;
            let seq = lost.seq.raw();
            let payload_len = bufs.send.peek_retransmit(offset, &mut payload_buf[..len]);
            if payload_len > 0 {
                d.sendmap.mark_retransmitted(lost.seq);
                let window = bufs.recv.window();
                let mut seg = SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
                seg.timestamp = d.ts_option(now_ms);
                // Schedule retransmit timer if none active.
                if bufs.send.rto_deadline_ms == 0 {
                    bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
                    if d.retransmit_token.is_none() {
                        let delay_ticks = (rto_ms / 10).max(1);
                        let token =
                            NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, id.0);
                        d.retransmit_token = Some(token);
                    }
                }
                pcb.assert_invariants();
                return Some((seg, payload_len));
            }
        }
    }

    // Priority 2: send new data.
    if d.sendmap.capacity_remaining() == 0 {
        return None;
    }

    let unsent = bufs.send.unsent_len();
    let mut max_send = core::cmp::min(unsent, peer_mss);
    max_send = core::cmp::min(max_send, effective_wnd);
    max_send = core::cmp::min(max_send, payload_buf.len());

    // Nagle (RFC 896): defer sub-MSS segments when data is in flight.
    if d.nagle_enabled && max_send < peer_mss && pipe > 0 {
        return None;
    }

    if max_send == 0 {
        return None;
    }

    let seq = d.snd_nxt.raw();
    let payload_len = bufs.send.peek_unsent(&mut payload_buf[..max_send]);
    if payload_len == 0 {
        return None;
    }

    bufs.send.mark_sent(payload_len);
    d.snd_nxt = d.snd_nxt.wrapping_add(payload_len as u32);

    // Record sent segment in the send map.
    let _ = d
        .sendmap
        .push_sent(SeqNum::new(seq), payload_len as u32, now_ms);

    // Schedule retransmit timer if none active.
    if bufs.send.rto_deadline_ms == 0 {
        bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
        if d.retransmit_token.is_none() {
            let delay_ticks = (rto_ms / 10).max(1);
            let token = NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, id.0);
            d.retransmit_token = Some(token);
        }
    }

    let window = bufs.recv.window();
    let mut seg = SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
    seg.timestamp = d.ts_option(now_ms);
    pcb.assert_invariants();

    Some((seg, payload_len))
}

// =============================================================================
// Timer callbacks
// =============================================================================

/// Handle a retransmit timer firing for connection `conn_id`.
pub fn on_retransmit(conn_id: u32) -> Option<ConnId> {
    let id = ConnId(conn_id);
    if id.is_listener() {
        return None;
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();

    // Pre-check with a short borrow: bail if not applicable, flag if
    // max retransmits exceeded.
    let should_release = {
        let (pcb, _bufs) = shard.get_with_bufs(slot)?;
        let PcbState::Data(d) = &pcb.state else {
            return None;
        };
        if d.sendmap.is_empty() {
            return None;
        }
        d.rtt.consecutive_timeouts >= MAX_RETRANSMITS
    };

    if should_release {
        klog_debug!("tcp: retransmit timeout id={} -> releasing", conn_id);
        shard.release(slot);
        return None;
    }

    // Main retransmit path: mark all entries Lost so the next
    // poll_transmit selectively retransmits them.
    let (pcb, bufs) = shard.get_with_bufs(slot)?;
    let PcbState::Data(d) = &mut pcb.state else {
        return None;
    };
    // Project once through the KBox `DerefMut` so the borrow checker
    // can split disjoint field borrows below.
    let d: &mut DataState = &mut **d;

    d.retransmit_token = None;

    d.cc.on_timeout(d.sendmap.pipe());
    d.sendmap.mark_all_lost();
    d.rtt.back_off();

    let rto_ms = d.rtt.rto_ms() as u64;
    let delay_ticks = (rto_ms / 10).max(1);
    let token = NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, conn_id);
    d.retransmit_token = Some(token);

    let now_ms = slopos_utils::clock::uptime_ms();
    bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
    pcb.assert_invariants();

    klog_debug!("tcp: retransmit fired id={} rto_ms={}", conn_id, rto_ms);

    Some(id)
}

/// Handle a keepalive timer firing.
pub fn on_keepalive(conn_id: u32) -> Option<TcpOutSegment> {
    let id = ConnId(conn_id);
    if id.is_listener() {
        return None;
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();
    let pcb = shard.get_mut(slot)?;

    let PcbState::Data(d) = &mut pcb.state else {
        return None;
    };
    if d.close_phase != ClosePhase::Established {
        return None;
    }

    d.keepalive_token = None;

    if d.keepalive_probes_sent >= TCP_KEEPALIVE_PROBES_MAX {
        klog_debug!(
            "tcp: keepalive max probes reached id={} -> releasing",
            conn_id
        );
        shard.release(slot);
        return None;
    }

    let mut probe_seg =
        SegmentBuilder::keepalive_probe(pcb.tuple, d.snd_una.raw(), d.rcv_nxt.raw(), d.rcv_wnd);
    probe_seg.timestamp = d.ts_option(clock::now_ms());

    d.keepalive_probes_sent = d.keepalive_probes_sent.saturating_add(1);
    let token = NET_TIMER_WHEEL.schedule(
        TCP_KEEPALIVE_INTERVAL_TICKS,
        TimerKind::TcpKeepalive,
        conn_id,
    );
    d.keepalive_token = Some(token);

    Some(probe_seg)
}

/// Handle a TIME_WAIT timer expiry.
pub fn on_time_wait_expire(conn_id: u32) {
    let id = ConnId(conn_id);
    if id.is_listener() {
        return;
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();

    let Some(pcb) = shard.get(slot) else {
        return;
    };

    if matches!(pcb.state, PcbState::TimeWait(_)) {
        klog_debug!("tcp: TIME_WAIT timer expired id={}", conn_id);
        shard.release(slot);
    }
}

/// Handle a FIN_WAIT_2 timer expiry.
pub fn on_fin_wait2_timeout(conn_id: u32) {
    let id = ConnId(conn_id);
    if id.is_listener() {
        return;
    }

    let mut shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();

    let Some(pcb) = shard.get(slot) else {
        return;
    };

    if let PcbState::Data(d) = &pcb.state {
        if d.close_phase == ClosePhase::FinWait2 {
            klog_debug!("tcp: FIN_WAIT_2 timeout id={}", conn_id);
            shard.release(slot);
        }
    }
}

// =============================================================================
// Test-only helpers
// =============================================================================

/// Deterministic retransmit probe — test-only.
///
/// Walks all shards and triggers retransmit for the first
/// connection whose RTO deadline has expired at `now_ms`.
#[cfg(feature = "itests")]
pub fn retransmit_check(now_ms: u64) -> Option<ConnId> {
    for (shard_idx, shard_lock) in TCP_SHARDS.iter().enumerate() {
        let mut shard = shard_lock.lock();

        let mut to_release: Option<usize> = None;
        let mut retransmitted: Option<(usize, ConnId)> = None;

        for (slot, pcb, bufs) in shard.iter_mut_with_bufs() {
            let send = &bufs.send;
            if send.inflight == 0 || send.rto_deadline_ms == 0 || now_ms < send.rto_deadline_ms {
                continue;
            }

            let PcbState::Data(d) = &mut pcb.state else {
                continue;
            };
            // Project once through the KBox `DerefMut` so the borrow
            // checker can split disjoint field borrows below.
            let d: &mut DataState = &mut **d;

            let conn_id = ConnId::new_shard(shard_idx, slot);

            // Check max retransmits — if exceeded, mark for release.
            let retransmits = d.rtt.consecutive_timeouts;
            if retransmits >= MAX_RETRANSMITS {
                to_release = Some(slot);
                break;
            }

            d.cc.on_timeout(d.sendmap.pipe());
            d.sendmap.mark_all_lost();
            d.rtt.back_off();
            bufs.send.rto_deadline_ms = now_ms.saturating_add(d.rtt.rto_ms() as u64);
            pcb.assert_invariants();

            retransmitted = Some((slot, conn_id));
            break;
        }

        if let Some(slot) = to_release {
            shard.release(slot);
            return None;
        }

        if let Some((_slot, conn_id)) = retransmitted {
            return Some(conn_id);
        }
    }

    None
}

/// Check all connections for pending delayed ACKs.
pub fn delayed_ack_check(now_ms: u64) -> Option<(ConnId, TcpOutSegment)> {
    for (shard_idx, shard_lock) in TCP_SHARDS.iter().enumerate() {
        let mut shard = shard_lock.lock();

        for (slot, pcb, bufs) in shard.iter_mut_with_bufs() {
            let PcbState::Data(d) = &pcb.state else {
                continue;
            };
            if let Some(seg) = d.check_delayed_ack(pcb.tuple, bufs, now_ms) {
                let id = ConnId::new_shard(shard_idx, slot);
                return Some((id, seg));
            }
        }
    }

    None
}

/// Generate a zero-window probe for a connection with snd_wnd == 0.
pub fn zero_window_probe(id: ConnId, _now_ms: u64) -> Option<TcpOutSegment> {
    if id.is_listener() {
        return None;
    }
    let shard = TCP_SHARDS[id.shard()].lock();
    let slot = id.slot();
    let pcb = shard.get(slot)?;
    let PcbState::Data(d) = &pcb.state else {
        return None;
    };
    d.check_zero_window_probe(pcb.tuple, shard.bufs(slot)?)
}

/// Release all connections (for testing).
pub fn reset_all() {
    table::clear_all();
    isn::reset_for_tests();
    #[cfg(feature = "itests")]
    challenge_ack::reset_for_tests();
}
