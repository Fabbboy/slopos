//! Timer wheel for the networking stack: entries carry a [`TimerKind`] and a
//! `key` naming the resource, not a callback.
//!
//! Deadlines are absolute milliseconds from [`crate::clock`]. There is no
//! per-tick stepping and no catch-up cap, so jumping the clock forward by an
//! hour fires an hour's worth of due timers in one pass — which is what lets
//! tests fast-forward instantly.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::KVec;
use slopos_ostd::klog_debug;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::{write_field, write_init_field};

/// Excess due entries wait for the next `process_due()` call; the bound keeps
/// per-call work from stalling interrupt context.
pub const MAX_TIMERS_PER_PROCESS: usize = 32;

/// Discriminant identifying which subsystem a timer belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    /// ARP neighbor entry has aged past `REACHABLE_TIME`; transition to `Stale`.
    ArpExpire,
    /// ARP request retry for an `Incomplete` neighbor entry.
    ArpRetransmit,
    TcpRetransmit,
    /// SYN-ACK retransmission for a half-open connection in a listener's SYN
    /// queue. Distinct from [`TimerKind::TcpRetransmit`] because its keys come
    /// from the SYN queue's own counter, not from the `ConnId` space.
    TcpSynAck,
    TcpDelayedAck,
    /// TCP TIME_WAIT 2×MSL has elapsed.
    TcpTimeWait,
    TcpKeepalive,
    /// TCP FIN_WAIT_2 timeout — releases stale half-closed connections.
    TcpFinWait2,
    /// IP reassembly timeout for a fragment group.
    ReassemblyTimeout,
    /// Connectivity re-evaluation, and the one active gateway probe.
    ConnProbe,
    DhcpRetransmit,
    /// T1 — time to renew with the server that granted the lease.
    DhcpT1,
    /// T2 — the granting server has stopped answering; ask anybody.
    DhcpT2,
    DhcpExpire,
}

/// Opaque token for timer cancellation. Never reused — the generator is a
/// 64-bit counter that will not wrap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimerToken(u64);

impl TimerToken {
    /// A sentinel token that never matches any scheduled timer.
    pub const INVALID: Self = Self(0);
}

// =============================================================================
// TimerEntry — internal per-timer state
// =============================================================================

/// A single pending timer.
///
/// Fires (unless `cancelled`) once `crate::clock::now_ms()` reaches or passes
/// `deadline_ms`.
struct TimerEntry {
    /// Absolute millisecond timestamp at which this entry should fire.
    deadline_ms: u64,
    /// Which subsystem this timer belongs to.
    kind: TimerKind,
    /// Opaque key identifying the specific resource (ARP entry ID, TCP
    /// connection ID, reassembly group ID, etc.).
    key: u32,
    /// Unique token for cancellation.
    token: TimerToken,
    /// Set to `true` by [`NetTimerWheel::cancel`]; drained entries are skipped.
    cancelled: bool,
}

// =============================================================================
// FiredTimer — returned from process_due() for dispatch
// =============================================================================

/// A timer that has expired and needs to be dispatched to its subsystem.
///
/// Returned by [`NetTimerWheel::process_due`].  The caller dispatches each
/// entry based on its [`kind`](Self::kind) field.  This design allows the
/// timer wheel to release its internal lock before dispatch, preventing
/// deadlocks when handlers schedule new timers.
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
// TimerWheelInner — state behind the SpinLock
// =============================================================================

/// Internal mutable state of the timer wheel, protected by [`SpinLock`].
#[derive(slopos_ostd::SlotFields)]
struct TimerWheelInner {
    /// Pending timer entries, keyed by absolute `deadline_ms`.
    ///
    /// The network timer population is small (a handful of ARP entries and a
    /// few timers per active TCP connection), so a flat list scanned by
    /// absolute deadline is both simpler and fast enough — and, unlike a
    /// rotating slot array, fast-forwarding the clock can never skip a
    /// deadline.
    entries: KVec<TimerEntry>,
}

// =============================================================================
// NetTimerWheel
// =============================================================================

/// Data-driven timer wheel with typed dispatch and absolute-millisecond
/// deadlines.
///
/// See [module documentation](self) for the time model and concurrency
/// details.
///
/// # Usage
///
/// ```ignore
/// // Schedule a timer 30 ms from now:
/// let token = NET_TIMER_WHEEL.schedule(30, TimerKind::ArpExpire, entry_id);
///
/// // Cancel it before it fires:
/// NET_TIMER_WHEEL.cancel(token);
///
/// // Fire everything due as of the current clock (NAPI poll / idle callback):
/// let fired = NET_TIMER_WHEEL.process_due();
/// for timer in &fired {
///     match timer.kind {
///         TimerKind::ArpExpire => { /* handle */ },
///         _ => {}
///     }
/// }
/// ```
#[derive(slopos_ostd::SlotFields)]
pub struct NetTimerWheel {
    inner: SpinLock<TimerWheelInner>,
    /// Monotonically increasing token generator (lock-free).
    ///
    /// Starts at 1; [`TimerToken(0)`](TimerToken::INVALID) is the sentinel
    /// "invalid" value.
    next_token: AtomicU64,
}

