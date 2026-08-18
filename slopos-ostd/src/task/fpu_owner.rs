//! Which task's FPU/vector state is live in each CPU's register file.
//!
//! The switch is eager: every context switch saves the outgoing task's vector
//! state and restores the incoming one's. Deferring the restore until a `#NM`
//! trap is lazy FPU switching, which leaves a stale register file readable
//! across a privilege boundary (CVE-2018-3665). This module adds only a *tag*
//! that makes the eager sequence checkable.
//!
//! The tag is bidirectional — a per-CPU slot naming the task whose state is
//! loaded in that CPU's registers, and a per-task field naming the CPU those
//! registers live on — and [`fpu_state_valid`] requires both halves to agree.
//! The first half catches a cross-task hazard, the second a task that migrated
//! since its state was last live; a one-way counter reports *that* something
//! changed but not *whose* state is in the register file.
//!
//! The pair also degrades safely across allocation reuse: a dying task's
//! address can stay in a CPU's owner slot, but it is only ever compared, never
//! dereferenced, and the task landing at that address starts with
//! [`FPU_CPU_NONE`](self::FPU_CPU_NONE) in its own half, so the agreement check
//! fails and it takes the slow path.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::cpu::x86_64::pcr::{MAX_CPUS, get_current_cpu};
use crate::task::kernel_task::TaskInner;

/// Sentinel for [`TaskInner::fpu_last_cpu`]: no CPU's register file holds this
/// task's FPU state.
///
/// Deliberately not zero: the task initialiser bulk-zeroes, and a zero
/// sentinel would silently name CPU 0.
pub const FPU_CPU_NONE: i32 = -1;

/// Cache-line-isolated owner slot. Written on every context switch by the
/// owning CPU, so a shared line would false-share the switch path across every
/// CPU in the system.
#[repr(C, align(64))]
struct OwnerSlot(AtomicPtr<()>);

/// Per-CPU: the task whose FPU state is live in that CPU's register file, or
/// null when none is.
///
/// Type-erased because a `static` cannot be generic over `TaskInner`'s stack
/// handle parameters, and because the pointer is only ever *compared* — never
/// dereferenced, so no `K`/`U` is ever reconstituted from it.
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
/// never be dereferenced. An out-of-range `cpu` reads as null.
#[inline]
pub(crate) fn fpu_owner_on(cpu: usize) -> *mut () {
    if cpu >= MAX_CPUS {
        return core::ptr::null_mut();
    }
    FPU_OWNER[cpu].0.load(Ordering::Acquire)
}

/// Whether `cpu`'s register file is currently attributed to `task`.
///
/// The *first* half of the agreement only; callers deciding whether the
/// registers really hold this task's state want [`fpu_state_valid`].
#[inline]
pub(crate) fn fpu_owner_is<K, U>(task: &TaskInner<K, U>, cpu: usize) -> bool {
    fpu_owner_on(cpu) == owner_key(task)
}

/// The register file on `cpu` holds `task`'s FPU state, and `task` agrees that
/// `cpu` is where its state lives.
///
/// A false result means only that the fast path is unavailable, never that
/// anything is wrong.
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

/// The precondition of a **save**: this CPU's register file must be unowned, or
/// already owned by `task` itself. Violating it means the `XSAVE` captures a
/// different task's live vector state into `task`'s buffer.
///
/// Deliberately **not** applied before a restore: a restore is not always
/// preceded by a save on the same CPU — `prepare_switch_to` skips the save
/// when `prev` is null — so an incoming task legitimately finds the slot still
/// naming whoever ran there last.
///
/// Skipped while `task.fpu_last_cpu()` is [`FPU_CPU_NONE`]: a task that has
/// never been FPU-restored on any CPU cannot yet own a register file.
///
/// A debug assertion rather than a hard check: the switch path runs with
/// interrupts off, where a panic is its own hazard.
#[inline]
pub fn fpu_owner_assert_may_take<K, U>(task: &TaskInner<K, U>, cpu: usize) {
    if !cfg!(debug_assertions) {
        return;
    }
    // TODO(tech-debt): a context switched out before its first FPU restore
    // saves whatever the previously-restored task left in the registers — it
    // needs an FPU restore or reset before that first switch-out.
    if task.fpu_last_cpu() == FPU_CPU_NONE {
        return;
    }
    if !fpu_owner_may_take(task, cpu) {
        fpu_owner_violation();
    }
}

/// The panic half of [`fpu_owner_assert_may_take`], kept out of line so the
/// panic formatter never lands in the inlined context-switch frame.
#[cold]
#[inline(never)]
fn fpu_owner_violation() -> ! {
    panic!(
        "XSAVE into this task's buffer while the register file belongs to \
         another task: the save would capture the wrong task's vector state"
    )
}

