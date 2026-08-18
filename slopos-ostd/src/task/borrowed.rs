//! The borrow-taking task surface — inherent methods on [`TaskInner`].
//!
//! These replace the `*mut TaskInner` accessor layer: taking `&self` removes
//! the dereference that was the only reason for that layer's `unsafe`. Each
//! method states the ordering it uses and names the pairing where it is
//! load-bearing.

use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGNAL_KILLED, SIGNAL_MASK, SigSet};
use slopos_abi::task::{TaskExitReason, TaskFaultReason};

use crate::sync::LinkError;
use crate::sync::intrusive::Link;
use crate::task::exit_info::ExitInfo;
use crate::task::kernel_task::{SchedPlacement, SignalAction, TaskInner};
use crate::task::link_roles::{ReclaimRole, RemoteWakeRole};

impl<K, U> TaskInner<K, U> {
    /// Whether a CPU is physically executing this task.
    ///
    /// Acquire pairs with the switch tail's Release store: a peer observing
    /// `false` also observes everything the outgoing CPU published first.
    #[inline]
    pub fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// Publish or clear the on-CPU flag. See [`on_cpu`](Self::on_cpu).
    #[inline]
    pub fn set_on_cpu(&self, on: bool) {
        self.on_cpu.store(on, Ordering::Release);
    }

    #[inline]
    pub fn sched_placement(&self) -> SchedPlacement {
        SchedPlacement::from_u8(self.sched_placement.load(Ordering::Acquire))
    }

    #[inline]
    pub fn set_sched_placement(&self, placement: SchedPlacement) {
        self.sched_placement
            .store(placement.as_u8(), Ordering::Release);
    }

    /// Move the placement owner from `expected` to `target`, reporting whether
    /// this caller won.
    ///
    /// The cross-role gate that stops a task being in a ready queue and a
    /// remote-wake inbox at once: a loser must not proceed as though it had
    /// claimed the task.
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

    /// The `WaitQueue` this task is parked on, erased to `*mut c_void`, or
    /// null. Resolve it with
    /// [`slopos_ostd::sync::wait_queue::purge_parked_wait_node`].
    #[inline]
    pub fn parked_wait_queue(&self) -> *mut core::ffi::c_void {
        self.parked_wait_queue.load(Ordering::Acquire)
    }

    /// Publish or clear the park back-pointer. Written only by the wait
    /// protocol, on the task's own CPU.
    #[inline]
    pub fn set_parked_wait_queue(&self, queue: *mut core::ffi::c_void) {
        self.parked_wait_queue.store(queue, Ordering::Release);
    }

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

    /// Claim teardown steps, returning the bits *this* caller won.
    ///
    /// Teardown can be split between `task_terminate` and post-switch cleanup,
    /// so the returned mask is what makes each step run exactly once.
    #[inline]
    pub fn exit_cleanup_mark(&self, bits: u8) -> u8 {
        let previous = self.exit_cleanup_flags.fetch_or(bits, Ordering::AcqRel);
        bits & !previous
    }

    #[inline]
    pub fn exit_info(&self) -> &crate::sync::AtomicCell<ExitInfo> {
        &self.exit_info
    }

    /// Whether the exit value has been published.
    #[inline]
    pub fn exit_info_is_set(&self) -> bool {
        self.exit_info.is_set()
    }

    /// Stamp the exit state of a task killed by a fatal user-mode fault, and
    /// report its id so the caller can drive termination by id rather than by
    /// holding this borrow across the diverging switch tail.
    ///
    /// Release on each store, in the order a fault reader walks them.
    ///
    /// The exit code carries the signal a POSIX kernel would have killed the
    /// task with, so `waitpid` can distinguish the causes. `SIGBUS` for an
    /// out-of-memory demand fault: the mapping exists and the access is legal,
    /// but no page can be produced, which is the bus-error case.
    #[inline]
    pub fn record_user_fault_exit(&self, reason: TaskFaultReason) -> u32 {
        self.exit_reason
            .store(TaskExitReason::UserFault.as_u16(), Ordering::Release);
        self.fault_reason.store(reason.as_u16(), Ordering::Release);
        let signal = match reason {
            TaskFaultReason::UserOom => slopos_abi::signal::SIGBUS,
            TaskFaultReason::UserUd => slopos_abi::signal::SIGILL,
            _ => slopos_abi::signal::SIGSEGV,
        };
        self.exit_code.store(128 + signal as u32, Ordering::Release);
        self.task_id
    }

