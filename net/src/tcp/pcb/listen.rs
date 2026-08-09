//! `Listen` state: waiting for incoming SYNs.
//!
//! # Behavior
//!
//! 1. **RST in** — drop the matching half-open entry, if any, and say nothing.
//!    A RST aimed at a LISTEN socket can never be a legitimate "reset this
//!    connection", so answering one would be a stateless-reset amplifier.
//! 2. **SYN in** — admit it to the [`SynQueue`] and emit the SYN+ACK the queue
//!    builds. A full queue drops the SYN silently. **No** connection-table
//!    slot is spent here: half-open state is the listener's, and bounded.
//! 3. **ACK in** — offer it to the SYN queue as the final ACK of a handshake.
//!    On a match the handler fills [`Actions::accepted`] and the glue layer in
//!    `tcp::input` installs the child PCB. On no match, RST: RFC 793 §3.4, an
//!    ACK arriving at a LISTEN socket proves the sender has stale state.
//! 4. **Anything else** — dropped silently.
//!
//! The handler stays sans-IO — it never touches the connection table itself,
//! because doing so would need a second mutable borrow while the listener's
//! own slot lock is held.

use super::super::actions::{Actions, SocketNotify};
use super::super::header::{DEFAULT_MSS, TcpHeader, parse_tcp_options};
use super::super::listener::SynQueue;
use super::super::segment::SegmentBuilder;
use super::super::tuple::TcpTuple;
use super::{Pcb, PcbState};
use crate::types::{Ipv4Addr, Port, SockAddr};

/// State-specific payload for the Listen variant: the half-open connections
/// this listener is holding. The accept queue is the socket layer's.
#[derive(Debug, Default)]
pub struct ListenState {
    syn: SynQueue,
}

impl ListenState {
    pub const fn new() -> Self {
        Self {
            syn: SynQueue::new(),
        }
    }

    /// A listener carrying a SYN queue whose capacity is already reserved.
    pub const fn with_syn_queue(syn: SynQueue) -> Self {
        Self { syn }
    }

    pub fn syn_queue(&self) -> &SynQueue {
        &self.syn
    }

    pub fn syn_queue_mut(&mut self) -> &mut SynQueue {
        &mut self.syn
    }

    /// Apply an incoming segment to a Listen PCB.
    ///
    /// `incoming` is the segment's real four-tuple. The listener's own tuple
    /// carries a wildcard remote, so the peer's address has to arrive from the
    /// IPv4 layer rather than be read off the PCB.
    pub fn on_segment(
        pcb: &mut Pcb,
        incoming: &TcpTuple,
        hdr: &TcpHeader,
        options: &[u8],
        now_ms: u64,
    ) -> Actions {
        let mut actions = Actions::new();
        let remote = SockAddr::new(Ipv4Addr(incoming.remote_ip), Port(incoming.remote_port));

        let PcbState::Listen(listen) = &mut pcb.state else {
            return actions;
        };

        // Step 1: a RST retires the half-open entry it names, and is never
        // answered.
        if hdr.is_rst() {
            listen.syn.remove(remote);
            return actions;
        }

        // Step 2: a SYN takes a SYN-queue slot, not a connection-table slot.
        if hdr.is_syn() {
            let parsed = parse_tcp_options(options);
            let peer_mss = parsed.mss.unwrap_or(DEFAULT_MSS);
            let peer_tsval = parsed.timestamp.map(|(v, _)| v);
            if let Some(syn_ack) = listen.syn.on_syn(
                remote,
                hdr.seq_num,
                peer_mss,
                parsed.sack_permitted,
                now_ms,
                peer_tsval,
            ) {
                actions.push_segment(syn_ack);
            }
            return actions;
        }

        // Step 3: an ACK either completes a handshake this listener started or
        // names nothing, which RFC 793 §3.4 answers with a RST.
        if hdr.is_ack() {
            if let Some(accepted) = listen.syn.on_ack(remote, hdr.ack_num) {
                actions.accepted = Some(accepted);
                actions.notify |= SocketNotify::ACCEPT_WAKE;
            } else {
                actions.push_segment(SegmentBuilder::rst_for(
                    hdr,
                    incoming.local_ip,
                    incoming.remote_ip,
                ));
            }
            return actions;
        }

        actions
    }
}
