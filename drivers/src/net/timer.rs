//! Data-driven timer wheel for the SlopOS networking stack.
//!
//! All network timers (ARP aging, TCP retransmit, TCP delayed ACK, TCP keepalive,
//! TCP TIME_WAIT, reassembly timeout) use this timer wheel with typed dispatch.
//! No bare `fn()` callbacks — timers carry a [`TimerKind`] discriminant and a
//! `key` that identifies the specific resource (ARP entry ID, TCP connection ID,
//! reassembly group ID, etc.).
//!
//! # Architecture (CAD-4)
//!
//! The wheel has 256 slots.  Each slot is a `Vec<TimerEntry>`.  On each tick,
//! the wheel advances `current_tick` and drains all entries in the current slot
//! whose `deadline_tick <= current_tick`.  Entries with `cancelled == true` are
//! skipped.  Long delays (>256 ticks) use multiple rotations tracked by the
//! absolute `deadline_tick`.
//!
//! Per-tick work is bounded: if more than [`MAX_TIMERS_PER_TICK`] entries expire
//! in one slot, the remainder are deferred to the next tick to prevent
//! interrupt-context stalls.
//!
//! # Concurrency
//!
//! The wheel's internal state is protected by an [`IrqMutex`].  Expired entries
//! are collected under the lock, then dispatched **outside** the lock.  This
//! prevents deadlock when dispatch handlers schedule new timers.
//!
//! The token generator ([`AtomicU64`]) is lock-free — scheduling and cancelling
//! timers from interrupt context is safe.
//!
//! # Integration (2A.5)
//!
//! [`net_timer_process`] is the integration point.  It reads the kernel tick
//! counter, advances the wheel to catch up with elapsed ticks, and returns
//! expired entries for the caller to dispatch.  Call it from the NAPI poll
//! loop and the idle wakeup callback to ensure timers fire both during active
//! networking and during idle periods.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_lib::IrqMutex;
use slopos_lib::klog_debug;

/// Number of slots in the timer wheel.
const NUM_SLOTS: usize = 256;

/// Maximum number of timer entries that fire in a single `tick()` call.
///
/// If more than this many entries expire in one slot, the remainder are
/// deferred to the next tick.  This bounds per-tick work to prevent
/// interrupt-context stalls.
pub const MAX_TIMERS_PER_TICK: usize = 32;

// =============================================================================
// TimerKind — discriminant for typed dispatch
// =============================================================================

/// Discriminant identifying which subsystem a timer belongs to.
///
/// The `match` on `TimerKind` in the dispatch loop is exhaustive — adding a
/// new variant forces the caller to handle it.  This is the data-driven
/// alternative to bare `fn()` callbacks, which cannot carry state in Rust
/// without heap allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    /// ARP neighbor entry has aged past `REACHABLE_TIME`; transition to `Stale`.
    ArpExpire,
    /// ARP request retry for an `Incomplete` neighbor entry.
    ArpRetransmit,
    /// TCP retransmission timer fired.
    TcpRetransmit,
    /// TCP delayed ACK timer fired.
    TcpDelayedAck,
    /// TCP TIME_WAIT 2×MSL has elapsed.
    TcpTimeWait,
    /// TCP keepalive probe.
    TcpKeepalive,
    /// IP reassembly timeout for a fragment group.
    ReassemblyTimeout,
}

// =============================================================================
// TimerToken — opaque cancellation handle
// =============================================================================

/// Opaque, monotonically increasing token for timer cancellation.
///
/// Each scheduled timer receives a unique `TimerToken`.  Passing it to
/// [`NetTimerWheel::cancel`] marks the corresponding entry as cancelled
/// so it will be skipped when its slot is drained.
///
/// Tokens are never reused — the generator is a 64-bit counter that will
/// not wrap in any realistic scenario.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimerToken(u64);

impl TimerToken {
    /// A sentinel token that never matches any scheduled timer.
    pub const INVALID: Self = Self(0);
}

// =============================================================================
// TimerEntry — internal per-timer state
// =============================================================================

/// A single entry in the timer wheel.
///
/// Stored in the slot corresponding to `deadline_tick % NUM_SLOTS`.  When the
/// wheel's `current_tick` reaches or passes `deadline_tick`, the entry fires
/// (unless `cancelled` is `true`).
struct TimerEntry {
    /// Absolute tick at which this entry should fire.
    deadline_tick: u64,
    /// Which subsystem this timer belongs to.
    kind: TimerKind,
    /// Opaque key identifying the specific resource (ARP entry ID, TCP
    /// connection ID, reassembly group ID, etc.).
    key: u32,
    /// Unique token for cancellation.
    token: TimerToken,
    /// Set to `true` by [`NetTimerWheel::cancel`]; `tick()` skips cancelled entries.
    cancelled: bool,
}

