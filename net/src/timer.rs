//! Timer wheel for the networking stack: entries carry a [`TimerKind`] and a
//! `key` naming the resource, not a callback.
//!
//! Deadlines are absolute milliseconds from [`crate::clock`]. There is no
//! per-tick stepping and no catch-up cap, so jumping the clock forward by an
//! hour fires an hour's worth of due timers in one pass — which is what lets
//! tests fast-forward instantly.
//!
//! Due entries are collected under the wheel lock and dispatched outside it, so
//! a handler may re-enter `schedule` without deadlocking.

#[cfg(feature = "test-hooks")]
use core::sync::atomic::AtomicBool;
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

/// A single pending timer; fires unless `cancelled` once
/// `crate::clock::now_ms()` reaches `deadline_ms`.
struct TimerEntry {
    deadline_ms: u64,
    kind: TimerKind,
    key: u32,
    token: TimerToken,
    cancelled: bool,
}

/// A timer that has expired and needs dispatching to its subsystem.
#[derive(Clone, Copy, Debug)]
pub struct FiredTimer {
    pub kind: TimerKind,
    /// The resource key (ARP entry ID, TCP connection ID, etc.). Each
    /// subsystem must validate it still names a live resource: the original
    /// entry may have been closed or freed before the timer fires.
    pub key: u32,
}

#[derive(slopos_ostd::SlotFields)]
struct TimerWheelInner {
    /// A flat list scanned by absolute deadline: the timer population is small,
    /// and unlike a rotating slot array, fast-forwarding the clock can never
    /// skip a deadline.
    entries: KVec<TimerEntry>,
}

/// Data-driven timer wheel with typed dispatch and absolute-millisecond
/// deadlines.
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
    /// Starts at 1 so no live token equals [`TimerToken::INVALID`].
    next_token: AtomicU64,
}

/// Shared by `new` and `init_with`, which build the same logical lock.
const NET_TIMER_WHEEL_CLASS: &slopos_ostd::sync::lock_tracking::LockClassKey =
    slopos_ostd::lock_class!("NET_TIMER_WHEEL", LOCK_LEVEL_REGISTRY);

/// The scope's wheel is a distinct class, not a second instance of the class
/// above: same-class nesting is a lockdep finding, and a test-only ordering
/// must not be learned as an ordering of the live stack's wheel.
#[cfg(feature = "test-hooks")]
const TEST_TIMER_WHEEL_CLASS: &slopos_ostd::sync::lock_tracking::LockClassKey =
    slopos_ostd::lock_class!("NET_TEST_TIMER_WHEEL", LOCK_LEVEL_REGISTRY);

impl NetTimerWheel {
    pub const fn new() -> Self {
        Self::with_class(NET_TIMER_WHEEL_CLASS)
    }

    const fn with_class(class: &'static slopos_ostd::sync::lock_tracking::LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                TimerWheelInner {
                    entries: KVec::new(),
                },
                class,
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

    /// Schedule a timer to fire `delay_ms` milliseconds from now; a delay of
    /// `0` fires on the next `process_due()` call. The returned [`TimerToken`]
    /// cancels it via [`cancel`](Self::cancel).
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

    /// Cancel a previously scheduled timer: the entry is marked, then skipped
    /// and reclaimed when the pending list is next drained. `false` means it
    /// had already fired or was not found.
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

