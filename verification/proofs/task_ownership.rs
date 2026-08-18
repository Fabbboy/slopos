// Task ownership proof.
//
// A Verus-annotated mirror of the ownership core of SlopOS's task lifetime:
// `slopos_ostd::task::placement`, `KArc`'s deferred strong-release split
// (`release_deferrable` / `destroy_deferred`), `task_reclaim`'s `task_put` and
// graveyard, and `task_table::{register_task, reap_task_registration}`. It
// machine-checks seven ownership obligations:
//
//   (T1) The existence reference is handed out at most once and taken back at
//        most once, elected by the FLAG CAS and never by a count. The retain
//        precedes the claim, so a releaser observing the flag is guaranteed the
//        count it is about to take back already exists.
//   (T2) Container transitions conserve the strong count: retain/leak and their
//        matching reclaim MOVE one reference between ledgers, a clone mints
//        exactly one. Linked implies owned.
//   (T3) A task is registered if and only if it holds its existence reference,
//        which is what lets the registry hold only `KWeak` and makes a lookup a
//        liveness-checked upgrade rather than a fabricated strong reference.
//   (T4) A referenced task's body is live: `strong > 0` implies the
//        destructor has not run.
//   (T5) Exactly one caller wins the one-to-zero strong transition and uniquely
//        owns the allocation, and destruction runs exactly once. Finality is
//        decided BY THE DECREMENT, never by reading the count first.
//   (T6) A reap never fires on a dispatch-pinned task: a task some CPU is
//        executing, names as its `PCR.current_task`, or holds in its idle slot
//        stays in the registry and keeps its existence reference.
//   (T7) Destruction implies full detachment: a destroyed task is
//        unregistered, unpinned, in no container, and holds no existence
//        reference.
//
// Concept borrowed from documented Linux behaviour: `task_struct` carries a
// self-reference taken back at release, and PREEMPT_RT forbids the final
// `put_task_struct` in atomic context.
//
// Every operation on the ownership core touches shared state only through
// atomics, so each method body is one atomic-bounded `Step` against the shared
// task and an invariant inductive over `Step` holds in every reachable state of
// every interleaving — which is the concurrency claim.
//
// ONE exception. `DispatchPin` and `DispatchUnpin` are single steps here but
// TWO writes in the tree, in different functions, with a real window between
// them (`task_set_on_cpu` and the `PCR.current_task` publication, in opposite
// orders for pin and unpin). Modelling the pair as atomic is sound only because
// `pinned` mirrors `task_is_dispatch_pinned`, which is a DISJUNCTION —
// `task_on_cpu_load || task_is_current_on_any_cpu || is_idle_task` — so
// whichever write lands first another disjunct still holds and the intermediate
// state collapses onto the pre-state. It does not depend on the write order.
// Narrowing that predicate to a conjunction, or deleting a disjunct, makes T6
// stop describing the tree while THIS PROOF KEEPS VERIFYING, since the model
// never sees the intermediate state it just started admitting. `CurrentTask`
// (`slopos-ostd/src/task/cell.rs`) takes no reference count and rests on the
// `task_is_current_on_any_cpu` disjunct for the same reason.
//
// `is_idle_task` is set once by `install_idle_task` and never cleared, whereas
// `DispatchUnpin` sets `pinned: false` unconditionally — an OVER-approximation
// for idle tasks, and in the safe direction because every obligation mentioning
// `pinned` carries it in the ANTECEDENT. Do not "fix" `DispatchUnpin`: the
// unconditional `pinned: false` is what keeps the step total.
//
// TWO invariants, deliberately:
//
//   `own_inv`     — the sub-step-robust core: it survives every `Step` and also
//                   the intermediate states of a decomposed `Step`.
//   `flag_agrees` — the two biconditionals (`exist_refs == 1 <==> exist_flag`,
//                   `registered <==> exist_flag`). Inductive over `Step` given
//                   `own_inv`, but deliberately NOT sub-step robust:
//                   `register_task` inserts the registry entry before parking
//                   the reference and `reap_task_registration` releases before
//                   removing the entry, so neither biconditional holds
//                   throughout either.
//
// NOT covered, and deliberately excluded rather than assumed: the weak-memory
// ordering of the flag compare-exchange and of `refcount_release`'s CAS loop
// (Verus has no weak-memory model); the intrusive placement links and the
// provenance of the raw pointers `task_placement_leak` / `_reclaim` hand
// around; `KArc`'s saturation arm (`strong` is an unbounded `nat`, so the
// saturated-leak arm is excluded — leaking is not a memory-safety violation, so
// the exclusion is in the sound direction); the weak count and `KWeak::upgrade`;
// and `force_reap_registration`, the fixture path that bypasses the status and
// dispatch-pin gates. Those stay KernMiri-covered; see `../STATUS.md`.
//
// Where the model is *more permissive* than the tree it is deliberately so:
// `ReapAndRelease` drops `reap_task_registration`'s `TaskStatus::Terminated`
// and present-entry gates, so the step machine proves a superset of the real
// behaviours and every theorem below holds a fortiori.

use vstd::prelude::*;

