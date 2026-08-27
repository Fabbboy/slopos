//! Fused task lifecycle state.
//!
//! `TaskState(AtomicU64)` packs the [`TaskStatus`], [`BlockReason`], and a
//! 32-bit ABA epoch into a single 64-bit atomic, so no observer can see a
//! stale reason alongside a fresh status.
//!
//! # Layout (little-endian word, low bit first)
//!
//! ```text
//! bits  0..4    TaskStatus    (4 bits, 5 variants)
//! bits  4..12   BlockReason   (8 bits, 8 variants)
//! bits 12..14   poll          (2 bits: armed, pending — see below)
//! bits 14..16   poll_era      (2 bits, wrapping token generation)
//! bits 16..32   cpu_hint      (16 bits, currently zero — reserved for
//!                              future CPU-affinity-aware wakeup paths)
//! bits 32..64   epoch         (32 bits, ABA defence; bumped on every
//!                              wake/recycle so a stale comparator from
//!                              before the wake fails its CAS)
//! ```
//!
//! The poll bits are [`PollWaiter`](crate::sync::PollWaiter)'s; they live here
//! so arming a token and parking are one atomic each, rather than a flag plus a
//! state write that a wake could land between.
//!
//! `poll_era` is what makes the token an identity rather than a boolean.
//! `WaitQueue::wake_one` pops a node under the queue lock and delivers the wake
//! *after* releasing it; in that window the waiter can finish its poll, disarm,
//! and a fresh poll on the same task can arm. Against a single armed bit that
//! late wake marks the new poll's token and its first block returns without
//! parking, having consumed a wake it was never owed. A wake therefore carries
//! the era its registration was made under, and
//! [`poll_set_pending`](TaskState::poll_set_pending) refuses a mismatch.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::task::{BlockReason, TaskStatus};

const STATUS_BITS: u32 = 4;
const REASON_BITS: u32 = 8;
const POLL_BITS: u32 = 2;
const POLL_ERA_BITS: u32 = 2;
const CPU_HINT_BITS: u32 = 16;
const EPOCH_BITS: u32 = 32;

const STATUS_SHIFT: u32 = 0;
const REASON_SHIFT: u32 = STATUS_SHIFT + STATUS_BITS;
const POLL_SHIFT: u32 = REASON_SHIFT + REASON_BITS;
const POLL_ERA_SHIFT: u32 = POLL_SHIFT + POLL_BITS;
const CPU_HINT_SHIFT: u32 = POLL_ERA_SHIFT + POLL_ERA_BITS;
const EPOCH_SHIFT: u32 = CPU_HINT_SHIFT + CPU_HINT_BITS;

const STATUS_MASK: u64 = (1u64 << STATUS_BITS) - 1;
const REASON_MASK: u64 = (1u64 << REASON_BITS) - 1;
const _CPU_HINT_MASK: u64 = (1u64 << CPU_HINT_BITS) - 1;
const EPOCH_MASK: u64 = (1u64 << EPOCH_BITS) - 1;

/// Bit 12: a [`PollWaiter`](crate::sync::PollWaiter) is live for this task.
const POLL_ARMED_BIT: u64 = 1u64 << POLL_SHIFT;
/// Bit 13: a wake was aimed at this task while it was not `Blocked`.
const POLL_PENDING_BIT: u64 = 1u64 << (POLL_SHIFT + 1);
/// Bits 14..16: the live token's generation. See the [module docs](self).
const POLL_ERA_MASK: u64 = (1u64 << POLL_ERA_BITS) - 1;

/// Number of distinct token generations before the counter wraps.
pub const POLL_ERA_MODULUS: u8 = 1u8 << POLL_ERA_BITS;

const _: () = assert!(
    STATUS_BITS + REASON_BITS + POLL_BITS + POLL_ERA_BITS + CPU_HINT_BITS + EPOCH_BITS == 64
);

/// Snapshot view of a [`TaskState`] word — the unpacked form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskStateView {
    pub status: TaskStatus,
    pub reason: BlockReason,
    pub epoch: u32,
    /// A `PollWaiter` is live for this task. See the [module docs](self).
    pub poll_armed: bool,
    /// A wake landed while this task was not `Blocked`. See the
    /// [module docs](self).
    pub poll_pending: bool,
    /// Generation of the live token, wrapping at [`POLL_ERA_MODULUS`]. See the
    /// [module docs](self).
    pub poll_era: u8,
}

impl TaskStateView {
    /// Repack into the raw 64-bit word. Round-trips with [`unpack`]: every bit
    /// the layout defines is carried by a field, so no CAS built from an
    /// unpacked view can silently drop one.
    #[inline]
    const fn pack(self) -> u64 {
        let s = (self.status.as_u8() as u64) & STATUS_MASK;
        let r = (self.reason.as_u8() as u64) & REASON_MASK;
        let e = (self.epoch as u64) & EPOCH_MASK;
        let a = if self.poll_armed { POLL_ARMED_BIT } else { 0 };
        let p = if self.poll_pending {
            POLL_PENDING_BIT
        } else {
            0
        };
        let g = ((self.poll_era as u64) & POLL_ERA_MASK) << POLL_ERA_SHIFT;
        (s << STATUS_SHIFT) | (r << REASON_SHIFT) | (e << EPOCH_SHIFT) | a | p | g
    }

