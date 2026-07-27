//! Which task's FPU/vector state is live in each CPU's register file.
//!
//! The kernel switches the FPU eagerly: every context switch saves the
//! outgoing task's vector state and restores the incoming one's. Nothing about
//! that is optional — deferring the restore until a `#NM` trap is lazy FPU
//! switching, which leaves a stale register file readable across a privilege
//! boundary (CVE-2018-3665). This module does not make the switch lazy. It adds
//! a *tag* that makes the eager sequence checkable, so a mis-sequenced save or a
//! cross-task restore becomes a debug assertion instead of silently corrupted
//! vector state.
//!
//! # Why the tag is bidirectional
//!
//! Modelled on Linux (`arch/x86/kernel/fpu/context.h`), which carries the same
//! pair and requires both halves to agree:
//!
//! - a per-CPU slot naming the task whose state is loaded in that CPU's
//!   registers — Linux's `fpu_fpregs_owner_ctx`;
//! - a per-task field naming the CPU those registers live on — Linux's
//!   `fpu->last_cpu`.
//!
//! [`fpu_state_valid`] is Linux's `fpregs_state_valid()`: both must agree. Two
//! loads then catch two different bugs at once. A **cross-task** hazard — this
//! CPU's registers hold some other task's state — fails the first half. A
//! **stale-CPU** hazard — the task last ran its FPU on a different CPU and has
//! since migrated — fails the second. A one-way generation counter catches
//! neither reliably: it can tell you *that* something changed but not *whose*
//! state is in the register file, which is the fact every one of these
//! decisions actually turns on.
//!
//! The pair also degrades safely across allocation reuse. A dying task's
//! address can stay in a CPU's owner slot after the allocation is recycled;
//! nothing ever dereferences that pointer (it is only ever compared), and the
//! task that lands at the same address starts with
//! [`FPU_CPU_NONE`](self::FPU_CPU_NONE) in its own half — from `TaskInner::invalid`,
//! from `init_invalid`, and from the fork poison in `clone_from_raw` — so the
//! agreement check fails and the new task takes the slow path. That is the
//! concrete reason for two fields rather than one.
//!
//! # Protocol
//!
//! Exactly two transitions, and they are total: a save hands the register file
//! back, a restore takes it. Nothing in between needs a third call that a call
//! site could forget.
//!
//! - [`fpu_owner_take`] — the register file now holds this task's state.
//!   Follows an `XRSTOR`.
//! - [`fpu_owner_yield_after_save`] — this task's state has been captured into
//!   its buffer, so no task's state is authoritative in the register file any
//!   more. Follows an `XSAVE`.
//!
//! Both are preceded by [`fpu_owner_assert_may_take`], the shared precondition:
//! the register file must be unowned, or already owned by this very task. It is
//! the one check, and it is what fires on both bug shapes.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::cpu::x86_64::pcr::{MAX_CPUS, get_current_cpu};
use crate::task::kernel_task::TaskInner;

/// Sentinel for [`TaskInner::fpu_last_cpu`]: no CPU's register file holds this
/// task's FPU state.
///
/// Deliberately not zero. The task initialiser bulk-zeroes, and a zero sentinel
/// would silently name CPU 0 — the same trap [`TTY_INDEX_NONE`] avoids.
///
/// [`TTY_INDEX_NONE`]: crate::task::kernel_task::TTY_INDEX_NONE
pub const FPU_CPU_NONE: i32 = -1;

/// Cache-line-isolated owner slot. Written on every context switch by the
/// owning CPU, so a shared line would false-share the switch path across every
/// CPU in the system. Matches the `QsSlot` pattern in `crate::sync::rcu`.
#[repr(C, align(64))]
struct OwnerSlot(AtomicPtr<()>);