    /// This task's process-group id. See [`pgid`](TaskInner::pgid) for the
    /// ordering.
    #[inline]
    pub fn pgid(&self) -> u32 {
        self.pgid.load(Ordering::Relaxed)
    }

    /// Retarget this task's process group. The caller stamps the matching
    /// [`process_group`](TaskInner::process_group) membership afterwards; that
    /// slot's Release store is what orders the pair.
    #[inline]
    pub fn set_pgid(&self, pgid: u32) {
        self.pgid.store(pgid, Ordering::Relaxed);
    }

    /// This task's session id. See [`sid`](TaskInner::sid) for the ordering.
    #[inline]
    pub fn sid(&self) -> u32 {
        self.sid.load(Ordering::Relaxed)
    }

    /// Move this task into a session. Same pairing as
    /// [`set_pgid`](Self::set_pgid).
    #[inline]
    pub fn set_sid(&self, sid: u32) {
        self.sid.store(sid, Ordering::Relaxed);
    }

    /// Whether this task leads its own session — the `setsid`/`TIOCSCTTY`
    /// precondition, and what makes it the one that hangs up the terminal.
    #[inline]
    pub fn is_session_leader(&self) -> bool {
        let sid = self.sid();
        sid != 0 && sid == self.task_id
    }

    #[inline]
    pub fn signal_pending(&self) -> SigSet {
        self.signal_pending.load(Ordering::Acquire)
    }

    /// Overwrite the signal portion of the pending bitmask.
    ///
    /// Bits outside [`SIGNAL_MASK`] are kernel-private and are preserved: this
    /// is otherwise the one writer that could clear them wholesale.
    #[inline]
    pub fn set_signal_pending(&self, value: SigSet) {
        let want = value & SIGNAL_MASK;
        let mut current = self.signal_pending.load(Ordering::Relaxed);
        loop {
            let next = (current & !SIGNAL_MASK) | want;
            match self.signal_pending.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Whether this task has been marked for death.
    ///
    /// Deliberately does not consult `signal_blocked`: kill is not maskable,
    /// and consulting the blocked set would let `sigprocmask` defeat it.
    #[inline]
    pub fn is_killed(&self) -> bool {
        (self.signal_pending.load(Ordering::Acquire) & SIGNAL_KILLED) != 0
    }

    /// Clear `bits` from the pending set, returning the previous value. Bits
    /// outside [`SIGNAL_MASK`] are ignored.
    #[inline]
    pub fn clear_signal_pending(&self, bits: SigSet) -> SigSet {
        self.signal_pending
            .fetch_and(!(bits & SIGNAL_MASK), Ordering::AcqRel)
    }

    /// Raise `bits` in the pending set, returning the previous value. Bits
    /// outside [`SIGNAL_MASK`] are ignored.
    #[inline]
    pub fn raise_signal_pending(&self, bits: SigSet) -> SigSet {
        self.signal_pending
            .fetch_or(bits & SIGNAL_MASK, Ordering::AcqRel)
    }

    /// The handler registered for signal index `idx`, or `None` when out of
    /// range.
    #[inline]
    pub fn signal_handler(&self, idx: usize) -> Option<u64> {
        self.signal_actions.get(idx).map(|a| a.handler())
    }

    /// The whole disposition registered for signal index `idx`, or `None` when
    /// `idx` names no slot.
    ///
    /// Owner-only for the same reason as
    /// [`SignalActionCell::load_owner_only`](crate::task::kernel_task::SignalActionCell::load_owner_only):
    /// the group is not read atomically, so a remote CPU must use
    /// [`signal_handler`](Self::signal_handler) instead.
    #[inline]
    pub fn signal_action(&self, idx: usize) -> Option<SignalAction> {
        self.signal_actions.get(idx).map(|a| a.load_owner_only())
    }

    /// Publish a whole disposition at signal index `idx`, reporting whether
    /// `idx` named a slot.
    ///
    /// `false` means nothing was written; a caller that derived `idx` from user
    /// input must map that to an error.
    #[inline]
    pub fn set_signal_action(&self, idx: usize, action: SignalAction) -> bool {
        match self.signal_actions.get(idx) {
            Some(cell) => {
                cell.store(action);
                true
            }
            None => false,
        }
    }

    // Relaxed throughout: nothing is ordered against these, and `fetch_add`
    // wrapping at 2^32 is immaterial for a tally of yields or migrations.

    #[inline]
    pub fn yield_count(&self) -> u32 {
        self.yield_count.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn inc_yield_count(&self) {
        self.yield_count.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn migration_count(&self) -> u32 {
        self.migration_count.load(Ordering::Relaxed)
    }

    /// Record one migration. Called by the *thief* CPU, which is why the field
    /// is atomic.
    #[inline]
    pub fn inc_migration_count(&self) {
        self.migration_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Accumulated on-CPU time, in `kdiag_timestamp` ticks.
    #[inline]
    pub fn total_runtime(&self) -> u64 {
        self.total_runtime.load(Ordering::Relaxed)
    }

    /// Add one on-CPU slice, saturating.
    ///
    /// A compare-exchange loop rather than `fetch_add` because this one *does*
    /// want saturation: the tally is reported to userland as a duration, where
    /// a wrap would read as millennia.
    #[inline]
    pub fn add_total_runtime(&self, delta: u64) {
        let _ = self
            .total_runtime
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(delta))
            });
    }

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
    /// the ring lazily, hence the `SpinLock`. The `KBox` leaves with the return
    /// value so the caller drops it after the guard is released, keeping the
    /// allocator out of the critical section.
    #[inline]
    pub fn take_test_reports(
        &self,
    ) -> Option<crate::KBox<crate::task::test_reports::TestReportRing>> {
        self.test_reports.lock().take()
    }

    // Membership in this list *is* a parked strong reference: every method here
    // is half of a placement pair — linking pairs with a retain, unlinking with
    // a reclaim. See `task::placement`.

    #[inline]
    pub fn children_push(
        &self,
        child: core::ptr::NonNull<TaskInner<K, U>>,
    ) -> Result<(), LinkError> {
        self.children.push_back(child)
    }

    #[inline]
    pub fn children_pop(&self) -> Option<core::ptr::NonNull<TaskInner<K, U>>> {
        self.children.pop_front()
    }

    #[inline]
    pub fn children_peek(&self) -> Option<core::ptr::NonNull<TaskInner<K, U>>> {
        self.children.iter().next()
    }

    #[inline]
    pub fn children_remove(
        &self,
        child: core::ptr::NonNull<TaskInner<K, U>>,
    ) -> Result<(), LinkError> {
        self.children.remove(child).map(|_| ())
    }

    #[inline]
    pub fn children_is_empty(&self) -> bool {
        self.children.is_empty()
    }

    #[inline]
    pub fn children_len(&self) -> usize {
        self.children.len()
    }

    // Reference in, pointer out: a Treiber successor's lifetime is governed by
    // the parked reference the link represents, not by a Rust borrow.

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

    // `TaskContext` is `#[repr(C, packed)]`, so its `u64` fields carry no
    // alignment guarantee — hence `read_unaligned`. `as_ptr_racy` forms no
    // reference, so a concurrent write by the owning CPU tears rather than
    // being UB, and torn is acceptable: every consumer range-checks what it
    // reads. Reading a task this CPU is *not* running is the point, so a
    // witnessed accessor would be wrong here.

    /// The task's saved `CR3` — the address-space identity tag the user-fault
    /// dispatcher compares against live `CR3`.
    #[inline]
    pub fn context_cr3(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            cr3
        ))
    }