verus! {

/// Abstract image of one task's ownership state: the strong-count ledger split
/// by owner class, the existence-reference flag, the registry and dispatch
/// facts the reap gate keys on, and the destruction bookkeeping.
pub struct TaskOwn {
    /// `KArcInner::strong` — the total live strong reference count.
    pub strong: nat,
    /// How many of `strong` are parked in placement containers: ready queue,
    /// remote inbox, deferred previous-task slot, children list, wait maps,
    /// futex buckets.
    pub containers: nat,
    /// How many of `strong` are caller-held handles: `TaskRef` lookup guards,
    /// the live dispatch reference, the reap's temporary upgrade, `PendingTask`.
    pub transient: nat,
    /// How many of `strong` are the task's own existence reference. A separate
    /// field rather than a function of `exist_flag` so that their agreement is a
    /// breakable fact rather than a definition.
    pub exist_refs: nat,
    /// `TaskInner::existence_ref_parked`.
    pub exist_flag: bool,
    /// The registry holds a (weak) entry for this task.
    pub registered: bool,
    /// `task_is_dispatch_pinned` = `task_on_cpu_load ||
    /// task_is_current_on_any_cpu || per_cpu::is_idle_task`.
    pub pinned: bool,
    /// The `TaskInner` body is still initialised — the destructor has not run.
    pub body_live: bool,
    /// `task_release_strong` returned `Some(ParkedTask)` and the matching
    /// `task_destroy_parked` has not consumed it. The node is uniquely owned.
    pub parked_node: bool,
    /// Ghost: how many times the destructor ran. Must never exceed one.
    pub destroys: nat,
}

/// The core inductive invariant. Every reachable state satisfies it, every
/// `Step` preserves it, and — unlike `flag_agrees` — so does every intermediate
/// state of a decomposed `Step`.
pub open spec fn own_inv(s: TaskOwn) -> bool {
    // (T2) LEDGER CONSERVATION. The strong count is exactly the sum of its
    //      owner classes. This is "linked implies owned" stated as arithmetic:
    //      a container membership *is* a strong reference, so a container can
    //      never name a task it does not own. `task_placement_retain` /
    //      `task_placement_reclaim` move a reference between `containers` and
    //      `transient` and leave this sum alone.
    &&& s.strong == s.containers + s.transient + s.exist_refs
    // (T1) The existence reference is a single reference, and the flag never
    //      advertises one that has not been minted yet. This is the
    //      retain-before-claim ordering in `task_existence_park`, and it is
    //      the conjunct the broken CAS-first ordering violates.
    &&& s.exist_refs <= 1
    &&& (s.exist_flag ==> s.exist_refs == 1)
    // (T3) A task holding its existence reference is in the registry. Losing
    //      this direction strands a live task nothing can look up and nothing
    //      will ever reap, and makes `EXISTENCE_REFS_PARKED` diverge from
    //      registry occupancy — which is exactly what that counter is a
    //      tripwire for. It is why `reap_task_registration` unhashes only
    //      *after* winning the release, never before; see
    //      `broken_reap_unhash_before_release_violates_invariant`. The
    //      converse holds at every atomic-`Step` boundary but not inside
    //      `register_task`'s insert-then-park window, so it lives in
    //      `flag_agrees`.
    &&& (s.exist_flag ==> s.registered)
    // (T6) A dispatch-pinned task still holds its existence reference, and is
    //      therefore still registered. This is the fact `drain_previous_task`
    //      relies on when it calls the switch-tail release "a bare atomic
    //      decrement", and the fact that keeps a running task alive at all:
    //      the dispatching CPU's own dispatch reference is a caller handle it
    //      will hand on, whereas the existence reference is pinned down by the
    //      reap gate for as long as the task is on a CPU. Unhashing a
    //      still-pinned task takes that reference back, and the last release
    //      which follows runs the allocator-heavy destructor — freeing the
    //      kernel stack the CPU is executing on.
    //      `broken_reap_ignoring_pin_violates_invariant` breaks exactly this.
    //      `pinned` is set and cleared by two writes rather than one; see the
    //      disjunction note in the header for why modelling that pair as a
    //      single atomic step is sound, why the idle disjunct's one-way shape
    //      is an over-approximation in the safe direction, and what would
    //      silently invalidate either.
    &&& (s.pinned ==> s.exist_refs == 1)
    &&& (s.pinned ==> s.registered)
    // (T4) A referenced task's body is live. This is the no-use-after-free
    //      conjunct: any holder of a strong reference may dereference.
    &&& (s.strong > 0 ==> s.body_live)
    // (T5) Destruction runs at most once, and exactly once past teardown.
    &&& (s.body_live ==> s.destroys == 0)
    &&& (!s.body_live ==> s.destroys == 1)
    &&& (s.destroys <= 1)
    // (T5) The winner of the one-to-zero transition owns the allocation
    //      outright: nothing else holds a reference, and the body is still
    //      intact for the deferred destructor to run against. This pair is
    //      `with_parked`'s soundness argument — the borrow is exclusive not
    //      because someone else keeps the task alive but because nobody else
    //      can reach it. Conversely a dead-but-undestroyed task is ALWAYS
    //      parked for destruction: the graveyard never strands a corpse.
    &&& (s.parked_node ==> s.strong == 0 && s.body_live)
    &&& (s.strong == 0 && s.body_live ==> s.parked_node)
}

/// The two agreements that hold at every atomic-`Step` boundary but not inside
/// a decomposed step. Carried separately (see the header) and proved inductive
/// over `Step` in `flag_agreement_preserved`.
pub open spec fn flag_agrees(s: TaskOwn) -> bool {
    &&& (s.exist_refs == 1 <==> s.exist_flag)
    &&& (s.registered <==> s.exist_flag)
}

/// A freshly allocated task, before registration: one strong reference held by
/// its constructor (`allocate_task`'s `KArc::try_init`, whose `KArc::get_mut`
/// proves uniqueness and hands the handle to a `PendingTask`), no containers,
/// no existence reference, not registered, not pinned, body intact.
pub open spec fn own_init(s: TaskOwn) -> bool {
    &&& s.strong == 1
    &&& s.containers == 0
    &&& s.transient == 1
    &&& s.exist_refs == 0
    &&& s.exist_flag == false
    &&& s.registered == false
    &&& s.pinned == false
    &&& s.body_live == true
    &&& s.parked_node == false
    &&& s.destroys == 0
}

// ---------------------------------------------------------------------------
// Steps: one per atomic-bounded operation.
// ---------------------------------------------------------------------------

pub enum Step {
    /// `register_task` (`task_table.rs`): insert the weak registry entry, then
    /// `task_existence_park` mints and parks the task's own reference. The two
    /// are fused into one step because `register_task` is the ONLY production
    /// caller of `task_existence_park` — which is what makes T3's biconditional
    /// true. The guard mirrors the CAS: it fires only when the flag is
    /// currently down, and only for a caller holding an owning reference of its
    /// own (the `PendingTask`'s), which is `task_existence_park`'s liveness
    /// contract.
    RegisterAndPark,
    /// `task_existence_park` losing the flag compare-exchange: it minted a
    /// reference, lost `false -> true`, and gave the reference back
    /// (`drop(task_placement_reclaim(task))`). Net identity — which is the
    /// whole point of retaining first and undoing on loss.
    ParkLoses,
    /// `reap_task_registration` (`task_table.rs`): the winner of
    /// `task_existence_release`'s `true -> false` CAS takes the reference back
    /// as an ordinary handle (`task_placement_reclaim`) and the registry entry
    /// is dropped under the same lock. The reference MOVES from the existence
    /// ledger to the caller ledger; `strong` does not change. The reaper's own
    /// upgrade (`transient > 0`) is what makes the returned handle provably
    /// non-final at the moment of return.
    ReapAndRelease,
    /// `task_existence_release` losing the CAS: `None` for every caller after
    /// the first, and for a task that never held one. Identity — which is what
    /// makes a reap idempotent and stops two racing reapers both releasing.
    ReleaseLoses,
    /// `reap_task_registration`'s dispatch-pin gate declining
    /// (`task_is_dispatch_pinned` holds; `REAP_BLOCKED_BY_DISPATCH` is armed
    /// and the idle dispatcher retries). Identity.
    ReapDeclinedPinned,
    /// `task_placement_clone`: one atomic strong-count increment yielding a
    /// fresh caller handle. The wake/enqueue fast path; allocates nothing.
    PlacementClone,
    /// `task_placement_retain`: clone one reference straight into a container
    /// without materialising a handle (`task_placement_clone` then forget).
    ContainerRetain,
    /// `task_placement_leak`: move a caller's owning handle into a container.
    /// `strong` is untouched — the reference changes ledger, not existence.
    /// The switch tail parking the outgoing dispatch reference in the CPU's
    /// deferred previous-task slot is this step.
    ContainerLeak,
    /// `task_placement_reclaim`: take a parked reference back out as a handle.
    /// `strong` untouched — the reference changes ledger, not existence.
    ///
    /// The transition's own guard is `containers > 0`, and that is the whole
    /// of what this step claims. Individual call sites may add their own gate
    /// and some do: `task_family.rs`'s remove-child path reclaims only when
    /// `task_children_remove` reports the child was actually unlinked, which
    /// is correct — an ungated reclaim there would take a reference the list
    /// no longer holds. A gated site is still an instance of this step; it
    /// simply does not take it on every path.
    ///
    /// Deliberately not enumerated here. There are nine call sites across
    /// `per_cpu.rs`, `task_family.rs` and `scheduler.rs`, the set grows
    /// whenever a container is added — C4's remote-inbox drain added two —
    /// and a list in a comment rots silently while the guard above does not.
    ///
    /// The load-bearing instance is `ReadyQueue::dequeue`: the reference
    /// *moves* to the dispatcher rather than being released, so the task is
    /// pinned by a caller handle across the switch window including the
    /// unbounded `on_cpu` spin. That is why `pinned ==> containers >= 1` is
    /// false and `pinned ==> exist_refs == 1` is what holds.
    ContainerReclaim,
    /// A CPU claiming the task. Only a registered task still holding its
    /// existence reference can be claimed, and the dispatching CPU holds the
    /// dequeued reference as a caller handle for the whole window.
    ///
    /// Two writes, not one, and not in the same function: `task_set_on_cpu(_,
    /// true)` at `scheduler.rs:1221` and `:1355`, then the `PCR.current_task`
    /// publication inside `dispatch()`. `dispatch()` itself does *not* set
    /// `on_cpu` — it did once, and this comment said so for longer than it was
    /// true.
    ///
    /// `install_idle_task` is the third writer, and it fits this step's guard
    /// (`!pinned && exist_flag && transient > 0`) exactly: `create_idle_task_
    /// for_cpu` holds the idle task's registry guard across the call, and the
    /// task is registered by then. Unlike the other two it is a one-way write —
    /// see the header note on why that is an over-approximation in the safe
    /// direction rather than a hole.
    DispatchPin,
    /// The switch tail retiring the task. Also two writes in that order
    /// reversed: the successor takes `PCR.current_task` (`dispatch()` on the
    /// incoming task) and only then is `task_set_on_cpu(_, false)` cleared, at
    /// `scheduler.rs:1471`, once every still-Ready task has been published.
    DispatchUnpin,
    /// `task_release_strong` -> `KArc::release_deferrable`, non-final: the CAS
    /// loop observed a previous value above one. A bare atomic decrement, safe
    /// under a lock and with interrupts disabled.
    ReleaseStrongNonFinal,
    /// `task_release_strong`, final: the decrement took the count one-to-zero.
    /// This step's guard is `strong == 1`, but that is the OUTCOME of the
    /// decrement, not a pre-check by the caller — `release_deferrable` reads
    /// nothing before it CASes. The winner uniquely owns the allocation, so
    /// the node is parked for `task_destroy_parked`. By the ledger conjunct,
    /// `strong == 1` with a caller handle outstanding forces
    /// `containers == 0 && exist_refs == 0`: nothing else can reach the task.
    ReleaseStrongFinal,
    /// `task_destroy_parked` -> `KArc::destroy_deferred`, run either inline
    /// when `destroy_context_is_safe` (four facts about the calling CPU:
    /// interrupts on, no lock held, no preempt guard, not dispatch-pinned —
    /// never a count) or from `task_graveyard_drain` with interrupts on and no
    /// lock held. Consumes the parked node exactly once.
    DestroyParked,
}

/// Transition function. Each arm mirrors the corresponding method body.
pub open spec fn step(s: TaskOwn, t: Step) -> TaskOwn {
    match t {
        Step::RegisterAndPark =>
            // Insert succeeded and the flag CAS `false -> true` won. The
            // retain has already happened (it precedes the CAS), so the
            // reference exists before the flag advertises it.
            if !s.exist_flag && s.exist_refs == 0 && s.transient > 0 && s.body_live {
                TaskOwn {
                    strong: (s.strong + 1) as nat,
                    exist_refs: 1,
                    exist_flag: true,
                    registered: true,
                    ..s
                }
            } else {
                s
            },
        Step::ParkLoses =>
            // Flag was already claimed: retain, lose the CAS, undo the retain.
            s,
        Step::ReapAndRelease =>
            // `true -> false` won and the task is not dispatch-pinned, and the
            // reaper holds its own upgrade. The reference moves into the caller
            // ledger and the registry entry goes, under one lock.
            if s.exist_flag && !s.pinned && s.transient > 0 {
                TaskOwn {
                    transient: (s.transient + 1) as nat,
                    exist_refs: 0,
                    exist_flag: false,
                    registered: false,
                    ..s
                }
            } else {
                s
            },
        Step::ReleaseLoses => s,
        Step::ReapDeclinedPinned => s,
        Step::PlacementClone =>
            if s.strong > 0 {
                TaskOwn {
                    strong: (s.strong + 1) as nat,
                    transient: (s.transient + 1) as nat,
                    ..s
                }
            } else {
                s
            },
        Step::ContainerRetain =>
            if s.strong > 0 {
                TaskOwn {
                    strong: (s.strong + 1) as nat,
                    containers: (s.containers + 1) as nat,
                    ..s
                }
            } else {
                s
            },
        Step::ContainerLeak =>
            if s.transient > 0 {
                TaskOwn {
                    transient: (s.transient - 1) as nat,
                    containers: (s.containers + 1) as nat,
                    ..s
                }
            } else {
                s
            },
        Step::ContainerReclaim =>
            if s.containers > 0 {
                TaskOwn {
                    containers: (s.containers - 1) as nat,
                    transient: (s.transient + 1) as nat,
                    ..s
                }
            } else {
                s
            },
        Step::DispatchPin =>
            if !s.pinned && s.exist_flag && s.transient > 0 {
                TaskOwn { pinned: true, ..s }
            } else {
                s
            },
        Step::DispatchUnpin => TaskOwn { pinned: false, ..s },
        Step::ReleaseStrongNonFinal =>
            if s.transient > 0 && s.strong > 1 {
                TaskOwn {
                    strong: (s.strong - 1) as nat,
                    transient: (s.transient - 1) as nat,
                    ..s
                }
            } else {
                s
            },
        Step::ReleaseStrongFinal =>
            if s.transient > 0 && s.strong == 1 {
                TaskOwn {
                    strong: 0,
                    transient: 0,
                    parked_node: true,
                    ..s
                }
            } else {
                s
            },
        Step::DestroyParked =>
            if s.parked_node {
                TaskOwn {
                    parked_node: false,
                    body_live: false,
                    destroys: (s.destroys + 1) as nat,
                    ..s
                }
            } else {
                s
            },
    }
}

// ---------------------------------------------------------------------------
// Induction.
// ---------------------------------------------------------------------------

/// Every `Step` preserves `own_inv`. Because each step is the image of an
/// atomic-bounded method body, and any concurrent interleaving is a sequence
/// of such steps against the shared task, this single fact is the whole-system
/// concurrency guarantee.
pub proof fn step_preserves(s: TaskOwn, t: Step)
    requires
        own_inv(s),
    ensures
        own_inv(step(s, t)),
{
}

/// The biconditionals are inductive over `Step` given `own_inv`.
pub proof fn flag_agreement_preserved(s: TaskOwn, t: Step)
    requires
        own_inv(s),
        flag_agrees(s),
    ensures
        flag_agrees(step(s, t)),
{
}

/// A freshly allocated task satisfies both.
pub proof fn init_inv(s: TaskOwn)
    requires
        own_init(s),
    ensures
        own_inv(s),
        flag_agrees(s),
{
}

/// Replay a finite trace from a start state. A trace is any interleaving of
/// registrations, wakes, enqueues, dispatches, releases and reaps from any
/// number of CPUs, since each leaves the shared task only through these
/// atomic-bounded transitions.
pub open spec fn run(s: TaskOwn, trace: Seq<Step>) -> TaskOwn
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(run(s, trace.drop_last()), trace.last())
    }
}