/// The predicate behind [`fpu_owner_assert_may_take`], exposed so the state
/// machine is testable without relying on a panic.
#[inline]
pub(crate) fn fpu_owner_may_take<K, U>(task: &TaskInner<K, U>, cpu: usize) -> bool {
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
/// Only the per-CPU half is cleared: the task's half keeps naming `cpu`, which
/// is what makes the *stale-CPU* half of the agreement meaningful after a
/// migration.
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
/// Correctness does not depend on it — a recycled allocation already fails the
/// agreement check — but it keeps the diagnostic view of [`fpu_owner_on`]
/// honest. `MAX_CPUS` compare-exchanges, once per task *death*, not per switch.
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
/// Off-PCR (host tests, pre-`GS_BASE` boot) this reports CPU 0.
#[inline]
pub fn fpu_current_cpu() -> usize {
    get_current_cpu()
}

// These drive the tag state machine directly rather than through
// `TaskInner::fpu_save_current` / `fpu_restore_to_cpu`, whose inline
// `XSAVE64` / `XRSTOR64` cannot run under Miri.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::KArc;

    type HostTask = TaskInner<(), ()>;

    fn fresh() -> KArc<HostTask> {
        KArc::try_new(HostTask::invalid()).expect("task allocation")
    }

    // [`FPU_OWNER`] is a process-wide static and the harness runs these in
    // parallel threads, so each test owns a distinct slot rather than racing
    // on a shared one. Every slot starts null, so owning one needs no setup.
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
    const CPU_EXEMPT: usize = 14;

    #[test]
    fn a_fresh_task_owns_no_cpu() {
        let task = fresh();
        assert_eq!(task.fpu_last_cpu(), FPU_CPU_NONE);
        assert!(!fpu_state_valid(&task, CPU_FRESH));
        assert!(fpu_owner_may_take(&task, CPU_FRESH));
    }

    #[test]
    fn take_makes_the_state_valid() {
        let task = fresh();
        assert!(!fpu_state_valid(&task, CPU_TAKE));

        fpu_owner_take(&task, CPU_TAKE);

        assert!(fpu_state_valid(&task, CPU_TAKE));
        assert!(fpu_owner_is(&task, CPU_TAKE));
        assert_eq!(task.fpu_last_cpu(), CPU_TAKE as i32);
    }

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

    #[test]
    fn an_eager_switch_sequence_never_trips_the_precondition() {
        let prev = fresh();
        let next = fresh();

        assert!(fpu_owner_may_take(&prev, CPU_SWITCH));
        fpu_owner_take(&prev, CPU_SWITCH);

        assert!(fpu_owner_may_take(&prev, CPU_SWITCH));
        fpu_owner_yield_after_save(&prev, CPU_SWITCH);

        assert!(fpu_owner_may_take(&next, CPU_SWITCH));
        fpu_owner_take(&next, CPU_SWITCH);

        assert!(fpu_state_valid(&next, CPU_SWITCH));
        assert!(!fpu_state_valid(&prev, CPU_SWITCH));
    }

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

    #[test]
    fn saving_another_tasks_live_registers_is_refused() {
        let prev = fresh();
        let next = fresh();
        fpu_owner_take(&prev, CPU_SAVE_REFUSED);

        assert!(!fpu_owner_may_take(&next, CPU_SAVE_REFUSED));
    }

    /// `next` is given an ownership claim first, because a task that has never
    /// been restored anywhere is deliberately exempt.
    #[test]
    #[should_panic(expected = "belongs to another task")]
    #[cfg(debug_assertions)]
    fn the_precondition_assertion_actually_fires() {
        let prev = fresh();
        let next = fresh();
        next.set_fpu_last_cpu(CPU_ASSERT_FIRES as i32);
        fpu_owner_take(&prev, CPU_ASSERT_FIRES);

        fpu_owner_assert_may_take(&next, CPU_ASSERT_FIRES);
    }

    #[test]
    fn a_never_restored_task_is_exempt_from_the_precondition() {
        let prev = fresh();
        let newcomer = fresh();
        fpu_owner_take(&prev, CPU_EXEMPT);

        assert_eq!(newcomer.fpu_last_cpu(), FPU_CPU_NONE);
        assert!(
            !fpu_owner_may_take(&newcomer, CPU_EXEMPT),
            "the raw predicate still reports the mismatch"
        );
        // ...but the assertion declines to panic on it.
        fpu_owner_assert_may_take(&newcomer, CPU_EXEMPT);
    }

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

    #[test]
    fn a_stale_task_half_alone_does_not_make_state_valid() {
        let prev = fresh();
        let next = fresh();

        // A task half naming the CPU that the CPU itself never agreed to —
        // the shape a one-way per-task counter would produce on its own.
        next.set_fpu_last_cpu(CPU_HALF as i32);
        fpu_owner_take(&prev, CPU_HALF);

        assert_eq!(next.fpu_last_cpu(), CPU_HALF as i32);
        assert!(
            !fpu_state_valid(&next, CPU_HALF),
            "one agreeing half must never be enough"
        );
    }

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

    /// The tag is consulted from diagnostic paths that must not add a failure
    /// mode of their own.
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
