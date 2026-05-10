//! Fused task lifecycle state.
//!
//! `TaskState(AtomicU64)` packs the [`TaskStatus`], [`BlockReason`],
//! and a 32-bit ABA epoch into a single 64-bit atomic. Replaces the
//! pre-Phase-5 split of `state_atomic: AtomicU8` + `block_reason:
//! AtomicU8`, which required two unrelated atomic stores to advertise a
//! Blocked task's reason and exposed a window in which an observer
//! could see a stale reason for a fresh status (or vice versa).
//!
//! # Layout (little-endian word, low bit first)
//!
//! ```text
//! bits  0..4    TaskStatus    (4 bits, 5 variants after WillBlock died)
//! bits  4..12   BlockReason   (8 bits, 8 variants)
//! bits 12..16   reserved      (must be zero)
//! bits 16..32   cpu_hint      (16 bits, currently zero — reserved for
//!                              future CPU-affinity-aware wakeup paths)
//! bits 32..64   epoch         (32 bits, ABA defence; bumped on every
//!                              wake/recycle so a stale comparator from
//!                              before the wake fails its CAS)
//! ```
//!
//! `epoch` lets a CAS that targets a specific (status, reason) pair
//! distinguish "the value never changed" from "it changed and changed
//! back": the epoch monotonically advances at every wake-related
//! transition, so any caller that snapshotted before a wake will see a
//! mismatched epoch in `try_transition`'s 64-bit comparator and the
//! CAS will fail. Today no caller exploits this — the scheduler's
//! correctness arguments are status-only — but it costs nothing to
//! reserve the bits and the field is needed when we layer a bounded
//! work-stealer on top in Phase 7.
//!
//! Memory ordering follows the Linux task-state convention:
//! - Writers use Release on the publishing CAS so the data they
//!   synchronise (e.g. wake-up payloads guarded by the new state)
//!   happens-before any reader that observes the state with Acquire.
//! - Readers use Acquire on the observing load.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::task::{BlockReason, TaskStatus};

const STATUS_BITS: u32 = 4;
const REASON_BITS: u32 = 8;
const RESERVED_BITS: u32 = 4;
const CPU_HINT_BITS: u32 = 16;
const EPOCH_BITS: u32 = 32;

const STATUS_SHIFT: u32 = 0;
const REASON_SHIFT: u32 = STATUS_SHIFT + STATUS_BITS;
const _RESERVED_SHIFT: u32 = REASON_SHIFT + REASON_BITS;
const CPU_HINT_SHIFT: u32 = _RESERVED_SHIFT + RESERVED_BITS;
const EPOCH_SHIFT: u32 = CPU_HINT_SHIFT + CPU_HINT_BITS;

const STATUS_MASK: u64 = (1u64 << STATUS_BITS) - 1;
const REASON_MASK: u64 = (1u64 << REASON_BITS) - 1;
const _CPU_HINT_MASK: u64 = (1u64 << CPU_HINT_BITS) - 1;
const EPOCH_MASK: u64 = (1u64 << EPOCH_BITS) - 1;

const _: () = assert!(STATUS_BITS + REASON_BITS + RESERVED_BITS + CPU_HINT_BITS + EPOCH_BITS == 64);

/// Snapshot view of a [`TaskState`] word — the unpacked form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskStateView {
    pub status: TaskStatus,
    pub reason: BlockReason,
    pub epoch: u32,
}

impl TaskStateView {
    /// Repack into the raw 64-bit word. Round-trips with [`unpack`].
    #[inline]
    const fn pack(self) -> u64 {
        let s = (self.status.as_u8() as u64) & STATUS_MASK;
        let r = (self.reason.as_u8() as u64) & REASON_MASK;
        let e = (self.epoch as u64) & EPOCH_MASK;
        (s << STATUS_SHIFT) | (r << REASON_SHIFT) | (e << EPOCH_SHIFT)
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
        }
    }
}

/// Fused lifecycle state. See module docs for the bit layout.
#[repr(transparent)]
pub struct TaskState(AtomicU64);