// =============================================================================
// FiredTimer — returned from tick() for dispatch
// =============================================================================

/// A timer that has expired and needs to be dispatched to its subsystem.
///
/// Returned by [`NetTimerWheel::tick`].  The caller dispatches each entry
/// based on its [`kind`](Self::kind) field.  This design allows the timer
/// wheel to release its internal lock before dispatch, preventing deadlocks
/// when handlers schedule new timers.
#[derive(Clone, Copy, Debug)]
pub struct FiredTimer {
    /// Which subsystem should handle this timer.
    pub kind: TimerKind,
    /// The resource key (ARP entry ID, TCP connection ID, etc.).
    ///
    /// Each subsystem must validate that the key still refers to a live
    /// resource — the original entry may have been closed/freed before the
    /// timer fires (the timer-cancellation race).
    pub key: u32,
}

// =============================================================================
// TimerWheelInner — state behind the IrqMutex
// =============================================================================

/// Internal mutable state of the timer wheel, protected by [`IrqMutex`].
struct TimerWheelInner {
    /// 256 slots, each containing pending timer entries.
    slots: [Vec<TimerEntry>; NUM_SLOTS],
    /// Current position in the wheel (monotonically increasing).
    ///
    /// This is the last tick that was processed.  `tick()` advances this by
    /// one and processes the new slot.
    current_tick: u64,
}

// =============================================================================
// NetTimerWheel
// =============================================================================

/// Data-driven timer wheel with 256 slots and typed dispatch.
///
/// See [module documentation](self) for architecture and concurrency details.
///
/// # Usage
///
/// ```ignore
/// // Schedule a timer 30 ticks from now:
/// let token = NET_TIMER_WHEEL.schedule(30, TimerKind::ArpExpire, entry_id);
///
/// // Cancel it before it fires:
/// NET_TIMER_WHEEL.cancel(token);
///
/// // Advance the wheel (called from NAPI poll / idle callback):
/// let fired = NET_TIMER_WHEEL.tick();
/// for timer in &fired {
///     match timer.kind {
///         TimerKind::ArpExpire => { /* handle */ },
///         _ => {}
///     }
/// }
/// ```
pub struct NetTimerWheel {
    inner: IrqMutex<TimerWheelInner>,
    /// Monotonically increasing token generator (lock-free).
    ///
    /// Starts at 1; [`TimerToken(0)`](TimerToken::INVALID) is the sentinel
    /// "invalid" value.
    next_token: AtomicU64,
}

// SAFETY: All mutable state is behind IrqMutex (ticket lock with IRQ disable)
// or AtomicU64.  No unsynchronized shared mutation.
unsafe impl Send for NetTimerWheel {}
unsafe impl Sync for NetTimerWheel {}

impl NetTimerWheel {
    /// Create a new, empty timer wheel with `current_tick = 0`.
    pub const fn new() -> Self {
        Self {
            inner: IrqMutex::new(TimerWheelInner {
                slots: [const { Vec::new() }; NUM_SLOTS],
                current_tick: 0,
            }),
            next_token: AtomicU64::new(1),
        }
    }

    // =========================================================================
    // 2A.2 — schedule
    // =========================================================================

    /// Schedule a timer to fire after `delay_ticks` ticks.
    ///
    /// Returns a [`TimerToken`] that can be passed to [`cancel`](Self::cancel)
    /// to prevent the timer from firing.
    ///
    /// # Parameters
    ///
    /// - `delay_ticks`: Number of ticks from now until the timer fires.
    ///   A delay of 0 fires on the next `tick()` call.
    /// - `kind`: Which subsystem should handle the expiry.
    /// - `key`: Opaque resource identifier (ARP entry ID, TCP connection ID, etc.).
    pub fn schedule(&self, delay_ticks: u64, kind: TimerKind, key: u32) -> TimerToken {
        let token = TimerToken(self.next_token.fetch_add(1, Ordering::Relaxed));
        let mut inner = self.inner.lock();
        let deadline = inner.current_tick.wrapping_add(delay_ticks);
        let slot_idx = (deadline % NUM_SLOTS as u64) as usize;
        inner.slots[slot_idx].push(TimerEntry {
            deadline_tick: deadline,
            kind,
            key,
            token,
            cancelled: false,
        });
        token
    }

    // =========================================================================
    // 2A.3 — cancel
    // =========================================================================