    #[inline]
    pub fn context_rip(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rip
        ))
    }

    #[inline]
    pub fn context_rsp(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rsp
        ))
    }

    #[inline]
    pub fn context_cs(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            cs
        ))
    }

    #[inline]
    pub fn context_ss(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            ss
        ))
    }

    #[inline]
    pub fn context_rflags(&self) -> u64 {
        self.read_context_field(core::mem::offset_of!(
            crate::task::kernel_task::TaskContext,
            rflags
        ))
    }

    /// One racy, unaligned `u64` read out of the saved `context`.
    ///
    /// Two distinct types share the name `TaskContext`: `context` is
    /// `kernel_task::TaskContext`, the packed interrupt-frame snapshot, while
    /// `switch_ctx` is `task::task::TaskContext`, the callee-saved frame the
    /// switch asm uses. Different layouts, so the two readers cannot be one.
    #[inline]
    fn read_context_field(&self, offset: usize) -> u64 {
        let base = self.context.as_ptr_racy().cast::<u8>();
        // SAFETY: `offset` is an `offset_of!` into the very `TaskContext` this
        // pointer addresses, so the read is in bounds; `read_unaligned` because
        // the struct is packed, and no reference is formed.
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

    #[inline]
    pub fn switch_ctx_rbp(&self) -> u64 {
        self.read_switch_ctx_field(core::mem::offset_of!(crate::task::TaskContext, rbp))
    }

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
