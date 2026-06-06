//! Async signal handling via signalfd — signals delivered as in-band ring
//! events, never as an out-of-band `EINTR` (Phase 1).

use slopos_abi::signal::{SIGINT, sig_bit};

use crate::sys::signalfd::{block_signals, signalfd, unblock_signals};

/// Awaits delivery of signals in a mask via a signalfd. The signals are
/// blocked on construction so they queue (drainable) instead of interrupting
/// the reactor's waits.
pub struct SignalListener {
    fd: i32,
}

impl SignalListener {
    /// Listen for the signals in `mask`. Returns `None` if the signalfd could
    /// not be created — in which case only the bits this call newly blocked
    /// are unblocked again (a signal the caller had already blocked stays
    /// blocked), so the signals keep their normal delivery instead of
    /// queueing forever with no fd to drain them.
    pub fn new(mask: u64) -> Option<Self> {
        let newly_blocked = match block_signals(mask) {
            Ok(old) => mask & !old,
            Err(_) => 0,
        };
        let fd = signalfd(mask, 0);
        if fd < 0 {
            if newly_blocked != 0 {
                let _ = unblock_signals(newly_blocked);
            }
            return None;
        }
        Some(Self { fd })
    }

    /// Await the next signal; resolves to its number (1-based), or 0 on error.
    /// An `OP_READ` on the signalfd blocks via the deferred path until a
    /// masked signal is pending, then drains one `SignalfdSiginfo`.
    pub async fn recv(&self) -> u32 {
        let br = super::read(self.fd, vec![0u8; 16], 16).await;
        if br.res < 4 {
            return 0;
        }
        u32::from_le_bytes([br.buf[0], br.buf[1], br.buf[2], br.buf[3]])
    }
}

impl Drop for SignalListener {
    fn drop(&mut self) {
        let _ = slopos_slibc::ffi::close(self.fd);
    }
}

/// Resolve when SIGINT (Ctrl-C) is delivered.
pub async fn ctrl_c() {
    if let Some(listener) = SignalListener::new(sig_bit(SIGINT)) {
        let _ = listener.recv().await;
    }
}
