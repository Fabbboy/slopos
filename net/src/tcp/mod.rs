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
pub use crate::clock;
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

use buffer::SegmentSource;
pub use buffer::{
    DELAYED_ACK_MS, DELAYED_ACK_SEGMENTS, TCP_BUFFER_SIZE, TcpBuffer, TcpBufferPair, TcpRecvState,
    TcpSendState, ZWP_INTERVAL_MS, ZcSource,
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
pub use segment::{SegmentBuilder, TcpOutSegment, write_tcp_segment};
pub use seq::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};
pub use table::ConnId;

use self::cong::CongestionControl;
use crate::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};
use crate::types::{Ipv4Addr, Port, SockAddr};

use slopos_ostd::klog_debug;
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::{KVec, ZcNotifToken};

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

/// Keepalive idle period before the first probe (RFC 1122 default 2 h).
const TCP_KEEPALIVE_IDLE_MS: u64 = 7_200 * 1_000;
/// Interval between keepalive probes once probing has started (75 s).
const TCP_KEEPALIVE_INTERVAL_MS: u64 = 75 * 1_000;
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

    // Lock-free demux: `table::find` enters NET_EPOCH and reads the
    // RCU-published shard/listener indices. No SpinLock is taken on
    // the dispatch read.
    let Some(id) = table::find(&incoming_tuple) else {
        return if hdr.is_rst() {
            Actions::new()
        } else {
            input_no_match_rst(hdr, dst_ip, src_ip)
        };
    };

    if id.is_listener() {
        let (mut actions, parent_sock) =
            input_process_listener(id, &incoming_tuple, hdr, options, now_ms);
        // `accepted` means the listener's SYN queue matched this segment as
        // the final ACK of a handshake. Installing the child runs *outside*
        // the listener's per-slot lock (already dropped) so it can take the
        // matching shard's write lock.
        if actions.accepted.is_some()
            && let Some(child_id) =
                install_accepted_child(&incoming_tuple, &actions, hdr, parent_sock)
        {
            // Hand the child its own ACK. Its SynRecv handler runs the
            // transition to Data, which is where the connection buffers and
            // the NEW_ESTABLISHED notify come from — the same path every
            // other segment takes, rather than a second copy of it here.
            let mut child_actions =
                input_process_established(child_id, &incoming_tuple, hdr, options, payload, now_ms);
            child_actions.merge_segments_from(&mut actions);
            return child_actions;
        }
        if actions.accepted.is_some() {
            // The table had no room. The peer believes the handshake
            // completed, so it has to be told otherwise.
            actions.accepted = None;
            actions.notify = SocketNotify::empty();
            actions.push_segment(SegmentBuilder::rst_for(hdr, dst_ip, src_ip));
        }
        actions.conn_id = Some(id);
        actions
    } else {
        input_process_established(id, &incoming_tuple, hdr, options, payload, now_ms)
    }
}

/// Run the listener state machine on `id` under its per-slot lock.
/// Returns the `Actions` and the parent socket id (needed to wire the
/// child PCB to the same socket).
#[inline(never)]
fn input_process_listener(
    id: ConnId,
    incoming: &TcpTuple,
    hdr: &TcpHeader,
    options: &[u8],
    now_ms: u64,
) -> (Actions, Option<pcb::SocketId>) {
    table::with_pcb_mut(id, |pcb| {
        let mut actions = pcb.on_segment(None, incoming, hdr, options, &[], now_ms);
        actions.conn_id = Some(id);
        (actions, pcb.socket_id)
    })
    .unwrap_or_else(|| (Actions::new(), None))
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
) -> Option<ConnId> {
    let accepted = match &actions.accepted {
        Some(a) => a,
        None => return None,
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

    // The failure is not discarded: the peer completed a handshake, and a
    // connection it cannot be given has to be reset rather than left open on
    // its side alone.
    table::install_established(*incoming_tuple, PcbState::SynRecv(child_state), |child| {
        child.socket_id = parent_sock;
    })
    .ok()
}

/// A SYN-ACK retransmit timer fired for `key`.
///
/// The key belongs to a SYN-queue entry, not to a `ConnId`, so the owning
/// listener is found by scanning the listener slots — there are
/// `MAX_LISTENERS` of them and this runs once per timer, not per segment.
pub fn on_syn_ack_retransmit(key: u32) -> Option<TcpOutSegment> {
    for slot in 0..table::MAX_LISTENERS {
        let found = table::with_listener_slot_mut(slot, |pcb| {
            let PcbState::Listen(listen) = &mut pcb.state else {
                return None;
            };
            if !listen.syn_queue().has_key(key) {
                return None;
            }
            listen.syn_queue_mut().on_retransmit(key)
        });
        if let Some(Some(seg)) = found {
            return Some(seg);
        }
    }
    None
}

/// Release every established child a closing listener still owns.
///
/// A child installed by [`install_accepted_child`] carries its parent's
/// `socket_id` and holds a shard slot. Nothing else reclaims those when the
/// listener goes away — they would sit until a RST, a FIN or `TIME_WAIT`
/// expiry — so the close releases them, and returns their tuples for the
/// caller to reset the peers it was still speaking to.
pub fn release_children_of(socket_id: pcb::SocketId) -> KVec<(TcpTuple, u32)> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let count = table::snapshot_shard_conn_ids(&mut ids);
    let mut orphans = KVec::new();
    for id in ids.iter().take(count).flatten() {
        let owned = table::with_pcb(*id, |pcb| {
            (pcb.socket_id == Some(socket_id)).then(|| (pcb.tuple, pcb.state.snd_nxt_raw()))
        })
        .flatten();
        if let Some(entry) = owned {
            if orphans.push(entry).is_err() {
                break;
            }
            table::release(*id);
        }
    }
    orphans
}