/// MAIN THEOREM. From any freshly allocated task, after any trace of ownership
/// steps (any concurrent interleaving), `own_inv` still holds.
pub proof fn invariant_holds_on_every_trace(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        own_inv(run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        init_inv(s0);
    } else {
        invariant_holds_on_every_trace(s0, trace.drop_last());
        step_preserves(run(s0, trace.drop_last()), trace.last());
    }
}

/// Same, for the two agreements.
pub proof fn flag_agreement_on_every_trace(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        flag_agrees(run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        init_inv(s0);
    } else {
        flag_agreement_on_every_trace(s0, trace.drop_last());
        invariant_holds_on_every_trace(s0, trace.drop_last());
        flag_agreement_preserved(run(s0, trace.drop_last()), trace.last());
    }
}

// ---------------------------------------------------------------------------
// Named corollaries, one per obligation.
// ---------------------------------------------------------------------------

/// (T1) The existence reference is singular: in every reachable state a task
/// holds at most one, and the flag never advertises one that has not been
/// minted. This is what makes the parked tally (`EXISTENCE_REFS_PARKED`) a
/// faithful leak tripwire against registry occupancy.
pub proof fn t1_existence_reference_is_singular(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).exist_refs <= 1,
        run(s0, trace).exist_flag ==> run(s0, trace).exist_refs == 1,
        run(s0, trace).exist_refs == 1 <==> run(s0, trace).exist_flag,
{
    invariant_holds_on_every_trace(s0, trace);
    flag_agreement_on_every_trace(s0, trace);
}

