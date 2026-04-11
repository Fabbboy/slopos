//! `Listen` state: waiting for incoming SYNs.
//!
//! A `Listen` PCB carries no sequence numbers or buffers of its own.
//! When a SYN arrives on its local port, the two-queue listen model in
//! `tcp::listener::TcpListenState` (which lives alongside this module;
//! see P2.4) creates a `SynRecvEntry` and sends a SYN-ACK.  The full
//! handshake completion turns that half-open entry into a brand-new
//! `Pcb` in the `Data` state, wired up to a socket via
//! `Actions::accepted`.
//!
//! The B.1 commit ports the real `process_listen` body into
//! [`ListenState::on_segment`].

use super::super::actions::Actions;
use super::super::header::TcpHeader;
use super::Pcb;

/// State-specific payload for the Listen variant.  Intentionally empty
/// today — the SYN queue lives in `tcp::listener::TcpListenState`, not
/// inside this variant.  A later commit may fold that into here if
/// it simplifies the code.
#[derive(Debug, Default)]
pub struct ListenState {
    _private: (),
}

impl ListenState {
    pub const fn new() -> Self {
        Self { _private: () }
    }

    /// Apply an incoming segment to a Listen PCB.
    ///
    /// The real implementation lands in **B.1**.  The skeleton returns
    /// an empty `Actions` so the module compiles until B.1 replaces
    /// the body with the ported `process_listen` logic.
    pub fn on_segment(_pcb: &mut Pcb, _hdr: &TcpHeader, _options: &[u8], _now_ms: u64) -> Actions {
        // Intentional no-op until B.1 replaces it with a real handler.
        Actions::new()
    }

    /// Per-state debug invariants.  Listen has no send buffer, no
    /// retransmit timer, and no sequence-space state to validate.
    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, pcb: &Pcb) {
        debug_assert!(
            pcb.buffers.send.buffered_len() == 0,
            "Listen PCB has unexpected send data"
        );
        debug_assert!(
            pcb.buffers.recv.available() == 0,
            "Listen PCB has unexpected recv data"
        );
    }
}