/// Give `id` the send/receive rings its next transition assumes, if it does not
/// already have them.
///
/// The pair is 2 x 32 KiB and is allocated outside the PCB lock: the allocator
/// is where every subsystem meets, and a slab refill under the
/// `TCP_PCB_SLOTS` cli-spinlock — a lock an unauthenticated remote peer drives
/// — is the shape that deadlocks. A PCB is installed in `SynRecv` and never
/// re-enters it, so a peek that says "will not need rings" cannot go stale in
/// the direction that matters; the reverse only wastes an allocation.
///
/// Kept out of the state closures on purpose: a `TcpBufferPair` rvalue on one
/// of those frames costs more than the 2 KiB stack gate allows.
#[inline(never)]
fn ensure_connection_buffer(id: ConnId) -> Result<(), TcpError> {
    let wanted = table::with_pcb_and_bufs(id, |pcb, buf| {
        buf.is_none() && matches!(pcb.state, PcbState::SynRecv(_) | PcbState::SynSent(_))
    });
    if wanted != Some(true) {
        return Ok(());
    }
    let pair = TcpBufferPair::new(buffer::TCP_BUFFER_SIZE)?;
    table::with_pcb_and_bufs(id, |_, slot| {
        if slot.is_none() {
            *slot = Some(pair);
        }
    });
    Ok(())
}

/// Abandon a connection that reached `Data` with no rings to serve it.
///
/// Nothing downstream is told it established, the peer is reset rather than
/// left holding a handshake this side cannot honour, and the slot goes back.
#[inline(never)]
fn reset_for_no_buffer(actions: &mut Actions, tuple: &TcpTuple, hdr: &TcpHeader) {
    klog_debug!("tcp: no memory for a new connection's buffers; resetting peer");
    for seg in actions.segments.iter_mut() {
        *seg = None;
    }
    actions.segments_len = 0;
    actions.notify = SocketNotify::empty();
    actions.accepted = None;
    actions.push_segment(SegmentBuilder::rst_for(
        hdr,
        tuple.local_ip,
        tuple.remote_ip,
    ));
    actions.release = true;
}