/// (T1) The release is FLAG-elected, not count-elected, and idempotent: a task
/// that does not hold the flag yields nothing, a second attempt is a no-op, and
/// the winner's reference is MOVED rather than minted (`strong` unchanged) —
/// which is why two racing reapers cannot both release.
pub proof fn t1_release_is_flag_elected_and_idempotent(s: TaskOwn)
    requires
        own_inv(s),
    ensures
        // Never held one, or already released: `None`.
        !s.exist_flag ==> step(s, Step::ReapAndRelease) == s,
        // Idempotent: the second reaper sees the flag down.
        step(step(s, Step::ReapAndRelease), Step::ReapAndRelease) == step(
            s,
            Step::ReapAndRelease,
        ),
        // The winner moves a reference, it does not mint one.
        step(s, Step::ReapAndRelease).strong == s.strong,
        // And the handle it returns is not the last one at the moment of
        // return, because the reaper's own upgrade is still outstanding.
        (s.exist_flag && !s.pinned && s.transient > 0) ==> step(
            s,
            Step::ReapAndRelease,
        ).transient >= 2,
{
}

/// (T2) Step-level ledger algebra: leak/reclaim/release move a reference
/// between ledgers, clone/retain mint exactly one, and nothing else touches
/// the count.
pub proof fn t2_step_ledger(s: TaskOwn)
    requires
        own_inv(s),
    ensures
        step(s, Step::ContainerLeak).strong == s.strong,
        step(s, Step::ContainerReclaim).strong == s.strong,
        step(s, Step::ReapAndRelease).strong == s.strong,
        s.strong > 0 ==> step(s, Step::PlacementClone).strong == s.strong + 1,
        s.strong > 0 ==> step(s, Step::ContainerRetain).strong == s.strong + 1
            && step(s, Step::ContainerRetain).containers == s.containers + 1,
        s.transient > 0 ==> step(s, Step::ContainerLeak).containers == s.containers + 1
            && step(s, Step::ContainerLeak).transient == s.transient - 1,
        s.containers > 0 ==> step(s, Step::ContainerReclaim).containers == s.containers - 1
            && step(s, Step::ContainerReclaim).transient == s.transient + 1,
        step(s, Step::DispatchPin).strong == s.strong,
        step(s, Step::DispatchUnpin).strong == s.strong,
{
}