    #[inline]
    const fn unpack(word: u64) -> Self {
        let status = TaskStatus::from_u8(((word >> STATUS_SHIFT) & STATUS_MASK) as u8);
        let reason = BlockReason::from_u8(((word >> REASON_SHIFT) & REASON_MASK) as u8);
        let epoch = ((word >> EPOCH_SHIFT) & EPOCH_MASK) as u32;
        Self {
            status,
            reason,
            epoch,
            poll_armed: (word & POLL_ARMED_BIT) != 0,
            poll_pending: (word & POLL_PENDING_BIT) != 0,
            poll_era: ((word >> POLL_ERA_SHIFT) & POLL_ERA_MASK) as u8,
        }
    }
}

/// Fused lifecycle state. See module docs for the bit layout.
#[repr(transparent)]
pub struct TaskState(AtomicU64);

impl TaskState {
    /// Initial state for a freshly-allocated slot: `Invalid`, no reason,
    /// epoch 0.
    #[inline]
    pub const fn invalid() -> Self {
        let view = TaskStateView {
            status: TaskStatus::Invalid,
            reason: BlockReason::None,
            epoch: 0,
            poll_armed: false,
            poll_pending: false,
            poll_era: 0,
        };
        Self(AtomicU64::new(view.pack()))
    }

    /// Acquire-load the full state and unpack it.
    #[inline]
    pub fn snapshot(&self) -> TaskStateView {
        TaskStateView::unpack(self.0.load(Ordering::Acquire))
    }

    #[inline]
    pub fn status(&self) -> TaskStatus {
        self.snapshot().status
    }

    #[inline]
    pub fn reason(&self) -> BlockReason {
        self.snapshot().reason
    }