    /// Fire every non-cancelled entry whose `deadline_ms` has been reached as
    /// of `crate::clock::now_ms()`, earliest deadline first. At most
    /// [`MAX_TIMERS_PER_PROCESS`] fire per call; the rest wait for the next.
    /// The internal lock is released before returning, so dispatch handlers
    /// may freely schedule or cancel timers.
    pub fn process_due(&self) -> KVec<FiredTimer> {
        let now = crate::clock::now_ms();
        let mut fired = KVec::new();
        let mut inner = self.inner.lock();

        let mut i = 0;
        while i < inner.entries.len() {
            if inner.entries[i].cancelled {
                inner.entries.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Selecting the minimum deadline each round keeps dispatch order
        // deterministic without sorting the whole list.
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

        fired
    }

    /// [`process_due`](Self::process_due) restricted to one [`TimerKind`].
    ///
    /// Entries of every other kind stay pending and are not charged against
    /// [`MAX_TIMERS_PER_PROCESS`], so a test that fast-forwards the clock by
    /// hours cannot consume — and discard — a timer it does not own.
    #[cfg(feature = "test-hooks")]
    pub fn process_due_matching(&self, kind: TimerKind) -> KVec<FiredTimer> {
        let now = crate::clock::now_ms();
        let mut fired = KVec::new();
        let mut inner = self.inner.lock();

        let mut i = 0;
        while i < inner.entries.len() {
            if inner.entries[i].cancelled && inner.entries[i].kind == kind {
                inner.entries.swap_remove(i);
            } else {
                i += 1;
            }
        }

        while fired.len() < MAX_TIMERS_PER_PROCESS {
            let mut best: Option<usize> = None;
            let mut best_deadline = u64::MAX;
            for (idx, entry) in inner.entries.iter().enumerate() {
                if entry.kind == kind
                    && !entry.cancelled
                    && entry.deadline_ms <= now
                    && entry.deadline_ms < best_deadline
                {
                    best = Some(idx);
                    best_deadline = entry.deadline_ms;
                }
            }
            match best {
                Some(idx) => {
                    let key = inner.entries[idx].key;
                    inner.entries.swap_remove(idx);
                    let _ = fired.push(FiredTimer { kind, key });
                }
                None => break,
            }
        }

        fired
    }

    /// Drop every pending entry.
    ///
    /// A `NetTestScope` calls this on the way out so no token minted in its
    /// wheel survives to be cancelled against the live stack's.
    #[cfg(feature = "test-hooks")]
    pub fn clear(&self) {
        self.inner.lock().entries.clear();
    }

    /// Total number of pending (non-cancelled) timers (diagnostic).
    pub fn pending_count(&self) -> usize {
        let inner = self.inner.lock();
        inner.entries.iter().filter(|e| !e.cancelled).count()
    }
}

/// The wheel the live stack schedules through and `net_timer_process` drains.
static LIVE_TIMER_WHEEL: NetTimerWheel = NetTimerWheel::new();

/// Second wheel a `NetTestScope` diverts every `schedule` to for its duration.
///
/// Deadlines are absolute, so a schedule taken while a mock clock is installed
/// records a mock-time deadline; landing those in [`LIVE_TIMER_WHEEL`] would
/// leave the live stack holding timers due hours of real uptime later.
#[cfg(feature = "test-hooks")]
pub static TEST_TIMER_WHEEL: NetTimerWheel = NetTimerWheel::with_class(TEST_TIMER_WHEEL_CLASS);

#[cfg(feature = "test-hooks")]
static TEST_WHEEL_SELECTED: AtomicBool = AtomicBool::new(false);

/// The wheel a `schedule` or `cancel` issued right now belongs in.
#[cfg(feature = "test-hooks")]
#[inline]
pub fn wheel() -> &'static NetTimerWheel {
    if TEST_WHEEL_SELECTED.load(Ordering::Acquire) {
        &TEST_TIMER_WHEEL
    } else {
        &LIVE_TIMER_WHEEL
    }
}

/// The wheel a `schedule` or `cancel` issued right now belongs in.
#[cfg(not(feature = "test-hooks"))]
#[inline]
pub fn wheel() -> &'static NetTimerWheel {
    &LIVE_TIMER_WHEEL
}

/// Divert scheduling to [`TEST_TIMER_WHEEL`], returning the previous selection.
///
/// A token is only cancellable in the wheel that minted it, so a caller must
/// settle the live stack's outstanding timers before selecting and empty the
/// test wheel before deselecting.
#[cfg(feature = "test-hooks")]
pub fn select_test_wheel(on: bool) -> bool {
    TEST_WHEEL_SELECTED.swap(on, Ordering::AcqRel)
}

#[cfg(feature = "test-hooks")]
pub fn test_wheel_selected() -> bool {
    TEST_WHEEL_SELECTED.load(Ordering::Acquire)
}

/// A zero-sized stand-in for whichever wheel [`wheel`] selects, so the stack's
/// hundred-odd `NET_TIMER_WHEEL.schedule(…)` sites follow the selection
/// without naming it. In a build without `test-hooks` it derefs to
/// [`LIVE_TIMER_WHEEL`] unconditionally.
pub struct SelectedWheel;

impl core::ops::Deref for SelectedWheel {
    type Target = NetTimerWheel;

    #[inline]
    fn deref(&self) -> &NetTimerWheel {
        wheel()
    }
}

/// The one wheel every networking subsystem schedules through.
pub static NET_TIMER_WHEEL: SelectedWheel = SelectedWheel;

/// Process pending network timers up to the current clock and dispatch them.
/// Called from both the NAPI poll loop and the idle wakeup callback, so timers
/// fire during active networking and during idle periods alike.
pub fn net_timer_process() {
    // A net test fixture holds the data plane still by making this a no-op:
    // draining here would fire the fixture's own timers from the kthread, and
    // re-arming handlers would write live-stack timers into the fixture's wheel
    // for it to discard.
    if crate::ingress::dataplane_quiesced() {
        return;
    }

    // The classifier's tick arms itself here because the first call to this
    // function is when both the wheel and the thread draining it exist.
    super::connectivity::ensure_armed();

    let fired = LIVE_TIMER_WHEEL.process_due();

    for timer in &fired {
        dispatch_fired_timer(timer);
    }
}

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
                // TODO(tech-debt): one global device handle — multi-NIC needs a
                // per-device lookup.
                if let Some(handle) =
                    crate::net_driver_service::net_driver().and_then(|d| (d.get_device_handle)())
                {
                    super::arp::execute_neighbor_action(handle, act);
                }
            }
        }
        TimerKind::TcpRetransmit => {
            klog_debug!("net_timer: TCP retransmit fired, key={}", timer.key);
            match super::tcp::on_retransmit(timer.key) {
                super::tcp::RetransmitAction::Data(idx) => dispatch_tcp_retransmit_send(idx),
                super::tcp::RetransmitAction::Segment(seg) => {
                    let _ = super::socket::socket_send_tcp_segment(&seg, &[]);
                }
                super::tcp::RetransmitAction::Nothing => {}
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