/// (T2) Linked implies owned, on every trace: the strong count is exactly the
/// sum of its owner classes, so a container can never name a task it does not
/// own and an unmatched park is arithmetically impossible.
pub proof fn t2_ledger_holds_on_every_trace(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).strong == run(s0, trace).containers + run(s0, trace).transient
            + run(s0, trace).exist_refs,
        run(s0, trace).containers > 0 ==> run(s0, trace).strong > 0,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (T3) Registered iff holding the existence reference. This is the statement
/// that lets the registry hold only `KWeak` and own nothing: a registry entry
/// keeps nothing alive, and every registered task is kept alive by exactly the
/// reference this proof tracks — so `reap_task_registration`'s "this upgrade
/// cannot fail for an entry that is present" is a theorem, not a hope.
pub proof fn t3_registered_iff_existence_reference(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).registered <==> run(s0, trace).exist_flag,
        run(s0, trace).registered ==> run(s0, trace).strong > 0,
{
    invariant_holds_on_every_trace(s0, trace);
    flag_agreement_on_every_trace(s0, trace);
}

/// (T4) No use-after-free: in every reachable state a positive strong count
/// implies the body is still initialised. Every holder of a strong reference
/// may dereference.
pub proof fn t4_referenced_task_body_is_live(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).strong > 0 ==> run(s0, trace).body_live,
        run(s0, trace).registered ==> run(s0, trace).body_live,
        run(s0, trace).pinned ==> run(s0, trace).body_live,
{
    invariant_holds_on_every_trace(s0, trace);
    flag_agreement_on_every_trace(s0, trace);
}

/// (T5) The one-to-zero transition elects exactly one owner. A caller whose
/// decrement did not land on zero gets nothing; the one whose did owns the
/// allocation outright — no container, no existence reference, no other
/// handle can reach it — and a second attempt is a no-op. Finality is the
/// outcome of the decrement: with `strong != 1` the step is identity, so no
/// count pre-check appears anywhere.
pub proof fn t5_final_release_elects_one_owner(s: TaskOwn)
    requires
        own_inv(s),
    ensures
        s.strong != 1 ==> step(s, Step::ReleaseStrongFinal) == s,
        step(step(s, Step::ReleaseStrongFinal), Step::ReleaseStrongFinal) == step(
            s,
            Step::ReleaseStrongFinal,
        ),
        step(s, Step::ReleaseStrongFinal).parked_node ==> step(
            s,
            Step::ReleaseStrongFinal,
        ).strong == 0 && step(s, Step::ReleaseStrongFinal).containers == 0 && step(
            s,
            Step::ReleaseStrongFinal,
        ).transient == 0 && step(s, Step::ReleaseStrongFinal).exist_refs == 0 && step(
            s,
            Step::ReleaseStrongFinal,
        ).body_live,
        own_inv(step(s, Step::ReleaseStrongFinal)),
{
    step_preserves(s, Step::ReleaseStrongFinal);
}

/// (T5) Destruction runs at most once on any trace, and only from a parked
/// node — so the graveyard's single-pusher assumption holds and a double
/// `task_destroy_parked` is unreachable. The last two conjuncts are
/// `with_parked`'s safety contract restated: a parked node is unreachable by
/// anyone else and its body is still initialised, so the borrow it hands out
/// is exclusive by construction.
pub proof fn t5_destruction_runs_at_most_once(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).destroys <= 1,
        !run(s0, trace).body_live ==> run(s0, trace).destroys == 1,
        run(s0, trace).parked_node ==> run(s0, trace).destroys == 0,
        run(s0, trace).parked_node ==> run(s0, trace).strong == 0 && run(s0, trace).body_live,
{
    invariant_holds_on_every_trace(s0, trace);
}

/// (T6) The reap declines while dispatch-pinned, and a pinned task therefore
/// stays in the registry and keeps its existence reference on every trace.
/// `CurrentTask`'s soundness rests on exactly this: the guard takes no
/// reference count, and what stops the task being freed underneath it is that
/// the reap gate's second disjunct is the very condition under which the guard
/// exists.
pub proof fn t6_reap_declines_while_dispatch_pinned(s: TaskOwn, s0: TaskOwn, trace: Seq<Step>)
    requires
        own_inv(s),
        own_init(s0),
    ensures
        s.pinned ==> step(s, Step::ReapAndRelease) == s,
        s.pinned ==> step(s, Step::ReapDeclinedPinned) == s,
        run(s0, trace).pinned ==> run(s0, trace).registered,
        run(s0, trace).pinned ==> run(s0, trace).exist_flag,
        run(s0, trace).pinned ==> run(s0, trace).strong > 0,
{
    invariant_holds_on_every_trace(s0, trace);
    flag_agreement_on_every_trace(s0, trace);
}

/// (T7) A destroyed task is fully detached: unregistered, unpinned, in no
/// container, holding no existence reference, with no outstanding handle.
/// This is the fact each of `Task::drop`'s debug assertions checks at runtime.
pub proof fn t7_destruction_implies_full_detachment(s0: TaskOwn, trace: Seq<Step>)
    requires
        own_init(s0),
    ensures
        run(s0, trace).destroys == 1 ==> run(s0, trace).strong == 0 && run(
            s0,
            trace,
        ).containers == 0 && run(s0, trace).transient == 0 && run(s0, trace).exist_refs == 0
            && !run(s0, trace).exist_flag && !run(s0, trace).registered && !run(s0, trace).pinned
            && !run(s0, trace).parked_node,
{
    invariant_holds_on_every_trace(s0, trace);
    flag_agreement_on_every_trace(s0, trace);
}