/// Process a segment for an established/transient connection. Takes the
/// per-slot lock for the PCB+buffer mutation; releases it before
/// dispatching `table::release` to avoid recursive lock acquisition.
///
/// `#[inline(never)]` so the ~400 B `Actions` return value stays in
/// this function's frame rather than doubling `tcp::input`'s frame
/// via inlining.
#[inline(never)]
fn input_process_established(
    id: ConnId,
    incoming: &TcpTuple,
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> Actions {
    let _ = ensure_connection_buffer(id);

    let actions = table::with_pcb_and_bufs(id, |pcb, buffer_slot| {
        let mut actions = pcb.on_segment(
            buffer_slot.as_mut(),
            incoming,
            hdr,
            options,
            payload,
            now_ms,
        );
        actions.conn_id = Some(id);

        // Apply timer operations. State handlers emit `key: 0` as a
        // sentinel — we substitute the real ConnId.
        for i in 0..actions.timer_ops_len as usize {
            if let Some(ref op) = actions.timer_ops[i] {
                match *op {
                    TimerOp::Schedule {
                        kind,
                        key: _,
                        delay_ms,
                    } => {
                        let token = NET_TIMER_WHEEL.schedule(delay_ms, kind, id.raw());
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
                    TimerOp::Cancel { token } => {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                }
            }
        }

        // Install the buffer on NEW_ESTABLISHED (SynRecv/SynSent → Data). A
        // connection that reaches Data without one has no send or receive ring,
        // so a peer that cannot be given memory is reset rather than left
        // holding a handshake this side cannot honour.
        if actions.notify.contains(SocketNotify::NEW_ESTABLISHED) && buffer_slot.is_none() {
            reset_for_no_buffer(&mut actions, &pcb.tuple, hdr);
        }

        // Free buffer on Data → TimeWait.
        if matches!(pcb.state, PcbState::TimeWait(_)) && buffer_slot.is_some() {
            *buffer_slot = None;
        }

        // Reset keepalive timer on data activity.
        if !actions.release
            && actions
                .notify
                .intersects(SocketNotify::RECV_WAKE | SocketNotify::SEND_WAKE)
        {
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some((old_token, delay)) = d.reset_keepalive_on_activity() {
                    NET_TIMER_WHEEL.cancel(old_token);
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.raw());
                    d.keepalive_token = Some(token);
                }
            }
        }

        actions
    });

    // Schedule the initial keepalive timer for a newly-established
    // connection, outside the PCB lock.
    //
    // The option lives in the socket table, and the socket layer takes that
    // table before calling down into a PCB — so reading it from under the
    // PCB lock would invert the socket -> tcp order. Establishment happens
    // once per connection, so the extra lock round-trip is not on any hot
    // path.
    if actions
        .as_ref()
        .is_some_and(|a| a.notify.contains(SocketNotify::NEW_ESTABLISHED))
    {
        let socket_id = table::with_pcb(id, |pcb| pcb.socket_id).flatten();
        let keepalive_enabled = socket_id
            .map(|sid| crate::socket::socket_keepalive_enabled_by_index(sid.0 as usize))
            .unwrap_or(false);
        table::with_pcb_mut(id, |pcb| {
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some(delay) = d.schedule_initial_keepalive(keepalive_enabled) {
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.raw());
                    d.keepalive_token = Some(token);
                }
            }
        });
    }

    let actions = actions.unwrap_or_else(|| {
        let mut a = Actions::new();
        a.conn_id = Some(id);
        a
    });

    // Release outside the per-slot lock: `table::release` re-acquires
    // the same SpinLock to clear the slot, plus the per-shard write
    // lock to publish the new index.
    if actions.release {
        table::release(id);
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
        id
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
    // The queue is built here, before the listener slot lock is taken: a
    // `push` that grew it inside `on_syn` would run the allocator beneath a
    // cli-spinlock an unauthenticated peer drives.
    let local = SockAddr::new(Ipv4Addr(local_ip), Port(local_port));
    let syn_queue = listener::SynQueue::with_capacity(local)?;
    let id = table::install_listener(
        tuple,
        PcbState::Listen(pcb::ListenState::with_syn_queue(syn_queue)),
        |_| {},
    )?;

    klog_debug!("tcp: LISTEN on port {} id={}", local_port, id);
    Ok(id)
}

