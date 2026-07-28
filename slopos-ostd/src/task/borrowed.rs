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
//! Each method states the ordering it uses, and names the pairing wherever it
//! is load-bearing.

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

    // ── Diagnostic counters ───────────────────────────────────────────
    //
    // Relaxed throughout: nothing is ordered against these. `fetch_add` wraps
    // at 2^32, which for a tally of yields or migrations is immaterial and
    // costs one instruction where saturation would cost a compare-exchange
    // loop.

    /// How many times this task has voluntarily yielded.
    #[inline]
    pub fn yield_count(&self) -> u32 {
        self.yield_count.load(Ordering::Relaxed)
    }

    /// Record one voluntary yield.
    #[inline]
    pub fn inc_yield_count(&self) {
        self.yield_count.fetch_add(1, Ordering::Relaxed);
    }

    /// How many times this task has migrated between CPUs.
    #[inline]
    pub fn migration_count(&self) -> u32 {
        self.migration_count.load(Ordering::Relaxed)
    }

    /// Record one migration. Called by the *thief* CPU, which is why the field
    /// is atomic — see its declaration.
    #[inline]
    pub fn inc_migration_count(&self) {
        self.migration_count.fetch_add(1, Ordering::Relaxed);
    }

    // ── Runtime accounting ────────────────────────────────────────────

    /// Accumulated on-CPU time, in `kdiag_timestamp` ticks.
    #[inline]
    pub fn total_runtime(&self) -> u64 {
        self.total_runtime.load(Ordering::Relaxed)
    }

    /// Add one on-CPU slice, saturating.
    ///
    /// A compare-exchange loop rather than `fetch_add` because this one *does*
    /// want saturation: the tally is reported to userland as a duration, and a
    /// wrap would show a task that has run for a few microseconds as having run
    /// for millennia.
    #[inline]
    pub fn add_total_runtime(&self, delta: u64) {
        let _ = self
            .total_runtime
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(delta))
            });
    }

    // ── Thread-exit futex address ─────────────────────────────────────

    /// The `clear_child_tid` address, or 0 if this task set none.
    #[inline]
    pub fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    /// Install (or clear, with 0) the address to zero and futex-wake on exit.
    #[inline]
    pub fn set_clear_child_tid(&self, addr: u64) {
        self.clear_child_tid.store(addr, Ordering::Relaxed);
    }

    /// Claim the address, leaving none behind.
    ///
    /// A swap rather than a read followed by a store, so that two teardown
    /// paths racing on the same task cannot both perform the futex wake.
    #[inline]
    pub fn take_clear_child_tid(&self) -> u64 {
        self.clear_child_tid.swap(0, Ordering::Relaxed)
    }

    // ── Wheel of Fate ─────────────────────────────────────────────────

    /// Publish a pending outcome. The flag store is Release so a consumer that
    /// sees the flag sees both values.
    #[inline]
    pub fn set_fate(&self, token: u32, value: u32) {
        self.fate_token.store(token, Ordering::Relaxed);
        self.fate_value.store(value, Ordering::Relaxed);
        self.fate_pending.store(1, Ordering::Release);
    }

    /// Consume the pending outcome, if there is one. `None` for a task with
    /// nothing pending, and for every loser of a race to consume it.
    #[inline]
    pub fn take_fate(&self) -> Option<(u32, u32)> {
        if self
            .fate_pending
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        Some((
            self.fate_token.load(Ordering::Relaxed),
            self.fate_value.load(Ordering::Relaxed),
        ))
    }

    /// Drop any pending outcome without consuming it — the exit path, where
    /// there is no longer anyone to hand it to.
    #[inline]
    pub fn clear_fate(&self) {
        self.fate_pending.store(0, Ordering::Release);
        self.fate_token.store(0, Ordering::Relaxed);
        self.fate_value.store(0, Ordering::Relaxed);
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

    /// Take this task's `SYSCALL_TEST_REPORT` ring, leaving the slot empty.
    ///
    /// The taker is a foreign task draining a corpse while the owner installs
    /// the ring lazily, so the `SpinLock` is what makes the two safe against
    /// each other. The `KBox` leaves with the return value and is therefore
    /// dropped by the caller after the guard is released — freeing a ring under
    /// the lock would put an allocator call in the critical section.
    #[inline]
    pub fn take_test_reports(
        &self,
    ) -> Option<crate::KBox<crate::task::test_reports::TestReportRing>> {
        self.test_reports.lock().take()
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

    // ── Saved register state: racy diagnostic reads ───────────────────
    //
    // `TaskContext` is `#[repr(C, packed)]`, so a pointer to one of its `u64`
    // fields carries no alignment guarantee — hence `read_unaligned` rather
    // than a plain read, and hence the only `unsafe` in this file. The
    // unsafety is about alignment and tearing, not about whether the task
    // exists: `as_ptr_racy` is `TaskOwnCell`'s sanctioned unsynchronised read
    // path and forms no reference, so a concurrent write by the owning CPU is
    // a torn value rather than UB.
    //
    // Torn is acceptable for every consumer: these feed log lines, the
    // cr3-identity scan, and stack-probe bounds that range-check each address
    // they read. A witnessed accessor would be wrong here — the whole point is
    // to read a task this CPU is *not* running.

    /// The task's saved `CR3`. The address-space identity tag the user-fault
    /// dispatcher compares against live `CR3`.
    #[inline]
    pub fn context_cr3(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            cr3
        ))
    }

    /// The task's saved instruction pointer.
    #[inline]
    pub fn context_rip(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rip
        ))
    }

    /// The task's saved stack pointer.
    #[inline]
    pub fn context_rsp(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rsp
        ))
    }

    /// The task's saved code selector.
    #[inline]
    pub fn context_cs(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            cs
        ))
    }

    /// The task's saved stack selector.
    #[inline]
    pub fn context_ss(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            ss
        ))
    }

    /// The task's saved RFLAGS.
    #[inline]
    pub fn context_rflags(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rflags
        ))
    }

    /// One racy, unaligned `u64` read out of the saved `context`.
    ///
    /// Note the two distinct `TaskContext` types: `context` is
    /// `kernel_task::TaskContext`, the packed 25-register interrupt-frame
    /// snapshot, while `switch_ctx` is `task::task::TaskContext`, the
    /// callee-saved frame the switch asm uses. They share a name and not a
    /// layout, which is why the two readers below cannot be one.
    ///
    /// Offset-driven so the six getters above share a single `unsafe` block
    /// rather than six copies of the same three lines.
    #[inline]
    fn read_context_field(&self, offset: usize) -> u64 {
        let base = self.context.as_ptr_racy().cast::<u8>();
        // SAFETY: `offset` is an `offset_of!` into the very `TaskContext` this
        // pointer addresses, so the read is in bounds. `read_unaligned`
        // because the struct is packed; no reference is formed, so a
        // concurrent write is a torn value rather than UB.
        unsafe { base.add(offset).cast::<u64>().read_unaligned() }
    }

    /// The saved callee-saved frame's `(rip, rsp)` — the seed for walking a
    /// descheduled task's parked call chain.
    #[inline]
    pub fn switch_ctx_rip_rsp(&self) -> (u64, u64) {
        (
            self.read_switch_ctx_field(core::mem::offset_of!(crate::task::TaskContext, rip)),
            self.read_switch_ctx_field(core::mem::offset_of!(crate::task::TaskContext, rsp)),
        )
    }

    /// The saved frame pointer of a descheduled task.
    #[inline]
    pub fn switch_ctx_rbp(&self) -> u64 {
        self.read_switch_ctx_field(core::mem::offset_of!(crate::task::TaskContext, rbp))
    }

    /// The saved RFLAGS of a descheduled task.
    #[inline]
    pub fn switch_ctx_rflags(&self) -> u64 {
        self.read_switch_ctx_field(core::mem::offset_of!(crate::task::TaskContext, rflags))
    }

    /// See [`read_context_field`](Self::read_context_field); same contract,
    /// other cell.
    #[inline]
    fn read_switch_ctx_field(&self, offset: usize) -> u64 {
        let base = self.switch_ctx.as_ptr_racy().cast::<u8>();
        // SAFETY: as `read_context_field`.
        unsafe { base.add(offset).cast::<u64>().read_unaligned() }
    }
}