// ---------------------------------------------------------------------------
// Broken variant 1 — a release elected by reading `strong_count == 1`.
// ---------------------------------------------------------------------------

/// A release that decides finality by READING the count first, then destroying
/// — the shape `task_put` and `reap_task_registration` deliberately refuse
/// (`task_reclaim.rs`: "finality is decided by the decrement rather than by
/// reading the count first"; `placement.rs`: "a `strong_count == 1` pre-check
/// is racy"). It destroys whenever it observes one, without performing the
/// electing decrement and therefore without discovering who else owns the
/// task.
pub open spec fn broken_release_by_count(s: TaskOwn) -> TaskOwn {
    if s.strong == 1 {
        TaskOwn {
            strong: 0,
            body_live: false,
            parked_node: false,
            destroys: (s.destroys + 1) as nat,
            ..s
        }
    } else {
        s
    }
}

/// Witness that deciding finality by the decrement is load-bearing.
///
/// Take a task whose one remaining reference is PARKED IN A CONTAINER and not
/// held by any caller — a reaped-but-still-queued task, reached by the trace in
/// `broken_release_witness_is_reachable` below. A count-elected releaser reads
/// `strong == 1`, concludes it is the last, and destroys — while the
/// container's parked reference still names the allocation. The result violates
/// the ledger conjunct (`strong == 0` with `containers == 1`) and the
/// no-use-after-free conjunct. The real `ReleaseStrongFinal` is identity here
/// (its guard needs a caller handle, which the decrement is what would
/// produce), so the fix genuinely depends on electing by the decrement.
pub proof fn broken_count_elected_release_violates_ledger()
    ensures
        exists|s: TaskOwn|
            #![trigger broken_release_by_count(s)]
            own_inv(s) && !own_inv(broken_release_by_count(s)),
        forall|s: TaskOwn| own_inv(s) ==> #[trigger] own_inv(step(s, Step::ReleaseStrongFinal)),
{
    // A task whose sole reference is a ready-queue / wait-map membership.
    let queued = TaskOwn {
        strong: 1,
        containers: 1,
        transient: 0,
        exist_refs: 0,
        exist_flag: false,
        registered: false,
        pinned: false,
        body_live: true,
        parked_node: false,
        destroys: 0,
    };
    assert(own_inv(queued));
    let freed = broken_release_by_count(queued);
    assert(freed.strong == 0);
    assert(freed.containers == 1);
    // strong == 0 with a container still owning it — the ledger conjunct.
    assert(!own_inv(freed));
    assert(own_inv(queued) && !own_inv(broken_release_by_count(queued)));
    assert(exists|s: TaskOwn|
        #![trigger broken_release_by_count(s)]
        own_inv(s) && !own_inv(broken_release_by_count(s)));
    assert forall|s: TaskOwn| own_inv(s) implies #[trigger] own_inv(
        step(s, Step::ReleaseStrongFinal),
    ) by {
        step_preserves(s, Step::ReleaseStrongFinal);
    }
}

// ---------------------------------------------------------------------------
// Broken variant 2 — a park that CASes the flag BEFORE retaining.
// ---------------------------------------------------------------------------
//
// `task_existence_park` is two writes another CPU can interleave with: mint the
// reference, then claim the flag. `RegisterAndPark` models them atomically
// *because the fix retains first*: the only sub-step a peer observes (the flag
// going up) happens after the reference already exists, so a releaser that wins
// the `true -> false` CAS is guaranteed a reference to take back. The specs
// below expose both orderings and prove the fix is not cosmetic.

/// FIXED, sub-step 1: mint the reference (`task_placement_retain`). The flag is
/// still down, so no releaser can act on it yet. The cost of this ordering is
/// that a loser must undo its retain — which `ParkLoses` does.
pub open spec fn park_retain(s: TaskOwn) -> TaskOwn {
    TaskOwn { strong: (s.strong + 1) as nat, exist_refs: 1, ..s }
}

/// FIXED, sub-step 2: win the `false -> true` compare-exchange. The reference
/// the flag now advertises already exists.
pub open spec fn park_claim(s: TaskOwn) -> TaskOwn {
    TaskOwn { exist_flag: true, ..s }
}

/// Witness that the retain-before-claim ordering is load-bearing.
///
/// The BROKEN ordering is `park_claim` applied FIRST: claim the flag, retain
/// afterwards. In the window between the two, the flag advertises a reference
/// that has not been minted; a concurrent `task_existence_release` wins the
/// `true -> false` CAS and reclaims a strong reference that does not exist —
/// one decrement too many, with no bad pointer anywhere in sight. The fixed
/// ordering keeps both sub-states invariant, and composing the two sub-steps
/// reproduces the atomic `RegisterAndPark` exactly.
pub proof fn broken_park_ordering_violates_invariant()
    ensures
        exists|s: TaskOwn|
            #![trigger park_claim(s)]
            own_inv(s) && s.registered && !s.exist_flag && !own_inv(park_claim(s)),
        forall|s: TaskOwn|
            (own_inv(s) && s.registered && !s.exist_flag && s.exist_refs == 0 && s.transient > 0)
                ==> #[trigger] own_inv(park_retain(s)) && own_inv(park_claim(park_retain(s)))
                && park_claim(park_retain(s)) == step(s, Step::RegisterAndPark),
{
    // A registered task an instant before it is handed its existence
    // reference — `register_task`'s window between the registry insert and
    // `task_existence_park`.
    let fresh = TaskOwn {
        strong: 1,
        containers: 0,
        transient: 1,
        exist_refs: 0,
        exist_flag: false,
        registered: true,
        pinned: false,
        body_live: true,
        parked_node: false,
        destroys: 0,
    };
    assert(own_inv(fresh));
    let advertised = park_claim(fresh);
    assert(advertised.exist_flag && advertised.exist_refs == 0);
    // Flag up with no reference minted — the (T1) conjunct.
    assert(!own_inv(advertised));
    assert(own_inv(fresh) && fresh.registered && !fresh.exist_flag && !own_inv(park_claim(fresh)));
    assert(exists|s: TaskOwn|
        #![trigger park_claim(s)]
        own_inv(s) && s.registered && !s.exist_flag && !own_inv(park_claim(s)));
    // The fixed ordering keeps both sub-states invariant.
    assert forall|s: TaskOwn|
        (own_inv(s) && s.registered && !s.exist_flag && s.exist_refs == 0
            && s.transient > 0) implies #[trigger] own_inv(park_retain(s)) && own_inv(
        park_claim(park_retain(s)),
    ) && park_claim(park_retain(s)) == step(s, Step::RegisterAndPark) by {
        let minted = park_retain(s);
        assert(own_inv(minted));
        assert(own_inv(park_claim(minted)));
    }
}