/// Per-CPU: the task whose FPU state is live in that CPU's register file, or
/// null when none is.
///
/// Type-erased because a `static` cannot be generic over `TaskInner`'s stack
/// handle parameters, and because the pointer is only ever *compared* — never
/// dereferenced, so no `K`/`U` is ever reconstituted from it. The PCR's
/// current-task slot is type-erased for the same reason.
static FPU_OWNER: [OwnerSlot; MAX_CPUS] =
    [const { OwnerSlot(AtomicPtr::new(core::ptr::null_mut())) }; MAX_CPUS];

/// Erase a task reference to the comparison key stored in [`FPU_OWNER`].
#[inline]
fn owner_key<K, U>(task: &TaskInner<K, U>) -> *mut () {
    core::ptr::from_ref(task).cast::<()>().cast_mut()
}

/// The task whose FPU state is live in `cpu`'s register file, or null.
///
/// Diagnostics and assertions only: the result is a bare address that must
/// never be dereferenced. Out-of-range `cpu` reads as null.
#[inline]
pub fn fpu_owner_on(cpu: usize) -> *mut () {
    if cpu >= MAX_CPUS {
        return core::ptr::null_mut();
    }
    FPU_OWNER[cpu].0.load(Ordering::Acquire)
}

/// Whether `cpu`'s register file is currently attributed to `task`.
///
/// This is the *first* half of the agreement only. Callers deciding whether the
/// registers really hold this task's state want [`fpu_state_valid`], which
/// checks both.
#[inline]
pub fn fpu_owner_is<K, U>(task: &TaskInner<K, U>, cpu: usize) -> bool {
    fpu_owner_on(cpu) == owner_key(task)
}

/// Linux's `fpregs_state_valid()`: the register file on `cpu` holds `task`'s
/// FPU state, and `task` agrees that `cpu` is where its state lives.
///
/// Both halves are load-bearing — see the module header. A false result means
/// only that the fast path is unavailable, never that anything is wrong.
#[inline]
pub fn fpu_state_valid<K, U>(task: &TaskInner<K, U>, cpu: usize) -> bool {
    fpu_owner_is(task, cpu) && task.fpu_last_cpu() == cpu_tag(cpu)
}

/// Narrow a CPU index to the `i32` tag domain, mapping anything out of range to
/// [`FPU_CPU_NONE`] so it can never accidentally match a real CPU.
#[inline]
fn cpu_tag(cpu: usize) -> i32 {
    if cpu >= MAX_CPUS {
        return FPU_CPU_NONE;
    }
    cpu as i32
}

/// The shared precondition of both transitions: this CPU's register file must
/// be unowned, or already owned by `task` itself.
///
/// Violating it is one of two real bugs, and which one depends on the caller:
///
/// - before an `XSAVE`, it means the live registers belong to a *different*
///   task, so the save would capture that task's vector state into `task`'s
///   buffer — a silent cross-task corruption with no bad pointer in sight;
/// - before an `XRSTOR`, it means a different task's state is live and has not
///   been saved, so the restore would discard it outright.
///
/// A debug assertion rather than a hard check: on a correctly sequenced switch
/// it is never false, and the switch path runs with interrupts off where a
/// panic is its own hazard.
#[inline]
pub fn fpu_owner_assert_may_take<K, U>(task: &TaskInner<K, U>, cpu: usize) {
    debug_assert!(
        fpu_owner_may_take(task, cpu),
        "FPU register file on this CPU belongs to another task: a save would \
         capture its vector state into the wrong buffer, a restore would \
         discard it"
    );
}

/// The predicate behind [`fpu_owner_assert_may_take`], exposed so the state
/// machine is testable without relying on a panic.
#[inline]
pub fn fpu_owner_may_take<K, U>(task: &TaskInner<K, U>, cpu: usize) -> bool {
    let owner = fpu_owner_on(cpu);
    owner.is_null() || owner == owner_key(task)
}