    /// Cancel a previously scheduled timer.
    ///
    /// Marks the entry as `cancelled = true` so it will be skipped when its
    /// slot is drained.  This is O(n) in the slot size, bounded by the number
    /// of timers in a single slot.
    ///
    /// Returns `true` if the timer was found and cancelled, `false` if it had
    /// already fired or was not found.
    ///
    /// # Why O(n) is acceptable
    ///
    /// Each slot typically has very few entries.  For a hobby OS, scanning a
    /// small Vec is faster than maintaining a separate cancellation hash set
    /// due to lower constant factors and no heap allocation overhead.
    pub fn cancel(&self, token: TimerToken) -> bool {
        if token == TimerToken::INVALID {
            return false;
        }
        let mut inner = self.inner.lock();
        for slot in inner.slots.iter_mut() {
            for entry in slot.iter_mut() {
                if entry.token == token && !entry.cancelled {
                    entry.cancelled = true;
                    return true;
                }
            }
        }
        false
    }

    // =========================================================================
    // 2A.4 — tick
    // =========================================================================

    /// Advance the wheel by one tick and collect expired entries.
    ///
    /// Returns a `Vec<FiredTimer>` containing all entries whose `deadline_tick`
    /// has been reached and that were not cancelled.  The caller dispatches
    /// each entry based on its `kind` field.
    ///
    /// # Per-tick bounds
    ///
    /// At most [`MAX_TIMERS_PER_TICK`] entries fire per call.  If more entries
    /// expire in the current slot, they remain in the slot and will fire on the
    /// next `tick()` call (their `deadline_tick <= current_tick` will still hold).
    ///
    /// # Lock discipline
    ///
    /// The internal lock is held only while collecting expired entries.  It is
    /// released before this function returns, so dispatch handlers are free to
    /// call [`schedule`](Self::schedule) or [`cancel`](Self::cancel) without
    /// deadlocking.
    pub fn tick(&self) -> Vec<FiredTimer> {
        let mut inner = self.inner.lock();
        inner.current_tick = inner.current_tick.wrapping_add(1);
        let current = inner.current_tick;
        let slot_idx = (current % NUM_SLOTS as u64) as usize;

        let slot = &mut inner.slots[slot_idx];
        let mut fired = Vec::new();
        let mut i = 0;
        let mut fired_count = 0usize;

        while i < slot.len() {
            if fired_count >= MAX_TIMERS_PER_TICK {
                // Defer the rest to the next tick.
                break;
            }

            let entry = &slot[i];

            // Skip cancelled entries — remove them to reclaim memory.
            if entry.cancelled {
                slot.swap_remove(i);
                continue; // Don't increment i — swap_remove moved the last element here.
            }

            // Check if this entry's deadline has been reached.
            // Entries with deadline_tick > current_tick are future timers that
            // happen to land in the same slot (via modular wraparound).
            if entry.deadline_tick <= current {
                let kind = entry.kind;
                let key = entry.key;
                slot.swap_remove(i);
                fired.push(FiredTimer { kind, key });
                fired_count += 1;
                // Don't increment i — swap_remove moved the last element here.
            } else {
                i += 1;
            }
        }

        // Lock is released here (drop of IrqMutexGuard).
        fired
    }

    /// Advance the wheel to `target_tick`, processing all intermediate ticks.
    ///
    /// This is the catch-up variant of [`tick`](Self::tick).  If the wheel has
    /// fallen behind (e.g., because `net_timer_process` wasn't called for
    /// several kernel ticks), this advances one tick at a time up to
    /// `target_tick`, collecting all expired entries.
    ///
    /// To prevent unbounded work on first call (when `current_tick` is 0 and
    /// `target_tick` may be very large), the catch-up is capped at `NUM_SLOTS`
    /// ticks per call.
    pub fn advance_to(&self, target_tick: u64) -> Vec<FiredTimer> {
        let current = {
            let inner = self.inner.lock();
            inner.current_tick
        };

        if target_tick <= current {
            return Vec::new();
        }

        // Cap catch-up to NUM_SLOTS ticks to prevent unbounded work.
        let ticks_behind = target_tick.saturating_sub(current);
        let ticks_to_process = ticks_behind.min(NUM_SLOTS as u64);

        let mut all_fired = Vec::new();
        for _ in 0..ticks_to_process {
            let mut fired = self.tick();
            all_fired.append(&mut fired);
        }

        // If we were more than NUM_SLOTS behind, snap the wheel forward.
        if ticks_behind > NUM_SLOTS as u64 {
            let mut inner = self.inner.lock();
            inner.current_tick = target_tick;
        }

        all_fired
    }

