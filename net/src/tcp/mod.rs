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
pub use reasm::TcpOooQueue;
pub use segment::{TcpOutSegment, write_tcp_segment};
pub use seq::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};
pub use table::{ConnId, PCB_TABLE};

use self::cong::CongestionControl;
use self::segment::SegmentBuilder;
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
/// Locks `PCB_TABLE`, dispatches to the matching `Pcb::on_segment`,
/// applies timer ops, installs child PCBs from LISTEN accepts, and
/// returns `Actions` for the caller to drain (segments to send,
/// socket-layer wake-ups, etc.).
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

    let mut table = PCB_TABLE.lock();

    let id = match table.find(&incoming_tuple) {
        Some(id) => id,
        None => {
            if hdr.is_rst() {
                return Actions::new();
            }
            let mut actions = Actions::new();
            actions.push_segment(SegmentBuilder::rst_for(hdr, dst_ip, src_ip));
            return actions;
        }
    };

    let (pcb, bufs) = table.get_with_bufs(id).expect("find returned a live id");
    let mut actions = pcb.on_segment(bufs, hdr, options, payload, now_ms);
    actions.conn_id = Some(id);

    // Install child PCB from LISTEN accept.  ListenState::on_segment
    // populates `actions.accepted` with metadata but does not allocate
    // the child — that's our job while the lock is held.
    if let Some(ref accepted) = actions.accepted {
        let child_iss = SeqNum::new(accepted.iss);
        let child_irs = SeqNum::new(accepted.irs);
        let mut child_state = pcb::SynRecvState::new(child_iss, child_irs);
        child_state.peer_mss = accepted.peer_mss;
        child_state.sack_permitted = accepted.sack_permitted;
        child_state.snd_wnd = hdr.window_size as u32;
        let parent_sock = table.get(id).and_then(|p| p.socket_id);

        let _ = table.install_with(incoming_tuple, PcbState::SynRecv(child_state), |child| {
            child.socket_id = parent_sock;
        });
    }

    // Apply timer operations while the lock is held.  State handlers
    // emit `key: 0` as a sentinel — we substitute the real ConnId.
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
                    if let Some(pcb) = table.get_mut(id) {
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

    // Schedule keepalive timer for newly-established connections
    // (only if the owning socket has keepalive enabled).
    if actions.notify.contains(SocketNotify::NEW_ESTABLISHED) {
        if let Some(pcb) = table.get_mut(id) {
            let keepalive_enabled = pcb
                .socket_id
                .map(|sid| crate::socket::socket_keepalive_enabled_by_index(sid.0 as usize))
                .unwrap_or(false);
            if keepalive_enabled {
                if let PcbState::Data(d) = &mut pcb.state {
                    if d.keepalive_token.is_none() {
                        let token = NET_TIMER_WHEEL.schedule(
                            TCP_KEEPALIVE_IDLE_TICKS,
                            TimerKind::TcpKeepalive,
                            id.0,
                        );
                        d.keepalive_token = Some(token);
                    }
                }
            }
        }
    }

    // Reset keepalive timer on any data activity — only if keepalive
    // was already active (has a token).  Don't create one from scratch.
    if !actions.release
        && actions
            .notify
            .intersects(SocketNotify::RECV_WAKE | SocketNotify::SEND_WAKE)
    {
        if let Some(pcb) = table.get_mut(id) {
            if let PcbState::Data(d) = &mut pcb.state {
                if d.keepalive_token.is_some() {
                    if let Some(token) = d.keepalive_token.take() {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                    let token = NET_TIMER_WHEEL.schedule(
                        TCP_KEEPALIVE_IDLE_TICKS,
                        TimerKind::TcpKeepalive,
                        id.0,
                    );
                    d.keepalive_token = Some(token);
                    d.keepalive_probes_sent = 0;
                }
            }
        }
    }

    if actions.release {
        table.release(id);
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
    let mut table = PCB_TABLE.lock();
    let local_port = table.alloc_ephemeral_port();
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

    let id = table.install_with(tuple, PcbState::SynSent(syn_sent), |_| {})?;

    klog_debug!(
        "tcp: CONNECT {}:{} -> {}:{} ISS={} id={}",
        local_ip[0],
        local_ip[1],
        local_port,
        remote_port,
        iss,
        id.0
    );

    let seg = SegmentBuilder::active_syn(tuple, iss, wscale);
    Ok((id, seg))
}

/// Open a passive connection (server: → LISTEN).
pub fn listen(local_ip: [u8; 4], local_port: u16) -> Result<ConnId, TcpError> {
    let mut table = PCB_TABLE.lock();

    if table.port_in_use(local_ip, local_port) {
        return Err(TcpError::AddrInUse);
    }

    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    let id = table.install_with(tuple, PcbState::Listen(pcb::ListenState::new()), |_| {})?;

    klog_debug!("tcp: LISTEN on port {} id={}", local_port, id.0);
    Ok(id)
}

/// Close a connection (initiate graceful teardown).
///
/// Returns the outgoing FIN segment if one should be sent.
pub fn close(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = PCB_TABLE.lock();
    let pcb = table.get_mut(id).ok_or(TcpError::NotFound)?;

    match &mut pcb.state {
        PcbState::Listen(_) | PcbState::SynSent(_) => {
            let name = pcb.state.name();
            table.release(id);
            klog_debug!("tcp: CLOSE id={} from {} — released", id.0, name);
            Ok(None)
        }
        PcbState::SynRecv(s) => {
            let tuple = pcb.tuple;
            let seq = s.snd_nxt.raw();
            let ack = s.rcv_nxt.raw();
            let window = s.rcv_wnd;
            // Transition to Data(FinWait1)
            let mut ds = DataState::from_syn_recv(s);
            ds.close_phase = ClosePhase::FinWait1;
            ds.snd_nxt = ds.snd_nxt.wrapping_add(1); // FIN consumes 1
            pcb.state = PcbState::Data(ds);
            let seg = SegmentBuilder::fin_ack(tuple, seq, ack, window);
            klog_debug!("tcp: CLOSE id={} SYN_RECV -> FIN_WAIT_1", id.0);
            Ok(Some(seg))
        }
        PcbState::Data(d) => {
            let tuple = pcb.tuple;
            match d.close_phase {
                ClosePhase::Established => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::FinWait1;
                    cancel_keepalive(d);
                    let seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
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
                    let seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
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
        PcbState::TimeWait(_) => {
            klog_debug!("tcp: CLOSE id={} TIME_WAIT — no-op", id.0);
            Ok(None)
        }
    }
}

/// Abort a connection (send RST, release immediately).
pub fn abort(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = PCB_TABLE.lock();
    let pcb = table.get(id).ok_or(TcpError::NotFound)?;

    let seg = match &pcb.state {
        PcbState::Listen(_) => None,
        PcbState::SynSent(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
        PcbState::SynRecv(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
        PcbState::Data(d) => Some(SegmentBuilder::bare_rst(pcb.tuple, d.snd_nxt.raw())),
        PcbState::TimeWait(tw) => Some(SegmentBuilder::bare_rst(pcb.tuple, tw.last_snd_nxt.raw())),
    };

    klog_debug!("tcp: ABORT id={} from {}", id.0, pcb.state.name());
    table.release(id);
    Ok(seg)
}

/// Shutdown the write half of a connection (send FIN without releasing).
pub fn shutdown_write(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = PCB_TABLE.lock();
    let pcb = table.get_mut(id).ok_or(TcpError::NotFound)?;

    match &mut pcb.state {
        PcbState::Data(d) => {
            let tuple = pcb.tuple;
            match d.close_phase {
                ClosePhase::Established => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::FinWait1;
                    cancel_keepalive(d);
                    let seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                    klog_debug!("tcp: SHUTDOWN_WR id={} ESTABLISHED -> FIN_WAIT_1", id.0);
                    Ok(Some(seg))
                }
                ClosePhase::CloseWait => {
                    let seq = d.snd_nxt.raw();
                    d.snd_nxt = d.snd_nxt.wrapping_add(1);
                    d.close_phase = ClosePhase::LastAck;
                    cancel_keepalive(d);
                    let seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
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
        PcbState::SynRecv(_) => {
            // Transition through Data(FinWait1)
            let tuple = pcb.tuple;
            if let PcbState::SynRecv(s) = &pcb.state {
                let seq = s.snd_nxt.raw();
                let ack = s.rcv_nxt.raw();
                let window = s.rcv_wnd;
                let mut ds = DataState::from_syn_recv(s);
                ds.close_phase = ClosePhase::FinWait1;
                ds.snd_nxt = ds.snd_nxt.wrapping_add(1);
                pcb.state = PcbState::Data(ds);
                let seg = SegmentBuilder::fin_ack(tuple, seq, ack, window);
                Ok(Some(seg))
            } else {
                unreachable!()
            }
        }
        _ => Err(TcpError::InvalidState),
    }
}

/// Discard all data in the receive buffer (for SHUT_RD).
pub fn recv_discard(id: ConnId) {
    let mut table = PCB_TABLE.lock();
    if table.get(id).is_some() {
        table.bufs_mut(id).recv.clear();
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
    PCB_TABLE.lock().get(id).map(|pcb| pcb.state.tcp_state())
}

/// Get the number of active connections.
pub fn active_count() -> usize {
    PCB_TABLE.lock().active_count()
}

/// Find a connection by tuple.
pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    PCB_TABLE.lock().find(tuple)
}

/// Set or clear the socket back-pointer on a connection.
pub fn set_socket_idx(id: ConnId, socket_id: Option<SocketId>) {
    let mut table = PCB_TABLE.lock();
    if let Some(pcb) = table.get_mut(id) {
        pcb.socket_id = socket_id;
    }
}

/// Check whether the peer has closed their write half (sent FIN).
pub fn is_peer_closed(id: ConnId) -> bool {
    let table = PCB_TABLE.lock();
    match table.get(id).map(|p| &p.state) {
        Some(PcbState::Data(d)) => d.peer_closed,
        Some(PcbState::TimeWait(_)) => true,
        _ => false,
    }
}

/// Check whether the connection was reset.
pub fn is_reset(id: ConnId) -> bool {
    let table = PCB_TABLE.lock();
    match table.get(id).map(|p| &p.state) {
        Some(PcbState::Data(d)) => d.reset_received,
        _ => false,
    }
}

/// Available send buffer space for a connection.
pub fn send_buffer_space(id: ConnId) -> usize {
    let table = PCB_TABLE.lock();
    if table.get(id).is_some() {
        table.bufs(id).send.free_space()
    } else {
        0
    }
}

/// Bytes available to read from a connection's receive buffer.
pub fn recv_available(id: ConnId) -> usize {
    let table = PCB_TABLE.lock();
    if table.get(id).is_some() {
        table.bufs(id).recv.available()
    } else {
        0
    }
}

/// Whether a connection has data pending transmission.
pub fn has_pending_data(id: ConnId) -> bool {
    let table = PCB_TABLE.lock();
    if table.get(id).is_some() {
        table.bufs(id).send.unsent_len() > 0
    } else {
        false
    }
}

/// Closure-based read access to a PCB (locks PCB_TABLE).
pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    PCB_TABLE.lock().with_pcb(id, f)
}

/// Closure-based mutable access to a PCB (locks PCB_TABLE).
pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    PCB_TABLE.lock().with_pcb_mut(id, f)
}

// =============================================================================
// Data path
// =============================================================================

/// Write data into a connection's send buffer.
pub fn send(id: ConnId, data: &[u8]) -> Result<usize, TcpError> {
    let mut table = PCB_TABLE.lock();
    let (pcb, bufs) = table.get_with_bufs(id).ok_or(TcpError::NotFound)?;
    match &pcb.state {
        PcbState::Data(d)
            if matches!(
                d.close_phase,
                ClosePhase::Established | ClosePhase::CloseWait
            ) => {}
        _ => return Err(TcpError::InvalidState),
    }
    Ok(bufs.send.enqueue(data))
}

/// Read data from a connection's receive buffer.
pub fn recv(id: ConnId, out: &mut [u8]) -> Result<usize, TcpError> {
    let mut table = PCB_TABLE.lock();
    let (pcb, bufs) = table.get_with_bufs(id).ok_or(TcpError::NotFound)?;

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
    let mut table = PCB_TABLE.lock();
    let (pcb, bufs) = table.get_with_bufs(id)?;

    let PcbState::Data(d) = &mut pcb.state else {
        return None;
    };
    if !matches!(
        d.close_phase,
        ClosePhase::Established | ClosePhase::CloseWait | ClosePhase::FinWait1
    ) {
        return None;
    }

    // Back-pressure: don't send if the retx queue is full.
    if d.retx.capacity_remaining() == 0 {
        return None;
    }

    let tuple = pcb.tuple;
    let seq = d.snd_nxt.raw();
    let rto_ms = d.rtt.rto_ms() as u64;
    let peer_mss = d.peer_mss as usize;
    let snd_wnd = d.snd_wnd as usize;

    let inflight = bufs.send.inflight;
    let wnd_avail = snd_wnd.saturating_sub(inflight);
    let cwnd_avail = (d.cc.cwnd() as usize).saturating_sub(inflight);
    let effective_wnd = core::cmp::min(wnd_avail, cwnd_avail);
    let unsent = bufs.send.unsent_len();
    let mut max_send = core::cmp::min(unsent, peer_mss);
    max_send = core::cmp::min(max_send, effective_wnd);
    max_send = core::cmp::min(max_send, payload_buf.len());

    if max_send == 0 {
        return None;
    }

    let payload_len = bufs.send.peek_unsent(&mut payload_buf[..max_send]);
    if payload_len == 0 {
        return None;
    }

    bufs.send.mark_sent(payload_len);
    d.snd_nxt = d.snd_nxt.wrapping_add(payload_len as u32);

    // Record sent segment for retransmission tracking + RTT sampling.
    let _ = d
        .retx
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
    let seg = SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);

    Some((seg, payload_len))
}

// =============================================================================
// Timer callbacks
// =============================================================================

/// Handle a retransmit timer firing for connection `conn_id`.
pub fn on_retransmit(conn_id: u32) -> Option<ConnId> {
    let mut table = PCB_TABLE.lock();
    let id = ConnId(conn_id);

    // Pre-check with a short borrow: bail if not applicable, flag if
    // max retransmits exceeded.
    let should_release = {
        let (pcb, bufs) = table.get_with_bufs(id)?;
        let PcbState::Data(d) = &pcb.state else {
            return None;
        };
        if bufs.send.inflight == 0 {
            return None;
        }
        d.rtt.consecutive_timeouts >= MAX_RETRANSMITS
    };

    if should_release {
        klog_debug!("tcp: retransmit timeout id={} -> releasing", conn_id);
        table.release(id);
        return None;
    }

    // Main retransmit path.
    let (pcb, bufs) = table.get_with_bufs(id)?;
    let PcbState::Data(d) = &mut pcb.state else {
        return None;
    };

    d.retransmit_token = None;

    d.cc.on_timeout(d.retx.inflight_bytes());
    d.retx.clear();
    bufs.send.retransmit_timeout();
    d.snd_nxt = d.snd_una;
    d.rtt.back_off();

    let rto_ms = d.rtt.rto_ms() as u64;
    let delay_ticks = (rto_ms / 10).max(1);
    let token = NET_TIMER_WHEEL.schedule(delay_ticks, TimerKind::TcpRetransmit, conn_id);
    d.retransmit_token = Some(token);

    let now_ms = slopos_utils::clock::uptime_ms();
    bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);

    klog_debug!("tcp: retransmit fired id={} rto_ms={}", conn_id, rto_ms);

    Some(id)
}

/// Handle a keepalive timer firing.
pub fn on_keepalive(conn_id: u32) -> Option<TcpOutSegment> {
    let mut table = PCB_TABLE.lock();
    let id = ConnId(conn_id);
    let pcb = table.get_mut(id)?;

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
        table.release(id);
        return None;
    }

    let probe_seg =
        SegmentBuilder::keepalive_probe(pcb.tuple, d.snd_una.raw(), d.rcv_nxt.raw(), d.rcv_wnd);

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
    let mut table = PCB_TABLE.lock();
    let id = ConnId(conn_id);

    let Some(pcb) = table.get(id) else {
        return;
    };

    if matches!(pcb.state, PcbState::TimeWait(_)) {
        klog_debug!("tcp: TIME_WAIT timer expired id={}", conn_id);
        table.release(id);
    }
}

// =============================================================================
// Test-only helpers
// =============================================================================

/// Deterministic retransmit probe — test-only.
///
/// Walks the connection table and triggers retransmit for the first
/// connection whose RTO deadline has expired at `now_ms`.
#[cfg(feature = "itests")]
pub fn retransmit_check(now_ms: u64) -> Option<ConnId> {
    let mut table = PCB_TABLE.lock();

    let mut to_release: Option<ConnId> = None;
    let mut retransmitted: Option<ConnId> = None;

    for (id, pcb, bufs) in table.iter_mut_with_bufs() {
        let send = &bufs.send;
        if send.inflight == 0 || send.rto_deadline_ms == 0 || now_ms < send.rto_deadline_ms {
            continue;
        }

        let PcbState::Data(d) = &mut pcb.state else {
            continue;
        };

        // Check max retransmits — if exceeded, mark for release.
        let retransmits = d.rtt.consecutive_timeouts;
        if retransmits >= MAX_RETRANSMITS {
            to_release = Some(id);
            break;
        }

        d.cc.on_timeout(d.retx.inflight_bytes());
        d.retx.clear();
        bufs.send.retransmit_timeout();
        d.snd_nxt = d.snd_una;
        d.rtt.back_off();
        bufs.send.rto_deadline_ms = now_ms.saturating_add(d.rtt.rto_ms() as u64);

        retransmitted = Some(id);
        break;
    }

    if let Some(id) = to_release {
        table.release(id);
        return None;
    }

    retransmitted
}

/// Check all connections for pending delayed ACKs.
pub fn delayed_ack_check(now_ms: u64) -> Option<(ConnId, TcpOutSegment)> {
    let mut table = PCB_TABLE.lock();

    for (id, pcb, bufs) in table.iter_mut_with_bufs() {
        if bufs.recv.should_ack_now(now_ms) {
            let PcbState::Data(d) = &pcb.state else {
                continue;
            };
            let window = bufs.recv.window();
            let seg = SegmentBuilder::ack(pcb.tuple, d.snd_nxt.raw(), d.rcv_nxt.raw(), window);
            bufs.recv.ack_sent();
            return Some((id, seg));
        }
    }

    None
}

/// Generate a zero-window probe for a connection with snd_wnd == 0.
pub fn zero_window_probe(id: ConnId, _now_ms: u64) -> Option<TcpOutSegment> {
    let table = PCB_TABLE.lock();
    let pcb = table.get(id)?;

    let PcbState::Data(d) = &pcb.state else {
        return None;
    };
    let bufs = table.bufs(id);
    if d.snd_wnd != 0 || bufs.send.buffered_len() == 0 {
        return None;
    }

    let mut byte = [0u8; 1];
    let peeked = bufs.send.peek_unsent(&mut byte);
    if peeked == 0 {
        return None;
    }

    let window = bufs.recv.window();
    Some(SegmentBuilder::data_push(
        pcb.tuple,
        d.snd_nxt.raw(),
        d.rcv_nxt.raw(),
        window,
    ))
}

/// Release all connections (for testing).
pub fn reset_all() {
    PCB_TABLE.lock().clear();
    isn::reset_for_tests();
}
