//! Async signal handling via signalfd — signals delivered as in-band ring
//! events, never as an out-of-band `EINTR` (Phase 1).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use slopos_abi::signal::{NSIG, SIGINT, sig_bit};

use crate::sys::signalfd::{block_signals, signalfd, unblock_signals};

static SIGNAL_BLOCK_REFS: [AtomicUsize; NSIG] = [const { AtomicUsize::new(0) }; NSIG];
static ORIGINAL_UNBLOCK_ON_DROP: AtomicU64 = AtomicU64::new(0);

/// Awaits delivery of signals in a mask via a signalfd. The signals are
/// blocked on construction so they queue (drainable) instead of interrupting
/// the reactor's waits. Dropping the listener closes the fd and unblocks the
/// bits it newly blocked, restoring the caller's signal mask — so a listener
/// scoped to one operation (a prompt, a download) does not leave its signals
/// masked for the rest of the process, or for children forked afterwards. If
/// multiple listeners share a signal bit, the final dropped reference performs
/// that restoration.
pub struct SignalListener {
    fd: i32,
    /// Bits whose listener reference this instance owns.
    owned_mask: u64,
}

impl SignalListener {
    /// Listen for the signals in `mask`. Returns `None` if the signalfd could
    /// not be created — in which case only the bits this call newly blocked
    /// are unblocked again (a signal the caller had already blocked stays
    /// blocked), so the signals keep their normal delivery instead of
    /// queueing forever with no fd to drain them.
    pub fn new(mask: u64) -> Option<Self> {
        let mut owned_mask = 0u64;
        let mut to_block = 0u64;
        for_each_signal_bit(mask, |idx, bit| {
            owned_mask |= bit;
            if SIGNAL_BLOCK_REFS[idx].fetch_add(1, Ordering::AcqRel) == 0 {
                to_block |= bit;
            }
        });

        let old_blocked = if to_block == 0 {
            0
        } else {
            match block_signals(to_block) {
                Ok(old) => old,
                Err(_) => {
                    release_signal_refs(owned_mask);
                    return None;
                }
            }
        };
        let unblock_on_drop = to_block & !old_blocked;
        if unblock_on_drop != 0 {
            ORIGINAL_UNBLOCK_ON_DROP.fetch_or(unblock_on_drop, Ordering::AcqRel);
        }

        let fd = signalfd(mask, 0);
        if fd < 0 {
            release_signal_refs(owned_mask);
            return None;
        }
        Some(Self { fd, owned_mask })
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
        // Close before unblocking: an undrained pending signal then takes its
        // normal delivery path (handler or default) instead of being stranded
        // behind a mask with no fd left to drain it.
        let _ = slopos_slibc::ffi::close(self.fd);
        release_signal_refs(self.owned_mask);
    }
}

fn for_each_signal_bit(mut mask: u64, mut f: impl FnMut(usize, u64)) {
    mask &= if NSIG >= 64 {
        u64::MAX
    } else {
        (1u64 << NSIG) - 1
    };
    while mask != 0 {
        let bit_index = mask.trailing_zeros() as usize;
        let bit = 1u64 << bit_index;
        f(bit_index, bit);
        mask &= !bit;
    }
}

fn release_signal_refs(mask: u64) {
    let mut to_unblock = 0u64;
    for_each_signal_bit(mask, |idx, bit| {
        let counter = &SIGNAL_BLOCK_REFS[idx];
        let mut current = counter.load(Ordering::Acquire);
        while current != 0 {
            match counter.compare_exchange(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(prev) => {
                    if prev == 1 && (ORIGINAL_UNBLOCK_ON_DROP.load(Ordering::Acquire) & bit) != 0 {
                        to_unblock |= bit;
                    }
                    break;
                }
                Err(next) => current = next,
            }
        }
    });
    if to_unblock != 0 {
        let _ = unblock_signals(to_unblock);
        ORIGINAL_UNBLOCK_ON_DROP.fetch_and(!to_unblock, Ordering::AcqRel);
    }
}

/// Resolve when SIGINT (Ctrl-C) is delivered.
pub async fn ctrl_c() {
    if let Some(listener) = SignalListener::new(sig_bit(SIGINT)) {
        let _ = listener.recv().await;
    }
}