/// Close a connection (initiate graceful teardown).
///
/// Returns the outgoing FIN segment if one should be sent.
pub fn close(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let name = table::with_pcb(id, |pcb| pcb.state.name()).ok_or(TcpError::NotFound)?;
        table::release(id);
        klog_debug!("tcp: CLOSE id={} from {} — released", id, name);
        return Ok(None);
    }

    enum Outcome {
        Release(&'static str),
        Segment(TcpOutSegment),
        NoOp,
    }

    ensure_connection_buffer(id)?;

    let result = table::with_pcb_and_bufs(id, |pcb, buffer_slot| -> Result<Outcome, TcpError> {
        if matches!(pcb.state, PcbState::Listen(_) | PcbState::SynSent(_)) {
            return Ok(Outcome::Release(pcb.state.name()));
        }
        if matches!(pcb.state, PcbState::TimeWait(_)) {
            klog_debug!("tcp: CLOSE id={} TIME_WAIT — no-op", id);
            return Ok(Outcome::NoOp);
        }
        // SynRecv → Data(FinWait1) needs the rings the transition assumes.
        if matches!(pcb.state, PcbState::SynRecv(_)) && buffer_slot.is_none() {
            return Err(TcpError::OutOfMemory);
        }
        match &mut pcb.state {
            PcbState::SynRecv(_) => close_syn_recv_transition(pcb, id)
                .map(|s| s.map_or(Outcome::NoOp, Outcome::Segment)),
            PcbState::Data(d) => {
                let tuple = pcb.tuple;
                match d.close_phase {
                    ClosePhase::Established => {
                        let seq = d.snd_nxt.raw();
                        d.snd_nxt = d.snd_nxt.wrapping_add(1);
                        d.close_phase = ClosePhase::FinWait1;
                        cancel_keepalive(d);
                        let mut seg =
                            SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                        seg.timestamp = d.ts_option(clock::now_ms());
                        pcb.assert_invariants();
                        klog_debug!(
                            "tcp: CLOSE id={} ESTABLISHED -> FIN_WAIT_1, FIN seq={}",
                            id,
                            seq
                        );
                        Ok(Outcome::Segment(seg))
                    }
                    ClosePhase::CloseWait => {
                        let seq = d.snd_nxt.raw();
                        d.snd_nxt = d.snd_nxt.wrapping_add(1);
                        d.close_phase = ClosePhase::LastAck;
                        cancel_keepalive(d);
                        let mut seg =
                            SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                        seg.timestamp = d.ts_option(clock::now_ms());
                        pcb.assert_invariants();
                        klog_debug!(
                            "tcp: CLOSE id={} CLOSE_WAIT -> LAST_ACK, FIN seq={}",
                            id,
                            seq
                        );
                        Ok(Outcome::Segment(seg))
                    }
                    _ => {
                        klog_debug!("tcp: CLOSE id={} already closing ({:?})", id, d.close_phase);
                        Ok(Outcome::NoOp)
                    }
                }
            }
            _ => Err(TcpError::InvalidState),
        }
    });

    match result {
        None => Err(TcpError::NotFound),
        Some(Err(e)) => Err(e),
        Some(Ok(Outcome::Release(name))) => {
            table::release(id);
            klog_debug!("tcp: CLOSE id={} from {} — released", id, name);
            Ok(None)
        }
        Some(Ok(Outcome::NoOp)) => Ok(None),
        Some(Ok(Outcome::Segment(s))) => Ok(Some(s)),
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
    let mut ds = slopos_ostd::KBox::try_init(DataState::init_from_syn_recv(s))?;
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
    klog_debug!("tcp: CLOSE id={} SYN_RECV -> FIN_WAIT_1", id);
    Ok(Some(seg))
}

/// Abort a connection (send RST, release immediately).
pub fn abort(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let name = table::with_pcb(id, |pcb| pcb.state.name()).ok_or(TcpError::NotFound)?;
        klog_debug!("tcp: ABORT id={} from {}", id, name);
        table::release(id);
        return Ok(None);
    }

    let seg = table::with_pcb(id, |pcb| {
        klog_debug!("tcp: ABORT id={} from {}", id, pcb.state.name());
        match &pcb.state {
            PcbState::Listen(_) => None,
            PcbState::SynSent(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
            PcbState::SynRecv(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
            PcbState::Data(d) => Some(SegmentBuilder::bare_rst(pcb.tuple, d.snd_nxt.raw())),
            PcbState::TimeWait(tw) => {
                Some(SegmentBuilder::bare_rst(pcb.tuple, tw.last_snd_nxt.raw()))
            }
        }
    })
    .ok_or(TcpError::NotFound)?;

    table::release(id);
    Ok(seg)
}

/// Shutdown the write half of a connection (send FIN without releasing).
pub fn shutdown_write(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }

    ensure_connection_buffer(id)?;

    let result = table::with_pcb_and_bufs(
        id,
        |pcb, buffer_slot| -> Result<Option<TcpOutSegment>, TcpError> {
            // SynRecv → Data(FinWait1) needs the rings the transition assumes.
            if matches!(pcb.state, PcbState::SynRecv(_)) && buffer_slot.is_none() {
                return Err(TcpError::OutOfMemory);
            }
            match &mut pcb.state {
                PcbState::Data(d) => {
                    let tuple = pcb.tuple;
                    match d.close_phase {
                        ClosePhase::Established => {
                            let seq = d.snd_nxt.raw();
                            d.snd_nxt = d.snd_nxt.wrapping_add(1);
                            d.close_phase = ClosePhase::FinWait1;
                            cancel_keepalive(d);
                            let mut seg =
                                SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                            seg.timestamp = d.ts_option(clock::now_ms());
                            pcb.assert_invariants();
                            klog_debug!("tcp: SHUTDOWN_WR id={} ESTABLISHED -> FIN_WAIT_1", id);
                            Ok(Some(seg))
                        }
                        ClosePhase::CloseWait => {
                            let seq = d.snd_nxt.raw();
                            d.snd_nxt = d.snd_nxt.wrapping_add(1);
                            d.close_phase = ClosePhase::LastAck;
                            cancel_keepalive(d);
                            let mut seg =
                                SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.rcv_wnd);
                            seg.timestamp = d.ts_option(clock::now_ms());
                            pcb.assert_invariants();
                            klog_debug!("tcp: SHUTDOWN_WR id={} CLOSE_WAIT -> LAST_ACK", id);
                            Ok(Some(seg))
                        }
                        _ => {
                            klog_debug!(
                                "tcp: SHUTDOWN_WR id={} already closing ({:?})",
                                id,
                                d.close_phase
                            );
                            Ok(None)
                        }
                    }
                }
                PcbState::SynRecv(_) => shutdown_write_syn_recv_transition(pcb, id),
                _ => Err(TcpError::InvalidState),
            }
        },
    );

    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
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
    let mut ds = slopos_ostd::KBox::try_init(DataState::init_from_syn_recv(s))?;
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
    klog_debug!("tcp: SHUTDOWN_WR id={} SYN_RECV -> FIN_WAIT_1", id);
    Ok(Some(seg))
}

