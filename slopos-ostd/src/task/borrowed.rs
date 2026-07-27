//! The borrow-taking task surface — inherent methods on [`TaskInner`].
//!
//! # What this is for
//!
//! `slopos-ostd/src/task/accessors.rs` is a layer of ~114 functions over
//! `*mut TaskInner<K, U>`. Each one null-checks a pointer, dereferences it, and
//! does one small thing. That layer is what the task-ownership migration exists
//! to remove: a `*mut Task` in a signature carries no lifetime, so the contract
//! is enforced by review rather than by the compiler, and `check 1` / `check 4`
//! of `check_task_ownership.sh` count exactly that surface.
//!
//! These are the replacements. Every method here takes `&self`, which is the
//! claim the caller could already make — every accessor call site has a
//! registry guard, a `CurrentTask`, or a `&Task` in scope, and was converting
//! it *back* into a pointer to call through the layer.
//!
//! # Why they are safe
//!
//! Nearly every `unsafe` block in the accessor layer exists for one reason: to
//! dereference the raw pointer. Nothing else about the operations is unsafe —
//! they are atomic loads, atomic stores, intrusive-link operations, and reads
//! of plain fields. Taking `&self` removes the dereference, and with it the
//! `unsafe`. Every method in this file is safe, and that is the point: the
//! accessor layer's `unsafe` was never about the operation.
//!
//! The one group that will still need it when it moves here is the
//! `#[repr(C, packed)]` register-state reads through a `TaskOwnCell`'s racy
//! pointer — and there the unsafety is about *alignment and tearing*, not about
//! whether the task exists. That group is not in this file yet.
//!
//! # Where the orderings come from
//!
//! Each method carries the ordering its accessor had, and the pairing is noted
//! where it is load-bearing. This is a retyping, not a redesign: an ordering
//! change smuggled in here would be invisible against a diff whose stated
//! purpose is "take a borrow instead of a pointer".

use core::sync::atomic::Ordering;

use slopos_abi::signal::SigSet;

use crate::sync::LinkError;
use crate::sync::intrusive::Link;
use crate::task::exit_info::ExitInfo;
use crate::task::kernel_task::{SchedPlacement, TaskInner};
use crate::task::link_roles::{ReclaimRole, RemoteWakeRole};

impl<K, U> TaskInner<K, U> {
    // ── Dispatch state ────────────────────────────────────────────────

    /// Whether a CPU is physically executing this task.
    ///
    /// Acquire, pairing with the switch tail's Release store: a peer that
    /// observes `false` must also observe everything the outgoing CPU published
    /// before clearing it.
    #[inline]
    pub fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// Publish or clear the on-CPU flag. See [`on_cpu`](Self::on_cpu).
    #[inline]
    pub fn set_on_cpu(&self, on: bool) {
        self.on_cpu.store(on, Ordering::Release);
    }

    /// This task's scheduler placement owner.
    #[inline]
    pub fn sched_placement(&self) -> SchedPlacement {
        SchedPlacement::from_u8(self.sched_placement.load(Ordering::Acquire))
    }

    /// Store the scheduler placement owner unconditionally.
    #[inline]
    pub fn set_sched_placement(&self, placement: SchedPlacement) {
        self.sched_placement
            .store(placement.as_u8(), Ordering::Release);
    }