// SAFETY: All mutable state is behind SpinLock (ticket lock with IRQ disable)
// or AtomicU64.  No unsynchronized shared mutation.

/// Shared by `new` and `init_with`, which build the same logical lock.
const NET_TIMER_WHEEL_CLASS: &slopos_ostd::sync::lock_tracking::LockClassKey =
    slopos_ostd::lock_class!("NET_TIMER_WHEEL", LOCK_LEVEL_REGISTRY);

impl NetTimerWheel {
    /// Create a new, empty timer wheel.
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(
                TimerWheelInner {
                    entries: KVec::new(),
                },
                NET_TIMER_WHEEL_CLASS,
            ),
            next_token: AtomicU64::new(1),
        }
    }

    /// In-place [`Init`] recipe equivalent to [`Self::new`] for runtime
    /// heap allocation via `KBox::try_init(NetTimerWheel::init())`.
    pub fn init() -> impl Init<Self, AllocError> {
        let inner_init = init_struct_with(
            |slot: SlotPtr<TimerWheelInner>| -> Result<Initialised<TimerWheelInner>, AllocError> {
                write_field!(slot, entries, KVec::<TimerEntry>::new());
                Ok(slot.finish())
            },
        );
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    inner,
                    SpinLock::<TimerWheelInner>::init_with(NET_TIMER_WHEEL_CLASS, inner_init)
                )?;
                write_field!(slot, next_token, AtomicU64::new(1));
                Ok(slot.finish())
            },
        )
    }

    // =========================================================================
    // schedule
    // =========================================================================

    /// Schedule a timer to fire `delay_ms` milliseconds from now.
    ///
    /// Returns a [`TimerToken`] that can be passed to [`cancel`](Self::cancel)
    /// to prevent the timer from firing.
    ///
    /// # Parameters
    ///
    /// - `delay_ms`: Milliseconds from now until the timer fires.  A delay of
    ///   `0` fires on the next `process_due()` call.
    /// - `kind`: Which subsystem should handle the expiry.
    /// - `key`: Opaque resource identifier (ARP entry ID, TCP connection ID, etc.).
    pub fn schedule(&self, delay_ms: u64, kind: TimerKind, key: u32) -> TimerToken {
        let token = TimerToken(self.next_token.fetch_add(1, Ordering::Relaxed));
        let deadline_ms = crate::clock::now_ms().wrapping_add(delay_ms);
        let mut inner = self.inner.lock();
        let _ = inner.entries.push(TimerEntry {
            deadline_ms,
            kind,
            key,
            token,
            cancelled: false,
        });
        token
    }

    // =========================================================================
    // cancel
    // =========================================================================

    /// Cancel a previously scheduled timer.
    ///
    /// Marks the entry as `cancelled = true` so it is skipped (and reclaimed)
    /// when the pending list is next drained.  This is O(n) in the number of
    /// pending timers, which is small.
    ///
    /// Returns `true` if the timer was found and cancelled, `false` if it had
    /// already fired or was not found.
    pub fn cancel(&self, token: TimerToken) -> bool {
        if token == TimerToken::INVALID {
            return false;
        }
        let mut inner = self.inner.lock();
        for entry in inner.entries.iter_mut() {
            if entry.token == token && !entry.cancelled {
                entry.cancelled = true;
                return true;
            }
        }
        false
    }

    // =========================================================================
    // process_due
    // =========================================================================

    /// Fire every non-cancelled entry whose `deadline_ms` has been reached as
    /// of `crate::clock::now_ms()`, earliest deadline first.
    ///
    /// Cancelled entries are reclaimed.  At most [`MAX_TIMERS_PER_PROCESS`]
    /// entries fire per call; any further due entries remain pending and fire
    /// on the next call.  The internal lock is released before this function
    /// returns, so dispatch handlers may freely schedule or cancel timers.
    pub fn process_due(&self) -> KVec<FiredTimer> {
        let now = crate::clock::now_ms();
        let mut fired = KVec::new();
        let mut inner = self.inner.lock();

        // Reclaim cancelled entries first.
        let mut i = 0;
        while i < inner.entries.len() {
            if inner.entries[i].cancelled {
                inner.entries.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Fire due entries earliest-first, up to the per-call bound.  Selecting
        // the minimum deadline each round keeps dispatch order deterministic
        // (and fair under the cap) without sorting the whole list.
        while fired.len() < MAX_TIMERS_PER_PROCESS {
            let mut best: Option<usize> = None;
            let mut best_deadline = u64::MAX;
            for (idx, entry) in inner.entries.iter().enumerate() {
                if entry.deadline_ms <= now && entry.deadline_ms < best_deadline {
                    best = Some(idx);
                    best_deadline = entry.deadline_ms;
                }
            }
            match best {
                Some(idx) => {
                    let kind = inner.entries[idx].kind;
                    let key = inner.entries[idx].key;
                    inner.entries.swap_remove(idx);
                    let _ = fired.push(FiredTimer { kind, key });
                }
                None => break,
            }
        }

        // Lock is released here (drop of SpinLockGuard).
        fired
    }

    /// Total number of pending (non-cancelled) timers (diagnostic).
    pub fn pending_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.entries.iter().filter(|e| !e.cancelled).count()
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
// Integration: net_timer_process
// =============================================================================

/// Process pending network timers up to the current clock and dispatch them.
///
/// Reads `crate::clock::now_ms()` (via [`NetTimerWheel::process_due`]) and
/// dispatches every expired entry.  Call it from:
///
/// - The NAPI poll loop (fires during active networking)
/// - The idle wakeup callback (fires during idle periods)
pub fn net_timer_process() {
    // The classifier's tick arms itself here because the first call to this
    // function is when both the wheel and the thread draining it exist.
    super::connectivity::ensure_armed();

    let fired = NET_TIMER_WHEEL.process_due();

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
                if let Some(handle) =
                    crate::net_driver_service::net_driver().and_then(|d| (d.get_device_handle)())
                {
                    super::arp::execute_neighbor_action(handle, act);
                }
            }
        }
        TimerKind::TcpRetransmit => {
            klog_debug!("net_timer: TCP retransmit fired, key={}", timer.key);
            if let Some(idx) = super::tcp::on_retransmit(timer.key) {
                dispatch_tcp_retransmit_send(idx);
            }
        }
        TimerKind::TcpSynAck => {
            klog_debug!("net_timer: SYN-ACK retransmit fired, key={}", timer.key);
            if let Some(seg) = super::tcp::on_syn_ack_retransmit(timer.key) {
                let _ = super::socket::socket_send_tcp_segment(&seg, &[]);
            }
        }
        TimerKind::TcpDelayedAck => {
            klog_debug!("net_timer: TCP delayed ACK fired, key={}", timer.key);
            if let Some((_idx, seg)) = super::tcp::delayed_ack_check(crate::clock::now_ms()) {
                let _ = super::socket::socket_send_tcp_segment(&seg, &[]);
            }
        }
        TimerKind::TcpTimeWait => {
            klog_debug!("net_timer: TCP TIME_WAIT expired, key={}", timer.key);
            super::tcp::on_time_wait_expire(timer.key);
        }
        TimerKind::TcpKeepalive => {
            klog_debug!("net_timer: TCP keepalive fired, key={}", timer.key);
            if let Some(probe_seg) = super::tcp::on_keepalive(timer.key) {
                let _ = super::socket::socket_send_tcp_segment(&probe_seg, &[]);
            }
        }
        TimerKind::TcpFinWait2 => {
            klog_debug!("net_timer: TCP FIN_WAIT_2 timeout, key={}", timer.key);
            super::tcp::on_fin_wait2_timeout(timer.key);
        }
        TimerKind::ReassemblyTimeout => {
            klog_debug!("net_timer: reassembly timeout fired, key={}", timer.key);
            super::reassembly::REASSEMBLY_TABLE
                .lock()
                .on_timeout(timer.key);
        }
        TimerKind::ConnProbe => {
            super::connectivity::on_timer();
        }
        TimerKind::DhcpRetransmit => {
            super::dhcp::on_retransmit_timer(timer.key);
        }
        TimerKind::DhcpT1 => {
            super::dhcp::on_t1_timer(timer.key);
        }
        TimerKind::DhcpT2 => {
            super::dhcp::on_t2_timer(timer.key);
        }
        TimerKind::DhcpExpire => {
            super::dhcp::on_expire_timer(timer.key);
        }
    }
}

fn dispatch_tcp_retransmit_send(id: super::tcp::ConnId) {
    use super::socket;

    if let Some(sock_idx) = socket::socket_from_tcp_idx_pub(id) {
        let _ = socket::socket_send_queued(sock_idx);
    }
}