/// Discard all data in the receive buffer (for SHUT_RD).
pub fn recv_discard(id: ConnId) {
    if id.is_listener() {
        return;
    }
    let cleared = table::with_pcb_and_bufs(id, |_pcb, buf| {
        if let Some(b) = buf.as_mut() {
            b.recv.clear();
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if cleared {
        klog_debug!("tcp: RECV_DISCARD id={} — recv buffer cleared", id);
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
    table::with_bufs(id, |b| b.send.free_space()).unwrap_or(0)
}

/// Bytes available to read from a connection's receive buffer.
pub fn recv_available(id: ConnId) -> usize {
    table::with_bufs(id, |b| b.recv.available()).unwrap_or(0)
}

/// Whether a connection has data pending transmission.
pub fn has_pending_data(id: ConnId) -> bool {
    table::with_bufs(id, |b| b.send.unsent_len() > 0).unwrap_or(false)
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
    let capped = core::cmp::min(bytes, buffer::TCP_BUFFER_SIZE);
    table::with_pcb_and_bufs(id, |_pcb, buf| {
        if let Some(b) = buf.as_mut() {
            b.send.effective_capacity = capped;
        }
    });
}

/// Set the effective receive buffer capacity (SO_RCVBUF).
/// Values above TCP_BUFFER_SIZE are silently capped.
pub fn set_rcvbuf(id: ConnId, bytes: usize) {
    let capped = core::cmp::min(bytes, buffer::TCP_BUFFER_SIZE);
    table::with_pcb_and_bufs(id, |_pcb, buf| {
        if let Some(b) = buf.as_mut() {
            b.recv.effective_capacity = capped;
        }
    });
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
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        match &pcb.state {
            PcbState::Data(d)
                if matches!(
                    d.close_phase,
                    ClosePhase::Established | ClosePhase::CloseWait
                ) => {}
            _ => return Err(TcpError::InvalidState),
        }
        let Some(bufs) = buf.as_mut() else {
            return Err(TcpError::InvalidState);
        };
        Ok(bufs.send.enqueue(data))
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Single-direct-copy [`send`]: buffer the payload by pulling it straight from
/// the pinned user pages (via `reader`) into the send ring with one volatile
/// copy — no kernel scratch. Returns the number of bytes buffered.
pub fn send_from(
    id: ConnId,
    reader: &mut slopos_ostd::mm::VmReader<'_>,
) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        match &pcb.state {
            PcbState::Data(d)
                if matches!(
                    d.close_phase,
                    ClosePhase::Established | ClosePhase::CloseWait
                ) => {}
            _ => return Err(TcpError::InvalidState),
        }
        let Some(bufs) = buf.as_mut() else {
            return Err(TcpError::InvalidState);
        };
        Ok(bufs.send.enqueue_from(reader))
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Read data from a connection's receive buffer.
pub fn recv(id: ConnId, out: &mut [u8]) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        let Some(bufs) = buf.as_mut() else {
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
        let recv_window = bufs.recv.window();
        if let PcbState::Data(d) = &mut pcb.state {
            d.rcv_wnd = recv_window;
        }
        Ok(read)
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Single-direct-copy [`recv`]: drain received bytes straight into the pinned
/// user pages (via `writer`) with one volatile copy — no kernel scratch.
/// Mirrors [`recv`]'s EOF / reset / window-update semantics; the byte count is
/// what the writer accepted.
pub fn recv_into(
    id: ConnId,
    writer: &mut slopos_ostd::mm::VmWriter<'_>,
) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        let Some(bufs) = buf.as_mut() else {
            return Err(TcpError::InvalidState);
        };
        let read = bufs.recv.dequeue_into(writer);
        if read == 0 && bufs.recv.available() == 0 {
            if let PcbState::Data(d) = &pcb.state {
                if d.reset_received {
                    return Err(TcpError::ConnectionReset);
                }
            }
        }
        let recv_window = bufs.recv.window();
        if let PcbState::Data(d) = &mut pcb.state {
            d.rcv_wnd = recv_window;
        }
        Ok(read)
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Append a TCP `MSG_ZEROCOPY` chunk to the send queue: the NIC will DMA `len`
/// bytes straight from the pinned pages `keepalive` (data starting at the pin's
/// `base_off`), held across retransmits until the bytes are cumulatively ACKed.
/// `token` is the refcounted notification token (owning the chunk's reference);
/// the ring posts `F_NOTIF` when it reaches zero. Returns `Some(len)` on success,
/// or `None` (connection not in a sendable Data state, the chunk does not fit
/// SO_SNDBUF, or the chunk store cannot grow) so the caller falls back to the
/// single-direct-copy leaf. On `None` the `keepalive`/`token` are dropped here.
pub fn enqueue_zerocopy(
    id: ConnId,
    keepalive: KeepaliveFrames,
    base_off: usize,
    len: usize,
    token: ZcNotifToken,
) -> Option<usize> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(id, |pcb, buf| -> Option<usize> {
        match &pcb.state {
            PcbState::Data(d)
                if matches!(
                    d.close_phase,
                    ClosePhase::Established | ClosePhase::CloseWait
                ) => {}
            _ => return None,
        }
        let bufs = buf.as_mut()?;
        // The copy leaf handles SO_SNDBUF blocking correctly, so a chunk that
        // does not fit just falls back to it.
        if bufs.send.zc_free_space() < len {
            return None;
        }
        if bufs
            .send
            .enqueue_zerocopy(keepalive, base_off, len as u32, token)
        {
            Some(len)
        } else {
            None
        }
    })
    .flatten()
}

/// Resolve the source of one segment at stream offset `off` (≤ `max_len` bytes,
/// never crossing a chunk boundary): copy inline bytes into `payload_buf`, or
/// carry a [`ZcSource`] the caller DMAs straight from the pinned pages.
/// `#[inline(never)]` so its (re)transmit-source temporaries stay off
/// `poll_transmit`'s frame (the per-segment stack-size budget). `None` = nothing
/// buffered there.
#[inline(never)]
fn resolve_segment(
    send: &TcpSendState,
    off: usize,
    max_len: usize,
    payload_buf: &mut [u8],
) -> Option<(usize, Option<ZcSource>)> {
    match send.segment_source(off, max_len) {
        SegmentSource::Empty => None,
        SegmentSource::Inline { len } => {
            let copied = send.peek_retransmit(off, &mut payload_buf[..len]);
            if copied == 0 {
                None
            } else {
                Some((copied, None))
            }
        }
        SegmentSource::Zerocopy {
            keepalive,
            byte_start,
            len,
            token,
        } => Some((
            len,
            Some(ZcSource {
                keepalive,
                byte_start,
                len,
                token,
            }),
        )),
    }
}

/// Generate the next outgoing data segment for a connection.
///
/// Returns the segment, its payload byte count, and — when the bytes live in a
/// zero-copy chunk — a [`ZcSource`] the caller DMAs straight from the pinned
/// pages (or copies from on a cold neighbor). `None` zero-copy source means the
/// payload was copied into `payload_buf` (the inline / copy path).
pub fn poll_transmit(
    id: ConnId,
    payload_buf: &mut [u8],
    now_ms: u64,
) -> Option<(TcpOutSegment, usize, Option<ZcSource>)> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(
        id,
        |pcb, buf| -> Option<(TcpOutSegment, usize, Option<ZcSource>)> {
            let bufs = buf.as_mut()?;

            let PcbState::Data(d) = &mut pcb.state else {
                return None;
            };
            // Project once through the KBox `DerefMut` so the borrow
            // checker can split sub-field borrows below.
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

            // Window calculation uses pipe (RFC 6675) — bytes believed to
            // be in the network — instead of raw inflight.
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
                    // Re-DMA on retransmit: a zero-copy segment re-reads the same
                    // pinned pages; an inline segment re-copies from the ring.
                    if let Some((seg_len, zc)) =
                        resolve_segment(&bufs.send, offset, len, payload_buf)
                    {
                        d.sendmap.mark_retransmitted(lost.seq);
                        let window = bufs.recv.window();
                        let mut seg =
                            SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
                        seg.timestamp = d.ts_option(now_ms);
                        if bufs.send.rto_deadline_ms == 0 {
                            bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
                            if d.retransmit_token.is_none() {
                                let token = NET_TIMER_WHEEL.schedule(
                                    rto_ms.max(1),
                                    TimerKind::TcpRetransmit,
                                    id.raw(),
                                );
                                d.retransmit_token = Some(token);
                            }
                        }
                        pcb.assert_invariants();
                        return Some((seg, seg_len, zc));
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
            let unsent_off = bufs.send.inflight;
            // New data: copy from the inline ring, or carry a zero-copy source the
            // caller DMAs straight from the pinned pages.
            let Some((payload_len, zc)) =
                resolve_segment(&bufs.send, unsent_off, max_send, payload_buf)
            else {
                return None;
            };

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
                    let token =
                        NET_TIMER_WHEEL.schedule(rto_ms.max(1), TimerKind::TcpRetransmit, id.raw());
                    d.retransmit_token = Some(token);
                }
            }

            let window = bufs.recv.window();
            let mut seg = SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
            seg.timestamp = d.ts_option(now_ms);
            pcb.assert_invariants();

            Some((seg, payload_len, zc))
        },
    )
    .flatten()
}

// =============================================================================
// Timer callbacks
// =============================================================================

/// Handle a retransmit timer firing for connection `conn_id`.
pub fn on_retransmit(conn_id: u32) -> Option<ConnId> {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return None;
    }

    enum Outcome {
        Released,
        Retransmitted,
        Skip,
    }

    let outcome = table::with_pcb_and_bufs(id, |pcb, buf| -> Outcome {
        let Some(bufs) = buf.as_mut() else {
            return Outcome::Skip;
        };
        let PcbState::Data(d) = &mut pcb.state else {
            return Outcome::Skip;
        };
        if d.sendmap.is_empty() {
            return Outcome::Skip;
        }
        if d.rtt.consecutive_timeouts >= MAX_RETRANSMITS {
            return Outcome::Released;
        }

        // Project once through the KBox `DerefMut` so the borrow
        // checker can split disjoint field borrows below.
        let d: &mut DataState = &mut **d;

        d.retransmit_token = None;
        d.cc.on_timeout(d.sendmap.pipe());
        d.sendmap.mark_all_lost();
        d.rtt.back_off();

        let rto_ms = d.rtt.rto_ms() as u64;
        let token = NET_TIMER_WHEEL.schedule(rto_ms.max(1), TimerKind::TcpRetransmit, conn_id);
        d.retransmit_token = Some(token);

        let now_ms = clock::now_ms();
        bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
        pcb.assert_invariants();
        klog_debug!("tcp: retransmit fired id={} rto_ms={}", conn_id, rto_ms);
        Outcome::Retransmitted
    });

    match outcome {
        None | Some(Outcome::Skip) => None,
        Some(Outcome::Released) => {
            klog_debug!("tcp: retransmit timeout id={} -> releasing", conn_id);
            table::release(id);
            None
        }
        Some(Outcome::Retransmitted) => Some(id),
    }
}

/// Handle a keepalive timer firing.
pub fn on_keepalive(conn_id: u32) -> Option<TcpOutSegment> {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return None;
    }

    enum Outcome {
        Released,
        Probe(TcpOutSegment),
        Skip,
    }

    let outcome = table::with_pcb_mut(id, |pcb| -> Outcome {
        let PcbState::Data(d) = &mut pcb.state else {
            return Outcome::Skip;
        };
        if d.close_phase != ClosePhase::Established {
            return Outcome::Skip;
        }
        d.keepalive_token = None;
        if d.keepalive_probes_sent >= TCP_KEEPALIVE_PROBES_MAX {
            return Outcome::Released;
        }
        let mut probe_seg =
            SegmentBuilder::keepalive_probe(pcb.tuple, d.snd_una.raw(), d.rcv_nxt.raw(), d.rcv_wnd);
        probe_seg.timestamp = d.ts_option(clock::now_ms());

        d.keepalive_probes_sent = d.keepalive_probes_sent.saturating_add(1);
        let token =
            NET_TIMER_WHEEL.schedule(TCP_KEEPALIVE_INTERVAL_MS, TimerKind::TcpKeepalive, conn_id);
        d.keepalive_token = Some(token);
        Outcome::Probe(probe_seg)
    });

    match outcome {
        None | Some(Outcome::Skip) => None,
        Some(Outcome::Released) => {
            klog_debug!(
                "tcp: keepalive max probes reached id={} -> releasing",
                conn_id
            );
            table::release(id);
            None
        }
        Some(Outcome::Probe(seg)) => Some(seg),
    }
}

/// Handle a TIME_WAIT timer expiry.
pub fn on_time_wait_expire(conn_id: u32) {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return;
    }
    let is_time_wait =
        table::with_pcb(id, |pcb| matches!(pcb.state, PcbState::TimeWait(_))).unwrap_or(false);
    if is_time_wait {
        klog_debug!("tcp: TIME_WAIT timer expired id={}", conn_id);
        table::release(id);
    }
}