    /// Move the placement owner from `expected` to `target`, reporting whether
    /// this caller won.
    ///
    /// The cross-role gate: it is what stops a task being in a ready queue and
    /// a remote-wake inbox at once, so the *result* is the interesting part —
    /// a loser must not proceed as though it had claimed the task.
    #[inline]
    pub fn sched_placement_compare_exchange(
        &self,
        expected: SchedPlacement,
        target: SchedPlacement,
    ) -> bool {
        self.sched_placement
            .compare_exchange(
                expected.as_u8(),
                target.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    // ── Panic-recovery depths ─────────────────────────────────────────

    /// Saved panic-recovery nesting depth. The live value lives in
    /// `PCR.recovery_depth`; this is the copy that travels with the task.
    #[inline]
    pub fn recovery_depth(&self) -> u32 {
        self.recovery_depth.load(Ordering::Acquire)
    }

    /// Stash the recovery depth across a deschedule.
    #[inline]
    pub fn set_recovery_depth(&self, depth: u32) {
        self.recovery_depth.store(depth, Ordering::Release);
    }

    /// Saved panic-in-flight depth. See [`recovery_depth`](Self::recovery_depth).
    #[inline]
    pub fn panic_in_flight(&self) -> u32 {
        self.panic_in_flight.load(Ordering::Acquire)
    }

    /// Stash the panic-in-flight depth across a deschedule.
    #[inline]
    pub fn set_panic_in_flight(&self, depth: u32) {
        self.panic_in_flight.store(depth, Ordering::Release);
    }

    // ── Exit bookkeeping ──────────────────────────────────────────────

    /// Claim teardown steps, returning the bits *this* caller won.
    ///
    /// Teardown can be split between `task_terminate` and post-switch cleanup,
    /// so the returned mask — not the flags themselves — is what makes each
    /// step run exactly once.
    #[inline]
    pub fn exit_cleanup_mark(&self, bits: u8) -> u8 {
        let previous = self.exit_cleanup_flags.fetch_or(bits, Ordering::AcqRel);
        bits & !previous
    }

    /// The durable exit-value cell.
    #[inline]
    pub fn exit_info(&self) -> &crate::sync::AtomicCell<ExitInfo> {
        &self.exit_info
    }

    /// Whether the exit value has been published.
    #[inline]
    pub fn exit_info_is_set(&self) -> bool {
        self.exit_info.is_set()
    }

    // ── Signals ───────────────────────────────────────────────────────

    /// Pending-signal bitmask.
    #[inline]
    pub fn signal_pending(&self) -> SigSet {
        self.signal_pending.load(Ordering::Acquire)
    }

    /// Overwrite the pending-signal bitmask.
    #[inline]
    pub fn set_signal_pending(&self, value: SigSet) {
        self.signal_pending.store(value, Ordering::Release);
    }

    /// Clear `bits` from the pending set, returning the previous value.
    #[inline]
    pub fn clear_signal_pending(&self, bits: SigSet) -> SigSet {
        self.signal_pending.fetch_and(!bits, Ordering::AcqRel)
    }

    /// Raise `bits` in the pending set, returning the previous value.
    #[inline]
    pub fn raise_signal_pending(&self, bits: SigSet) -> SigSet {
        self.signal_pending.fetch_or(bits, Ordering::AcqRel)
    }

    /// The handler registered for signal index `idx`, or `None` when out of
    /// range.
    #[inline]
    pub fn signal_handler(&self, idx: usize) -> Option<u64> {
        self.signal_actions.get(idx).map(|a| a.handler())
    }

    // ── Stacks and identity ───────────────────────────────────────────

    /// Kernel-stack bounds as `(base, top)`. `(0, 0)` when unset.
    ///
    /// The two are read separately and are not published together, so a
    /// concurrent reader can see a torn pair. Every consumer bounds a
    /// diagnostic probe with them, which range-checks each address anyway.
    #[inline]
    pub fn kernel_stack_bounds(&self) -> (u64, u64) {
        (self.kernel_stack_base, self.kernel_stack_top)
    }

    /// The raw, NUL-padded name bytes.
    #[inline]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name
    }

    // ── Children list ─────────────────────────────────────────────────
    //
    // Membership in this list *is* a parked strong reference, so every method
    // here is half of a placement pair — see `task::placement`. They are
    // deliberately not `pub` conveniences over the list: linking must stay
    // paired with a retain, and unlinking with a reclaim.

    /// Link `child` into this task's children list.
    #[inline]
    pub fn children_push(
        &self,
        child: core::ptr::NonNull<TaskInner<K, U>>,
    ) -> Result<(), LinkError> {
        self.children.push_back(child)
    }

    /// Detach the head child, or `None` when the list is empty.
    #[inline]
    pub fn children_pop(&self) -> Option<core::ptr::NonNull<TaskInner<K, U>>> {
        self.children.pop_front()
    }

    /// The head child without detaching it.
    #[inline]
    pub fn children_peek(&self) -> Option<core::ptr::NonNull<TaskInner<K, U>>> {
        self.children.iter().next()
    }

    /// Detach a specific child.
    #[inline]
    pub fn children_remove(
        &self,
        child: core::ptr::NonNull<TaskInner<K, U>>,
    ) -> Result<(), LinkError> {
        self.children.remove(child)
    }

    /// Whether this task has no children.
    #[inline]
    pub fn children_is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// How many children this task has.
    #[inline]
    pub fn children_len(&self) -> usize {
        self.children.len()
    }

    // ── Intrusive links ───────────────────────────────────────────────
    //
    // Reference in, pointer out: a Treiber successor *is* a raw pointer, and
    // its lifetime is governed by the parked reference the link represents
    // rather than by a Rust borrow. That asymmetry is deliberate.

    /// This task's remote-wake inbox successor.
    #[inline]
    pub fn inbox_link(&self) -> &Link<TaskInner<K, U>, RemoteWakeRole> {
        &self.remote_inbox_link
    }

    /// This task's graveyard link.
    ///
    /// Unlike every other link slot, membership here does **not** imply a
    /// parked strong reference: the pusher won the final release, so the count
    /// is already zero and it owns the allocation outright.
    #[inline]
    pub fn reclaim_link(&self) -> &Link<TaskInner<K, U>, ReclaimRole> {
        &self.reclaim_link
    }
}
