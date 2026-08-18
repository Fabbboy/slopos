//! `Listen` state: waiting for incoming SYNs.
//!
//! A RST is never answered: answering one aimed at a LISTEN socket would make
//! this a stateless-reset amplifier. A SYN spends a [`SynQueue`] slot, never a
//! connection-table slot, so half-open state stays the listener's and bounded.
//! The handler is sans-IO — touching the connection table would need a second
//! mutable borrow while the listener's own slot lock is held.

use super::super::actions::{Actions, SocketNotify};
use super::super::header::{DEFAULT_MSS, TcpHeader, parse_tcp_options};
use super::super::listener::SynQueue;
use super::super::segment::SegmentBuilder;
use super::super::tuple::TcpTuple;
use super::{Pcb, PcbState};
use crate::types::{Ipv4Addr, Port, SockAddr};

/// The half-open connections this listener holds; the accept queue is the
/// socket layer's.
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

    pub const fn with_syn_queue(syn: SynQueue) -> Self {
        Self { syn }
    }

    pub fn syn_queue(&self) -> &SynQueue {
        &self.syn
    }

    pub fn syn_queue_mut(&mut self) -> &mut SynQueue {
        &mut self.syn
    }

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

        if hdr.is_rst() {
            listen.syn.remove(remote);
            return actions;
        }

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

        // An ACK matching no half-open handshake: RFC 793 §3.4 answers with RST.
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
