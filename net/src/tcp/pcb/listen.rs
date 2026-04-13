//! `Listen` state: waiting for incoming SYNs.
//!
//! # Behavior
//!
//! 1. **RST in** — ignored.  A RST aimed at a LISTEN socket can never
//!    be a legitimate "reset this connection" because no connection
//!    exists yet; swallowing it silently prevents stateless reset
//!    attacks.
//! 2. **ACK in** — reject with RST.  RFC 793 §3.4: an ACK arriving
//!    at a LISTEN socket proves the sender has stale state and must
//!    be told.
//! 3. **SYN in** — accept.  The handler generates an ISS, parses the
//!    peer's MSS / WScale options, and emits a SYN+ACK back.  It
//!    **does not** allocate a child PCB itself — it fills
//!    [`Actions::accepted`] with the [`AcceptedConn`] metadata and
//!    lets the glue layer in `tcp::input` install the new PCB in
//!    [`crate::tcp::table::PcbTable`] while still holding the lock.
//!    This keeps `Listen::on_segment` sans-IO and avoids the
//!    awkwardness of nested table borrows inside a handler.
//! 4. **Anything else** — dropped silently.
//!
//! The SYN queue in `crate::tcp::listener::TcpListenState` (P2.4) is
//! unchanged and lives alongside this handler — it provides the
//! Linux-style two-queue protection against SYN floods for cases
//! where the socket layer wants that guarantee.  The handler here is
//! the simpler fallback path that directly spawns a child PCB on
//! SYN, mirroring the pre-migration `process_listen`.

use super::super::actions::{Actions, SocketNotify};
use super::super::header::{
    DEFAULT_MSS, DEFAULT_WINDOW_SIZE, TCP_FLAG_RST, TCP_FLAG_SYN, TcpHeader, parse_tcp_options,
};
use super::super::isn;
use super::super::listener::AcceptedConn;
use super::super::segment::{SegmentBuilder, TcpOutSegment};
use super::super::tuple::TcpTuple;
use super::Pcb;

/// State-specific payload for the Listen variant.  Intentionally empty
/// — the SYN queue + accept queue live in
/// [`crate::tcp::listener::TcpListenState`].
#[derive(Debug, Default)]
pub struct ListenState {
    _private: (),
}

impl ListenState {
    pub const fn new() -> Self {
        Self { _private: () }
    }

    /// Apply an incoming segment to a Listen PCB.
    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, options: &[u8], _now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let incoming = TcpTuple {
            local_ip: pcb.tuple.local_ip,
            local_port: hdr.dst_port,
            remote_ip: pcb.tuple.remote_ip, // unused — real value comes from IPv4 layer
            remote_port: hdr.src_port,
        };

        // Step 1: RST on a LISTEN socket is swallowed.  RFC 793 §3.4.
        if hdr.is_rst() {
            return actions;
        }

        // Step 2: ACK with no matching connection must trigger RST.
        if hdr.is_ack() {
            // SegmentBuilder::rst_for synthesizes a response that mirrors
            // the incoming seq/ack bookkeeping per RFC 793 §3.4.
            actions.push_segment(SegmentBuilder::rst_for(
                hdr,
                incoming.local_ip,
                incoming.remote_ip,
            ));
            return actions;
        }

        // Step 3: a SYN is the only other thing we care about.
        if !hdr.is_syn() {
            return actions;
        }

        // Parse options; a missing MSS option falls back to DEFAULT_MSS.
        let parsed = parse_tcp_options(options);
        let peer_mss = parsed.mss.unwrap_or(DEFAULT_MSS);

        // Generate a fresh ISS per RFC 6528 (see tcp::isn).
        let iss = isn::generate_isn(&incoming);
        let irs = hdr.seq_num;

        // Ask the glue layer to install a child PCB (in SynRecv state)
        // in the PcbTable by populating `Actions::accepted`.  The glue
        // layer has the table lock already — doing the install here
        // would require a second mutable borrow.
        actions.accepted = Some(AcceptedConn {
            tuple: incoming,
            iss,
            irs,
            peer_mss,
            sack_permitted: parsed.sack_permitted,
        });

        // Emit the SYN+ACK that completes step 1 of the 3WHS.
        let syn_ack = TcpOutSegment {
            tuple: incoming,
            seq_num: iss,
            ack_num: irs.wrapping_add(1),
            flags: TCP_FLAG_SYN | crate::tcp::header::TCP_FLAG_ACK,
            window_size: DEFAULT_WINDOW_SIZE,
            mss: DEFAULT_MSS,
            wscale: 255,
            sack_permitted: parsed.sack_permitted,
            sack_blocks: [(0, 0); 4],
            sack_block_count: 0,
        };
        let _ = TCP_FLAG_RST; // silence dead-import lint if options parser changes
        let _ = TCP_FLAG_SYN;
        actions.push_segment(syn_ack);

        actions.notify |= SocketNotify::ACCEPT_WAKE;
        actions
    }

    /// Per-state debug invariants.  Listen has no send buffer, no
    /// retransmit timer, and no sequence-space state to validate.
    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, bufs: &crate::tcp::buffer::TcpBufferPair) {
        debug_assert!(
            bufs.send.buffered_len() == 0,
            "Listen PCB has unexpected send data"
        );
        debug_assert!(
            bufs.recv.available() == 0,
            "Listen PCB has unexpected recv data"
        );
    }
}