    /// Try to transition from `expected` to `target` while stamping the block
    /// reason. Only the status gates the CAS; the epoch is bumped on success.
    ///
    /// `Err` carries the freshly-loaded state — a looping caller must use it as
    /// its next comparator's starting point.
    #[inline]
    pub fn try_transition(
        &self,
        expected: TaskStatus,
        target: TaskStatus,
        reason: BlockReason,
    ) -> Result<TaskStateView, TaskStateView> {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if current.status != expected {
                return Err(current);
            }
            let next = TaskStateView {
                status: target,
                reason,
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(_) => continue,
            }
        }
    }

    /// Force the state to (status, reason) and bump the epoch, ignoring the
    /// current status and reason.
    ///
    /// For single-owner contexts (slot init, slot reset, kernel-only state
    /// forcings). The CAS-loop tolerates concurrent epoch bumps but is not
    /// designed to interleave correctly with another `force_set`.
    #[inline]
    pub fn force_set(&self, status: TaskStatus, reason: BlockReason) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                status,
                reason,
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Publish `status`, refusing any transition out of a terminal state back
    /// into a live one. Returns `false`, having written nothing, when it
    /// refuses.
    ///
    /// The hole it closes: a task stamped `Zombie` by a peer and then
    /// force-restored to `Running` never reaches deferred cleanup, so its fd
    /// table, its process VM and its reap never run.
    ///
    /// Narrower than [`TaskStatus::can_transition_to`], which has no self-edges
    /// and would reject the `Running -> Running` re-publish in dispatch, the
    /// `Ready -> Ready` publication rollback, and slot init. The status is
    /// re-read inside the CAS loop, so the check has no time-of-check gap.
    #[inline]
    #[must_use = "a refused publish means the task is already dead; take the terminal path"]
    pub fn set_status_checked(&self, status: TaskStatus, reason: BlockReason) -> bool {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if is_terminal(current.status) && is_live(status) {
                return false;
            }
            let next = TaskStateView {
                status,
                reason,
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Bump only the epoch field, preserving status and reason — for a slot
    /// recycled whose terminal state is also its next initial state (e.g.
    /// Terminated → Terminated on observed-but-not-yet-reaped transitions).
    #[inline]
    pub fn bump_epoch(&self) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Try to transition from `expected` to `target`, preserving the block
    /// reason verbatim even when it is stale relative to the new status.
    #[inline]
    pub fn try_transition_keep_reason(
        &self,
        expected: TaskStatus,
        target: TaskStatus,
    ) -> Result<TaskStateView, TaskStateView> {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if current.status != expected {
                return Err(current);
            }
            let next = TaskStateView {
                status: target,
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(next),
                Err(_) => continue,
            }
        }
    }

    /// Claim the poll-waiter slot, returning the new token's era, or `None`
    /// because one is already live.
    ///
    /// Bumps the era and clears `pending` in the same CAS, so a token never
    /// begins life pre-signalled and no wake registered under the previous
    /// holder's era can address this one.
    #[inline]
    #[must_use = "a refused claim means a PollWaiter is already live for this task"]
    pub fn poll_arm(&self) -> Option<u8> {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if current.poll_armed {
                return None;
            }
            let era = current.poll_era.wrapping_add(1) % POLL_ERA_MODULUS;
            let next = TaskStateView {
                poll_armed: true,
                poll_pending: false,
                poll_era: era,
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(era),
                Err(_) => continue,
            }
        }
    }

    /// The live token's era, or `None` when no token is armed. Read by
    /// registration so a wake can name the era it was made under.
    #[inline]
    pub fn poll_era(&self) -> Option<u8> {
        let current = TaskStateView::unpack(self.0.load(Ordering::Acquire));
        current.poll_armed.then_some(current.poll_era)
    }

    /// Release the poll-waiter slot and discard any unconsumed wake.
    #[inline]
    pub fn poll_disarm(&self) {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                poll_armed: false,
                poll_pending: false,
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Record a wake for a task that is not parked yet; `false` means the wake
    /// found no token of `era` to take it and the caller must fall back to its
    /// ordinary wake path.
    ///
    /// `era` is the generation the *registration* was made under. Refusing a
    /// mismatch is what stops a wake owed to a finished poll from marking a
    /// later poll's token — see the [module docs](self).
    ///
    /// The epoch is deliberately not bumped: this records a wake rather than
    /// transitioning, and a bump would fail a concurrent comparator that has no
    /// reason to retry.
    #[inline]
    pub fn poll_set_pending(&self, era: u8) -> bool {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if !current.poll_armed || current.poll_era != era {
                return false;
            }
            if current.poll_pending {
                return true;
            }
            let next = TaskStateView {
                poll_pending: true,
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Consume a pending wake (`Ok(false)`, status untouched, caller must not
    /// block) or transition `expected -> Blocked` (`Ok(true)`), in one
    /// compare-exchange. `Err` carries the state when it was not `expected`.
    ///
    /// Fused deliberately: a separate flag test before a separate block CAS
    /// would let a wake land between them, setting a bit nobody re-reads
    /// against a task about to become `Blocked`.
    ///
    /// Gated on [`TaskStatus::can_transition_to`] like every other path into
    /// `Blocked`: publishing a transition the state machine forbids is how a
    /// task stamped terminal by a peer gets restored to a live status and never
    /// reaches cleanup. An illegal `expected` is `Err`, having written nothing.
    #[inline]
    pub fn poll_consume_or_block(
        &self,
        expected: TaskStatus,
        reason: BlockReason,
    ) -> Result<bool, TaskStateView> {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if current.poll_pending {
                let next = TaskStateView {
                    poll_pending: false,
                    ..current
                };
                match self.0.compare_exchange_weak(
                    current_word,
                    next.pack(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(false),
                    Err(_) => continue,
                }
            }
            if current.status != expected || !expected.can_transition_to(TaskStatus::Blocked) {
                return Err(current);
            }
            let next = TaskStateView {
                status: TaskStatus::Blocked,
                reason,
                epoch: current.epoch.wrapping_add(1),
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(true),
                Err(_) => continue,
            }
        }
    }

    /// Clear an unconsumed wake, keeping the token armed. See
    /// [`PollWaiter::clear_pending`](crate::sync::PollWaiter::clear_pending)
    /// for the ordering this must be called in.
    #[inline]
    pub fn poll_clear_pending(&self) {
        loop {
            let current_word = self.0.load(Ordering::Acquire);
            let current = TaskStateView::unpack(current_word);
            if !current.poll_pending {
                return;
            }
            let next = TaskStateView {
                poll_pending: false,
                ..current
            };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Stamp the block reason without touching status, for paths that hand off
    /// a reason just before a CAS that ends in `Blocked` (futex, sleep
    /// timeout). Relaxed: the publishing fence is the subsequent Release-CAS
    /// on status.
    #[inline]
    pub fn store_reason(&self, reason: BlockReason) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView { reason, ..current };
            match self.0.compare_exchange_weak(
                current_word,
                next.pack(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }
}

/// A status a task never comes back from.
#[inline]
const fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Zombie | TaskStatus::Terminated)
}

/// A status in which a task can still be dispatched or woken.
#[inline]
const fn is_live(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Ready | TaskStatus::Running | TaskStatus::Blocked
    )
}

impl Default for TaskState {
    fn default() -> Self {
        Self::invalid()
    }
}

// Runtime coverage lives in `core/src/syscall/tests.rs::test_task_state_fused_cas`:
// the kernel-under-QEMU harness is the only environment with the required
// atomic backing.