/// Handle a FIN_WAIT_2 timer expiry.
pub fn on_fin_wait2_timeout(conn_id: u32) {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return;
    }
    let is_fw2 = table::with_pcb(id, |pcb| match &pcb.state {
        PcbState::Data(d) => d.close_phase == ClosePhase::FinWait2,
        _ => false,
    })
    .unwrap_or(false);
    if is_fw2 {
        klog_debug!("tcp: FIN_WAIT_2 timeout id={}", conn_id);
        table::release(id);
    }
}

// =============================================================================
// Test-only helpers
// =============================================================================

/// Deterministic retransmit probe — test-only.
///
/// Walks the RCU-published shard indices to snapshot live ConnIds,
/// then takes each per-slot lock independently. Triggers retransmit
/// for the first connection whose RTO deadline has expired at `now_ms`.
#[cfg(feature = "test-hooks")]
pub fn retransmit_check(now_ms: u64) -> Option<ConnId> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let n = table::snapshot_shard_conn_ids(&mut ids);
    for entry in &ids[..n] {
        let Some(id) = *entry else { continue };

        #[derive(Clone, Copy)]
        enum Outcome {
            Released,
            Retransmitted,
            Skip,
        }

        let outcome = table::with_pcb_and_bufs(id, |pcb, buf| -> Outcome {
            let Some(bufs) = buf.as_mut() else {
                return Outcome::Skip;
            };
            let send = &bufs.send;
            if send.inflight == 0 || send.rto_deadline_ms == 0 || now_ms < send.rto_deadline_ms {
                return Outcome::Skip;
            }
            let PcbState::Data(d) = &mut pcb.state else {
                return Outcome::Skip;
            };
            let d: &mut DataState = &mut **d;
            if d.rtt.consecutive_timeouts >= MAX_RETRANSMITS {
                return Outcome::Released;
            }
            d.cc.on_timeout(d.sendmap.pipe());
            d.sendmap.mark_all_lost();
            d.rtt.back_off();
            bufs.send.rto_deadline_ms = now_ms.saturating_add(d.rtt.rto_ms() as u64);
            pcb.assert_invariants();
            Outcome::Retransmitted
        });

        match outcome {
            None | Some(Outcome::Skip) => continue,
            Some(Outcome::Released) => {
                table::release(id);
                return None;
            }
            Some(Outcome::Retransmitted) => return Some(id),
        }
    }
    None
}