impl TaskState {
    /// Initial state for a freshly-allocated slot: `Invalid`, no
    /// reason, epoch 0. Reads through this value before
    /// [`force_set`] is called observe `TaskStatus::Invalid`.
    #[inline]
    pub const fn invalid() -> Self {
        let view = TaskStateView {
            status: TaskStatus::Invalid,
            reason: BlockReason::None,
            epoch: 0,
        };
        Self(AtomicU64::new(view.pack()))
    }

    /// Acquire-load the full state and unpack it.
    #[inline]
    pub fn snapshot(&self) -> TaskStateView {
        TaskStateView::unpack(self.0.load(Ordering::Acquire))
    }

    /// Convenience wrapper around [`snapshot`] that returns just the
    /// status field. Use this on hot paths that don't need the reason.
    #[inline]
    pub fn status(&self) -> TaskStatus {
        self.snapshot().status
    }

    /// Convenience wrapper around [`snapshot`] that returns just the
    /// block reason.
    #[inline]
    pub fn reason(&self) -> BlockReason {
        self.snapshot().reason
    }

    /// Try to transition the state from `expected` to `target` while
    /// stamping the block reason. The caller's `expected` is matched
    /// against the current status; the current reason and epoch are
    /// preserved on the comparator (they don't gate the CAS) but
    /// the epoch is bumped on success.
    ///
    /// Returns `Ok(view_after)` on success, `Err(view_now)` on failure.
    /// The error path's view is the freshly-loaded state — callers
    /// that loop must use it as their next comparator's starting
    /// point.
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

    /// Force the state to (status, reason) and bump the epoch. The
    /// "force" is relative to the current value's status and reason —
    /// it ignores them — but the operation is implemented as a
    /// Release CAS-loop because the epoch has to be derived from the
    /// current word and incremented atomically, so a stale comparator
    /// from any pre-call observer fails its next CAS.
    ///
    /// Intended for single-owner contexts (slot init, slot reset,
    /// kernel-only state forcings). The CAS-loop tolerates concurrent
    /// epoch bumps from `bump_epoch` or successful `try_transition`
    /// callers but is not designed to interleave correctly with other
    /// `force_set` callers.
    #[inline]
    pub fn force_set(&self, status: TaskStatus, reason: BlockReason) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                status,
                reason,
                epoch: current.epoch.wrapping_add(1),
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

    /// Bump only the epoch field, preserving status and reason.
    /// Used when a slot is recycled but its terminal state is also
    /// the next initial state (e.g. Terminated → Terminated on
    /// observed-but-not-yet-reaped transitions).
    #[inline]
    pub fn bump_epoch(&self) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                status: current.status,
                reason: current.reason,
                epoch: current.epoch.wrapping_add(1),
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

    /// Try to transition from `expected` to `target` without changing
    /// the block reason. The reason is preserved verbatim (even if
    /// stale relative to the new status — this matches the existing
    /// `try_transition_from` semantics where reason was only meaningful
    /// for transitions ending in `Blocked`).
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
                reason: current.reason,
                epoch: current.epoch.wrapping_add(1),
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

    /// Stamp the block reason without touching status. Used by paths
    /// that hand-off a reason just before a CAS that ends in `Blocked`
    /// (futex, sleep timeout). Relaxed because the publishing fence is
    /// the subsequent Release-CAS on status.
    #[inline]
    pub fn store_reason(&self, reason: BlockReason) {
        loop {
            let current_word = self.0.load(Ordering::Relaxed);
            let current = TaskStateView::unpack(current_word);
            let next = TaskStateView {
                status: current.status,
                reason,
                epoch: current.epoch,
            };
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

impl Default for TaskState {
    fn default() -> Self {
        Self::invalid()
    }
}

// Per-API runtime coverage of pack/unpack roundtrip, transition
// success/failure, epoch advance, bump_epoch preservation, and the
// (status, reason) bit-field maxima lives in
// `core/src/syscall/tests.rs::test_task_state_fused_cas`. That test
// runs in the kernel-under-QEMU harness — the only environment with
// the required atomic backing — so a `cfg(test)` host module here
// would be dead weight.
