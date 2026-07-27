//! Coverage for the witness-gated task cells, from outside the crate.
//!
//! `slopos-ostd/src/task/cell.rs` carries the aliasing proofs, because those
//! must call the production `pub(crate) TaskOwnCell::get_ptr` — a shim would be
//! a different function, and the regression being guarded against is precisely
//! a change to that function's return type. What this file covers instead is
//! the surface `sched`/`core` can actually reach: the witness-taking methods on
//! `TaskInner`, which are the only way a `#![forbid(unsafe_code)]` crate writes
//! register-adjacent state after publication.
//!
//! `SwitchWindow` is the witness a host test can mint. `CurrentTask::get()`
//! reads the PCR through `pcr::current_task_id`, which short-circuits on
//! `GS_BASE_SET` and reports `INVALID_TASK_ID` off-kernel — so it is always
//! `None` here, which is itself worth asserting: it is why the switch witness
//! exists as a separate type rather than being folded into the current-task
//! one.
//!
//! `just check-miri` runs with `-Zmiri-ignore-leaks`. That suppresses leak
//! reports only; every property below is a value or borrow-model property, so
//! nothing here is hidden by it.

use slopos_ostd::KArc;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{CurrentTask, SwitchWindow};

type HostTask = TaskInner<(), ()>;

fn fresh() -> KArc<HostTask> {
    KArc::try_new(HostTask::invalid()).expect("task allocation")
}

/// Open a switch window over `task`.
///
/// # Safety
/// Single-threaded host test: this "CPU" performs the switch, holds the only
/// reference to `task`, and the window cannot be re-entered.
fn window(task: &HostTask) -> SwitchWindow<'_, (), ()> {
    unsafe { SwitchWindow::new(task) }
}

/// Without a PCR there is no current task. This is the reason a host test mints
/// a `SwitchWindow` rather than a `CurrentTask`, and the reason the dispatcher
/// needs the former at all: it covers the outgoing task, which is no longer the
/// CPU's current by the time its registers are saved.
#[test]
fn current_task_is_none_without_a_pcr() {
    assert!(CurrentTask::<(), ()>::get().is_none());
}

/// The witnessed write is readable back through a witness for the same task,
/// including its NUL terminator, and the published length agrees with the bytes.
#[test]
fn cwd_round_trips_through_a_witness() {
    let task = fresh();
    let w = window(&task);

    assert!(task.set_cwd(&w, b"/usr/local/share"));
    task.with_cwd(&w, |bytes| {
        assert_eq!(bytes, b"/usr/local/share\0");
    });

    // Overwriting with a shorter path must republish the length, not leave the
    // old tail visible.
    assert!(task.set_cwd(&w, b"/tmp"));
    task.with_cwd(&w, |bytes| {
        assert_eq!(bytes, b"/tmp\0");
    });
}

/// A path that cannot fit with its terminator is refused rather than truncated,
/// and the previous value survives the refusal.
#[test]
fn an_oversized_cwd_is_refused_and_leaves_the_old_value() {
    let task = fresh();
    let w = window(&task);

    assert!(task.set_cwd(&w, b"/keep"));
    let too_long = [b'x'; 256];
    assert!(
        !task.set_cwd(&w, &too_long),
        "256 bytes leaves no room for NUL"
    );
    task.with_cwd(&w, |bytes| assert_eq!(bytes, b"/keep\0"));
}

/// Each task's cell is its own storage. A shared buffer would make a forked
/// child's working directory track its parent's — the failure the fork-time
/// inheritance test guards against from the other side.
#[test]
fn cells_are_per_task() {
    let first = fresh();
    let second = fresh();
    let w1 = window(&first);
    let w2 = window(&second);

    assert!(first.set_cwd(&w1, b"/first"));
    assert!(second.set_cwd(&w2, b"/second"));

    first.with_cwd(&w1, |b| assert_eq!(b, b"/first\0"));
    second.with_cwd(&w2, |b| assert_eq!(b, b"/second\0"));
}

/// Two witnesses for one task may coexist — an interrupt handler above a
/// syscall on the same task — and both authorise the same storage. The aliasing
/// half of this claim is proved in the crate's own `task::cell` unit tests,
/// which can reach `get_ptr`; here it is the observable behaviour that matters.
#[test]
fn two_witnesses_for_one_task_agree() {
    let task = fresh();
    let outer = window(&task);
    let inner = window(&task);

    assert!(task.set_cwd(&outer, b"/via-outer"));
    task.with_cwd(&inner, |b| assert_eq!(b, b"/via-outer\0"));

    assert!(task.set_cwd(&inner, b"/via-inner"));
    task.with_cwd(&outer, |b| assert_eq!(b, b"/via-inner\0"));
}

/// A witness authorises exactly one task. Using one task's witness to write
/// another's state is the mistake the owner check exists to catch, and it must
/// be loud rather than silent — a witness is a safety argument, and one that
/// names the wrong object is not merely wrong, it is unsound.
#[test]
#[should_panic(expected = "witness names a different task")]
fn a_witness_for_another_task_is_refused() {
    let first = fresh();
    let second = fresh();
    let w = window(&first);
    // Debug assertions are on under `cargo test`, which is where this check
    // earns its keep; in release it compiles out and the type-level sealing of
    // `TaskExclusive` is what remains.
    second.set_cwd(&w, b"/nope");
}