// ---------------------------------------------------------------------------
// Broken variant 3 — a reap that unhashes BEFORE winning the release.
// ---------------------------------------------------------------------------
//
// `reap_task_registration` is also two writes: win `task_existence_release`,
// then drop the registry entry. Both happen under the registry cli-spinlock,
// so no peer observes the intermediate — but the ORDER is still load-bearing,
// and the code spells out why it unhashes second: while the existence
// reference is still held, the weak count is at least two, so dropping the
// entry is a bare decrement that provably cannot reach the allocator from
// under that lock. The model exposes the ownership half of the same ordering.

/// FIXED, sub-step 1: win the `true -> false` compare-exchange and reclaim the
/// existence reference as an ordinary handle. The registry entry is still
/// present, so the task remains reachable by id throughout.
pub open spec fn reap_release(s: TaskOwn) -> TaskOwn {
    TaskOwn {
        transient: (s.transient + 1) as nat,
        exist_refs: 0,
        exist_flag: false,
        ..s
    }
}

/// FIXED, sub-step 2: drop the registry entry.
pub open spec fn reap_unhash(s: TaskOwn) -> TaskOwn {
    TaskOwn { registered: false, ..s }
}

/// Witness that unhashing only after winning the release is load-bearing.
///
/// The BROKEN ordering is `reap_unhash` applied FIRST. It reaches a state where
/// the task still holds its own existence reference but no longer has a
/// registry entry: nothing can look it up, so nothing will ever reap it, and
/// `EXISTENCE_REFS_PARKED` permanently exceeds registry occupancy — the exact
/// divergence that counter is a tripwire for. That violates `own_inv`'s
/// `exist_flag ==> registered` conjunct. The fixed order keeps both sub-states
/// invariant and composes to the atomic `ReapAndRelease`. Note that
/// `flag_agrees` is momentarily false in the fixed intermediate (flag down,
/// entry still present) — which is precisely why the two biconditionals are
/// carried outside `own_inv`.
pub proof fn broken_reap_unhash_before_release_violates_invariant()
    ensures
        exists|s: TaskOwn|
            #![trigger reap_unhash(s)]
            own_inv(s) && s.exist_flag && s.registered && !own_inv(reap_unhash(s)),
        forall|s: TaskOwn|
            (own_inv(s) && s.exist_flag && !s.pinned && s.transient > 0) ==> #[trigger] own_inv(
                reap_release(s),
            ) && own_inv(reap_unhash(reap_release(s))) && reap_unhash(reap_release(s)) == step(
                s,
                Step::ReapAndRelease,
            ),
{
    // A terminated, unpinned task the reaper has just upgraded.
    let reaping = TaskOwn {
        strong: 2,
        containers: 0,
        transient: 1,
        exist_refs: 1,
        exist_flag: true,
        registered: true,
        pinned: false,
        body_live: true,
        parked_node: false,
        destroys: 0,
    };
    assert(own_inv(reaping));
    let stranded = reap_unhash(reaping);
    assert(stranded.exist_flag && !stranded.registered);
    // Holds its existence reference but is in no registry — the (T3) conjunct.
    assert(!own_inv(stranded));
    assert(own_inv(reaping) && reaping.exist_flag && reaping.registered && !own_inv(
        reap_unhash(reaping),
    ));
    assert(exists|s: TaskOwn|
        #![trigger reap_unhash(s)]
        own_inv(s) && s.exist_flag && s.registered && !own_inv(reap_unhash(s)));
    assert forall|s: TaskOwn|
        (own_inv(s) && s.exist_flag && !s.pinned && s.transient > 0) implies #[trigger] own_inv(
        reap_release(s),
    ) && own_inv(reap_unhash(reap_release(s))) && reap_unhash(reap_release(s)) == step(
        s,
        Step::ReapAndRelease,
    ) by {
        let released = reap_release(s);
        assert(own_inv(released));
        assert(own_inv(reap_unhash(released)));
    }
}

// ---------------------------------------------------------------------------
// Broken variant 4 — a reap that ignores the dispatch-pinned gate.
// ---------------------------------------------------------------------------

/// A reap without the `task_is_dispatch_pinned` check: it unhashes and takes
/// the existence reference back while a CPU is still executing the task or
/// still names it as `PCR.current_task`.
pub open spec fn broken_reap_ignoring_pin(s: TaskOwn) -> TaskOwn {
    if s.exist_flag && s.transient > 0 {
        TaskOwn {
            transient: (s.transient + 1) as nat,
            exist_refs: 0,
            exist_flag: false,
            registered: false,
            ..s
        }
    } else {
        s
    }
}