/// Check all connections for pending delayed ACKs.
pub fn delayed_ack_check(now_ms: u64) -> Option<(ConnId, TcpOutSegment)> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let n = table::snapshot_shard_conn_ids(&mut ids);
    for entry in &ids[..n] {
        let Some(id) = *entry else { continue };
        if let Some(seg) = table::with_pcb_and_bufs(id, |pcb, buf| {
            let bufs = buf.as_mut()?;
            let PcbState::Data(d) = &pcb.state else {
                return None;
            };
            d.check_delayed_ack(pcb.tuple, bufs, now_ms)
        })
        .flatten()
        {
            return Some((id, seg));
        }
    }
    None
}

/// Generate a zero-window probe for a connection with snd_wnd == 0.
pub fn zero_window_probe(id: ConnId, _now_ms: u64) -> Option<TcpOutSegment> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(id, |pcb, buf| {
        let bufs = buf.as_ref()?;
        let PcbState::Data(d) = &pcb.state else {
            return None;
        };
        d.check_zero_window_probe(pcb.tuple, bufs)
    })
    .flatten()
}

/// Release all connections (for testing).
pub fn reset_all() {
    table::clear_all();
    isn::reset_for_tests();
    #[cfg(feature = "test-hooks")]
    challenge_ack::reset_for_tests();
}