    /// Read the current tick of the wheel (diagnostic).
    pub fn current_tick(&self) -> u64 {
        self.inner.lock().current_tick
    }

    /// Total number of pending (non-cancelled) timers across all slots (diagnostic).
    pub fn pending_count(&self) -> usize {
        let inner = self.inner.lock();
        inner
            .slots
            .iter()
            .map(|s| s.iter().filter(|e| !e.cancelled).count())
            .sum()
    }
}

// =============================================================================
// Global timer wheel instance
// =============================================================================

/// The global network timer wheel.
///
/// All networking subsystems (ARP neighbor cache, TCP engine, IP reassembly)
/// schedule their timers through this single wheel.
pub static NET_TIMER_WHEEL: NetTimerWheel = NetTimerWheel::new();

// =============================================================================
// 2A.5 — Integration: net_timer_process
// =============================================================================

/// Process pending network timers up to the current kernel tick.
///
/// Reads the kernel tick counter, advances the timer wheel to catch up, and
/// dispatches expired entries.  Call this from:
///
/// - The NAPI poll loop (fires during active networking)
/// - The idle wakeup callback (fires during idle periods)
///
/// Dispatch is currently stubbed — TODO: add neighbor cache dispatch,
/// TODO: add TCP engine dispatch, TODO: add reassembly dispatch.
pub fn net_timer_process() {
    use slopos_lib::kernel_services::platform;

    let kernel_ticks = platform::timer_ticks();
    let fired = NET_TIMER_WHEEL.advance_to(kernel_ticks);

    for timer in &fired {
        dispatch_fired_timer(timer);
    }
}

/// Dispatch a single fired timer to the appropriate subsystem.
///
/// ARP timers call into the [`NeighborCache`] and execute any
/// returned I/O actions via the single VirtIO-net device handle.  TCP and
/// reassembly dispatch remain stubbed for the corresponding.
fn dispatch_fired_timer(timer: &FiredTimer) {
    match timer.kind {
        TimerKind::ArpExpire => {
            klog_debug!("net_timer: ARP expire fired, key={}", timer.key);
            super::neighbor::NEIGHBOR_CACHE.on_expire(timer.key);
        }
        TimerKind::ArpRetransmit => {
            klog_debug!("net_timer: ARP retransmit fired, key={}", timer.key);
            let (action, _dropped) = super::neighbor::NEIGHBOR_CACHE.on_retransmit(timer.key);
            if let Some(act) = action {
                // Execute the returned action (send ARP request).
                // multi-NIC support will need per-device handle lookup.
                if let Some(handle) = crate::virtio_net::get_device_handle() {
                    super::arp::execute_neighbor_action(handle, act);
                }
            }
        }
        TimerKind::TcpRetransmit => {
            klog_debug!("net_timer: TCP retransmit fired, key={}", timer.key);
            let handled_by_listen = dispatch_tcp_syn_ack_retransmit(timer.key);
            if !handled_by_listen && let Some(idx) = super::tcp::tcp_on_retransmit(timer.key) {
                dispatch_tcp_retransmit_send(idx);
            }
        }
        TimerKind::TcpDelayedAck => {
            klog_debug!("net_timer: TCP delayed ACK fired, key={}", timer.key);
            if let Some((_idx, seg)) =
                super::tcp::tcp_delayed_ack_check(slopos_lib::clock::uptime_ms())
            {
                let _ = super::socket::socket_send_tcp_segment(&seg, &[]);
            }
        }
        TimerKind::TcpTimeWait => {
            klog_debug!("net_timer: TCP TIME_WAIT expired, key={}", timer.key);
            super::tcp::tcp_on_time_wait_expire(timer.key);
        }
        TimerKind::TcpKeepalive => {
            klog_debug!("net_timer: TCP keepalive fired, key={}", timer.key);
            // tcp_engine.on_keepalive(timer.key)
        }
        TimerKind::ReassemblyTimeout => {
            klog_debug!("net_timer: reassembly timeout fired, key={}", timer.key);
            // reassembly.on_timeout(timer.key)
        }
    }
}

fn dispatch_tcp_syn_ack_retransmit(key: u32) -> bool {
    use super::socket;

    if let Some(seg) = socket::socket_dispatch_syn_ack_retransmit(key) {
        let _ = socket::socket_send_tcp_segment(&seg, &[]);
        return true;
    }

    false
}

fn dispatch_tcp_retransmit_send(idx: usize) {
    use super::socket;

    if let Some(sock_idx) = socket::socket_from_tcp_idx_pub(idx) {
        let _ = socket::socket_send_queued(sock_idx);
    }
}