/// Witness that the dispatch-pin gate is load-bearing.
///
/// Take a task that is registered, holds its existence reference, is the
/// dispatching CPU's live dispatch reference, and is currently on a CPU — the
/// ordinary running state. The ungated reap unhashes it and takes back the one
/// reference that is not the dispatcher's own, reaching a state where a CPU's
/// `PCR.current_task` names a task the registry does not know and whose last
/// reference is now the dispatcher's. When that one drops, the destructor FREES
/// THE KERNEL STACK THE CPU IS EXECUTING ON. The gated reap is identity on that
/// state. This also deletes `CurrentTask`'s and `IdleTask`'s soundness
/// arguments, which is why the gate is spelled out at both the reap and the
/// destructor (`destroy_context_is_safe` shares the predicate so the two can
/// never disagree).
pub proof fn broken_reap_ignoring_pin_violates_invariant()
    ensures
        exists|s: TaskOwn|
            #![trigger broken_reap_ignoring_pin(s)]
            own_inv(s) && s.pinned && !own_inv(broken_reap_ignoring_pin(s)),
        forall|s: TaskOwn|
            (own_inv(s) && s.pinned) ==> #[trigger] step(s, Step::ReapAndRelease) == s,
{
    let running = TaskOwn {
        strong: 3,
        containers: 1,  // e.g. the parent's children-list membership
        transient: 1,  // the dispatching CPU's live dispatch reference
        exist_refs: 1,
        exist_flag: true,
        registered: true,
        pinned: true,
        body_live: true,
        parked_node: false,
        destroys: 0,
    };
    assert(own_inv(running));
    let unhashed = broken_reap_ignoring_pin(running);
    assert(unhashed.pinned && !unhashed.registered && unhashed.exist_refs == 0);
    // A CPU's current task holds no existence reference and is no longer in
    // the registry — the (T6) conjuncts.
    assert(!own_inv(unhashed));
    assert(own_inv(running) && running.pinned && !own_inv(broken_reap_ignoring_pin(running)));
    assert(exists|s: TaskOwn|
        #![trigger broken_reap_ignoring_pin(s)]
        own_inv(s) && s.pinned && !own_inv(broken_reap_ignoring_pin(s)));
    assert forall|s: TaskOwn| (own_inv(s) && s.pinned) implies #[trigger] step(
        s,
        Step::ReapAndRelease,
    ) == s by {}
}

// ---------------------------------------------------------------------------
// Broken variant 5 — a destroy that runs without winning the one-to-zero.
// ---------------------------------------------------------------------------

/// `task_destroy_parked` called on a node that was NOT handed back by a `Some`
/// release — the mistake `KArc::destroy_deferred`'s safety contract exists to
/// forbid. It runs the destructor regardless of who still owns the task.
pub open spec fn broken_destroy_unelected(s: TaskOwn) -> TaskOwn {
    TaskOwn { body_live: false, destroys: (s.destroys + 1) as nat, parked_node: false, ..s }
}

/// Witness that `task_destroy_parked`'s "must be the result of exactly one
/// `Some` release" contract is load-bearing: destroying a task two owners
/// still reference reaches a state with a positive strong count and a dead
/// body — the classic use-after-free — while `DestroyParked` is identity
/// unless a node was actually won.
pub proof fn broken_destroy_without_winning_violates_invariant()
    ensures
        exists|s: TaskOwn|
            #![trigger broken_destroy_unelected(s)]
            own_inv(s) && s.strong > 0 && !own_inv(broken_destroy_unelected(s)),
        forall|s: TaskOwn|
            (own_inv(s) && !s.parked_node) ==> #[trigger] step(s, Step::DestroyParked) == s,
{
    let live = TaskOwn {
        strong: 2,
        containers: 1,
        transient: 1,
        exist_refs: 0,
        exist_flag: false,
        registered: false,
        pinned: false,
        body_live: true,
        parked_node: false,
        destroys: 0,
    };
    assert(own_inv(live));
    let torn = broken_destroy_unelected(live);
    assert(torn.strong == 2 && !torn.body_live);
    // Positive strong count with a destroyed body — the (T4) conjunct.
    assert(!own_inv(torn));
    assert(own_inv(live) && live.strong > 0 && !own_inv(broken_destroy_unelected(live)));
    assert(exists|s: TaskOwn|
        #![trigger broken_destroy_unelected(s)]
        own_inv(s) && s.strong > 0 && !own_inv(broken_destroy_unelected(s)));
    assert forall|s: TaskOwn| (own_inv(s) && !s.parked_node) implies #[trigger] step(
        s,
        Step::DestroyParked,
    ) == s by {}
}

// ---------------------------------------------------------------------------
// Reachability of the flagship broken variant's witness.
// ---------------------------------------------------------------------------

/// Appending one step to a trace applies one `step` to its result. `run` is
/// defined by `drop_last`, so a trace built by `push` needs this to unfold;
/// without it the SMT backend sees `run` of an opaque sequence.
pub proof fn run_push(s: TaskOwn, trace: Seq<Step>, t: Step)
    ensures
        run(s, trace.push(t)) == step(run(s, trace), t),
{
    assert(trace.push(t).len() == trace.len() + 1);
    assert(trace.push(t).last() == t);
    assert(trace.push(t).drop_last() =~= trace);
}

/// The count-elected release's witness state is genuinely reachable, not merely
/// invariant-satisfying: allocate, register, retain into a container, reap,
/// then release the two caller handles. What remains is exactly one reference,
/// parked in a container — the state `broken_release_by_count` frees out from
/// under that container. Exhibiting the trace makes "reachable" machine-checked
/// rather than argued in prose.
pub proof fn broken_release_witness_is_reachable(s0: TaskOwn)
    requires
        own_init(s0),
    ensures
        ({
            let trace = Seq::<Step>::empty().push(Step::RegisterAndPark).push(
                Step::ContainerRetain,
            ).push(Step::ReapAndRelease).push(Step::ReleaseStrongNonFinal).push(
                Step::ReleaseStrongNonFinal,
            );
            let t = run(s0, trace);
            &&& t.strong == 1
            &&& t.containers == 1
            &&& t.transient == 0
            &&& own_inv(t)
            &&& !own_inv(broken_release_by_count(t))
        }),
{
    let empty = Seq::<Step>::empty();
    let registered = empty.push(Step::RegisterAndPark);
    let retained = registered.push(Step::ContainerRetain);
    let reaped = retained.push(Step::ReapAndRelease);
    let put_once = reaped.push(Step::ReleaseStrongNonFinal);
    let put_twice = put_once.push(Step::ReleaseStrongNonFinal);
    assert(empty.len() == 0);
    assert(run(s0, empty) == s0);
    run_push(s0, empty, Step::RegisterAndPark);
    run_push(s0, registered, Step::ContainerRetain);
    run_push(s0, retained, Step::ReapAndRelease);
    run_push(s0, reaped, Step::ReleaseStrongNonFinal);
    run_push(s0, put_once, Step::ReleaseStrongNonFinal);
    invariant_holds_on_every_trace(s0, put_twice);
}

} // verus!