/// The register file on `cpu` now holds `task`'s FPU state. Follows an
/// `XRSTOR`.
///
/// Stamps both halves of the tag, so [`fpu_state_valid`] is true for exactly
/// this `(task, cpu)` pair afterwards.
#[inline]
pub fn fpu_owner_take<K, U>(task: &TaskInner<K, U>, cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    task.set_fpu_last_cpu(cpu_tag(cpu));
    FPU_OWNER[cpu].0.store(owner_key(task), Ordering::Release);
}

/// `task`'s state has been captured into its own buffer, so the register file
/// on `cpu` is no longer authoritative for anybody. Follows an `XSAVE`.
///
/// The task's half still records `cpu` — that is where its state was last live,
/// which is what makes the *stale-CPU* half of the agreement meaningful after a
/// migration. Only the per-CPU half is cleared, which is what makes the next
/// task's restore pass [`fpu_owner_assert_may_take`] without any third call
/// standing between the save and the restore.
#[inline]
pub fn fpu_owner_yield_after_save<K, U>(task: &TaskInner<K, U>, cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    task.set_fpu_last_cpu(cpu_tag(cpu));
    FPU_OWNER[cpu]
        .0
        .store(core::ptr::null_mut(), Ordering::Release);
}

/// Drop `task` from every CPU's owner slot.
///
/// Belt-and-braces for teardown. Correctness does not depend on it — a stale
/// address is never dereferenced, and the per-task half of the tag already
/// makes a recycled allocation fail the agreement check (see the module
/// header). It exists so a task's death leaves no slot naming it at all, which
/// keeps the diagnostic view of [`fpu_owner_on`] honest.
///
/// `MAX_CPUS` compare-exchanges, once per task *death* — not per switch.
#[inline]
pub fn fpu_owner_forget<K, U>(task: &TaskInner<K, U>) {
    let key = owner_key(task);
    for slot in FPU_OWNER.iter() {
        let _ = slot.0.compare_exchange(
            key,
            core::ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
    task.set_fpu_last_cpu(FPU_CPU_NONE);
}

/// This CPU's index, for callers that do not already have it.
///
/// Off-PCR (host tests, pre-`GS_BASE` boot) this reports CPU 0, which is the
/// existing behaviour of `get_current_cpu` and what makes the protocol
/// testable natively.
#[inline]
pub fn fpu_current_cpu() -> usize {
    get_current_cpu()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// These drive the tag state machine directly rather than through
// `TaskInner::fpu_save_current` / `fpu_restore_to_cpu`, because those execute
// `XSAVE64` / `XRSTOR64` and inline asm cannot run under Miri. The typed ops
// are thin: assert, run the instruction, commit — and they commit by calling
// exactly the functions exercised here, in the order documented on each.
//
// `just check-miri` runs this file under both Stacked and Tree Borrows. Every
// property below is a value property, so `-Zmiri-ignore-leaks` hides nothing.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::KArc;

    type HostTask = TaskInner<(), ()>;

    fn fresh() -> KArc<HostTask> {
        KArc::try_new(HostTask::invalid()).expect("task allocation")
    }

    // [`FPU_OWNER`] is a process-wide static and the test harness runs these
    // in parallel threads, so each test owns a distinct slot rather than
    // resetting a shared one. A `reset`-then-use helper would be a race:
    // whichever test ran concurrently would see the other's writes, and the
    // failure would be intermittent — the worst kind to debug in a file whose
    // whole subject is cross-CPU state. Every slot starts null, so a test that
    // owns its index needs no setup at all.
    const CPU_FRESH: usize = 1;
    const CPU_TAKE: usize = 2;
    const CPU_YIELD: usize = 3;
    const CPU_SWITCH: usize = 4;
    const CPU_RESTORE_REFUSED: usize = 5;
    const CPU_SAVE_REFUSED: usize = 6;
    const CPU_ASSERT_FIRES: usize = 7;
    const CPU_ASSERT_QUIET: usize = 8;
    const CPU_MIGRATE_FROM: usize = 9;
    const CPU_MIGRATE_TO: usize = 10;
    const CPU_HALF: usize = 11;
    const CPU_FORGET: usize = 12;
    const CPU_FORGET_OTHER: usize = 13;

    #[test]
    fn a_fresh_task_owns_no_cpu() {
        let task = fresh();
        assert_eq!(task.fpu_last_cpu(), FPU_CPU_NONE);
        assert!(!fpu_state_valid(&task, CPU_FRESH));
        assert!(fpu_owner_may_take(&task, CPU_FRESH));
    }

    /// The restore transition: after taking the register file, and only then,
    /// both halves agree.
    #[test]
    fn take_makes_the_state_valid() {
        let task = fresh();
        assert!(!fpu_state_valid(&task, CPU_TAKE));

        fpu_owner_take(&task, CPU_TAKE);

        assert!(fpu_state_valid(&task, CPU_TAKE));
        assert!(fpu_owner_is(&task, CPU_TAKE));
        assert_eq!(task.fpu_last_cpu(), CPU_TAKE as i32);
    }

    /// The save transition: the CPU half is released so the next task may take
    /// it, while the task half keeps naming where its state was last live.
    #[test]
    fn yield_after_save_releases_the_cpu_but_keeps_the_task_half() {
        let task = fresh();
        fpu_owner_take(&task, CPU_YIELD);

        fpu_owner_yield_after_save(&task, CPU_YIELD);

        assert!(
            !fpu_state_valid(&task, CPU_YIELD),
            "the registers are no longer authoritative"
        );
        assert!(fpu_owner_on(CPU_YIELD).is_null());
        assert_eq!(
            task.fpu_last_cpu(),
            CPU_YIELD as i32,
            "still records where it last ran"
        );
    }

    /// THE test this module exists for: an eager switch sequence leaves the
    /// incoming task able to take the register file, and never trips the
    /// precondition.
    #[test]
    fn an_eager_switch_sequence_never_trips_the_precondition() {
        let prev = fresh();
        let next = fresh();

        // prev is running: it owns the register file.
        assert!(fpu_owner_may_take(&prev, CPU_SWITCH));
        fpu_owner_take(&prev, CPU_SWITCH);

        // Switch out: save prev.
        assert!(fpu_owner_may_take(&prev, CPU_SWITCH));
        fpu_owner_yield_after_save(&prev, CPU_SWITCH);

        // Switch in: restore next. This is the step that would fire if the
        // save above had been skipped.
        assert!(fpu_owner_may_take(&next, CPU_SWITCH));
        fpu_owner_take(&next, CPU_SWITCH);

        assert!(fpu_state_valid(&next, CPU_SWITCH));
        assert!(!fpu_state_valid(&prev, CPU_SWITCH));
    }

    /// A restore that skips saving the outgoing task is exactly what the
    /// precondition refuses — the bug that silently discards `prev`'s live
    /// vector state.
    #[test]
    fn restoring_over_an_unsaved_task_is_refused() {
        let prev = fresh();
        let next = fresh();
        fpu_owner_take(&prev, CPU_RESTORE_REFUSED);

        assert!(
            !fpu_owner_may_take(&next, CPU_RESTORE_REFUSED),
            "next must not take a register file still holding prev's unsaved state"
        );
    }

    /// A save into `next`'s buffer while `prev`'s state is live would capture
    /// the wrong task's vector registers. Same predicate, other bug shape.
    #[test]
    fn saving_another_tasks_live_registers_is_refused() {
        let prev = fresh();
        let next = fresh();
        fpu_owner_take(&prev, CPU_SAVE_REFUSED);

        assert!(!fpu_owner_may_take(&next, CPU_SAVE_REFUSED));
    }

    /// The assertion is not decorative: it panics on the refused sequence.
    /// A check that cannot fail is not a check.
    #[test]
    #[should_panic(expected = "belongs to another task")]
    #[cfg(debug_assertions)]
    fn the_precondition_assertion_actually_fires() {
        let prev = fresh();
        let next = fresh();
        fpu_owner_take(&prev, CPU_ASSERT_FIRES);

        fpu_owner_assert_may_take(&next, CPU_ASSERT_FIRES);
    }

    /// ...and stays quiet on the legal sequences, so it is not merely always-on.
    #[test]
    fn the_precondition_assertion_passes_a_legal_sequence() {
        let task = fresh();
        fpu_owner_assert_may_take(&task, CPU_ASSERT_QUIET);
        fpu_owner_take(&task, CPU_ASSERT_QUIET);
        // Re-taking one's own register file is legal: a redundant restore.
        fpu_owner_assert_may_take(&task, CPU_ASSERT_QUIET);
        fpu_owner_yield_after_save(&task, CPU_ASSERT_QUIET);
        fpu_owner_assert_may_take(&task, CPU_ASSERT_QUIET);
    }

    /// The stale-CPU half. A task whose state was left live on one CPU must not
    /// be considered valid on another — this is the half a per-CPU owner slot
    /// alone cannot express.
    #[test]
    fn a_migrated_task_is_not_valid_on_its_new_cpu() {
        let task = fresh();
        fpu_owner_take(&task, CPU_MIGRATE_FROM);

        assert!(fpu_state_valid(&task, CPU_MIGRATE_FROM));
        assert!(
            !fpu_state_valid(&task, CPU_MIGRATE_TO),
            "its state is not live on the CPU it migrated to"
        );
    }

    /// The cross-task half, stated against the same pair of loads: the owner
    /// slot names `prev`, so `next` fails the first half even though its own
    /// `fpu_last_cpu` agrees.
    #[test]
    fn a_stale_task_half_alone_does_not_make_state_valid() {
        let prev = fresh();
        let next = fresh();

        // Give `next` a task half naming the CPU without the CPU ever agreeing
        // — the shape a one-way per-task counter would produce on its own.
        next.set_fpu_last_cpu(CPU_HALF as i32);
        fpu_owner_take(&prev, CPU_HALF);

        assert_eq!(next.fpu_last_cpu(), CPU_HALF as i32);
        assert!(
            !fpu_state_valid(&next, CPU_HALF),
            "one agreeing half must never be enough"
        );
    }

    /// Teardown leaves no CPU naming the dead task, and resets its own half.
    #[test]
    fn forget_clears_every_slot_naming_the_task() {
        let task = fresh();
        fpu_owner_take(&task, CPU_FORGET);
        assert!(fpu_owner_is(&task, CPU_FORGET));

        fpu_owner_forget(&task);

        assert!(fpu_owner_on(CPU_FORGET).is_null());
        assert_eq!(task.fpu_last_cpu(), FPU_CPU_NONE);
        assert!(!fpu_state_valid(&task, CPU_FORGET));
    }

    /// Forgetting one task must not clear a slot owned by another.
    #[test]
    fn forget_leaves_other_owners_alone() {
        let owner = fresh();
        let other = fresh();
        fpu_owner_take(&owner, CPU_FORGET_OTHER);

        fpu_owner_forget(&other);

        assert!(
            fpu_owner_is(&owner, CPU_FORGET_OTHER),
            "another task's slot is untouched"
        );
        assert!(fpu_state_valid(&owner, CPU_FORGET_OTHER));
    }

    /// An out-of-range CPU index is inert rather than a panic or an
    /// out-of-bounds index: the tag is consulted from diagnostic paths that
    /// must not add a failure mode of their own.
    #[test]
    fn an_out_of_range_cpu_is_inert() {
        let task = fresh();
        assert!(fpu_owner_on(MAX_CPUS).is_null());
        assert!(!fpu_state_valid(&task, MAX_CPUS));

        fpu_owner_take(&task, MAX_CPUS);
        assert_eq!(
            task.fpu_last_cpu(),
            FPU_CPU_NONE,
            "an out-of-range take must not stamp a bogus CPU"
        );
    }
}
